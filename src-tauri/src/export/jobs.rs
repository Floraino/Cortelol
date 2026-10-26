//! Command boundary for one reviewed local export at a time.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::media::MediaInfo;
use crate::projects::{self, StoredClipDraft, StoredFramingMode};

use super::{CaptionCue, ClipRange, ExportRequest, Framing, FramingMode, render_clip};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportJobRequest {
    start_ms: u64,
    end_ms: u64,
    destination_path: String,
    captions: Vec<CaptionCue>,
    framing: Framing,
    reviewed: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportStatus {
    state: ExportState,
    stage: String,
    error: Option<String>,
    output_path: Option<String>,
    source_path: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum ExportState {
    Idle,
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl Default for ExportStatus {
    fn default() -> Self {
        Self {
            state: ExportState::Idle,
            stage: String::new(),
            error: None,
            output_path: None,
            source_path: None,
        }
    }
}

async fn recent_media(app: &AppHandle) -> Result<Option<MediaInfo>, String> {
    let handle = app.clone();
    tokio::task::spawn_blocking(move || projects::load_recent_media(handle))
        .await
        .map_err(|_| "The media lookup stopped unexpectedly.".to_string())?
}

#[derive(Default)]
pub struct ExportRuntime {
    inner: Mutex<ExportRuntimeInner>,
    draft_gate: tokio::sync::Mutex<()>,
}

#[tauri::command]
pub async fn get_export_draft(app: AppHandle) -> Result<Option<StoredClipDraft>, String> {
    let media = recent_media(&app)
        .await?
        .ok_or_else(|| "Import a recording before editing a clip.".to_string())?;
    let runtime = app.state::<ExportRuntime>();
    let _gate = runtime.draft_gate.lock().await;
    let worker_app = app.clone();
    tokio::task::spawn_blocking(move || projects::load_clip_draft(&worker_app, &media.path))
        .await
        .map_err(|_| "The clip draft lookup stopped unexpectedly.".to_string())?
}

#[tauri::command]
pub async fn save_export_draft(
    app: AppHandle,
    draft: StoredClipDraft,
) -> Result<StoredClipDraft, String> {
    let media = recent_media(&app)
        .await?
        .ok_or_else(|| "Import a recording before editing a clip.".to_string())?;
    let runtime = app.state::<ExportRuntime>();
    let _gate = runtime.draft_gate.lock().await;
    if runtime
        .inner
        .lock()
        .map_err(|_| "The export runtime is unavailable.")?
        .cancelled
        .is_some()
    {
        return Err("Wait for the current export to finish before editing.".into());
    }
    let worker_app = app.clone();
    tokio::task::spawn_blocking(move || projects::save_clip_draft(&worker_app, &media.path, &draft))
        .await
        .map_err(|_| "The clip draft save stopped unexpectedly.".to_string())?
}

#[tauri::command]
pub async fn review_export_draft(
    app: AppHandle,
    draft: StoredClipDraft,
) -> Result<StoredClipDraft, String> {
    let media = recent_media(&app)
        .await?
        .ok_or_else(|| "Import a recording before reviewing a clip.".to_string())?;
    let runtime = app.state::<ExportRuntime>();
    let _gate = runtime.draft_gate.lock().await;
    if runtime
        .inner
        .lock()
        .map_err(|_| "The export runtime is unavailable.")?
        .cancelled
        .is_some()
    {
        return Err("Wait for the current export to finish before reviewing.".into());
    }
    let worker_app = app.clone();
    tokio::task::spawn_blocking(move || {
        projects::save_clip_draft(&worker_app, &media.path, &draft)?;
        projects::mark_clip_reviewed(&worker_app, &media.path)
    })
    .await
    .map_err(|_| "The clip review stopped unexpectedly.".to_string())?
}

#[derive(Default)]
struct ExportRuntimeInner {
    cancelled: Option<Arc<AtomicBool>>,
    status: ExportStatus,
    completed_hash: Option<[u8; 32]>,
    scoped_export_path: Option<PathBuf>,
    preview_cancelled: Option<Arc<AtomicBool>>,
    preview_dir: Option<tempfile::TempDir>,
    preview_source_path: Option<String>,
    preview_running_source_path: Option<String>,
}

fn matches_reviewed_draft(saved: &StoredClipDraft, request: &ExportJobRequest) -> bool {
    let requested_mode = match request.framing.mode {
        FramingMode::Crop => StoredFramingMode::Crop,
        FramingMode::Fit => StoredFramingMode::Fit,
    };
    saved.reviewed
        && saved.start_ms == request.start_ms
        && saved.end_ms == request.end_ms
        && saved.framing_mode == requested_mode
        && saved.horizontal_percent == request.framing.horizontal_percent
        && saved.framing_keyframes.len() == request.framing.keyframes.len()
        && saved
            .framing_keyframes
            .iter()
            .zip(&request.framing.keyframes)
            .all(|(saved, requested)| {
                saved.timestamp_ms == requested.timestamp_ms
                    && saved.horizontal_percent == requested.horizontal_percent
                    && matches!(
                        (saved.mode, requested.mode),
                        (StoredFramingMode::Crop, FramingMode::Crop)
                            | (StoredFramingMode::Fit, FramingMode::Fit)
                    )
            })
        && saved.captions.len() == request.captions.len()
        && saved
            .captions
            .iter()
            .zip(&request.captions)
            .all(|(saved, requested)| {
                saved.start_ms == requested.start_ms
                    && saved.end_ms == requested.end_ms
                    && saved.text == requested.text
            })
}

fn sha256_file(path: &std::path::Path) -> Result<[u8; 32], String> {
    let mut file = fs::File::open(path).map_err(|_| "The exported MP4 could not be verified.")?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let size = file
            .read(&mut buffer)
            .map_err(|_| "The exported MP4 could not be verified.")?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    Ok(digest.finalize().into())
}

pub(crate) fn latest_completed_export_with_sha256(
    app: &AppHandle,
) -> Result<Option<(PathBuf, [u8; 32])>, String> {
    let Some(media) = projects::load_recent_media(app.clone())? else {
        return Ok(None);
    };
    let runtime = app.state::<ExportRuntime>();
    let (current, can_restore) = {
        let inner = runtime
            .inner
            .lock()
            .map_err(|_| "The export runtime is unavailable.")?;
        if matches!(inner.status.state, ExportState::Completed)
            && inner.status.source_path.as_deref() == Some(&media.path)
        {
            (
                inner
                    .status
                    .output_path
                    .as_ref()
                    .map(|path| (PathBuf::from(path), inner.completed_hash)),
                false,
            )
        } else {
            (
                None,
                matches!(inner.status.state, ExportState::Idle)
                    || inner.status.source_path.as_deref() != Some(&media.path),
            )
        }
    };
    let (path, expected) = if let Some((path, Some(hash))) = current {
        (path, hash)
    } else if can_restore {
        let Some(path) = projects::load_completed_export(app, &media.path)? else {
            return Ok(None);
        };
        let hash = sha256_file(&path)?;
        (path, hash)
    } else {
        return Ok(None);
    };
    let canonical = path
        .canonicalize()
        .map_err(|_| "The reviewed export no longer exists.")?;
    if !canonical.is_file() || sha256_file(&canonical)? != expected {
        return Err(
            "The reviewed export changed on disk. Export and review it again before publishing."
                .into(),
        );
    }
    Ok(Some((canonical, expected)))
}

pub(crate) fn latest_completed_export(app: &AppHandle) -> Result<Option<PathBuf>, String> {
    Ok(latest_completed_export_with_sha256(app)?.map(|(path, _)| path))
}

#[tauri::command]
pub async fn get_export_status(app: AppHandle) -> Result<ExportStatus, String> {
    let source = recent_media(&app).await?.map(|media| media.path);
    let runtime = app.state::<ExportRuntime>();
    {
        let inner = runtime
            .inner
            .lock()
            .map_err(|_| "The export runtime is unavailable.")?;
        if inner.status.source_path == source && !matches!(inner.status.state, ExportState::Idle) {
            return Ok(inner.status.clone());
        }
    }
    let Some(source_path) = source else {
        return Ok(ExportStatus::default());
    };
    let lookup_app = app.clone();
    let lookup_source = source_path.clone();
    let restored = tokio::task::spawn_blocking(move || {
        let Some(path) = projects::load_completed_export(&lookup_app, &lookup_source)? else {
            return Ok::<_, String>(None);
        };
        Ok(Some((path.clone(), sha256_file(&path)?)))
    })
    .await
    .map_err(|_| "The completed export lookup stopped unexpectedly.".to_string())?;
    let mut inner = runtime
        .inner
        .lock()
        .map_err(|_| "The export runtime is unavailable.")?;
    if inner.status.source_path == Some(source_path.clone())
        && !matches!(inner.status.state, ExportState::Idle)
    {
        return Ok(inner.status.clone());
    }
    if inner.status.source_path.as_deref() != Some(&source_path) {
        if let Some(old) = inner.scoped_export_path.take() {
            let _ = app.asset_protocol_scope().forbid_file(old);
        }
        inner.completed_hash = None;
    }
    if let Err(error) = &restored {
        inner.status = ExportStatus {
            state: ExportState::Failed,
            stage: "Previous export unavailable".into(),
            error: Some(error.clone()),
            output_path: None,
            source_path: Some(source_path),
        };
        return Ok(inner.status.clone());
    }
    if let Some((path, hash)) = restored? {
        let scope_error = app.asset_protocol_scope().allow_file(&path).is_err();
        inner.scoped_export_path = if scope_error {
            None
        } else {
            Some(path.clone())
        };
        inner.completed_hash = Some(hash);
        inner.status = ExportStatus {
            state: ExportState::Completed,
            stage: "Previous export ready".into(),
            error: scope_error.then(|| {
                "The MP4 is available on disk, but its in-app preview could not be opened.".into()
            }),
            output_path: Some(path.to_string_lossy().into_owned()),
            source_path: Some(source_path),
        };
    } else {
        inner.status = ExportStatus {
            source_path: Some(source_path),
            ..ExportStatus::default()
        };
    }
    Ok(inner.status.clone())
}

#[tauri::command]
pub fn cancel_export(app: AppHandle) -> Result<(), String> {
    let runtime = app.state::<ExportRuntime>();
    let inner = runtime
        .inner
        .lock()
        .map_err(|_| "The export runtime is unavailable.")?;
    if let Some(cancelled) = &inner.cancelled {
        cancelled.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
pub fn cancel_export_preview(app: AppHandle) -> Result<(), String> {
    let runtime = app.state::<ExportRuntime>();
    let inner = runtime
        .inner
        .lock()
        .map_err(|_| "The export runtime is unavailable.")?;
    if let Some(cancelled) = &inner.preview_cancelled {
        cancelled.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
pub fn discard_export_preview(app: AppHandle, source_path: String) -> Result<(), String> {
    let runtime = app.state::<ExportRuntime>();
    let mut inner = runtime
        .inner
        .lock()
        .map_err(|_| "The export runtime is unavailable.")?;
    if inner.preview_running_source_path.as_deref() == Some(&source_path) {
        if let Some(cancelled) = &inner.preview_cancelled {
            cancelled.store(true, Ordering::Relaxed);
        }
    }
    if inner.preview_source_path.as_deref() == Some(&source_path) {
        if let Some(directory) = inner.preview_dir.take() {
            app.asset_protocol_scope()
                .forbid_file(directory.path().join("preview.mp4"))
                .map_err(|_| "The preview could not be closed.".to_string())?;
            drop(directory);
        }
        inner.preview_source_path = None;
    }
    Ok(())
}

#[tauri::command]
pub async fn prepare_export_preview(
    app: AppHandle,
    start_ms: u64,
    end_ms: u64,
) -> Result<String, String> {
    let media = recent_media(&app)
        .await?
        .ok_or_else(|| "Import a recording before preparing a preview.".to_string())?;
    let range = ClipRange { start_ms, end_ms }.validate(media.duration_seconds)?;
    if range.duration_ms() > 300_000 {
        return Err(
            "Temporary previews are limited to five minutes. Shorten the cut and try again.".into(),
        );
    }
    let source = PathBuf::from(&media.path)
        .canonicalize()
        .map_err(|_| "The imported recording could not be found.".to_string())?;
    let cache = app
        .path()
        .app_cache_dir()
        .map_err(|_| "The preview cache is unavailable.")?;
    fs::create_dir_all(&cache).map_err(|_| "The preview cache could not be created.")?;
    let directory = tempfile::Builder::new()
        .prefix("cortelol-preview-")
        .tempdir_in(cache)
        .map_err(|_| "The preview cache could not be created.")?;
    let output = directory.path().join("preview.mp4");
    let source_path = media.path.clone();
    let runtime = app.state::<ExportRuntime>();
    let cancelled = {
        let mut inner = runtime
            .inner
            .lock()
            .map_err(|_| "The export runtime is unavailable.")?;
        if inner.cancelled.is_some() || inner.preview_cancelled.is_some() {
            return Err("Another media job is already running.".into());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        inner.preview_cancelled = Some(cancelled.clone());
        inner.preview_running_source_path = Some(source_path.clone());
        cancelled
    };
    let worker_cancelled = cancelled.clone();
    let worker_output = output.clone();
    let has_video = media.has_video;
    let result = tokio::task::spawn_blocking(move || {
        render_preview(&source, &worker_output, range, has_video, &worker_cancelled)
    })
    .await
    .map_err(|_| "The preview worker stopped unexpectedly.".to_string())
    .and_then(|result| result);
    let mut inner = runtime
        .inner
        .lock()
        .map_err(|_| "The export runtime is unavailable.")?;
    inner.preview_cancelled = None;
    inner.preview_running_source_path = None;
    result?;
    app.asset_protocol_scope()
        .allow_file(&output)
        .map_err(|_| "The local preview could not be opened.".to_string())?;
    if let Some(old) = inner.preview_dir.replace(directory) {
        app.asset_protocol_scope()
            .forbid_file(old.path().join("preview.mp4"))
            .map_err(|_| "The previous preview could not be closed.".to_string())?;
        drop(old);
    }
    inner.preview_source_path = Some(source_path);
    Ok(output.to_string_lossy().into_owned())
}

fn render_preview(
    source: &std::path::Path,
    output: &std::path::Path,
    range: ClipRange,
    has_video: bool,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let mut command = Command::new(crate::media_tools::ffmpeg()?);
    crate::media_tools::hide_console(&mut command);
    command.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    command.arg("-i").arg(source);
    if has_video {
        command.args([
            "-ss",
            &super::seconds(range.start_ms),
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-vf",
            "scale=960:960:force_original_aspect_ratio=decrease:force_divisible_by=2,setsar=1",
        ]);
    } else {
        command.args(["-f", "lavfi", "-i", "color=c=0x11151B:s=540x960:r=24"]);
        command.args([
            "-map",
            "1:v:0",
            "-map",
            "0:a:0",
            "-af",
            &format!(
                "atrim=start={}:duration={},asetpts=PTS-STARTPTS",
                super::seconds(range.start_ms),
                super::seconds(range.duration_ms())
            ),
        ]);
    }
    command.args([
        "-t",
        &super::seconds(range.duration_ms()),
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-crf",
        "28",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "96k",
        "-movflags",
        "+faststart",
        "-fs",
        "262144000",
        "-f",
        "mp4",
    ]);
    command
        .arg(output)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| "FFmpeg is unavailable for local preview.".to_string())?;
    let started = Instant::now();
    let status = loop {
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Preview preparation was cancelled.".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < Duration::from_secs(15 * 60) => {
                thread::sleep(Duration::from_millis(100))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Preview preparation timed out.".into());
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("The preview process could not be monitored.".into());
            }
        }
    };
    if !status.success() {
        return Err("This recording could not be converted for local preview.".into());
    }
    let length = fs::metadata(output)
        .map_err(|_| "The preview file could not be read.".to_string())?
        .len();
    if length == 0 || length >= 262_144_000 {
        return Err("The preview exceeded its size limit.".into());
    }
    Ok(())
}

#[tauri::command]
pub async fn start_export_reviewed(
    app: AppHandle,
    request: ExportJobRequest,
) -> Result<(), String> {
    if !request.reviewed {
        return Err("Review the cut and captions before exporting.".into());
    }
    let media = recent_media(&app)
        .await?
        .ok_or_else(|| "Import a recording before exporting.".to_string())?;
    let range = ClipRange {
        start_ms: request.start_ms,
        end_ms: request.end_ms,
    }
    .validate(media.duration_seconds)?;
    if request.destination_path.trim().is_empty() {
        return Err("Choose where to save the clip.".into());
    }
    let runtime = app.state::<ExportRuntime>();
    let draft_gate = runtime.draft_gate.lock().await;
    let lookup_app = app.clone();
    let lookup_source = media.path.clone();
    let saved =
        tokio::task::spawn_blocking(move || projects::load_clip_draft(&lookup_app, &lookup_source))
            .await
            .map_err(|_| "The reviewed clip lookup stopped unexpectedly.".to_string())?
            .and_then(|draft| {
                draft.ok_or_else(|| "Save and review this clip before export.".to_string())
            })?;
    if !matches_reviewed_draft(&saved, &request) {
        return Err("The clip changed since review. Review it again before export.".into());
    }
    let cancelled = {
        let mut inner = runtime
            .inner
            .lock()
            .map_err(|_| "The export runtime is unavailable.")?;
        if inner.cancelled.is_some() || inner.preview_cancelled.is_some() {
            return Err("An export is already running.".into());
        }
        if let Some(old_path) = inner.scoped_export_path.take() {
            app.asset_protocol_scope()
                .forbid_file(old_path)
                .map_err(|_| "The previous export preview could not be closed.".to_string())?;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        inner.cancelled = Some(cancelled.clone());
        inner.completed_hash = None;
        inner.status = ExportStatus {
            state: ExportState::Running,
            stage: "Rendering video and captions".into(),
            error: None,
            output_path: None,
            source_path: Some(media.path.clone()),
        };
        cancelled
    };
    drop(draft_gate);
    let source_path = media.path;
    let export = ExportRequest {
        source_path: PathBuf::from(&source_path),
        destination_path: PathBuf::from(request.destination_path),
        range,
        captions: request.captions,
        framing: request.framing,
    };
    let worker_cancelled = cancelled.clone();
    let worker_app = app.clone();
    let worker_source_path = source_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let path = render_clip(export, &worker_cancelled)?;
        let hash = sha256_file(&path)?;
        projects::save_completed_export(&worker_app, &worker_source_path, &path)?;
        Ok::<_, String>((path, hash))
    })
    .await
    .map_err(|_| "The export worker stopped unexpectedly.".to_string())
    .and_then(|result| result);
    let mut inner = runtime
        .inner
        .lock()
        .map_err(|_| "The export runtime is unavailable.")?;
    inner.cancelled = None;
    match &result {
        Ok((path, hash)) => {
            inner.status.state = ExportState::Completed;
            inner.status.stage = "Export ready".into();
            inner.status.output_path = Some(path.to_string_lossy().into_owned());
            inner.completed_hash = Some(*hash);
            if app.asset_protocol_scope().allow_file(path).is_ok() {
                inner.scoped_export_path = Some(path.clone());
            } else {
                inner.status.error =
                    Some("The MP4 was saved, but its in-app preview could not be opened.".into());
            }
        }
        Err(error) => {
            inner.status.state = if cancelled.load(Ordering::Relaxed) {
                ExportState::Cancelled
            } else {
                ExportState::Failed
            };
            inner.status.stage = if cancelled.load(Ordering::Relaxed) {
                "Export cancelled".into()
            } else {
                "Export failed".into()
            };
            inner.status.error = Some(error.clone());
        }
    }
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{
        ClipRange, ExportJobRequest, Framing, FramingMode, matches_reviewed_draft, render_preview,
    };
    use crate::projects::{StoredCaptionCue, StoredClipDraft, StoredFramingMode};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn unsupported_source_codec_gets_bounded_playable_preview() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.mkv");
        let output = directory.path().join("preview.mp4");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=320x180:rate=24:duration=1",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1",
                    "-c:v",
                    "ffv1",
                    "-c:a",
                    "pcm_s16le",
                    "-shortest",
                    "-y"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        render_preview(
            &source,
            &output,
            ClipRange {
                start_ms: 100,
                end_ms: 700,
            },
            true,
            &AtomicBool::new(false),
        )
        .unwrap();
        let media = crate::media::probe_media(output.to_string_lossy().into_owned()).unwrap();
        assert!(media.duration_seconds > 0.5 && media.duration_seconds < 0.8);
        assert!(std::fs::metadata(&output).unwrap().len() < 104_857_600);
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "json",
            ])
            .arg(output)
            .output()
            .unwrap();
        assert!(probe.status.success());
        let json: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        assert!(
            json["streams"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stream| stream["codec_name"] == "h264")
        );
    }

    #[test]
    fn export_requires_exact_reviewed_draft() {
        let draft = StoredClipDraft {
            start_ms: 1_000,
            end_ms: 3_000,
            framing_mode: StoredFramingMode::Crop,
            horizontal_percent: 70,
            framing_keyframes: vec![],
            captions: vec![StoredCaptionCue {
                start_ms: 1_000,
                end_ms: 2_000,
                text: "Reviewed".into(),
            }],
            reviewed: true,
        };
        let mut request = ExportJobRequest {
            start_ms: 1_000,
            end_ms: 3_000,
            destination_path: "C:/clip.mp4".into(),
            captions: vec![super::CaptionCue {
                start_ms: 1_000,
                end_ms: 2_000,
                text: "Reviewed".into(),
            }],
            framing: Framing {
                mode: FramingMode::Crop,
                horizontal_percent: 70,
                keyframes: vec![],
            },
            reviewed: true,
        };
        assert!(matches_reviewed_draft(&draft, &request));
        request.captions[0].text = "Changed".into();
        assert!(!matches_reviewed_draft(&draft, &request));
        request.captions[0].text = "Reviewed".into();
        request.framing.horizontal_percent = 71;
        assert!(!matches_reviewed_draft(&draft, &request));
        request.framing.horizontal_percent = 70;
        request
            .framing
            .keyframes
            .push(crate::export::FramingKeyframe {
                timestamp_ms: 1_000,
                mode: FramingMode::Fit,
                horizontal_percent: 50,
            });
        assert!(!matches_reviewed_draft(&draft, &request));
        request.framing.keyframes.clear();
        let unreviewed = StoredClipDraft {
            reviewed: false,
            ..draft
        };
        assert!(!matches_reviewed_draft(&unreviewed, &request));
    }
}
