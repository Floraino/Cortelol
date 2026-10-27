//! Tauri commands coordinating local extraction, transcription, and durable progress.

use std::path::Path;
use std::sync::Mutex;

use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

use crate::credentials::{self, Provider};
use crate::projects::{self, TranscriptionJob, TranscriptionState};

use super::elevenlabs::{ElevenLabsClient, ElevenLabsError};
use super::extraction::{ExtractionError, extract_wav_chunk};
use super::gemini::{GeminiClient, GeminiError};
use super::plan_chunks;
use super::{
    AudioChunk, TranscriptSegment, TranscriptionError, TranscriptionProvider, group_timed_words,
};

const CHUNK_MS: u64 = 15 * 60 * 1000;
const OVERLAP_MS: u64 = 2_000;
const RECOVERY_OVERLAP_MS: u64 = 2_000;
const MIN_SPLIT_MS: u64 = 10_000;

#[derive(Default)]
pub struct TranscriptionRuntime {
    pub(crate) running: Mutex<Option<CancellationToken>>,
}

fn empty_job() -> TranscriptionJob {
    TranscriptionJob {
        provider: TranscriptionProvider::Gemini,
        state: TranscriptionState::Idle,
        stage: String::new(),
        error: None,
        completed_chunks: 0,
        total_chunks: 0,
        chunks: Vec::new(),
        segments: Vec::new(),
    }
}

#[tauri::command]
pub async fn get_transcription_status(app: AppHandle) -> Result<TranscriptionJob, String> {
    tokio::task::spawn_blocking(move || read_transcription_status(app))
        .await
        .map_err(|_| "The transcription status worker stopped unexpectedly.".to_string())?
}

fn read_transcription_status(app: AppHandle) -> Result<TranscriptionJob, String> {
    let Some(media) = projects::load_recent_media(app.clone())? else {
        return Ok(empty_job());
    };
    let Some(job) = projects::load_transcription_job(&app, &media.path)? else {
        return Ok(empty_job());
    };
    if job.state == TranscriptionState::Running {
        let runtime = app.state::<TranscriptionRuntime>();
        let active = runtime
            .running
            .lock()
            .map_err(|_| "The transcription runtime is unavailable.")?
            .is_some();
        if !active {
            return projects::set_transcription_status(
                &app,
                &media.path,
                TranscriptionState::Failed,
                "Interrupted",
                Some(
                    "The previous transcription was interrupted. Retry to resume incomplete chunks.",
                ),
            );
        }
    }
    Ok(job)
}

