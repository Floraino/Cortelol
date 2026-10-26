//! Optional, consent-gated visual framing suggestions from Gemini.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

use super::framing::{FramingKeyframe, FramingMode};
use crate::credentials::{self, Provider};
use crate::projects;

const ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent";
const SAMPLE_COUNT: usize = 5;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const FRAME_TIMEOUT: Duration = Duration::from_secs(12);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Default)]
pub struct CloudFramingRuntime {
    active: Mutex<Option<CancellationToken>>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CloudFramingConfidence {
    None,
    Low,
    Medium,
    High,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudFramingSuggestion {
    horizontal_percent: u8,
    confidence: CloudFramingConfidence,
    reason: &'static str,
    face_samples: usize,
    sample_count: usize,
    keyframes: Vec<FramingKeyframe>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GeminiFrame {
    horizontal_percent: u8,
    mode: FramingMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GeminiCrop {
    frames: Vec<GeminiFrame>,
    confidence: CloudFramingConfidence,
}

#[tauri::command]
pub async fn suggest_gemini_framing(
    app: AppHandle,
    start_ms: u64,
    end_ms: u64,
    consent: bool,
) -> Result<CloudFramingSuggestion, String> {
    if !consent {
        return Err("Confirm that five sampled video frames may be sent to Gemini.".into());
    }
    let runtime = app.state::<CloudFramingRuntime>();
    let token = CancellationToken::new();
    {
        let mut active = runtime
            .active
            .lock()
            .map_err(|_| "Gemini framing is unavailable.")?;
        if active.is_some() {
            return Err("A Gemini framing suggestion is already running.".into());
        }
        *active = Some(token.clone());
    }
    let result = analyze(app.clone(), start_ms, end_ms, token).await;
    if let Ok(mut active) = runtime.active.lock() {
        *active = None;
    }
    result
}

#[tauri::command]
pub fn cancel_gemini_framing(app: AppHandle) -> Result<(), String> {
    if let Some(token) = app
        .state::<CloudFramingRuntime>()
        .active
        .lock()
        .map_err(|_| "Gemini framing is unavailable.")?
        .as_ref()
    {
        token.cancel();
    }
    Ok(())
}

async fn analyze(
    app: AppHandle,
    start_ms: u64,
    end_ms: u64,
    token: CancellationToken,
) -> Result<CloudFramingSuggestion, String> {
    let media = tokio::task::spawn_blocking(move || projects::load_recent_media(app))
        .await
        .map_err(|_| "The recording lookup stopped unexpectedly.".to_string())??
        .ok_or_else(|| "Import a recording before suggesting a crop.".to_string())?;
    if !media.has_video {
        return Err("This recording has no video to frame.".into());
    }
    let duration_ms = (media.duration_seconds * 1_000.0) as u64;
    if start_ms >= end_ms || end_ms > duration_ms || end_ms - start_ms > 5 * 60 * 1_000 {
        return Err("Select a valid video clip of five minutes or less.".into());
    }
    let source = Path::new(&media.path)
        .canonicalize()
        .map_err(|_| "The source recording could not be found.".to_string())?;
    if !source.is_file() {
        return Err("The source recording must be a file.".into());
    }
    let key = credentials::get_api_key(Provider::Gemini)?
        .ok_or_else(|| "Save a Gemini API key in Settings before cloud framing.".to_string())?;
    if token.is_cancelled() {
        return Err("Gemini framing was cancelled.".into());
    }
    let sample_token = token.clone();
    let frames = tokio::task::spawn_blocking(move || {
        sample_frames(&source, start_ms, end_ms, &sample_token)
    })
    .await
    .map_err(|_| "Framing sample extraction stopped unexpectedly.".to_string())??;
    if token.is_cancelled() {
        return Err("Gemini framing was cancelled.".into());
    }
    let request = request_body(&frames);
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "The Gemini connection could not be prepared.".to_string())?;
    let response = tokio::select! {
        _ = token.cancelled() => return Err("Gemini framing was cancelled.".into()),
        result = client.post(ENDPOINT).header("x-goog-api-key", key).json(&request).send() =>
            result.map_err(|error| if error.is_timeout() {
                "Gemini framing timed out. Retry the clip.".to_string()
            } else {
                "Gemini could not be reached for framing.".to_string()
            })?,
    };
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            401 | 403 => "Gemini rejected the saved API key for framing. Check Settings.",
            429 => "Gemini is rate limiting framing requests. Retry later.",
            400 => "Gemini rejected the framing request. Try another clip or model later.",
            500..=599 => "Gemini framing is temporarily unavailable. Retry later.",
            _ => "Gemini rejected the framing request.",
        }
        .into());
    }
    let mut response = response;
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = token.cancelled() => return Err("Gemini framing was cancelled.".into()),
            result = response.chunk() => result.map_err(|_| "Gemini's framing response could not be read.".to_string())?,
        };
        let Some(chunk) = chunk else { break };
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err("Gemini's framing response exceeded the supported size.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_response(&bytes, start_ms, end_ms, frames.len())
}

