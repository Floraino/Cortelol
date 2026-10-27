"""Offline tests for the standalone local transcription CLI."""

import importlib.util
import io
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
import wave
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "Transcribe-Video.py"
SPEC = importlib.util.spec_from_file_location("transcribe_video", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def segment(start, end, text, words=None, logprob=-0.2):
    return SimpleNamespace(start=start, end=end, text=text, words=words,
                           avg_logprob=logprob, no_speech_prob=0.1,
                           compression_ratio=1.0)


class TimelineTests(unittest.TestCase):
    def test_windows_use_bounded_overlap_and_cover_end(self):
        self.assertEqual(list(MODULE.windows(620, 300, 2)), [
            (0.0, 300.0, 0.0, 302.0),
            (300.0, 600.0, 298.0, 602.0),
            (600.0, 620, 598.0, 620),
        ])

    def test_boundary_words_appear_once_and_keep_source_time(self):
        words = [SimpleNamespace(start=1.0, end=2.0, word=" before"),
                 SimpleNamespace(start=2.0, end=3.0, word=" after")]
        raw = [segment(1.0, 3.0, " before after", words)]
        first = MODULE.decode_segments(raw, 10, 298, 0, 300, 600, "en")
        second = MODULE.decode_segments(raw, 10, 298, 300, 600, 600, "en")
        self.assertEqual([item["text"] for item in first + second], ["before", "after"])
        self.assertEqual([item["start"] for item in first + second], [299.0, 300.0])

    def test_invalid_timestamp_is_rejected(self):
        for start, end in [(float("nan"), 2), (3, 2), (-2, 1), (0, 20)]:
            with self.subTest(start=start, end=end), self.assertRaises(ValueError):
                MODULE.check_interval(start, end, 10)

    def test_desktop_word_ownership_at_900_seconds_precedes_phrase_grouping(self):
        first_words = [SimpleNamespace(start=899, end=899.8, word=" before"),
                       SimpleNamespace(start=899.8, end=900.4, word=" crossing"),
                       SimpleNamespace(start=900.4, end=901, word=" after")]
        second_words = [SimpleNamespace(start=word.start - 898, end=word.end - 898, word=word.word)
                        for word in first_words]
        left = MODULE.decode_segments([segment(899, 901, " before crossing after", first_words)],
                                      902, 0, 0, 900, 1800, "pt", True)
        right = MODULE.decode_segments([segment(1, 3, " before crossing after", second_words)],
                                       904, 898, 900, 1800, 1800, "pt", True)
        self.assertEqual(" ".join(item["text"] for item in left + right), "before crossing after")
        self.assertEqual(left[0]["end"], 899.8)
        self.assertEqual(right[0]["start"], 899.8)
        self.assertEqual(right[0]["end"], 901)
        with self.assertRaisesRegex(ValueError, "word timestamps"):
            MODULE.decode_segments([segment(899, 901, " no words")],
                                   902, 0, 0, 900, 1800, "pt", True)

    def test_quality_gaps_and_review_flags(self):
        raw = [segment(1, 3, " uncertain", logprob=-1.4)]
        decoded = MODULE.decode_segments(raw, 10, 0, 0, 10, 10, "pt")
        self.assertIn("low_model_confidence", decoded[0]["review_reasons"])
        self.assertIn("no_word_timestamps", decoded[0]["review_reasons"])
        quality = MODULE.validate_timeline(decoded, 10)
        self.assertEqual(quality["timed_span_fraction"], 0.2)
        self.assertEqual(quality["gaps_at_least_5_seconds"], [{"start": 3.0, "end": 10}])

    def test_wer_is_optional_and_uses_unicode_casefold(self):
        self.assertEqual(MODULE.word_error_rate("Olá mundo", "olá Mundo"),
                         {"errors": 0, "reference_words": 2, "wer": 0.0})
        self.assertEqual(MODULE.word_error_rate("one two three", "one four three")["wer"], 0.3333)
        with self.assertRaisesRegex(ValueError, "bounded"):
            MODULE.word_error_rate("word " * 2001, "word " * 2001)

    def test_overlaps_are_counted_without_double_counting_timed_span(self):
        quality = MODULE.validate_timeline([
            {"start": 1, "end": 3}, {"start": 2, "end": 4},
        ], 10)
        self.assertEqual(quality["overlapping_segment_count"], 1)
        self.assertEqual(quality["timed_span_fraction"], 0.3)

    def test_srt_time_rounds_across_second(self):
        self.assertEqual(MODULE.srt_time(59.9996), "00:01:00,000")


class OfflineWorkflowTests(unittest.TestCase):
    def test_prepared_wav_uses_no_subprocess_filters_core_and_emits_progress(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "prepared.wav"
            with wave.open(str(source), "wb") as audio:
                audio.setnchannels(1)
                audio.setsampwidth(2)
                audio.setframerate(16_000)
                audio.writeframes(b"\0\0" * 160_000)
            original = source.read_bytes()
            model = root / "model"
            model.mkdir()
            for name in ("config.json", "tokenizer.json", "model.bin"):
                (model / name).write_bytes(b"fixture")

            class FakeModel:
                def __init__(self, *args, **kwargs):
                    assert kwargs["local_files_only"] is True

                def transcribe(self, wav, **kwargs):
                    words = [SimpleNamespace(start=1, end=2, word=" before"),
                             SimpleNamespace(start=3, end=4, word=" owned"),
                             SimpleNamespace(start=8, end=9, word=" after")]
                    return iter([segment(1, 9, " before owned after", words)]), SimpleNamespace(language="pt")

            progress = io.StringIO()
            with patch.dict(sys.modules, {"faster_whisper": SimpleNamespace(WhisperModel=FakeModel)}), \
                 patch.object(MODULE, "command_output", side_effect=AssertionError("No subprocess allowed")), \
                 redirect_stdout(progress):
                report = MODULE.transcribe(source, model, root / "result", ffmpeg="missing",
                    ffprobe="missing", chunk_seconds=10, overlap_seconds=2, language="pt",
                    device="cpu", compute_type="int8", reference=None, timeout=30,
                    prepared_wav=True, progress_json=True, ownership_start=2, ownership_end=8)
            output = json.loads((root / "result.json").read_text(encoding="utf-8"))
            self.assertEqual([item["text"] for item in output["segments"]], ["owned"])
            self.assertEqual(json.loads(progress.getvalue()),
                             {"type": "progress", "completed_chunks": 1, "total_chunks": 1})
            self.assertEqual(report["runtime"]["engine"], "faster-whisper")
            self.assertEqual(source.read_bytes(), original)

    @unittest.skipUnless(shutil.which("ffmpeg") and shutil.which("ffprobe"), "FFmpeg tools unavailable")
    def test_video_longer_than_audio_does_not_schedule_empty_tail(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "long_video_short_audio.mp4"
            subprocess.run([
                "ffmpeg", "-v", "error", "-y",
                "-f", "lavfi", "-i", "color=c=black:s=16x16:r=1:d=14",
                "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000:duration=3",
                "-map", "0:v:0", "-map", "1:a:0", "-c:v", "mpeg4", "-c:a", "aac",
                str(source),
            ], check=True, capture_output=True, timeout=30)
            container = json.loads(MODULE.command_output([
                "ffprobe", "-v", "error", "-show_entries", "format=duration",
                "-of", "json", str(source),
            ], 30))["format"]["duration"]
            self.assertGreater(float(container), 13)
            self.assertAlmostEqual(MODULE.probe_duration(source, "ffprobe"), 3, delta=0.1)
            original = source.read_bytes()
            model_dir = root / "model"
            model_dir.mkdir()
            (model_dir / "config.json").write_text("{}", encoding="utf-8")
            (model_dir / "tokenizer.json").write_text("{}", encoding="utf-8")

            class FakeModel:
                def __init__(self, *args, **kwargs):
                    pass

                def transcribe(self, wav, **kwargs):
                    word = SimpleNamespace(start=1.0, end=2.0, word=" spoken")
                    return iter([segment(1, 2, " spoken", [word])]), SimpleNamespace(language="en")

            with patch.dict(sys.modules, {"faster_whisper": SimpleNamespace(WhisperModel=FakeModel)}):
                for name, fallback_duration in (("measured", None), ("fallback", 14.0)):
                    with self.subTest(mode=name):
                        if fallback_duration is None:
                            duration_patch = patch.object(MODULE, "probe_duration", wraps=MODULE.probe_duration)
                        else:
                            duration_patch = patch.object(MODULE, "probe_duration", return_value=fallback_duration)
                        with duration_patch:
                            report = MODULE.transcribe(source, model_dir, root / name,
                                                       ffmpeg="ffmpeg", ffprobe="ffprobe",
                                                       chunk_seconds=10, overlap_seconds=2,
                                                       language=None, device="cpu",
                                                       compute_type="int8", reference=None,
                                                       timeout=30)
                        self.assertEqual(report["chunk_count"], 1)
                        self.assertAlmostEqual(report["duration_seconds"], 3, delta=0.1)
                        transcript = json.loads((root / f"{name}.json").read_text(encoding="utf-8"))
                        self.assertEqual(len(transcript["segments"]), 1)
            self.assertEqual(source.read_bytes(), original)

    @unittest.skipUnless(shutil.which("ffmpeg") and shutil.which("ffprobe"), "FFmpeg tools unavailable")
    def test_real_ffmpeg_probe_and_bounded_pcm_extraction(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.wav"
            with wave.open(str(source), "wb") as audio:
                audio.setnchannels(1)
                audio.setsampwidth(2)
                audio.setframerate(16_000)
                audio.writeframes(b"\0\0" * 32_000)
            before = source.read_bytes()
            self.assertAlmostEqual(MODULE.probe_duration(source, "ffprobe"), 2.0)
            extracted = MODULE.extract_chunk(source, root / "chunk.wav", 0.5, 1.0,
                                             "ffmpeg", 30)
            self.assertAlmostEqual(extracted, 1.0, delta=0.01)
            self.assertEqual(source.read_bytes(), before)

    def test_writes_json_srt_quality_without_real_model_or_ffmpeg(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "recording.mp4"
            source.write_bytes(b"not real media")
            model_dir = root / "model"
            model_dir.mkdir()
            (model_dir / "config.json").write_text("{}", encoding="utf-8")
            (model_dir / "tokenizer.json").write_text("{}", encoding="utf-8")
            reference = root / "reference.txt"
            reference.write_text("hello world", encoding="utf-8")
            output = root / "result"
            calls = []

            class FakeModel:
                def __init__(self, path, **kwargs):
                    self.assertion = (path, kwargs)
                    self_count = len(calls)
                    calls.append(("load", self_count, kwargs))

                def transcribe(self, wav, **kwargs):
                    calls.append(("transcribe", wav, kwargs))
                    word = SimpleNamespace(start=1.0, end=2.0, word=" hello world")
                    return iter([segment(1, 2, " hello world", [word])]), SimpleNamespace(language="en")

            fake_module = SimpleNamespace(WhisperModel=FakeModel)
            with patch.dict(sys.modules, {"faster_whisper": fake_module}), \
                 patch.object(MODULE, "probe_duration", return_value=10), \
                 patch.object(MODULE, "extract_chunk", return_value=10):
                report = MODULE.transcribe(source, model_dir, output, ffmpeg="ffmpeg",
                                           ffprobe="ffprobe", chunk_seconds=300,
                                           overlap_seconds=2, language=None, device="cpu",
                                           compute_type="int8", reference=reference, timeout=30)
            self.assertEqual(report["reference_wer"]["wer"], 0)
            self.assertEqual(json.loads((root / "result.json").read_text(encoding="utf-8"))["segments"][0]["start"], 1)
            self.assertIn("00:00:01,000 --> 00:00:02,000", (root / "result.srt").read_text(encoding="utf-8"))
            self.assertEqual(json.loads((root / "result.quality.json").read_text(encoding="utf-8"))["segment_count"], 1)
            self.assertTrue(calls[0][2]["local_files_only"])

    def test_source_cannot_be_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "recording.json"
            source.write_text("original", encoding="utf-8")
            model = root / "model"
            model.mkdir()
            (model / "config.json").write_text("{}", encoding="utf-8")
            (model / "tokenizer.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "overwrite"):
                MODULE.transcribe(source, model, root / "recording", ffmpeg="ffmpeg",
                                  ffprobe="ffprobe", chunk_seconds=300,
                                  overlap_seconds=2, language=None, device="cpu",
                                  compute_type="int8", reference=None, timeout=30)
            self.assertEqual(source.read_text(encoding="utf-8"), "original")


if __name__ == "__main__":
    unittest.main()
