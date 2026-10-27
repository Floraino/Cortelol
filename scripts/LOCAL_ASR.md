# Local video transcription script

`Transcribe-Video.py` is an optional command-line tool and the embedded local ASR engine used by `src-tauri/src/local_asr/`. Audio, model inference, and output remain local. The script makes no paid provider call. Python, faster-whisper, and a model are **not bundled or downloaded** by the desktop bridge. The desktop component and command registration must be connected by the application shell as described below.

## Setup

Use Python 3.10 or newer. Install FFmpeg and FFprobe on `PATH` and install the optional Python dependency:

```powershell
python -m pip install -r scripts/requirements-local-asr.txt
```

Prepare a **multilingual CTranslate2 faster-whisper model directory** yourself, outside the repository. It must include `config.json`, `model.bin`, and `tokenizer.json`. The script accepts only an existing directory and passes `local_files_only=True`; it will not fetch a model. Choose a multilingual model rather than an English-only `.en` model for non-English recordings. See the [faster-whisper documentation](https://github.com/SYSTRAN/faster-whisper#usage) for model format and device support. Model size, CPU/GPU hardware, accent, background noise, and language switching affect speed and accuracy.

```powershell
python scripts/Transcribe-Video.py "C:\recordings\episode.mp4" `
  --model "C:\models\faster-whisper-medium" `
  --output-prefix "C:\transcripts\episode"
```

Optional flags: `--language pt` locks a language (omit it for detection in each chunk); `--device cuda --compute-type float16` uses a compatible GPU; `--chunk-seconds 300 --overlap-seconds 2` changes bounded extraction; `--reference reference.txt` calculates word error rate against a UTF-8 human reference. Run `python scripts/Transcribe-Video.py --help` for all options.

The script creates `episode.json` with source-time segments and review reasons, `episode.srt`, and `episode.quality.json`. The quality report lists flagged segment numbers, detected chunk languages, overlaps, gaps of at least five seconds, and the fraction of recording time covered by timestamped segments. A gap may simply be silence; that fraction is **not** speech recall or a proof of transcript completeness. WER appears only when a reference is supplied and the exact comparison fits the bounded limit of four million word-pair cells; otherwise the report explains why it is unavailable. Its simple Unicode word tokenization is most useful for space-delimited languages; use a language-appropriate tokenizer or character error rate for scripts without reliable word boundaries.

## Limits and review

Only one mono 16 kHz PCM chunk is extracted to a temporary directory at a time. The default core chunk is five minutes, with two seconds of overlap for boundary context; word midpoints assign words to one chunk. The tool supports recordings up to 12 hours, chunks up to 10 minutes, and a bounded number of segments per chunk. It validates numeric timestamps, order, duration, audio format, and output paths before writing. Each output uses a temporary file and atomic replacement. Intermediate extraction is removed after each chunk, including on failure. Keep enough free disk space for one WAV chunk and the outputs.

Low model confidence, likely nonspeech, repetition, missing word timestamps, and chunk boundaries are marked for human review. These heuristics can miss errors and can flag correct speech. Listen against the source, especially at long gaps, language changes, names, numbers, and chunk boundaries. SRT is a starting point for editing, not a verified caption track. The source recording is only read and never modified. The test suite makes no model download, ASR model invocation, or network/provider call. FFmpeg and FFprobe are required for the generated-media tests; those tests skip when the tools are unavailable:

```powershell
python -m unittest discover -s scripts/tests -p "test_*.py"
```

## Desktop integration contract

The bridge embeds the script at Rust compile time, writes a private staged copy, and invokes the selected Python executable with an argument array (`-I -u -X utf8`), without a shell. Select the actual Python executable from an installed runtime or virtual environment; an activated shell or `py` launcher is not required. Isolated mode ignores `PYTHONPATH` and user-site packages: install faster-whisper in the selected runtime or virtual environment itself. Setup checks the executable, Python 3.10+, dependency imports, and the three model files; the first inference verifies model compatibility. Desktop inference currently uses **CPU int8 only**, with a six-hour timeout per 15-minute parent chunk and a 30-second dependency-check timeout. Standalone GPU flags remain available in the CLI.

Rust uses the same 15-minute/two-second-overlap job plan as hosted providers and claims the shared transcription runtime gate. Completed chunks and corrections are retained. FFmpeg extraction runs in Rust with cancellation. Python receives `--prepared-wav --progress-json --ownership-start ... --ownership-end ...`, reads bounded 16 kHz mono PCM16 audio, and slices its five-minute windows without spawning children. Individual word midpoints determine core ownership **before** phrase grouping; missing word timestamps fail the desktop chunk instead of guessing phrase ownership. Rust revalidates output schema, source/model identity, durations, ordering, text bounds, and quality-report consistency before applying the source offset and saving a chunk. Source media is never overwritten. Retry re-evaluates zero-length audio tails, including after a hosted-provider handoff, so it does not require an in-memory end-of-audio marker.

Cancellation kills and awaits the direct Python process or Rust-owned FFmpeg process. `TEMP`, `TMP`, and `TMPDIR` point into the Rust staging directory so temporary Python WAVs are removed when the process stops. Completed database chunks survive cancellation or restart; an interrupted parent chunk is recomputed. A model is loaded again for each pending parent chunk, which adds overhead. Abrupt process/OS termination can leave cache staging directories; they contain generated audio rather than source media or user exports.

Before completing a database chunk, the bridge atomically writes a receipt under the application's **data** directory, `local-asr-receipts/<source-identity-hash>/chunk-N.json`. Receipts include the original quality report, source-time review ranges, Python/faster-whisper/CTranslate2 versions, and a model identifier. The model identifier fingerprints config/tokenizer content plus model.bin size and modification time; it is **not a complete model.bin content hash** or a security attestation. Source identity uses canonical path, file size, and modification time. Only receipts for completed database chunks with `local` attribution are exposed. Orphans from a failed database commit are ignored. Receipts are kept outside disposable audio cache and contain no Python/model paths in the frontend response. Missing or unreadable receipts are reported explicitly; they do not erase the transcript. Original receipt flags describe ASR output, not later corrected text. Gap counts include context windows and may count the same silence more than once.

The desktop app registers `mod local_asr`, manages `local_asr::LocalAsrRuntime::default()`, and exposes these Tauri commands through `src-tauri/src/lib.rs`:

| Command | JavaScript arguments | Result |
| --- | --- | --- |
| `validate_local_transcription_setup` | `{ request: { pythonPath, modelPath, language: null \| "pt" } }` | `void`; throws an actionable setup error |
| `transcribe_local_recent` | `{ request: { pythonPath, modelPath, language }, sourcePath }` | `LocalAsrSummary`; resolves after completion, rejects on failure/cancel |
| `get_local_transcription_status` | none | `LocalAsrStatus` progress snapshot, including current source path |
| `cancel_local_transcription` | none | `void`; requests cancellation |
| `get_local_transcription_report` | `{ sourcePath }` | `{ chunks: LocalChunkReceipt[], missingChunks: number[] }`; zero-based chunk indices |

`LocalTranscriptionSetup.tsx` exports the typed requests, results and component props. Mount it with the imported source's exact path:

```tsx
<LocalTranscriptionSetup
  sourcePath={media.path}
  disabled={hostedTranscriptionBusy}
  onBusyChange={setLocalTranscriptionBusy}
  onCompleted={async () => { await refreshTranscript(); }}
  copy={localizedLocalAsrCopy}
/>
```

`disabled` must include hosted transcription activity. Use `onBusyChange` to keep import/provider-start actions disabled while local work runs; backend concurrency protection also rejects simultaneous transcription. `onCompleted` refreshes the **existing** transcript workspace. Saved chunks use `TranscriptionProvider::Local`, requiring the shared enum, schema v11 migration, provider labels, and a cloud-command guard that rejects `local`. The local setup component does not edit the shell or existing transcript component. It polls progress on mount, supports cancellation/retry, reloads persisted quality receipts, and uses labeled controls with visible keyboard focus and compact-width stacking.

All component strings can be supplied through `copy: Partial<LocalTranscriptionCopy>` (including `{placeholder}` templates). English is the visible fallback; full ten-locale coverage for this new panel is not claimed. Raw version strings, model identifiers, review reason codes, and backend error/stage text are diagnostic values, currently English. Source transcript/caption content is never translated by UI language selection.

Focused backend checks:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml local_asr
npm.cmd run build
```

The tests use generated audio/video, deterministic ASR doubles, and a real Python child for cancellation. They establish boundary handling, output validation, receipt persistence, and process control. They do **not** establish transcription accuracy or real model throughput, and no real model is downloaded or inferred by these tests. Validate a user-provided model on representative recordings before relying on its transcript.
