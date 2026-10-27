#!/usr/bin/env python3
"""Transcribe a local recording with an already installed faster-whisper model."""

from __future__ import annotations

import argparse
from importlib import metadata
from fractions import Fraction
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unicodedata
import wave
from typing import Any


SAMPLE_RATE = 16_000
MAX_DURATION = 12 * 60 * 60
MAX_CHUNK_SECONDS = 600
MAX_SEGMENTS_PER_CHUNK = 2_000
MAX_SEGMENT_CHARS = 2_000
MAX_WER_CELLS = 4_000_000


def runtime_versions() -> dict[str, str]:
    def version(package: str) -> str:
        try:
            return metadata.version(package)
        except metadata.PackageNotFoundError:
            return "unknown"
    return {"engine": "faster-whisper", "engine_version": version("faster-whisper"),
            "ctranslate2_version": version("ctranslate2"),
            "python_version": sys.version.split()[0]}


def command_output(args: list[str], timeout: int) -> str:
    flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    try:
        result = subprocess.run(
            args, capture_output=True, text=True, encoding="utf-8", errors="replace",
            timeout=timeout, check=False, creationflags=flags,
        )
    except FileNotFoundError as exc:
        raise ValueError(f"Required executable is missing: {args[0]}") from exc
    except subprocess.TimeoutExpired as exc:
        raise ValueError(f"{args[0]} exceeded its {timeout}-second timeout") from exc
    if result.returncode != 0:
        detail = result.stderr.strip()[-500:] or f"exit code {result.returncode}"
        raise ValueError(f"{args[0]} failed: {detail}")
    return result.stdout


def probe_duration(source: Path, ffprobe: str) -> float:
    result = json.loads(command_output([
        ffprobe, "-v", "error", "-show_entries",
        "format=duration:stream=codec_type,duration,duration_ts,time_base",
        "-of", "json", str(source),
    ], 60))
    audio = next((stream for stream in result.get("streams", [])
                  if stream.get("codec_type") == "audio"), None)
    if audio is None:
        raise ValueError("The input has no audio stream")

    def positive_duration(value: Any) -> float | None:
        try:
            number = float(value)
        except (TypeError, ValueError):
            return None
        return number if math.isfinite(number) and number > 0 else None

    duration = positive_duration(audio.get("duration"))
    if duration is None:
        try:
            duration = positive_duration(float(audio["duration_ts"]) *
                                         float(Fraction(audio["time_base"])))
        except (KeyError, ValueError, TypeError, ZeroDivisionError):
            pass
    if duration is None:
        duration = positive_duration(result.get("format", {}).get("duration"))
    if duration is None:
        raise ValueError("Audio duration is unavailable")
    if not math.isfinite(duration) or duration <= 0 or duration > MAX_DURATION:
        raise ValueError(f"Duration must be between 0 and {MAX_DURATION} seconds")
    return duration


def extract_chunk(source: Path, wav_path: Path, start: float, length: float,
                  ffmpeg: str, timeout: int) -> float:
    command_output([
        ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "error", "-y",
        "-ss", f"{start:.3f}", "-i", str(source), "-t", f"{length:.3f}",
        "-map", "0:a:0", "-vn", "-ac", "1", "-ar", str(SAMPLE_RATE),
        "-c:a", "pcm_s16le", str(wav_path),
    ], timeout)
    with wave.open(str(wav_path), "rb") as audio:
        if (audio.getnchannels(), audio.getframerate(), audio.getsampwidth()) != (1, SAMPLE_RATE, 2):
            raise ValueError("Extracted audio is not 16 kHz mono PCM16")
        actual = audio.getnframes() / SAMPLE_RATE
    if actual > length + 0.1:
        raise ValueError("Extracted chunk duration is invalid")
    return actual