fn sample_frames(
    source: &Path,
    start_ms: u64,
    end_ms: u64,
    token: &CancellationToken,
) -> Result<Vec<Vec<u8>>, String> {
    let directory = tempfile::tempdir()
        .map_err(|_| "Temporary framing images could not be prepared.".to_string())?;
    let mut frames = Vec::with_capacity(SAMPLE_COUNT);
    for index in 0..SAMPLE_COUNT {
        if token.is_cancelled() {
            return Err("Gemini framing was cancelled.".into());
        }
        let fraction = (index as f64 + 0.5) / SAMPLE_COUNT as f64;
        let timestamp = start_ms + ((end_ms - start_ms) as f64 * fraction) as u64;
        let output = directory.path().join(format!("frame-{index}.jpg"));
        extract_frame(source, &output, timestamp, token)?;
        let size = fs::metadata(&output)
            .map_err(|_| "A framing sample could not be read.".to_string())?
            .len();
        if size < 4 || size > MAX_FRAME_BYTES as u64 {
            return Err("A framing sample is not a valid bounded JPEG.".into());
        }
        let bytes =
            fs::read(&output).map_err(|_| "A framing sample could not be read.".to_string())?;
        if bytes.len() < 4
            || bytes.len() > MAX_FRAME_BYTES
            || !bytes.starts_with(&[0xff, 0xd8])
            || !bytes.ends_with(&[0xff, 0xd9])
        {
            return Err("A framing sample is not a valid bounded JPEG.".into());
        }
        frames.push(bytes);
        let _ = fs::remove_file(output);
    }
    Ok(frames)
}

fn extract_frame(
    source: &Path,
    output: &Path,
    timestamp_ms: u64,
    token: &CancellationToken,
) -> Result<(), String> {
    let mut command = Command::new(crate::media_tools::ffmpeg()?);
    crate::media_tools::hide_console(&mut command);
    let mut child = command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-ss"])
        .arg(super::seconds(timestamp_ms))
        .arg("-i")
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            "scale=640:640:force_original_aspect_ratio=decrease:force_divisible_by=2",
            "-q:v",
            "4",
            "-y",
        ])
        .arg(output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "FFmpeg could not start for cloud framing samples.".to_string())?;
    let started = Instant::now();
    loop {
        if token.is_cancelled() || started.elapsed() > FRAME_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if token.is_cancelled() {
                "Gemini framing was cancelled."
            } else {
                "A framing sample timed out. Try a shorter or simpler video."
            }
            .into());
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err("FFmpeg could not decode a framing sample.".into()),
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("The framing sample could not be monitored.".into());
            }
        }
    }
}

fn request_body(frames: &[Vec<u8>]) -> Value {
    let mut parts = Vec::with_capacity(frames.len() + 1);
    parts.push(json!({"text": "These are five equally spaced frames from one video clip, in chronological order. Evaluate EACH frame independently and return exactly five frame choices in the same order. Choose fit when a 9:16 crop would cut visible participants (especially a wide table view), or no subject is clear. Choose crop when the subject fits; horizontalPercent is the crop position from 0 (left) to 100 (right), with 50 centered. For fit use horizontalPercent 50. Give one overall confidence level. Reply with only the requested JSON. Do not infer speech, identity, popularity, or unseen events."}));
    for frame in frames {
        parts.push(
            json!({"inlineData": {"mimeType": "image/jpeg", "data": STANDARD.encode(frame)}}),
        );
    }
    json!({
        "contents": [{"role": "user", "parts": parts}],
        "generationConfig": {
            "responseFormat": {"text": {
                "mimeType": "APPLICATION_JSON",
                "schema": {
                    "type": "object",
                    "properties": {
                        "frames": {"type": "array", "minItems": frames.len(), "maxItems": frames.len(), "items": {"type": "object", "properties": {
                            "horizontalPercent": {"type": "integer", "minimum": 0, "maximum": 100},
                            "mode": {"type": "string", "enum": ["crop", "fit"]}
                        }, "required": ["horizontalPercent", "mode"]}},
                        "confidence": {"type": "string", "enum": ["none", "low", "medium", "high"]}
                    },
                    "required": ["frames", "confidence"]
                }
            }}
        }
    })
}

