//! Local discovery orchestration. Only corrected transcript text leaves the device.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

use crate::credentials::{self, Provider as CredentialProvider};
use crate::projects::{
    self, DiscoveryJob, DiscoveryState, ReviewDecision, StoredTranscriptSegment, TranscriptionState,
};
use crate::transcription::TranscriptSegment;

use super::{
    AdapterError, DiscoveryError, Provider, TimeRange, discover_moments, quote_is_in_range,
};

const MAX_WINDOW_SECONDS: f64 = 15.0 * 60.0;
const MAX_WINDOW_TEXT_BYTES: usize = 180 * 1024;
const MAX_WINDOW_SEGMENTS: usize = 3_000;

#[derive(Default)]
pub struct DiscoveryRuntime {
    running: Mutex<Option<CancellationToken>>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryProvider {
    Claude,
    Openai,
    Deepseek,
}

impl DiscoveryProvider {
    fn adapter(self) -> Provider {
        match self {
            Self::Claude => Provider::Claude,
            Self::Openai => Provider::OpenAi,
            Self::Deepseek => Provider::DeepSeek,
        }
    }

    fn credential(self) -> CredentialProvider {
        match self {
            Self::Claude => CredentialProvider::Claude,
            Self::Openai => CredentialProvider::Openai,
            Self::Deepseek => CredentialProvider::Deepseek,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Openai => "openai",
            Self::Deepseek => "deepseek",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryStatus {
    transcript_ready: bool,
    transcript_current: bool,
    job: Option<DiscoveryJob>,
}

#[derive(Clone)]
struct Window {
    range: TimeRange,
}

fn corrected_segments(stored: &[StoredTranscriptSegment]) -> Vec<TranscriptSegment> {
    stored
        .iter()
        .map(|segment| TranscriptSegment {
            start_seconds: segment.start_seconds,
            end_seconds: segment.end_seconds,
            text: segment.text.clone(),
            speaker: segment.speaker.clone(),
        })
        .collect()
}

fn transcript_revision(segments: &[StoredTranscriptSegment]) -> String {
    let mut digest = Sha256::new();
    // Changing the discovery contract invalidates previously completed short cuts.
    digest.update(b"discovery-editorial-v2");
    for segment in segments {
        digest.update(segment.id.to_le_bytes());
        digest.update(segment.start_seconds.to_bits().to_le_bytes());
        digest.update(segment.end_seconds.to_bits().to_le_bytes());
        for value in [&segment.text, segment.speaker.as_deref().unwrap_or("")] {
            digest.update((value.len() as u64).to_le_bytes());
            digest.update(value.as_bytes());
        }
    }
    format!("{:x}", digest.finalize())
}

fn plan_windows(segments: &[TranscriptSegment], duration: f64) -> Result<Vec<Window>, String> {
    if segments.is_empty() || !duration.is_finite() || duration <= 0.0 {
        return Err("Complete a usable transcript before finding moments.".to_string());
    }
    let mut windows = Vec::new();
    let mut first: Option<f64> = None;
    let mut end: f64 = 0.0;
    let mut bytes = 0usize;
    let mut count = 0usize;
    let mut previous_start = 0.0;
    for segment in segments {
        if !segment.start_seconds.is_finite()
            || !segment.end_seconds.is_finite()
            || segment.start_seconds < previous_start
            || segment.start_seconds < 0.0
            || segment.end_seconds <= segment.start_seconds
            || segment.end_seconds > duration + 0.001
            || segment.text.trim().is_empty()
            || segment.text.len() + 100 > MAX_WINDOW_TEXT_BYTES
            || segment.end_seconds - segment.start_seconds > MAX_WINDOW_SECONDS
        {
            return Err(
                "The corrected transcript has invalid timing or text for discovery.".to_string(),
            );
        }
        previous_start = segment.start_seconds;
        let start = first.unwrap_or(segment.start_seconds);
        let next_end = end.max(segment.end_seconds);
        let next_bytes = bytes.saturating_add(segment.text.len() + 100);
        if first.is_some()
            && (next_end - start > MAX_WINDOW_SECONDS
                || next_bytes > MAX_WINDOW_TEXT_BYTES
                || count >= MAX_WINDOW_SEGMENTS)
        {
            windows.push(Window {
                range: TimeRange {
                    start_seconds: start,
                    end_seconds: end,
                },
            });
            first = Some(segment.start_seconds);
            end = segment.end_seconds;
            bytes = segment.text.len() + 100;
            count = 1;
        } else {
            first = Some(start);
            end = next_end;
            bytes = next_bytes;
            count += 1;
        }
    }
    if let Some(start) = first {
        windows.push(Window {
            range: TimeRange {
                start_seconds: start,
                end_seconds: end,
            },
        });
    }
    Ok(windows)
}

fn status_for(app: &AppHandle) -> Result<DiscoveryStatus, String> {
    let Some(media) = projects::load_recent_media(app.clone())? else {
        return Ok(DiscoveryStatus {
            transcript_ready: false,
            transcript_current: false,
            job: None,
        });
    };
    let transcript = projects::load_transcription_job(app, &media.path)?;
    let ready = transcript
        .as_ref()
        .is_some_and(|job| job.state == TranscriptionState::Completed && !job.segments.is_empty());
    let mut job = projects::load_discovery_job(app, &media.path)?;
    if job
        .as_ref()
        .is_some_and(|job| job.state == DiscoveryState::Running)
    {
        let runtime = app.state::<DiscoveryRuntime>();
        let running = runtime
            .running
            .lock()
            .map_err(|_| "The discovery runtime is unavailable.")?;
        if running.is_none() {
            job = Some(projects::set_discovery_status(
                app,
                &media.path,
                DiscoveryState::Failed,
                "Interrupted",
                Some(
                    "Discovery stopped when the application closed. Retry to continue incomplete windows.",
                ),
            )?);
        }
    }
    let current = match (&transcript, &job) {
        (Some(transcript), Some(job)) if ready => {
            job.transcript_revision == transcript_revision(&transcript.segments)
        }
        _ => false,
    };
    Ok(DiscoveryStatus {
        transcript_ready: ready,
        transcript_current: current,
        job,
    })
}

#[tauri::command]
pub async fn get_discovery_status(app: AppHandle) -> Result<DiscoveryStatus, String> {
    tokio::task::spawn_blocking(move || status_for(&app))
        .await
        .map_err(|_| "The discovery status worker stopped unexpectedly.".to_string())?
}

#[tauri::command]
pub fn cancel_discovery(app: AppHandle) -> Result<(), String> {
    let runtime = app.state::<DiscoveryRuntime>();
    let running = runtime
        .running
        .lock()
        .map_err(|_| "The discovery runtime is unavailable.")?;
    if let Some(token) = running.as_ref() {
        token.cancel();
    }
    Ok(())
}

#[tauri::command]
pub async fn discover_recent(
    app: AppHandle,
    provider: DiscoveryProvider,
    model: String,
    consent: bool,
) -> Result<DiscoveryStatus, String> {
    if !consent {
        return Err("Review the transcript sharing notice and agree before discovery.".to_string());
    }
    if model.is_empty()
        || model.len() > 100
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err("Enter a valid provider model ID.".to_string());
    }
    let runtime = app.state::<DiscoveryRuntime>();
    let token = {
        let mut running = runtime
            .running
            .lock()
            .map_err(|_| "The discovery runtime is unavailable.")?;
        if running.is_some() {
            return Err("Moment discovery is already running.".to_string());
        }
        let token = CancellationToken::new();
        *running = Some(token.clone());
        token
    };
    let result = run_discovery(&app, provider, &model, &token).await;
    if let Ok(mut running) = runtime.running.lock() {
        *running = None;
    }
    result?;
    status_for(&app)
}

async fn run_discovery(
    app: &AppHandle,
    provider: DiscoveryProvider,
    model: &str,
    token: &CancellationToken,
) -> Result<(), String> {
    let media = projects::load_recent_media(app.clone())?
        .ok_or_else(|| "Import a recording before discovery.".to_string())?;
    let transcript = projects::load_transcription_job(app, &media.path)?
        .ok_or_else(|| "Transcribe the recording before discovery.".to_string())?;
    if transcript.state != TranscriptionState::Completed || transcript.segments.is_empty() {
        return Err("Complete the transcript before finding moments.".to_string());
    }
    let segments = corrected_segments(&transcript.segments);
    let windows = plan_windows(&segments, media.duration_seconds)?;
    let revision = transcript_revision(&transcript.segments);
    let key = credentials::get_api_key(provider.credential())?
        .ok_or_else(|| "Save an API key for the selected provider before discovery.".to_string())?;
    let client = reqwest::Client::builder()
        .build()
        .map_err(|_| "The provider client could not be prepared.".to_string())?;
    let job = projects::create_or_resume_discovery_job(
        app,
        &media.path,
        provider.as_str(),
        model,
        &revision,
        windows.len(),
    )?;
    if job.state == DiscoveryState::Completed {
        return Ok(());
    }
    let result = run_pending_windows(
        app,
        &media.path,
        &segments,
        media.duration_seconds,
        &windows,
        job,
        provider,
        model,
        &key,
        &client,
        token,
    )
    .await;
    if let Err(message) = &result {
        let cancelled = token.is_cancelled();
        let state = if cancelled {
            DiscoveryState::Cancelled
        } else {
            DiscoveryState::Failed
        };
        let stage = if cancelled {
            "Cancelled"
        } else {
            "Discovery failed"
        };
        let _ = projects::set_discovery_status(app, &media.path, state, stage, Some(message));
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_pending_windows(
    app: &AppHandle,
    source_path: &str,
    segments: &[TranscriptSegment],
    duration: f64,
    windows: &[Window],
    job: DiscoveryJob,
    provider: DiscoveryProvider,
    model: &str,
    key: &str,
    client: &reqwest::Client,
    token: &CancellationToken,
) -> Result<(), String> {
    let mut rejected_count = 0usize;
    let mut has_suggestions = !job.suggestions.is_empty();
    for (index, window) in windows.iter().enumerate() {
        if job
            .windows
            .get(index)
            .is_some_and(|stored| stored.completed)
        {
            continue;
        }
        if token.is_cancelled() {
            return Err("Moment discovery cancelled.".to_string());
        }
        projects::set_discovery_status(
            app,
            source_path,
            DiscoveryState::Running,
            &format!(
                "Analyzing transcript window {} of {}",
                index + 1,
                windows.len()
            ),
            None,
        )?;
        let result = discover_moments(
            client,
            provider.adapter(),
            model,
            key,
            segments,
            duration,
            window.range,
            token,
        )
        .await;
        if token.is_cancelled() {
            return Err("Moment discovery cancelled.".to_string());
        }
        let suggestions = match result {
            Ok(result) => {
                rejected_count += result.rejected_count;
                result.suggestions
            }
            Err(error) => {
                if let Some(count) = rejected_batch_count(&error) {
                    rejected_count += count;
                    Vec::new()
                } else {
                    return Err(format!(
                        "Transcript window {} of {} ({:.0}–{:.0} s): {}",
                        index + 1,
                        windows.len(),
                        window.range.start_seconds,
                        window.range.end_seconds,
                        adapter_message(error)
                    ));
                }
            }
        };
        let saved = projects::append_discovery_window(app, source_path, index, &suggestions)?;
        has_suggestions |= !saved.suggestions.is_empty();
    }
    let final_stage = if rejected_count == 0 {
        "Suggestions ready".to_string()
    } else if !has_suggestions {
        format!("No verified suggestions; {rejected_count} invalid suggestions discarded")
    } else {
        format!("Suggestions ready; {rejected_count} invalid suggestions discarded")
    };
    projects::set_discovery_status(
        app,
        source_path,
        DiscoveryState::Completed,
        &final_stage,
        None,
    )?;
    Ok(())
}

fn adapter_message(error: AdapterError) -> String {
    error.to_string()
}

fn rejected_batch_count(error: &AdapterError) -> Option<usize> {
    match error {
        AdapterError::Validation(
            DiscoveryError::InvalidSuggestion(..) | DiscoveryError::InvalidResponseField(..),
        ) => Some(1),
        AdapterError::Validation(DiscoveryError::AllSuggestionsRejected(count, ..)) => Some(*count),
        _ => None,
    }
}

#[tauri::command]
pub async fn review_suggestion(
    app: AppHandle,
    id: i64,
    decision: ReviewDecision,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<DiscoveryStatus, String> {
    tokio::task::spawn_blocking(move || {
        review_suggestion_sync(app, id, decision, start_seconds, end_seconds)
    })
    .await
    .map_err(|_| "The suggestion review worker stopped unexpectedly.".to_string())?
}

fn review_suggestion_sync(
    app: AppHandle,
    id: i64,
    decision: ReviewDecision,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<DiscoveryStatus, String> {
    let media = projects::load_recent_media(app.clone())?
        .ok_or_else(|| "Import a recording before reviewing suggestions.".to_string())?;
    let status = status_for(&app)?;
    if !status.transcript_current {
        return Err(
            "The transcript changed. Find moments again before reviewing suggestions.".to_string(),
        );
    }
    let transcript = projects::load_transcription_job(&app, &media.path)?
        .ok_or_else(|| "The transcript is unavailable.".to_string())?;
    let job = status
        .job
        .ok_or_else(|| "There are no suggestions to review.".to_string())?;
    let suggestion = job
        .suggestions
        .iter()
        .find(|suggestion| suggestion.id == id)
        .ok_or_else(|| "The suggestion does not exist.".to_string())?;
    let range = TimeRange {
        start_seconds,
        end_seconds,
    };
    if !start_seconds.is_finite()
        || !end_seconds.is_finite()
        || start_seconds < 0.0
        || end_seconds <= start_seconds
        || end_seconds > media.duration_seconds + 0.001
        || !quote_is_in_range(
            &suggestion.source_quote,
            &corrected_segments(&transcript.segments),
            range,
        )
    {
        return Err("The adjusted range must contain the quoted source words.".to_string());
    }
    projects::review_discovery_suggestion(
        &app,
        &media.path,
        id,
        decision,
        start_seconds,
        end_seconds,
    )?;
    status_for(&app)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(start: f64, end: f64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_seconds: start,
            end_seconds: end,
            text: text.into(),
            speaker: None,
        }
    }

    #[test]
    fn windows_cover_long_transcript_without_exceeding_provider_limit() {
        let segments: Vec<_> = (0..120)
            .map(|i| segment(i as f64 * 30.0, (i + 1) as f64 * 30.0, "spoken words"))
            .collect();
        let windows = plan_windows(&segments, 3600.0).unwrap();
        assert_eq!(windows.len(), 4);
        assert!(
            windows
                .iter()
                .all(|window| window.range.end_seconds - window.range.start_seconds <= 900.0)
        );
        assert_eq!(windows.first().unwrap().range.start_seconds, 0.0);
        assert_eq!(windows.last().unwrap().range.end_seconds, 3600.0);
    }

    #[test]
    fn revision_changes_with_corrected_text_and_timing() {
        let first = StoredTranscriptSegment {
            id: 1,
            start_seconds: 1.0,
            end_seconds: 2.0,
            text: "one".into(),
            speaker: None,
            corrected: false,
        };
        let mut changed = first.clone();
        changed.text = "two".into();
        assert_ne!(
            transcript_revision(&[first.clone()]),
            transcript_revision(&[changed.clone()])
        );
        changed.text = first.text.clone();
        changed.end_seconds = 2.1;
        assert_ne!(
            transcript_revision(&[first]),
            transcript_revision(&[changed])
        );
    }

    #[test]
    fn only_source_mismatch_batches_can_be_discarded() {
        assert_eq!(
            rejected_batch_count(&AdapterError::Validation(
                DiscoveryError::InvalidSuggestion(0, "sourceQuote is absent")
            )),
            Some(1)
        );
        assert_eq!(
            rejected_batch_count(&AdapterError::Validation(
                DiscoveryError::AllSuggestionsRejected(3, 0, "sourceQuote is absent")
            )),
            Some(3)
        );
        assert_eq!(
            rejected_batch_count(&AdapterError::Validation(DiscoveryError::InvalidResponse(
                "JSON syntax is invalid"
            ))),
            None
        );
    }
}
