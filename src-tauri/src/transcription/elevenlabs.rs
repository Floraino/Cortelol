//! ElevenLabs Scribe v2 speech-to-text adapter. Only extracted WAV chunks cross
//! this boundary; credentials and raw provider responses stay in Rust.

use std::path::Path;
use std::time::Duration;

use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::{AudioChunk, TranscriptSegment};

const ENDPOINT: &str = "https://api.elevenlabs.io/v1/speech-to-text";
const MAX_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_WORDS: usize = 200_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);
const MAX_END_OVERHANG_SECONDS: f64 = 0.05;
const MAX_REVERSED_SECONDS: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevenLabsError {
    InvalidCredential,
    InvalidAudio,
    UploadTooLarge,
    Cancelled,
    TimedOut,
    Network,
    Http(u16),
    InvalidProviderResponse,
    InvalidTranscript,
}

#[derive(Debug, Clone)]
pub struct ScribeTranscript {
    /// Continuous text as supplied by Scribe. Timed words are kept separately
    /// for source alignment, captions, and clip selection.
    pub text: String,
    pub words: Vec<TranscriptSegment>,
}

#[derive(Deserialize)]
struct ScribeResponse {
    text: String,
    words: Vec<ScribeWord>,
}

#[derive(Deserialize)]
struct ScribeWord {
    #[serde(rename = "type")]
    kind: String,
    text: String,
    start: Option<f64>,
    end: Option<f64>,
    speaker_id: Option<String>,
}

pub struct ElevenLabsClient {
    http: reqwest::Client,
}

impl ElevenLabsClient {
    pub fn new() -> Result<Self, ElevenLabsError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ElevenLabsError::Network)?;
        Ok(Self { http })
    }

    pub async fn transcribe_wav_chunk(
        &self,
        audio_path: &Path,
        chunk: AudioChunk,
        api_key: &str,
        cancellation: &CancellationToken,
    ) -> Result<ScribeTranscript, ElevenLabsError> {
        validate_chunk(chunk)?;
        if api_key.is_empty() || api_key.chars().any(char::is_control) {
            return Err(ElevenLabsError::InvalidCredential);
        }
        let metadata = cancel(cancellation, tokio::fs::metadata(audio_path))
            .await?
            .map_err(|_| ElevenLabsError::InvalidAudio)?;
        if !metadata.is_file() || metadata.len() <= 44 {
            return Err(ElevenLabsError::InvalidAudio);
        }
        if metadata.len() > MAX_UPLOAD_BYTES {
            return Err(ElevenLabsError::UploadTooLarge);
        }
        let bytes = cancel(cancellation, tokio::fs::read(audio_path))
            .await?
            .map_err(|_| ElevenLabsError::InvalidAudio)?;
        if bytes.len() as u64 != metadata.len() {
            return Err(ElevenLabsError::InvalidAudio);
        }
        let file = Part::bytes(bytes)
            .file_name("cortelol-chunk.wav")
            .mime_str("audio/wav")
            .map_err(|_| ElevenLabsError::InvalidAudio)?;
        let form = Form::new()
            .part("file", file)
            .text("model_id", "scribe_v2")
            .text("diarize", "true")
            .text("timestamps_granularity", "word")
            .text("webhook", "false");
        let request = self
            .http
            .post(ENDPOINT)
            .header("xi-api-key", api_key)
            .multipart(form);
        let mut response = cancel(cancellation, request.send())
            .await?
            .map_err(map_network_error)?;
        if !response.status().is_success() {
            return Err(ElevenLabsError::Http(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ElevenLabsError::InvalidProviderResponse);
        }
        let mut body = Vec::new();
        while let Some(bytes) = cancel(cancellation, response.chunk())
            .await?
            .map_err(map_network_error)?
        {
            if bytes.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err(ElevenLabsError::InvalidProviderResponse);
            }
            body.extend_from_slice(&bytes);
        }
        parse_scribe_response(&body, chunk)
    }
}

