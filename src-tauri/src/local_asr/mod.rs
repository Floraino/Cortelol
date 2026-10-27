//! Optional installed Python + local CTranslate2 model, bridged into durable jobs.
pub mod receipts;
mod validation;

use crate::{
    media_tools,
    projects::{self, TranscriptionState},
    transcription::{
        TranscriptionProvider,
        extraction::{ExtractionError, extract_wav_chunk},
        jobs::TranscriptionRuntime,
        plan_chunks,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::Duration,
};
use tauri::{AppHandle, Manager, State};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;
use validation::{MAX_OUTPUT_BYTES, ValidatedSetup, parse_output, validate_setup};

const SCRIPT: &str = include_str!("../../../scripts/Transcribe-Video.py");
const RUNTIME_CHECK: &str = "import sys, faster_whisper, ctranslate2; assert sys.version_info >= (3,10); print('cortelol-local-asr-ready-v1')";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalAsrSetup {
    pub python_path: String,
    pub model_path: String,
    pub language: Option<String>,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalAsrSummary {
    pub processed_chunks: usize,
    pub segment_count: usize,
    pub review_segment_count: usize,
    pub gap_count: usize,
    pub detected_languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalAsrStatus {
    pub state: String,
    pub source_path: Option<String>,
    pub stage: String,
    pub completed_chunks: usize,
    pub total_chunks: usize,
    pub chunk_progress: f64,
    pub summary: LocalAsrSummary,
    pub error: Option<String>,
}

impl Default for LocalAsrStatus {
    fn default() -> Self {
        Self {
            state: "idle".into(),
            source_path: None,
            stage: String::new(),
            completed_chunks: 0,
            total_chunks: 0,
            chunk_progress: 0.0,
            summary: LocalAsrSummary::default(),
            error: None,
        }
    }
}

#[derive(Default)]
pub struct LocalAsrRuntime {
    inner: Mutex<RuntimeState>,
}
#[derive(Default)]
struct RuntimeState {
    token: Option<CancellationToken>,
    status: LocalAsrStatus,
}

fn change_status(
    runtime: &LocalAsrRuntime,
    update: impl FnOnce(&mut LocalAsrStatus),
) -> Result<(), String> {
    let mut inner = runtime
        .inner
        .lock()
        .map_err(|_| "Local transcription state is unavailable.")?;
    update(&mut inner.status);
    Ok(())
}

fn python_command(setup: &ValidatedSetup) -> Command {
    let mut command = Command::new(&setup.python);
    media_tools::hide_console_async(&mut command);
    command
        .args(["-I", "-u", "-X", "utf8"])
        .env("HF_HUB_OFFLINE", "1")
        .env("TRANSFORMERS_OFFLINE", "1")
        .env("HF_HUB_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn check_runtime(setup: &ValidatedSetup) -> Result<(), String> {
    let mut command = python_command(setup);
    command.args(["-c", RUNTIME_CHECK]);
    let directory = tempfile::tempdir().map_err(|_| "Cannot prepare a runtime check.")?;
    command.current_dir(directory.path());
    let mut child = command
        .spawn()
        .map_err(|_| "Python could not start. Select the installed executable.")?;
    let stdout = child.stdout.take().ok_or("Python output is unavailable.")?;
    let probe = async {
        let mut output = Vec::new();
        stdout
            .take(1025)
            .read_to_end(&mut output)
            .await
            .map_err(|_| "Cannot read Python output.")?;
        if output.len() > 1024 {
            return Err("Unexpected Python runtime output.");
        }
        let status = child
            .wait()
            .await
            .map_err(|_| "Cannot read Python status.")?;
        if !status.success()
            || output != b"cortelol-local-asr-ready-v1\r\n"
                && output != b"cortelol-local-asr-ready-v1\n"
        {
            return Err(
                "Use Python 3.10+ with faster-whisper installed in that runtime. See scripts/LOCAL_ASR.md.",
            );
        }
        Ok(())
    };
    match tokio::time::timeout(Duration::from_secs(30), probe).await {
        Ok(result) => {
            if result.is_err() {
                let _ = child.kill().await;
            }
            result.map_err(str::to_string)
        }
        Err(_) => {
            let _ = child.kill().await;
            Err("Python dependency check timed out after 30 seconds.".into())
        }
    }
}

#[tauri::command]
pub async fn validate_local_transcription_setup(request: LocalAsrSetup) -> Result<(), String> {
    let setup = validate_setup(&request)?;
    check_runtime(&setup).await
}

#[tauri::command]
pub fn get_local_transcription_status(
    runtime: State<'_, LocalAsrRuntime>,
) -> Result<LocalAsrStatus, String> {
    Ok(runtime
        .inner
        .lock()
        .map_err(|_| "Local transcription state is unavailable.")?
        .status
        .clone())
}

#[tauri::command]
pub fn cancel_local_transcription(runtime: State<'_, LocalAsrRuntime>) -> Result<(), String> {
    if let Some(token) = &runtime
        .inner
        .lock()
        .map_err(|_| "Local transcription state is unavailable.")?
        .token
    {
        token.cancel();
    } else {
        return Err(
            "No local transcription is running. If it is still starting, retry cancellation."
                .into(),
        );
    }
    Ok(())
}

#[tauri::command]
pub async fn get_local_transcription_report(
    app: AppHandle,
    source_path: String,
) -> Result<receipts::LocalTranscriptionReport, String> {
    tokio::task::spawn_blocking(move || {
        let job = projects::load_transcription_job(&app, &source_path)?;
        let mut report = receipts::LocalTranscriptionReport::default();
        if let Some(job) = job {
            let directory = receipts::receipt_directory(&app, Path::new(&source_path))?;
            // Ignore orphan receipts staged before a DB transaction failed or was cancelled.
            for chunk in job.chunks.iter().filter(|chunk| {
                chunk.completed && chunk.provider == Some(TranscriptionProvider::Local)
            }) {
                match receipts::load_receipt(&directory, chunk.index) {
                    Ok(receipt) => report.chunks.push(receipt),
                    Err(_) => report.missing_chunks.push(chunk.index),
                }
            }
        }
        Ok(report)
    })
    .await
    .map_err(|_| "Local quality report worker stopped unexpectedly.".to_string())?
}

#[tauri::command]
pub async fn transcribe_local_recent(
    app: AppHandle,
    runtime: State<'_, LocalAsrRuntime>,
    request: LocalAsrSetup,
    source_path: String,
) -> Result<LocalAsrSummary, String> {
    let setup = validate_setup(&request)?;
    let model_id = receipts::model_identifier(&setup.model)?;
    let shared = app.state::<TranscriptionRuntime>();
    let token = CancellationToken::new();
    {
        let mut running = shared
            .running
            .lock()
            .map_err(|_| "Transcription runtime is unavailable.")?;
        if running.is_some() {
            return Err("A transcription is already running.".into());
        }
        let mut local = runtime
            .inner
            .lock()
            .map_err(|_| "Local transcription state is unavailable.")?;
        *running = Some(token.clone());
        local.token = Some(token.clone());
        local.status = LocalAsrStatus {
            state: "running".into(),
            source_path: Some(source_path.clone()),
            stage: "Checking installed runtime".into(),
            ..LocalAsrStatus::default()
        };
    }
    let result = run_job(
        &app,
        &runtime,
        &request,
        &setup,
        &source_path,
        &model_id,
        &token,
    )
    .await;
    if let Ok(mut inner) = runtime.inner.lock() {
        inner.token = None;
        inner.status.state = if token.is_cancelled() {
            "cancelled"
        } else if result.is_ok() {
            "completed"
        } else {
            "failed"
        }
        .into();
        inner.status.stage = if token.is_cancelled() {
            "Cancelled"
        } else if result.is_ok() {
            "Transcript ready for review"
        } else {
            "Local transcription failed"
        }
        .into();
        inner.status.error = result.as_ref().err().cloned();
    }
    if let Ok(mut running) = shared.running.lock() {
        *running = None;
    }
    result
}

async fn run_job(
    app: &AppHandle,
    runtime: &LocalAsrRuntime,
    request: &LocalAsrSetup,
    setup: &ValidatedSetup,
    expected_source: &str,
    model_id: &str,
    token: &CancellationToken,
) -> Result<LocalAsrSummary, String> {
    // No job state is changed until setup succeeds and the selected source matches.
    tokio::select! {
        _ = token.cancelled() => return Err("Local transcription cancelled.".into()),
        result = check_runtime(setup) => result?,
    }
    let media = projects::load_recent_media(app.clone())?
        .ok_or("Import a recording before transcription.")?;
    if !media.has_audio
        || !media.duration_seconds.is_finite()
        || !(0.0..=43200.0).contains(&media.duration_seconds)
        || media.duration_seconds == 0.0
    {
        return Err(
            "Local transcription requires audio and a recording of at most 12 hours.".into(),
        );
    }
    let expected = Path::new(expected_source)
        .canonicalize()
        .map_err(|_| "The selected recording is unavailable.")?;
    let actual = Path::new(&media.path)
        .canonicalize()
        .map_err(|_| "The imported recording is unavailable.")?;
    if expected != actual {
        return Err("The selected recording changed. Open its transcription setup again.".into());
    }
    let chunks = plan_chunks(
        (media.duration_seconds * 1000.0).ceil() as u64,
        900_000,
        2_000,
    )
    .map_err(|_| "Cannot plan local transcription chunks.")?;
    let job = projects::load_or_create_transcription_job(
        app,
        &media,
        &chunks,
        TranscriptionProvider::Local,
    )?;
    change_status(runtime, |status| {
        status.completed_chunks = job.completed_chunks;
        status.total_chunks = job.total_chunks;
    })?;
    let result = async {
        let receipts = receipts::receipt_directory(app,Path::new(&media.path))?;
        let cache = app.path().app_cache_dir().map_err(|_| "Application cache is unavailable.")?.join("local-asr");
        std::fs::create_dir_all(&cache).map_err(|_| "Cannot prepare local transcription cache.")?;
        let mut reached_audio_end = false;
        for stored in job.chunks.iter().filter(|chunk| !chunk.completed) {
            if token.is_cancelled() { return Err("Local transcription cancelled.".to_string()); }
            let chunk = stored.audio_chunk();
            projects::set_transcription_status(app,&media.path,TranscriptionState::Running,"Preparing local audio",None)?;
            change_status(runtime, |status| { status.stage = format!("Preparing chunk {} of {}",chunk.index+1,job.total_chunks); status.chunk_progress = 0.0; })?;
            let mut quality_bytes = None;
            let parsed = if reached_audio_end { None } else {
                let extracted = extract_wav_chunk(Path::new(&media.path), &cache, chunk, token).await
                    .map_err(|error| match error {
                        ExtractionError::Cancelled => "Local transcription cancelled.".to_string(),
                        _ => format!("Local audio extraction failed ({error:?}). Check FFmpeg and the recording."),
                    })?;
                let short_tail = extracted.actual_duration_ms + 100 < chunk.end_ms - chunk.start_ms;
                // Re-evaluate empty tails from the source on every retry. This also
                // works when earlier chunks were saved by a hosted provider.
                    if empty_audio_tail(extracted.actual_duration_ms, chunk)? {
                    reached_audio_end = true;
                    None
                } else {
                let directory = tempfile::Builder::new().prefix("asr-").tempdir_in(&cache).map_err(|_| "Cannot stage local model output.")?;
                let script = directory.path().join("Transcribe-Video.py");
                std::fs::write(&script,SCRIPT).map_err(|_| "Cannot stage the local transcription script.")?;
                let prefix = directory.path().join("transcript");
                let mut command = python_command(setup);
                command.current_dir(directory.path()).arg(&script).arg(&extracted.path)
                    .arg("--model").arg(&setup.model).arg("--output-prefix").arg(&prefix)
                    .args(["--prepared-wav","--progress-json","--device","cpu","--compute-type","int8"])
                    .arg("--ownership-start").arg(format!("{:.3}",(chunk.core_start_ms-chunk.start_ms) as f64 / 1000.0))
                    .arg("--ownership-end").arg(format!("{:.3}", (chunk.core_end_ms-chunk.start_ms).min(extracted.actual_duration_ms) as f64 / 1000.0))
                    // Keep Python temporary WAVs inside the Rust-owned directory, even on kill.
                    .env("TMPDIR",directory.path()).env("TMP",directory.path()).env("TEMP",directory.path());
                if let Some(language) = &request.language { command.arg("--language").arg(language); }
                change_status(runtime, |status| status.stage = format!("Transcribing locally: chunk {} of {}",chunk.index+1,job.total_chunks))?;
                projects::set_transcription_status(app,&media.path,TranscriptionState::Running,"Transcribing locally",None)?;
                run_model(command,token,runtime).await?;
                let transcript = read_bounded(&prefix.with_extension("json")).await?;
                let quality = read_bounded(&directory.path().join("transcript.quality.json")).await?;
                let parsed = parse_output(&transcript,&quality,&extracted.path.canonicalize().map_err(|_| "Prepared audio disappeared.")?,
                    &setup.model,extracted.actual_duration_ms as f64 / 1000.0,media.duration_seconds,chunk)?;
                quality_bytes = Some(quality);
                reached_audio_end = short_tail;
                Some(parsed)
                }
            };
            if token.is_cancelled() { return Err("Local transcription cancelled.".to_string()); }
            let segments = parsed.as_ref().map_or(&[][..],|output| output.segments.as_slice());
            receipts::write_receipt(&receipts,chunk,model_id,parsed.as_ref(),quality_bytes.as_deref())?;
            let saved = projects::complete_transcription_chunk(app,&media.path,chunk.index,segments,TranscriptionProvider::Local)?;
            change_status(runtime, |status| {
                status.completed_chunks = saved.completed_chunks; status.chunk_progress = 0.0;
                status.summary.processed_chunks += 1;
                if let Some(parsed) = parsed {
                    status.summary.segment_count += parsed.segments.len();
                    status.summary.review_segment_count += parsed.review_count;
                    status.summary.gap_count += parsed.gap_count;
                    for language in parsed.languages { if !status.summary.detected_languages.contains(&language) { status.summary.detected_languages.push(language); } }
                }
            })?;
        }
        Ok(runtime.inner.lock().map_err(|_| "Local transcription state is unavailable.")?.status.summary.clone())
    }.await;
    if let Err(message) = &result {
        let state = if token.is_cancelled() {
            TranscriptionState::Cancelled
        } else {
            TranscriptionState::Failed
        };
        let _ = projects::set_transcription_status(
            app,
            &media.path,
            state,
            "Local transcription stopped",
            Some(message),
        );
    }
    result
}

fn empty_audio_tail(
    actual_duration_ms: u64,
    chunk: crate::transcription::AudioChunk,
) -> Result<bool, String> {
    if actual_duration_ms == 0 && chunk.index == 0 {
        return Err("The recording has no decodable audio samples.".into());
    }
    Ok(actual_duration_ms <= chunk.core_start_ms - chunk.start_ms)
}

async fn read_bounded(path: &PathBuf) -> Result<Vec<u8>, String> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "The local model did not write its output files.")?;
    let mut output = Vec::new();
    file.take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)
        .await
        .map_err(|_| "Cannot read local model output.")?;
    if output.len() as u64 > MAX_OUTPUT_BYTES {
        return Err("Local model output exceeded 16 MiB.".into());
    }
    Ok(output)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    r#type: String,
    completed_chunks: u32,
    total_chunks: u32,
}

async fn run_model(
    mut command: Command,
    token: &CancellationToken,
    runtime: &LocalAsrRuntime,
) -> Result<(), String> {
    let mut child = command
        .spawn()
        .map_err(|_| "The installed Python runtime could not start.")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("Cannot read local model progress.")?;
    let read_progress = async {
        let mut reader = BufReader::new(stdout.take(64 * 1024));
        let mut line = Vec::new();
        let mut previous = 0;
        let mut expected_total = None;
        loop {
            line.clear();
            let bytes = reader
                .read_until(b'\n', &mut line)
                .await
                .map_err(|_| "Cannot read local model progress.")?;
            if bytes == 0 {
                break;
            }
            if bytes > 4096 {
                return Err("Invalid local model progress output.".to_string());
            }
            let progress: Progress = serde_json::from_slice(&line)
                .map_err(|_| "Invalid local model progress output.")?;
            if progress.r#type != "progress"
                || progress.total_chunks == 0
                || progress.total_chunks > 8
                || progress.completed_chunks <= previous
                || progress.completed_chunks > progress.total_chunks
                || expected_total.is_some_and(|total| total != progress.total_chunks)
            {
                return Err("Invalid local model progress output.".into());
            }
            previous = progress.completed_chunks;
            expected_total = Some(progress.total_chunks);
            change_status(runtime, |status| {
                status.chunk_progress =
                    progress.completed_chunks as f64 / progress.total_chunks as f64
            })?;
        }
        let status = child
            .wait()
            .await
            .map_err(|_| "Cannot read local model process status.")?;
        if !status.success() || expected_total != Some(previous) {
            return Err("Local model execution failed. Check that the CTranslate2 model is complete, compatible with CPU int8, and that memory is available. See scripts/LOCAL_ASR.md.".into());
        }
        Ok(())
    };
    let result = tokio::select! {
        _ = token.cancelled() => Err("Local transcription cancelled.".into()),
        _ = tokio::time::sleep(Duration::from_secs(6*60*60)) => Err("Local model exceeded the six-hour timeout for one chunk. Retry with a smaller model.".into()),
        result = read_progress => result,
    };
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn cancellation_kills_python_and_releases_its_staged_file() {
        if std::process::Command::new("python")
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("Python unavailable: cancellation process test skipped.");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut command = Command::new("python");
        media_tools::hide_console_async(&mut command);
        command.current_dir(directory.path()).args(["-I","-u","-c",
            "import time; f=open('held.wav','w'); print('{\"type\":\"progress\",\"completed_chunks\":1,\"total_chunks\":1}',flush=True); time.sleep(60)"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).kill_on_drop(true);
        let runtime = Arc::new(LocalAsrRuntime::default());
        let token = CancellationToken::new();
        let child_runtime = runtime.clone();
        let child_token = token.clone();
        let worker =
            tokio::spawn(async move { run_model(command, &child_token, &child_runtime).await });
        for _ in 0..300 {
            if runtime.inner.lock().unwrap().status.chunk_progress == 1.0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let opened = directory.path().join("held.wav").is_file();
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(
            opened,
            "Python did not open its staged file before cancellation"
        );
        assert!(result.unwrap_err().contains("cancelled"));
        // On Windows an open Python file handle prevents deletion. Success
        // confirms cancellation waited for the process to release that handle.
        std::fs::remove_file(directory.path().join("held.wav")).unwrap();
    }

    #[tokio::test]
    async fn empty_audio_tail_is_recomputed_after_cancel_or_cloud_handoff() {
        let Ok(ffmpeg) = media_tools::ffmpeg() else {
            return;
        };
        if std::process::Command::new(&ffmpeg)
            .arg("-version")
            .stdout(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("FFmpeg unavailable: generated tail extraction test skipped.");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("long-video.mp4");
        let mut make = Command::new(&ffmpeg);
        media_tools::hide_console_async(&mut make);
        let generated = make
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=1:d=40",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=16000:duration=3",
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
            ])
            .arg(&source)
            .output()
            .await
            .unwrap();
        assert!(generated.status.success());
        let chunks = plan_chunks(40000, 10000, 2000).unwrap();
        let first = extract_wav_chunk(
            &source,
            directory.path(),
            chunks[0],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(first.actual_duration_ms < 4000);
        assert!(!empty_audio_tail(first.actual_duration_ms, chunks[0]).unwrap());
        // Discard all in-memory tail knowledge, as on cancel/restart or when
        // chunk zero was completed by Gemini/ElevenLabs instead of this bridge.
        for pending_index in [1, 2] {
            let retry = extract_wav_chunk(
                &source,
                directory.path(),
                chunks[pending_index],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(empty_audio_tail(retry.actual_duration_ms, chunks[pending_index]).unwrap());
        }
        assert!(empty_audio_tail(0, chunks[0]).is_err());
    }
}