#[tauri::command]
pub async fn update_transcript_segment(
    app: AppHandle,
    id: i64,
    text: String,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<TranscriptionJob, String> {
    tokio::task::spawn_blocking(move || {
        update_transcript_segment_sync(app, id, text, start_seconds, end_seconds)
    })
    .await
    .map_err(|_| "The transcript correction worker stopped unexpectedly.".to_string())?
}

fn update_transcript_segment_sync(
    app: AppHandle,
    id: i64,
    text: String,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<TranscriptionJob, String> {
    let media = projects::load_recent_media(app.clone())?
        .ok_or_else(|| "Import a recording before editing its transcript.".to_string())?;
    projects::update_transcript_segment(&app, &media.path, id, &text, start_seconds, end_seconds)
}

#[tauri::command]
pub fn cancel_transcription(app: AppHandle) -> Result<(), String> {
    let runtime = app.state::<TranscriptionRuntime>();
    let running = runtime
        .running
        .lock()
        .map_err(|_| "The transcription runtime is unavailable.")?;
    if let Some(token) = running.as_ref() {
        token.cancel();
    }
    Ok(())
}

#[tauri::command]
pub async fn transcribe_recent(
    app: AppHandle,
    consent: bool,
    provider: TranscriptionProvider,
) -> Result<(), String> {
    if provider == TranscriptionProvider::Local {
        return Err("Use the local transcription setup for the local provider.".to_string());
    }
    if !consent {
        return Err("Review the audio sharing notice and agree before transcription.".to_string());
    }
    let runtime = app.state::<TranscriptionRuntime>();
    let token = {
        let mut running = runtime
            .running
            .lock()
            .map_err(|_| "The transcription runtime is unavailable.")?;
        if running.is_some() {
            return Err("A transcription is already running.".to_string());
        }
        let token = CancellationToken::new();
        *running = Some(token.clone());
        token
    };

    let result = run_transcription(&app, provider, &token).await;
    if let Ok(mut running) = runtime.running.lock() {
        *running = None;
    }
    result
}

async fn run_transcription(
    app: &AppHandle,
    provider: TranscriptionProvider,
    token: &CancellationToken,
) -> Result<(), String> {
    let media = projects::load_recent_media(app.clone())?
        .ok_or_else(|| "Import a recording before transcription.".to_string())?;
    if !media.has_audio {
        return Err("The recording has no audio track to transcribe.".to_string());
    }
    let duration_ms = duration_milliseconds(media.duration_seconds)?;
    let chunks = plan_chunks(duration_ms, CHUNK_MS, OVERLAP_MS)
        .map_err(|_| "The recording cannot be split into transcription chunks.".to_string())?;
    let job = projects::load_or_create_transcription_job(app, &media, &chunks, provider)?;
    if job.state == TranscriptionState::Completed {
        return Ok(());
    }

    let result = run_pending_chunks(app, &media.path, job, token).await;
    if let Err(message) = &result {
        let cancelled = token.is_cancelled();
        let state = if cancelled {
            TranscriptionState::Cancelled
        } else {
            TranscriptionState::Failed
        };
        let stage = if cancelled {
            "Cancelled"
        } else {
            "Transcription failed"
        };
        let _ = projects::set_transcription_status(app, &media.path, state, stage, Some(message));
    }
    result
}

async fn run_pending_chunks(
    app: &AppHandle,
    source_path: &str,
    job: TranscriptionJob,
    token: &CancellationToken,
) -> Result<(), String> {
    let (key_provider, name) = match job.provider {
        TranscriptionProvider::Gemini => (Provider::Gemini, "Gemini"),
        TranscriptionProvider::Elevenlabs => (Provider::Elevenlabs, "ElevenLabs"),
        TranscriptionProvider::Local => {
            return Err("Use the local transcription setup for the local provider.".to_string());
        }
    };
    let key = credentials::get_api_key(key_provider)?
        .ok_or_else(|| format!("Save a {name} API key before transcription."))?;
    let client = match job.provider {
        TranscriptionProvider::Gemini => {
            TranscriptionClient::Gemini(GeminiClient::new().map_err(gemini_message)?)
        }
        TranscriptionProvider::Elevenlabs => {
            TranscriptionClient::Elevenlabs(ElevenLabsClient::new().map_err(elevenlabs_message)?)
        }
        TranscriptionProvider::Local => {
            return Err("Use the local transcription setup for the local provider.".to_string());
        }
    };
    let cache = app
        .path()
        .app_cache_dir()
        .map_err(|_| "The application cache directory is unavailable.".to_string())?;
    for stored in job.chunks.iter().filter(|chunk| !chunk.completed) {
        if token.is_cancelled() {
            return Err("Transcription cancelled.".to_string());
        }
        projects::set_transcription_status(
            app,
            source_path,
            TranscriptionState::Running,
            "Preparing audio",
            None,
        )?;
        let chunk = stored.audio_chunk();
        let words = match &client {
            TranscriptionClient::Gemini(client) => {
                transcribe_chunk_with_recovery(
                    app,
                    source_path,
                    &cache,
                    chunk,
                    job.total_chunks,
                    client,
                    &key,
                    token,
                )
                .await
            }
            TranscriptionClient::Elevenlabs(client) => {
                transcribe_chunk_with_recovery_elevenlabs(
                    app,
                    source_path,
                    &cache,
                    chunk,
                    job.total_chunks,
                    client,
                    &key,
                    token,
                )
                .await
            }
        }
        .map_err(|error| format!("Chunk {} of {}: {error}", chunk.index + 1, job.total_chunks))?;
        if token.is_cancelled() {
            return Err("Transcription cancelled.".to_string());
        }
        let phrases = group_timed_words(words);
        projects::complete_transcription_chunk(
            app,
            source_path,
            chunk.index,
            &phrases,
            job.provider,
        )?;
    }
    projects::set_transcription_status(
        app,
        source_path,
        TranscriptionState::Completed,
        "Transcript ready",
        None,
    )?;
    Ok(())
}

enum TranscriptionClient {
    Gemini(GeminiClient),
    Elevenlabs(ElevenLabsClient),
}

/// Scribe returns a complete set of timed words for an audio request. A
/// malformed timestamp gets one smaller, overlapping retry; the parent is
/// committed only after both recovery requests succeed.
async fn transcribe_chunk_with_recovery_elevenlabs(
    app: &AppHandle,
    source_path: &str,
    cache: &Path,
    chunk: AudioChunk,
    total_chunks: usize,
    client: &ElevenLabsClient,
    key: &str,
    token: &CancellationToken,
) -> Result<Vec<TranscriptSegment>, String> {
    match transcribe_one_chunk_elevenlabs(app, source_path, cache, chunk, client, key, token).await
    {
        Ok(words) => Ok(words),
        Err(error) if recoverable_elevenlabs_timing_parent(&error).is_some() => {
            let effective = recoverable_elevenlabs_timing_parent(&error).unwrap();
            let (left, right) = split_chunk_once(effective).ok_or_else(|| {
                "ElevenLabs returned invalid word timing in a short audio chunk. Retry later or inspect this part of the recording.".to_string()
            })?;
            projects::set_transcription_status(
                app,
                source_path,
                TranscriptionState::Running,
                &format!(
                    "Recovering chunk {} of {} with two shorter requests",
                    chunk.index + 1,
                    total_chunks
                ),
                None,
            )?;
            let mut words =
                transcribe_one_chunk_elevenlabs(app, source_path, cache, left, client, key, token)
                    .await
                    .map_err(|error| {
                        format!(
                            "The first recovery request failed: {}",
                            chunk_message(error)
                        )
                    })?;
            if token.is_cancelled() {
                return Err("Transcription cancelled.".to_string());
            }
            words.extend(
                transcribe_one_chunk_elevenlabs(app, source_path, cache, right, client, key, token)
                    .await
                    .map_err(|error| {
                        format!(
                            "The second recovery request failed: {}",
                            chunk_message(error)
                        )
                    })?,
            );
            words.sort_by(|a, b| a.start_seconds.total_cmp(&b.start_seconds));
            Ok(words)
        }
        Err(error) => Err(chunk_message(error)),
    }
}

fn recoverable_elevenlabs_timing_parent(error: &ChunkFailure) -> Option<AudioChunk> {
    match error {
        ChunkFailure::Elevenlabs(ElevenLabsError::InvalidTranscript, effective) => Some(*effective),
        _ => None,
    }
}

/// One bounded recovery is permitted for invalid word timing in a long chunk.
/// Two smaller overlapping requests recover the parent without committing a
/// partial result or repeatedly charging for bad output.
async fn transcribe_chunk_with_recovery(
    app: &AppHandle,
    source_path: &str,
    cache: &Path,
    chunk: AudioChunk,
    total_chunks: usize,
    client: &GeminiClient,
    key: &str,
    token: &CancellationToken,
) -> Result<Vec<TranscriptSegment>, String> {
    let first = transcribe_one_chunk(app, source_path, cache, chunk, client, key, token).await;
    match first {
        Err(error) if let Some(effective) = recoverable_timing_parent(chunk, &error) => {
            let (left, right) = split_chunk_once(effective).ok_or_else(|| {
                "Gemini returned invalid word timing in a short audio chunk. Review the source audio and retry later."
                    .to_string()
            })?;
            projects::set_transcription_status(
                app,
                source_path,
                TranscriptionState::Running,
                &format!(
                    "Recovering chunk {} of {} with two shorter requests",
                    chunk.index + 1,
                    total_chunks
                ),
                None,
            )?;
            let mut words = transcribe_one_chunk(app, source_path, cache, left, client, key, token)
                .await
                .map_err(|error| {
                    format!(
                        "The first recovery request failed: {}",
                        chunk_message(error)
                    )
                })?;
            if token.is_cancelled() {
                return Err("Transcription cancelled.".to_string());
            }
            words.extend(
                transcribe_one_chunk(app, source_path, cache, right, client, key, token)
                    .await
                    .map_err(|error| {
                        format!(
                            "The second recovery request failed: {}",
                            chunk_message(error)
                        )
                    })?,
            );
            words.sort_by(|a, b| a.start_seconds.total_cmp(&b.start_seconds));
            Ok(words)
        }
        Err(error) => Err(chunk_message(error)),
        Ok(words) => Ok(words),
    }
}

fn recoverable_timing_parent(chunk: AudioChunk, error: &ChunkFailure) -> Option<AudioChunk> {
    match error {
        ChunkFailure::Provider(GeminiError::InvalidTranscript(
            TranscriptionError::WordOutsideChunk {
                chunk_duration_ms, ..
            },
        )) => effective_audio_chunk(chunk, *chunk_duration_ms),
        ChunkFailure::Provider(GeminiError::InvalidTranscript(
            TranscriptionError::ReversedWord { .. },
        )) => Some(chunk),
        _ => None,
    }
}

enum ChunkFailure {
    Provider(GeminiError),
    Elevenlabs(ElevenLabsError, AudioChunk),
    Local(String),
}

fn chunk_message(error: ChunkFailure) -> String {
    match error {
        ChunkFailure::Provider(error) => gemini_message(error),
        ChunkFailure::Elevenlabs(error, _) => elevenlabs_message(error),
        ChunkFailure::Local(message) => message,
    }
}

async fn transcribe_one_chunk(
    app: &AppHandle,
    source_path: &str,
    cache: &Path,
    chunk: AudioChunk,
    client: &GeminiClient,
    key: &str,
    token: &CancellationToken,
) -> Result<Vec<TranscriptSegment>, ChunkFailure> {
    let extracted = extract_wav_chunk(Path::new(source_path), cache, chunk, token)
        .await
        .map_err(|error| ChunkFailure::Local(extraction_message(error)))?;
    // An otherwise valid WAV with no samples means the source audio has ended.
    // No provider upload is needed for this planned video tail.
    let Some(effective) = effective_audio_chunk(chunk, extracted.actual_duration_ms) else {
        return Ok(Vec::new());
    };
    projects::set_transcription_status(
        app,
        source_path,
        TranscriptionState::Running,
        "Sending audio to Gemini",
        None,
    )
    .map_err(ChunkFailure::Local)?;
    client
        .transcribe_wav_chunk(&extracted.path, effective, key, token)
        .await
        .map_err(ChunkFailure::Provider)
}

async fn transcribe_one_chunk_elevenlabs(
    app: &AppHandle,
    source_path: &str,
    cache: &Path,
    chunk: AudioChunk,
    client: &ElevenLabsClient,
    key: &str,
    token: &CancellationToken,
) -> Result<Vec<TranscriptSegment>, ChunkFailure> {
    let extracted = extract_wav_chunk(Path::new(source_path), cache, chunk, token)
        .await
        .map_err(|error| ChunkFailure::Local(extraction_message(error)))?;
    let Some(effective) = effective_audio_chunk(chunk, extracted.actual_duration_ms) else {
        return Ok(Vec::new());
    };
    projects::set_transcription_status(
        app,
        source_path,
        TranscriptionState::Running,
        "Sending audio to ElevenLabs",
        None,
    )
    .map_err(ChunkFailure::Local)?;
    let transcript = client
        .transcribe_wav_chunk(&extracted.path, effective, key, token)
        .await
        .map_err(|error| ChunkFailure::Elevenlabs(error, effective))?;
    Ok(transcript.words)
}

fn effective_audio_chunk(chunk: AudioChunk, duration_ms: u64) -> Option<AudioChunk> {
    if duration_ms == 0 {
        return None;
    }
    let end_ms = chunk.start_ms.checked_add(duration_ms)?.min(chunk.end_ms);
    let core_end_ms = chunk.core_end_ms.min(end_ms);
    if end_ms <= chunk.start_ms || core_end_ms <= chunk.core_start_ms {
        return None;
    }
    Some(AudioChunk {
        end_ms,
        core_end_ms,
        ..chunk
    })
}

fn split_chunk_once(parent: AudioChunk) -> Option<(AudioChunk, AudioChunk)> {
    if parent.end_ms - parent.start_ms < MIN_SPLIT_MS
        || parent.core_end_ms - parent.core_start_ms < MIN_SPLIT_MS
    {
        return None;
    }
    let cut = parent.core_start_ms + (parent.core_end_ms - parent.core_start_ms) / 2;
    if cut <= parent.core_start_ms || cut >= parent.core_end_ms {
        return None;
    }
    let overlap = RECOVERY_OVERLAP_MS / 2;
    let left = AudioChunk {
        end_ms: cut.saturating_add(overlap).min(parent.end_ms),
        core_end_ms: cut,
        ..parent
    };
    let right = AudioChunk {
        start_ms: cut.saturating_sub(overlap).max(parent.start_ms),
        core_start_ms: cut,
        ..parent
    };
    if left.end_ms <= left.start_ms || right.end_ms <= right.start_ms {
        return None;
    }
    Some((left, right))
}

fn duration_milliseconds(seconds: f64) -> Result<u64, String> {
    let milliseconds = (seconds * 1000.0).ceil();
    if !seconds.is_finite() || milliseconds < 1.0 || milliseconds > i64::MAX as f64 {
        return Err("The recording duration is invalid.".to_string());
    }
    Ok(milliseconds as u64)
}

fn extraction_message(error: ExtractionError) -> String {
    match error {
        ExtractionError::Cancelled => "Transcription cancelled.".to_string(),
        ExtractionError::FfmpegUnavailable => {
            "FFmpeg is required to extract audio. Add it to PATH and retry.".to_string()
        }
        ExtractionError::TimedOut => {
            "Audio extraction timed out. Retry this recording.".to_string()
        }
        ExtractionError::InvalidSource => {
            "The source recording is no longer available.".to_string()
        }
        _ => "The audio chunk could not be prepared. Check the recording and retry.".to_string(),
    }
}

fn gemini_message(error: GeminiError) -> String {
    match error {
        GeminiError::Cancelled => "Transcription cancelled.".to_string(),
        GeminiError::Http(401 | 403) | GeminiError::InvalidCredential => {
            "The Gemini API key was rejected. Check the saved key and retry.".to_string()
        }
        GeminiError::Http(429) => "Gemini is rate limiting requests. Retry later.".to_string(),
        GeminiError::UploadTooLarge => "An audio chunk exceeds Gemini's upload limit.".to_string(),
        GeminiError::TimedOut => {
            "Gemini took too long to respond. Retry the incomplete chunk.".to_string()
        }
        GeminiError::CleanupFailed => {
            "Gemini could not confirm deletion of a temporary uploaded audio chunk.".to_string()
        }
        GeminiError::InvalidProviderResponse => {
            "Gemini returned an unexpected API response. Retry this chunk; if it persists, check the provider status and model availability.".to_string()
        }
        GeminiError::InvalidTranscript(TranscriptionError::MissingWordAnnotations) => {
            "Gemini returned no word timestamps for speech. Retry this chunk; timed words are required for captions.".to_string()
        }
        GeminiError::InvalidTranscript(TranscriptionError::InvalidWord { index, reason }) => {
            format!("Gemini returned an invalid timed word at position {} ({reason}). Retry this chunk.", index + 1)
        }
        GeminiError::InvalidTranscript(TranscriptionError::ReversedWord {
            index,
            start_offset_ms,
            end_offset_ms,
        }) => format!(
            "Gemini returned reversed word timing at position {} (start {} ms, end {} ms). Automatic short-chunk recovery failed; retry later or inspect this part of the recording.",
            index + 1,
            start_offset_ms,
            end_offset_ms
        ),
        GeminiError::InvalidTranscript(TranscriptionError::WordOutsideChunk {
            index,
            end_offset_ms,
            chunk_duration_ms,
        }) => format!(
            "Gemini placed word {} at {} ms, beyond the extracted audio duration of {} ms. Automatic short-chunk recovery failed; retry later or inspect this part of the recording.",
            index + 1,
            end_offset_ms,
            chunk_duration_ms
        ),
        GeminiError::InvalidTranscript(TranscriptionError::ResponseTooLarge) => {
            "Gemini returned a transcript larger than the supported response size.".to_string()
        }
        GeminiError::InvalidTranscript(TranscriptionError::IncompleteResponse) => {
            "Gemini did not complete transcription. Retry this chunk.".to_string()
        }
        GeminiError::InvalidTranscript(_) => {
            "Gemini returned a transcript with invalid timing or structure. Retry this chunk.".to_string()
        }
        _ => "Gemini transcription failed. Retry the incomplete chunk.".to_string(),
    }
}

fn elevenlabs_message(error: ElevenLabsError) -> String {
    match error {
        ElevenLabsError::Cancelled => "Transcription cancelled.".to_string(),
        ElevenLabsError::InvalidCredential | ElevenLabsError::Http(401 | 403) => {
            "The ElevenLabs API key was rejected. Check the saved key and retry.".to_string()
        }
        ElevenLabsError::Http(429) => {
            "ElevenLabs is rate limiting requests. Retry the incomplete chunk later.".to_string()
        }
        ElevenLabsError::UploadTooLarge => {
            "The extracted audio chunk exceeds the ElevenLabs upload limit.".to_string()
        }
        ElevenLabsError::TimedOut => {
            "ElevenLabs took too long to respond. Retry the incomplete chunk.".to_string()
        }
        ElevenLabsError::InvalidTranscript => {
            "ElevenLabs returned a transcript with invalid or missing word timing. Retry this chunk.".to_string()
        }
        ElevenLabsError::InvalidProviderResponse => {
            "ElevenLabs returned an unexpected response. Retry the incomplete chunk.".to_string()
        }
        ElevenLabsError::InvalidAudio => {
            "The extracted audio chunk could not be read for ElevenLabs.".to_string()
        }
        ElevenLabsError::Http(status) => format!(
            "ElevenLabs returned HTTP {status}. Retry the incomplete chunk or check provider status."
        ),
        ElevenLabsError::Network => {
            "The connection to ElevenLabs failed. Retry the incomplete chunk.".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChunkFailure, duration_milliseconds, effective_audio_chunk, gemini_message,
        recoverable_elevenlabs_timing_parent, recoverable_timing_parent, split_chunk_once,
    };
    use crate::transcription::AudioChunk;
    use crate::transcription::TranscriptionError;
    use crate::transcription::gemini::GeminiError;

    #[test]
    fn duration_rounds_up_to_cover_the_end_of_the_recording() {
        assert_eq!(duration_milliseconds(1.0001).unwrap(), 1001);
        assert!(duration_milliseconds(f64::NAN).is_err());
        assert!(duration_milliseconds(0.0).is_err());
    }

    #[test]
    fn transcript_validation_errors_are_actionable_without_exposing_provider_text() {
        assert!(
            gemini_message(GeminiError::InvalidTranscript(
                TranscriptionError::MissingWordAnnotations
            ))
            .contains("word timestamps")
        );
        let message = gemini_message(GeminiError::InvalidTranscript(
            TranscriptionError::InvalidWord {
                index: 2,
                reason: "invalid end offset",
            },
        ));
        assert!(message.contains("position 3"));
        assert!(message.contains("invalid end offset"));
        let reversed = gemini_message(GeminiError::InvalidTranscript(
            TranscriptionError::ReversedWord {
                index: 19,
                start_offset_ms: 12_500,
                end_offset_ms: 11_900,
            },
        ));
        assert!(reversed.contains("position 20"));
        assert!(reversed.contains("12500 ms"));
        assert!(reversed.contains("11900 ms"));
    }

    #[test]
    fn recovery_children_cover_parent_core_once_with_overlap() {
        let parent = AudioChunk {
            index: 2,
            start_ms: 898_000,
            end_ms: 1_798_000,
            core_start_ms: 899_000,
            core_end_ms: 1_797_000,
        };
        let (left, right) = split_chunk_once(parent).unwrap();
        assert_eq!(left.core_start_ms, parent.core_start_ms);
        assert_eq!(left.core_end_ms, right.core_start_ms);
        assert_eq!(right.core_end_ms, parent.core_end_ms);
        assert_eq!(right.end_ms, parent.end_ms);
        assert_eq!(left.end_ms - right.start_ms, 2_000);
        assert!(
            split_chunk_once(AudioChunk {
                end_ms: parent.start_ms + 9_999,
                ..parent
            })
            .is_none()
        );
        assert!(
            split_chunk_once(AudioChunk {
                core_end_ms: parent.core_start_ms + 9_999,
                ..parent
            })
            .is_none()
        );
    }

    #[test]
    fn only_recoverable_provider_timing_errors_trigger_one_split() {
        let parent = AudioChunk {
            index: 0,
            start_ms: 0,
            end_ms: 900_000,
            core_start_ms: 0,
            core_end_ms: 900_000,
        };
        let reversed = ChunkFailure::Provider(GeminiError::InvalidTranscript(
            TranscriptionError::ReversedWord {
                index: 19,
                start_offset_ms: 12_500,
                end_offset_ms: 11_900,
            },
        ));
        assert_eq!(recoverable_timing_parent(parent, &reversed), Some(parent));
        let outside = ChunkFailure::Provider(GeminiError::InvalidTranscript(
            TranscriptionError::WordOutsideChunk {
                index: 19,
                end_offset_ms: 60_500,
                chunk_duration_ms: 60_000,
            },
        ));
        assert_eq!(
            recoverable_timing_parent(parent, &outside).unwrap().end_ms,
            60_000
        );
        let unrelated = ChunkFailure::Provider(GeminiError::InvalidTranscript(
            TranscriptionError::InvalidWord {
                index: 19,
                reason: "invalid start offset",
            },
        ));
        assert!(recoverable_timing_parent(parent, &unrelated).is_none());
    }

    #[test]
    fn measured_audio_duration_clamps_video_tail_without_fabricating_audio() {
        let chunk = AudioChunk {
            index: 0,
            start_ms: 0,
            end_ms: 15_000,
            core_start_ms: 0,
            core_end_ms: 15_000,
        };
        assert_eq!(effective_audio_chunk(chunk, 14_250).unwrap().end_ms, 14_250);
        assert_eq!(
            effective_audio_chunk(chunk, 14_250).unwrap().core_end_ms,
            14_250
        );
        assert!(effective_audio_chunk(chunk, 0).is_none());
        assert_eq!(effective_audio_chunk(chunk, 15_100).unwrap().end_ms, 15_000);
    }

    #[test]
    fn elevenlabs_recovery_splits_the_measured_audio_not_the_video_tail() {
        let planned = AudioChunk {
            index: 0,
            start_ms: 0,
            end_ms: 900_000,
            core_start_ms: 0,
            core_end_ms: 900_000,
        };
        let effective = effective_audio_chunk(planned, 120_000).unwrap();
        let failure = ChunkFailure::Elevenlabs(
            crate::transcription::elevenlabs::ElevenLabsError::InvalidTranscript,
            effective,
        );
        let retry_parent = recoverable_elevenlabs_timing_parent(&failure).unwrap();
        let (left, right) = split_chunk_once(retry_parent).unwrap();
        assert_eq!(left.core_end_ms, 60_000);
        assert_eq!(right.core_start_ms, 60_000);
        assert_eq!(right.end_ms, 120_000);
        assert!(
            recoverable_elevenlabs_timing_parent(&ChunkFailure::Local("bad extraction".into()))
                .is_none()
        );
    }
}