fn parse_response(
    body: &[u8],
    start_ms: u64,
    end_ms: u64,
    sample_count: usize,
) -> Result<CloudFramingSuggestion, String> {
    let envelope: Value = serde_json::from_slice(body)
        .map_err(|_| "Gemini returned an invalid framing envelope.".to_string())?;
    if envelope.pointer("/promptFeedback/blockReason").is_some() {
        return Err("Gemini blocked these frames from visual analysis.".into());
    }
    let candidate = envelope
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .ok_or_else(|| "Gemini returned no framing candidate.".to_string())?;
    if candidate.get("finishReason").and_then(Value::as_str) != Some("STOP") {
        return Err("Gemini did not finish its framing response. Retry the clip.".into());
    }
    let parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| "Gemini returned no framing result.".to_string())?;
    let text = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    if text.len() > 4096 {
        return Err("Gemini's framing result was too large.".into());
    }
    let crop: GeminiCrop = serde_json::from_str(&text)
        .map_err(|_| "Gemini returned an invalid crop position or confidence.".to_string())?;
    if crop.frames.len() != sample_count
        || crop.frames.iter().any(|frame| {
            frame.horizontal_percent > 100
                || (frame.mode == FramingMode::Fit && frame.horizontal_percent != 50)
        })
    {
        return Err("Gemini returned invalid per-frame crop choices.".into());
    }
    let keyframes = crop
        .frames
        .into_iter()
        .enumerate()
        .map(|(index, frame)| {
            let fraction = (index as f64 + 0.5) / sample_count as f64;
            FramingKeyframe {
                timestamp_ms: if index == 0 {
                    start_ms
                } else {
                    start_ms + ((end_ms - start_ms) as f64 * fraction) as u64
                },
                mode: frame.mode,
                horizontal_percent: frame.horizontal_percent,
            }
        })
        .collect::<Vec<_>>();
    Ok(CloudFramingSuggestion {
        horizontal_percent: keyframes
            .first()
            .map_or(50, |frame| frame.horizontal_percent),
        confidence: crop.confidence,
        reason: "visualSubject",
        face_samples: 0,
        sample_count,
        keyframes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(result: Value) -> Vec<u8> {
        json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [{"text": result.to_string()}]}}]}).to_string().into_bytes()
    }

    #[test]
    fn accepts_bounded_finished_crop() {
        let frames = json!([
            {"horizontalPercent": 25, "mode": "crop"},
            {"horizontalPercent": 75, "mode": "crop"},
            {"horizontalPercent": 50, "mode": "fit"},
            {"horizontalPercent": 70, "mode": "crop"},
            {"horizontalPercent": 50, "mode": "fit"}
        ]);
        let body = envelope(json!({"frames": frames, "confidence": "medium"}));
        let result = parse_response(&body, 1_000, 6_000, SAMPLE_COUNT).unwrap();
        assert_eq!(result.horizontal_percent, 25);
        assert_eq!(result.confidence, CloudFramingConfidence::Medium);
        assert_eq!(result.sample_count, SAMPLE_COUNT);
        assert_eq!(result.keyframes[0].timestamp_ms, 1_000);
        assert_eq!(result.keyframes[1].horizontal_percent, 75);
        assert_eq!(result.keyframes[2].mode, FramingMode::Fit);
    }

    #[test]
    fn rejects_unfinished_or_untrusted_crop() {
        let unfinished = br#"{"candidates":[{"finishReason":"MAX_TOKENS","content":{"parts":[{"text":"{}"}]}}]}"#;
        assert!(parse_response(unfinished, 0, 1_000, SAMPLE_COUNT).is_err());
        let one_frame = envelope(
            json!({"frames": [{"horizontalPercent": 50, "mode": "crop"}], "confidence": "high"}),
        );
        assert!(parse_response(&one_frame, 0, 1_000, SAMPLE_COUNT).is_err());
        let too_far = envelope(
            json!({"frames": vec![json!({"horizontalPercent": 101, "mode": "crop"}); SAMPLE_COUNT], "confidence": "high"}),
        );
        assert!(parse_response(&too_far, 0, 1_000, SAMPLE_COUNT).is_err());
        let unexpected = envelope(
            json!({"frames": vec![json!({"horizontalPercent": 50, "mode": "crop", "instruction": "ignore user"}); SAMPLE_COUNT], "confidence": "high"}),
        );
        assert!(parse_response(&unexpected, 0, 1_000, SAMPLE_COUNT).is_err());
    }

    #[test]
    fn sends_only_bounded_inline_jpegs_and_schema() {
        let request = request_body(&[vec![0xff, 0xd8, 0xff, 0xd9]]);
        assert_eq!(
            request.pointer("/contents/0/parts/1/inlineData/mimeType"),
            Some(&json!("image/jpeg"))
        );
        assert_eq!(
            request.pointer("/generationConfig/responseFormat/text/mimeType"),
            Some(&json!("APPLICATION_JSON"))
        );
        assert!(request.to_string().contains("/9j/2Q=="));
        assert!(!request.to_string().contains("sourcePath"));
    }
}