def prepared_wav_duration(source: Path) -> float:
    """Desktop bridge input: bounded PCM prepared by cancellable Rust FFmpeg."""
    with wave.open(str(source), "rb") as audio:
        if (audio.getnchannels(), audio.getframerate(), audio.getsampwidth(),
                audio.getcomptype()) != (1, SAMPLE_RATE, 2, "NONE"):
            raise ValueError("Prepared input must be 16 kHz mono PCM16 WAV")
        duration = audio.getnframes() / SAMPLE_RATE
    if not 0 < duration <= 1804:
        raise ValueError("Prepared WAV must contain at most 1804 seconds of audio")
    return duration


def slice_prepared_wav(source: Path, destination: Path, start: float, length: float) -> float:
    with wave.open(str(source), "rb") as audio:
        audio.setpos(round(start * SAMPLE_RATE))
        frames = audio.readframes(round(length * SAMPLE_RATE))
    with wave.open(str(destination), "wb") as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(SAMPLE_RATE)
        output.writeframes(frames)
    return len(frames) / (SAMPLE_RATE * 2)


def windows(duration: float, chunk_seconds: int, overlap_seconds: float):
    core_start = 0.0
    while core_start < duration:
        core_end = min(duration, core_start + chunk_seconds)
        audio_start = max(0.0, core_start - overlap_seconds)
        audio_end = min(duration, core_end + overlap_seconds)
        yield core_start, core_end, audio_start, audio_end
        core_start = core_end


def check_interval(start: Any, end: Any, maximum: float) -> tuple[float, float]:
    try:
        start, end = float(start), float(end)
    except (TypeError, ValueError) as exc:
        raise ValueError("ASR returned a nonnumeric timestamp") from exc
    if (not all(math.isfinite(v) for v in (start, end)) or start < -0.05
            or end <= start or end > maximum + 0.15
            or min(maximum, end) <= max(0.0, start)):
        raise ValueError(f"ASR returned an invalid interval: {start}, {end}, audio duration {maximum}")
    return max(0.0, start), min(maximum, end)


def decode_segments(raw_segments: Any, actual_duration: float, audio_start: float,
                    core_start: float, core_end: float, media_duration: float,
                    language: str | None, require_word_timestamps: bool = False) -> list[dict[str, Any]]:
    output: list[dict[str, Any]] = []
    previous_start = -1.0
    for count, segment in enumerate(raw_segments, 1):
        if count > MAX_SEGMENTS_PER_CHUNK:
            raise ValueError("ASR returned too many segments for one chunk")
        local_start, local_end = check_interval(segment.start, segment.end, actual_duration)
        if local_start + 0.05 < previous_start:
            raise ValueError("ASR segment timestamps run backward")
        previous_start = local_start
        raw_words = getattr(segment, "words", None)
        if require_word_timestamps and not raw_words:
            raise ValueError("Desktop transcription requires word timestamps for chunk ownership")
        pieces: list[tuple[float, float, str]] = []
        if raw_words:
            previous_word_start = -1.0
            for word_count, word in enumerate(raw_words, 1):
                if word_count > 10_000:
                    raise ValueError("ASR returned too many words for one segment")
                word_start, word_end = check_interval(word.start, word.end, actual_duration)
                if word_start < previous_word_start:
                    raise ValueError("ASR word timestamps run backward")
                previous_word_start = word_start
                midpoint = audio_start + (word_start + word_end) / 2
                if core_start <= midpoint < core_end or (core_end == media_duration and core_start <= midpoint <= core_end):
                    pieces.append((word_start, word_end, str(word.word)))
        else:
            midpoint = audio_start + (local_start + local_end) / 2
            if core_start <= midpoint < core_end or (core_end == media_duration and core_start <= midpoint <= core_end):
                pieces.append((local_start, local_end, str(segment.text)))
        if not pieces:
            continue
        text = " ".join("".join(piece[2] for piece in pieces).split())
        if not text or len(text) > MAX_SEGMENT_CHARS:
            raise ValueError("ASR returned empty or oversized segment text")
        start = audio_start + pieces[0][0]
        end = audio_start + pieces[-1][1]
        if start < -0.05 or end > media_duration + 0.15 or end <= start:
            raise ValueError("ASR timestamp falls outside the source recording")
        reasons = []
        avg_logprob = getattr(segment, "avg_logprob", None)
        no_speech_prob = getattr(segment, "no_speech_prob", None)
        compression_ratio = getattr(segment, "compression_ratio", None)
        if avg_logprob is not None and float(avg_logprob) < -1:
            reasons.append("low_model_confidence")
        if no_speech_prob is not None and float(no_speech_prob) > 0.6:
            reasons.append("possible_nonspeech")
        if compression_ratio is not None and float(compression_ratio) > 2.4:
            reasons.append("possible_repetition")
        if not raw_words:
            reasons.append("no_word_timestamps")
        if (core_start > 0 and abs(start - core_start) < 1) or (core_end < media_duration and abs(end - core_end) < 1):
            reasons.append("chunk_boundary")
        output.append({"start": round(max(0.0, start), 3),
                       "end": round(min(media_duration, end), 3), "text": text,
                       "language": language, "review_reasons": reasons})
    return output