fn parse_scribe_response(
    body: &[u8],
    chunk: AudioChunk,
) -> Result<ScribeTranscript, ElevenLabsError> {
    validate_chunk(chunk)?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(ElevenLabsError::InvalidProviderResponse);
    }
    let response: ScribeResponse =
        serde_json::from_slice(body).map_err(|_| ElevenLabsError::InvalidProviderResponse)?;
    if response.text.len() > MAX_RESPONSE_BYTES / 2
        || response.words.len() > MAX_WORDS
        || response
            .text
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t')
    {
        return Err(ElevenLabsError::InvalidProviderResponse);
    }
    let duration = (chunk.end_ms - chunk.start_ms) as f64 / 1000.0;
    let mut words = Vec::new();
    let mut timed_word_count = 0;
    for word in response.words {
        match word.kind.as_str() {
            "spacing" | "audio_event" => continue,
            "word" => {}
            _ => return Err(ElevenLabsError::InvalidProviderResponse),
        }
        timed_word_count += 1;
        let text = word.text.trim();
        if text.is_empty() || text.chars().any(char::is_control) {
            return Err(ElevenLabsError::InvalidTranscript);
        }
        let (Some(mut start), Some(mut end)) = (word.start, word.end) else {
            return Err(ElevenLabsError::InvalidTranscript);
        };
        if !start.is_finite() || !end.is_finite() || start < 0.0 || end < 0.0 {
            return Err(ElevenLabsError::InvalidTranscript);
        }
        if start == end {
            end = (start + 0.1).min(duration);
        } else if end < start && start - end <= MAX_REVERSED_SECONDS {
            std::mem::swap(&mut start, &mut end);
        }
        if end <= start || start >= duration || end > duration + MAX_END_OVERHANG_SECONDS {
            return Err(ElevenLabsError::InvalidTranscript);
        }
        end = end.min(duration);
        let absolute_midpoint = chunk.start_ms as f64 / 1000.0 + (start + end) / 2.0;
        if (absolute_midpoint < chunk.core_start_ms as f64 / 1000.0
            && chunk.core_start_ms > chunk.start_ms)
            || (absolute_midpoint >= chunk.core_end_ms as f64 / 1000.0
                && chunk.core_end_ms < chunk.end_ms)
        {
            continue;
        }
        let speaker = word
            .speaker_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if speaker.is_some_and(|s| s.len() > 64 || s.chars().any(char::is_control)) {
            return Err(ElevenLabsError::InvalidTranscript);
        }
        words.push(TranscriptSegment {
            start_seconds: chunk.start_ms as f64 / 1000.0 + start,
            end_seconds: chunk.start_ms as f64 / 1000.0 + end,
            text: text.to_owned(),
            speaker: speaker.map(str::to_owned),
        });
    }
    if timed_word_count == 0 && !response.text.trim().is_empty() {
        return Err(ElevenLabsError::InvalidTranscript);
    }
    words.sort_by(|left, right| left.start_seconds.total_cmp(&right.start_seconds));
    Ok(ScribeTranscript {
        text: response.text,
        words,
    })
}

fn validate_chunk(chunk: AudioChunk) -> Result<(), ElevenLabsError> {
    if chunk.start_ms >= chunk.end_ms
        || chunk.core_start_ms < chunk.start_ms
        || chunk.core_end_ms > chunk.end_ms
        || chunk.core_start_ms >= chunk.core_end_ms
    {
        return Err(ElevenLabsError::InvalidAudio);
    }
    Ok(())
}

fn map_network_error(error: reqwest::Error) -> ElevenLabsError {
    if error.is_timeout() {
        ElevenLabsError::TimedOut
    } else {
        ElevenLabsError::Network
    }
}

async fn cancel<F: std::future::Future>(
    token: &CancellationToken,
    future: F,
) -> Result<F::Output, ElevenLabsError> {
    tokio::select! {
        _ = token.cancelled() => Err(ElevenLabsError::Cancelled),
        result = future => Ok(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk() -> AudioChunk {
        AudioChunk {
            index: 0,
            start_ms: 1000,
            end_ms: 3000,
            core_start_ms: 1000,
            core_end_ms: 3000,
        }
    }

    #[test]
    fn retains_full_text_and_maps_words_to_source_time() {
        let body = br#"{"text":"Hello, world!","words":[{"type":"word","text":"Hello,","start":0.1,"end":0.4,"speaker_id":"speaker_0"},{"type":"spacing","text":" ","start":0.4,"end":0.5},{"type":"word","text":"world!","start":0.5,"end":0.9,"speaker_id":"speaker_0"}]}"#;
        let parsed = parse_scribe_response(body, chunk()).unwrap();
        assert_eq!(parsed.text, "Hello, world!");
        assert_eq!(parsed.words.len(), 2);
        assert_eq!(parsed.words[0].start_seconds, 1.1);
        assert_eq!(parsed.words[1].speaker.as_deref(), Some("speaker_0"));
    }

    #[test]
    fn rejects_word_outside_uploaded_audio() {
        let body =
            br#"{"text":"late","words":[{"type":"word","text":"late","start":2.5,"end":2.8}]}"#;
        assert_eq!(
            parse_scribe_response(body, chunk()).unwrap_err(),
            ElevenLabsError::InvalidTranscript
        );
    }

    #[test]
    fn overlap_words_have_one_owner() {
        let mut second = chunk();
        second.core_start_ms = 1500;
        let body = br#"{"text":"Before after","words":[{"type":"word","text":"Before","start":0.1,"end":0.2},{"type":"word","text":"after","start":0.6,"end":0.8}]}"#;
        let parsed = parse_scribe_response(body, second).unwrap();
        assert_eq!(parsed.words.len(), 1);
        assert_eq!(parsed.words[0].text, "after");
    }

    #[test]
    fn chunk_can_contain_only_words_owned_by_a_neighbor() {
        let mut second = chunk();
        second.core_start_ms = 1500;
        let body =
            br#"{"text":"Before","words":[{"type":"word","text":"Before","start":0.1,"end":0.2}]}"#;
        let parsed = parse_scribe_response(body, second).unwrap();
        assert!(parsed.words.is_empty());
        assert_eq!(parsed.text, "Before");
    }

    #[test]
    fn silent_audio_can_have_no_words() {
        let body = br#"{"text":"","words":[]}"#;
        assert!(
            parse_scribe_response(body, chunk())
                .unwrap()
                .words
                .is_empty()
        );
    }
}