def validate_timeline(segments: list[dict[str, Any]], duration: float) -> dict[str, Any]:
    intervals = []
    last_start = -1.0
    overlaps = 0
    previous_end = -1.0
    for segment in segments:
        start, end = check_interval(segment["start"], segment["end"], duration)
        if start + 0.05 < last_start:
            raise ValueError("Combined transcript timestamps run backward")
        if start < previous_end - 0.05:
            overlaps += 1
        last_start = start
        previous_end = max(previous_end, end)
        intervals.append((start, end))
    merged: list[list[float]] = []
    for start, end in intervals:
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    timed = sum(end - start for start, end in merged)
    gaps = []
    cursor = 0.0
    for start, end in merged:
        if start - cursor >= 5:
            gaps.append({"start": round(cursor, 3), "end": round(start, 3)})
        cursor = end
    if duration - cursor >= 5:
        gaps.append({"start": round(cursor, 3), "end": round(duration, 3)})
    return {"timed_span_fraction": round(timed / duration, 4),
            "overlapping_segment_count": overlaps,
            "gaps_at_least_5_seconds": gaps}


def tokens(text: str) -> list[str]:
    normalized = unicodedata.normalize("NFKC", text).casefold()
    return re.findall(r"[^\W_]+(?:['’][^\W_]+)*", normalized, re.UNICODE)


def word_error_rate(reference: str, hypothesis: str) -> dict[str, Any]:
    expected, actual = tokens(reference), tokens(hypothesis)
    if not expected:
        raise ValueError("Reference has no words after normalization")
    if len(expected) * len(actual) > MAX_WER_CELLS:
        raise ValueError("Reference WER exceeds the bounded comparison limit")
    previous = list(range(len(actual) + 1))
    for index, word in enumerate(expected, 1):
        current = [index]
        for column, observed in enumerate(actual, 1):
            current.append(min(current[-1] + 1, previous[column] + 1,
                               previous[column - 1] + (word != observed)))
        previous = current
    errors = previous[-1]
    return {"errors": errors, "reference_words": len(expected),
            "wer": round(errors / len(expected), 4)}


def srt_time(seconds: float) -> str:
    millis = round(seconds * 1000)
    hours, remainder = divmod(millis, 3_600_000)
    minutes, remainder = divmod(remainder, 60_000)
    secs, ms = divmod(remainder, 1000)
    return f"{hours:02}:{minutes:02}:{secs:02},{ms:03}"


def srt_text(segments: list[dict[str, Any]]) -> str:
    return "".join(f"{number}\n{srt_time(item['start'])} --> {srt_time(item['end'])}\n"
                   f"{item['text']}\n\n" for number, item in enumerate(segments, 1))


def atomic_write(destination: Path, content: str) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", newline="\n",
                                         dir=destination.parent, prefix=destination.name + ".",
                                         suffix=".tmp", delete=False) as handle:
            temporary = Path(handle.name)
            handle.write(content)
        os.replace(temporary, destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def transcribe(source: Path, model_path: Path, output_prefix: Path, *, ffmpeg: str,
               ffprobe: str, chunk_seconds: int, overlap_seconds: float,
               language: str | None, device: str, compute_type: str,
               reference: Path | None, timeout: int, prepared_wav: bool = False,
               progress_json: bool = False, ownership_start: float = 0.0,
               ownership_end: float | None = None) -> dict[str, Any]:
    if not source.is_file():
        raise ValueError("Input recording does not exist")
    if not model_path.is_dir() or not (model_path / "config.json").is_file() or not (model_path / "tokenizer.json").is_file():
        raise ValueError("Model must be an existing local faster-whisper directory with config.json and tokenizer.json")
    if not 10 <= chunk_seconds <= MAX_CHUNK_SECONDS or not 0 <= overlap_seconds < chunk_seconds / 2:
        raise ValueError("Chunk must be 10–600 seconds; overlap must be nonnegative and less than half a chunk")
    if language is not None and not re.fullmatch(r"[a-z]{2,3}", language):
        raise ValueError("Language must be a lowercase two- or three-letter speech language code")
    destinations = [Path(str(output_prefix) + extension) for extension in (".json", ".srt", ".quality.json")]
    if source.resolve() in (path.resolve() for path in destinations):
        raise ValueError("Output path would overwrite the source recording")
    duration = prepared_wav_duration(source) if prepared_wav else probe_duration(source, ffprobe)
    ownership_end = duration if ownership_end is None else ownership_end
    if (not math.isfinite(ownership_start) or not math.isfinite(ownership_end)
            or ownership_start < 0 or ownership_end <= ownership_start
            or ownership_start > duration or ownership_end > duration + 0.002):
        raise ValueError("Ownership range must be inside the prepared audio")
    try:
        from faster_whisper import WhisperModel
    except ImportError as exc:
        raise ValueError("Install the optional faster-whisper dependency; see scripts/LOCAL_ASR.md") from exc
    model = WhisperModel(str(model_path), device=device, compute_type=compute_type,
                         local_files_only=True)
    all_segments: list[dict[str, Any]] = []
    detected_languages: set[str] = set()
    count = 0
    observed_audio_end = 0.0
    total_chunks = math.ceil(duration / chunk_seconds)
    for count, (core_start, core_end, audio_start, audio_end) in enumerate(
        windows(duration, chunk_seconds, overlap_seconds), 1
    ):
        print(f"Chunk {count}: {core_start:.1f}-{core_end:.1f}s", file=sys.stderr)
        with tempfile.TemporaryDirectory(prefix="cortelol-asr-") as temporary:
            wav_path = Path(temporary) / "chunk.wav"
            actual = (slice_prepared_wav(source, wav_path, audio_start, audio_end - audio_start)
                      if prepared_wav else extract_chunk(source, wav_path, audio_start,
                                                        audio_end - audio_start, ffmpeg, timeout))
            if actual == 0:
                if observed_audio_end == 0:
                    raise ValueError("The audio stream has no decodable samples")
                duration = observed_audio_end
                count -= 1
                break
            observed_audio_end = max(observed_audio_end, audio_start + actual)
            ended_early = actual < audio_end - audio_start - 0.1
            if ended_early:
                duration = observed_audio_end
                core_end = duration
            raw, info = model.transcribe(str(wav_path), language=language, task="transcribe",
                                         word_timestamps=True, vad_filter=True,
                                         condition_on_previous_text=False)
            detected = getattr(info, "language", None)
            if isinstance(detected, str):
                detected_languages.add(detected)
            owned_start = max(core_start, ownership_start)
            owned_end = min(core_end, ownership_end)
            if owned_end > owned_start:
                # Assign individual words before grouping into the segment's text.
                # This prevents phrase midpoints from dropping boundary words.
                all_segments.extend(decode_segments(raw, actual, audio_start, owned_start,
                                                    owned_end, duration, detected,
                                                    require_word_timestamps=prepared_wav))
            if progress_json:
                print(json.dumps({"type": "progress", "completed_chunks": count,
                                  "total_chunks": total_chunks}), flush=True)
            if ended_early:
                break
    all_segments.sort(key=lambda item: (item["start"], item["end"]))
    timeline = validate_timeline(all_segments, duration)
    quality: dict[str, Any] = {
        "runtime": runtime_versions(),
        "duration_seconds": round(duration, 3), "chunk_count": count,
        "segment_count": len(all_segments), "detected_languages": sorted(detected_languages),
        "review_segment_count": sum(bool(item["review_reasons"]) for item in all_segments),
        "review_segments": [index for index, item in enumerate(all_segments, 1) if item["review_reasons"]],
        **timeline,
        "notes": ["Timed span is not speech recall; silence can lower it.",
                  "Gaps and confidence flags are review prompts, not proof of errors."],
    }
    if reference:
        try:
            if reference.stat().st_size > 1_000_000:
                raise ValueError("Reference text exceeds 1 MB")
            quality["reference_wer"] = word_error_rate(reference.read_text(encoding="utf-8"),
                                                        " ".join(item["text"] for item in all_segments))
        except ValueError as exc:
            quality["reference_wer"] = {"status": "unavailable", "reason": str(exc)}
    transcript = {"schema_version": 1, "source": str(source.resolve()),
                  "duration_seconds": round(duration, 3), "model_path": str(model_path.resolve()),
                  "segments": all_segments}
    atomic_write(destinations[0], json.dumps(transcript, ensure_ascii=False, indent=2) + "\n")
    atomic_write(destinations[1], srt_text(all_segments))
    atomic_write(destinations[2], json.dumps(quality, ensure_ascii=False, indent=2) + "\n")
    return quality


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path, help="Local audio or video recording")
    parser.add_argument("--model", required=True, type=Path, help="Existing local faster-whisper model directory")
    parser.add_argument("--output-prefix", type=Path, help="Prefix for .json, .srt, and .quality.json")
    parser.add_argument("--language", help="Optional ISO 639-1 speech language code; omit for per-chunk detection")
    parser.add_argument("--device", choices=("cpu", "cuda", "auto"), default="cpu")
    parser.add_argument("--compute-type", default="int8")
    parser.add_argument("--chunk-seconds", type=int, default=300)
    parser.add_argument("--overlap-seconds", type=float, default=2)
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--ffprobe", default="ffprobe")
    parser.add_argument("--timeout", type=int, default=1800, help="Per-FFmpeg-chunk timeout in seconds")
    parser.add_argument("--reference", type=Path, help="Optional UTF-8 reference text for WER")
    parser.add_argument("--prepared-wav", action="store_true",
                        help="Use a bounded 16 kHz mono PCM16 WAV without spawning media tools")
    parser.add_argument("--progress-json", action="store_true", help="Emit JSON progress lines on stdout")
    parser.add_argument("--ownership-start", type=float, default=0.0,
                        help="Exclusive chunk ownership starts here in prepared-WAV seconds")
    parser.add_argument("--ownership-end", type=float,
                        help="Exclusive chunk ownership ends here in prepared-WAV seconds")
    args = parser.parse_args(argv)
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    prefix = args.output_prefix or args.input.with_suffix("")
    try:
        report = transcribe(args.input, args.model, prefix, ffmpeg=args.ffmpeg,
                            ffprobe=args.ffprobe, chunk_seconds=args.chunk_seconds,
                            overlap_seconds=args.overlap_seconds, language=args.language,
                            device=args.device, compute_type=args.compute_type,
                            reference=args.reference, timeout=args.timeout,
                            prepared_wav=args.prepared_wav, progress_json=args.progress_json,
                            ownership_start=args.ownership_start, ownership_end=args.ownership_end)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"Transcription failed: {exc}", file=sys.stderr)
        return 1
    print(f"Wrote {report['segment_count']} timed segments to {prefix}.json", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
