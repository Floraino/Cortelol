//! Gemini Files and Interactions adapters. Credentials are supplied only by
//! backend code and are never returned or stored with transcript data.

use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::{
    AudioChunk, TranscriptSegment, TranscriptionError, gemini_request, parse_gemini_chunk,
};

const BASE: &str = "https://generativelanguage.googleapis.com";
const MAX_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const FILE_READY_TIMEOUT: Duration = Duration::from_secs(180);
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiError {
    InvalidCredential,
    InvalidAudio,
    UploadTooLarge,
    Cancelled,
    TimedOut,
    Network,
    Http(u16),
    InvalidProviderResponse,
    InvalidTranscript(TranscriptionError),
    ProviderFailed,
    CleanupFailed,
}

#[derive(Deserialize)]
struct UploadResult {
    file: RemoteFile,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteFile {
    name: String,
    uri: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

#[derive(Deserialize)]
struct InteractionState {
    id: Option<String>,
    status: String,
}

pub struct GeminiClient {
    http: reqwest::Client,
}

impl GeminiClient {
    /// Each request has a bounded transport timeout; polling has separate
    /// overall deadlines. Keep one client for connection reuse across chunks.
    pub fn new() -> Result<Self, GeminiError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| GeminiError::Network)?;
        Ok(Self { http })
    }

    /// Transcribe one previously extracted WAV chunk and delete its Files API
    /// upload even if transcription fails or the user cancels. The credential
    /// must be fetched from the OS credential store by the backend caller.
    pub async fn transcribe_wav_chunk(
        &self,
        audio_path: &Path,
        chunk: AudioChunk,
        api_key: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<TranscriptSegment>, GeminiError> {
        if api_key.is_empty() || api_key.chars().any(char::is_control) {
            return Err(GeminiError::InvalidCredential);
        }
        let file = self.upload_wav(audio_path, api_key, cancellation).await?;
        let file_name = file.name.clone();
        let result = async {
            let file = self.wait_for_file(file, api_key, cancellation).await?;
            let response = self
                .create_interaction(&file, api_key, cancellation)
                .await?;
            parse_gemini_chunk(&response, chunk).map_err(GeminiError::InvalidTranscript)
        }
        .await;
        // Cleanup deliberately ignores cancellation and uses the client's
        // transport timeout, so cancel does not leave uploaded media behind.
        let cleanup = self.delete_file(&file_name, api_key).await;
        match (result, cleanup) {
            (_, Err(_)) => Err(GeminiError::CleanupFailed),
            (result, Ok(())) => result,
        }
    }

    async fn upload_wav(
        &self,
        path: &Path,
        api_key: &str,
        cancellation: &CancellationToken,
    ) -> Result<RemoteFile, GeminiError> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|_| GeminiError::InvalidAudio)?;
        if !metadata.is_file() || metadata.len() <= 44 {
            return Err(GeminiError::InvalidAudio);
        }
        if metadata.len() > MAX_UPLOAD_BYTES {
            return Err(GeminiError::UploadTooLarge);
        }
        let start = self
            .http
            .post(format!("{BASE}/upload/v1beta/files"))
            .header("x-goog-api-key", api_key)
            .header("X-Goog-Upload-Protocol", "resumable")
            .header("X-Goog-Upload-Command", "start")
            .header(
                "X-Goog-Upload-Header-Content-Length",
                metadata.len().to_string(),
            )
            .header("X-Goog-Upload-Header-Content-Type", "audio/wav")
            .json(&serde_json::json!({"file": {"display_name": "Cortelol transcription audio"}}));
        let start_response = self.send(start, cancellation).await?;
        let upload_url = start_response
            .headers()
            .get("x-goog-upload-url")
            .and_then(|value| value.to_str().ok())
            .ok_or(GeminiError::InvalidProviderResponse)?;
        let parsed =
            reqwest::Url::parse(upload_url).map_err(|_| GeminiError::InvalidProviderResponse)?;
        if parsed.scheme() != "https"
            || parsed.host_str() != Some("generativelanguage.googleapis.com")
            || !parsed.path().starts_with("/upload/v1beta/files")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(GeminiError::InvalidProviderResponse);
        }
        let bytes = cancel(cancellation, tokio::fs::read(path))
            .await?
            .map_err(|_| GeminiError::InvalidAudio)?;
        if bytes.len() as u64 != metadata.len() {
            return Err(GeminiError::InvalidAudio);
        }
        let finish = self
            .http
            .post(parsed)
            .header(CONTENT_TYPE, "audio/wav")
            .header(CONTENT_LENGTH, metadata.len())
            .header("X-Goog-Upload-Offset", "0")
            .header("X-Goog-Upload-Command", "upload, finalize")
            .body(bytes);
        let response = self.send(finish, cancellation).await?;
        let body = self.body(response, cancellation).await?;
        let uploaded: UploadResult =
            serde_json::from_slice(&body).map_err(|_| GeminiError::InvalidProviderResponse)?;
        validate_file(&uploaded.file)?;
        Ok(uploaded.file)
    }

    async fn wait_for_file(
        &self,
        mut file: RemoteFile,
        api_key: &str,
        cancellation: &CancellationToken,
    ) -> Result<RemoteFile, GeminiError> {
        let deadline = Instant::now() + FILE_READY_TIMEOUT;
        loop {
            match file.state.as_deref() {
                Some("ACTIVE") => return Ok(file),
                Some("FAILED") => return Err(GeminiError::ProviderFailed),
                Some("PROCESSING") | None | Some("STATE_UNSPECIFIED") => {}
                _ => return Err(GeminiError::InvalidProviderResponse),
            }
            if Instant::now() >= deadline {
                return Err(GeminiError::TimedOut);
            }
            wait_poll(cancellation).await?;
            let request = self
                .http
                .get(format!("{BASE}/v1beta/{}", file.name))
                .header("x-goog-api-key", api_key);
            let response = self.send(request, cancellation).await?;
            let body = self.body(response, cancellation).await?;
            file =
                serde_json::from_slice(&body).map_err(|_| GeminiError::InvalidProviderResponse)?;
            validate_file(&file)?;
        }
    }

    async fn create_interaction(
        &self,
        file: &RemoteFile,
        api_key: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, GeminiError> {
        let payload = gemini_request(&file.uri, "audio/wav")
            .map_err(|_| GeminiError::InvalidProviderResponse)?;
        let request = self
            .http
            .post(format!("{BASE}/v1beta/interactions"))
            .header("x-goog-api-key", api_key)
            .json(&payload);
        let response = self.send(request, cancellation).await?;
        let mut body = self.body(response, cancellation).await?;
        let mut state: InteractionState =
            serde_json::from_slice(&body).map_err(|_| GeminiError::InvalidProviderResponse)?;
        let deadline = Instant::now() + INTERACTION_TIMEOUT;
        loop {
            match state.status.as_str() {
                "completed" => return Ok(body),
                "failed" | "cancelled" | "incomplete" | "requires_action" => {
                    return Err(GeminiError::ProviderFailed);
                }
                "in_progress" => {}
                _ => return Err(GeminiError::InvalidProviderResponse),
            }
            if Instant::now() >= deadline {
                return Err(GeminiError::TimedOut);
            }
            let id = state
                .id
                .as_deref()
                .filter(|id| valid_id(id))
                .ok_or(GeminiError::InvalidProviderResponse)?;
            wait_poll(cancellation).await?;
            let request = self
                .http
                .get(format!("{BASE}/v1beta/interactions/{id}"))
                .header("x-goog-api-key", api_key);
            let response = self.send(request, cancellation).await?;
            body = self.body(response, cancellation).await?;
            state =
                serde_json::from_slice(&body).map_err(|_| GeminiError::InvalidProviderResponse)?;
        }
    }

    async fn delete_file(&self, name: &str, api_key: &str) -> Result<(), GeminiError> {
        if !valid_file_name(name) {
            return Err(GeminiError::InvalidProviderResponse);
        }
        let response = self
            .http
            .delete(format!("{BASE}/v1beta/{name}"))
            .header("x-goog-api-key", api_key)
            .send()
            .await
            .map_err(|_| GeminiError::Network)?;
        if !response.status().is_success() {
            return Err(GeminiError::Http(response.status().as_u16()));
        }
        Ok(())
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::Response, GeminiError> {
        let response = cancel(cancellation, request.send())
            .await?
            .map_err(|_| GeminiError::Network)?;
        if !response.status().is_success() {
            return Err(GeminiError::Http(response.status().as_u16()));
        }
        Ok(response)
    }

    async fn body(
        &self,
        mut response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, GeminiError> {
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(GeminiError::InvalidProviderResponse);
        }
        let mut body = Vec::new();
        while let Some(chunk) = cancel(cancellation, response.chunk())
            .await?
            .map_err(|_| GeminiError::Network)?
        {
            append_bounded(&mut body, &chunk)?;
        }
        Ok(body)
    }
}

fn append_bounded(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), GeminiError> {
    if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
        return Err(GeminiError::InvalidProviderResponse);
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_file_name(name: &str) -> bool {
    name.strip_prefix("files/").is_some_and(valid_id)
}

fn validate_file(file: &RemoteFile) -> Result<(), GeminiError> {
    if !valid_file_name(&file.name)
        || file.uri != format!("{BASE}/v1beta/{}", file.name)
        || file
            .mime_type
            .as_deref()
            .is_some_and(|mime| mime != "audio/wav")
    {
        return Err(GeminiError::InvalidProviderResponse);
    }
    Ok(())
}

async fn cancel<F: std::future::Future>(
    token: &CancellationToken,
    future: F,
) -> Result<F::Output, GeminiError> {
    tokio::select! {
        _ = token.cancelled() => Err(GeminiError::Cancelled),
        result = future => Ok(result),
    }
}

async fn wait_poll(cancellation: &CancellationToken) -> Result<(), GeminiError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(GeminiError::Cancelled),
        _ = tokio::time::sleep(Duration::from_secs(3)) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_file_name_and_uri_must_match() {
        let valid = RemoteFile {
            name: "files/abc_123".into(),
            uri: format!("{BASE}/v1beta/files/abc_123"),
            state: Some("ACTIVE".into()),
            mime_type: Some("audio/wav".into()),
        };
        assert!(validate_file(&valid).is_ok());
        let invalid = RemoteFile {
            uri: "https://elsewhere.example/file".into(),
            ..valid
        };
        assert_eq!(
            validate_file(&invalid),
            Err(GeminiError::InvalidProviderResponse)
        );
        assert!(!valid_file_name("files/abc/other"));
    }

    #[test]
    fn streaming_response_rejects_bytes_over_limit() {
        let mut body = vec![0; MAX_RESPONSE_BYTES - 2];
        append_bounded(&mut body, &[1, 2]).unwrap();
        assert_eq!(body.len(), MAX_RESPONSE_BYTES);
        assert_eq!(
            append_bounded(&mut body, &[3]),
            Err(GeminiError::InvalidProviderResponse)
        );
        assert_eq!(body.len(), MAX_RESPONSE_BYTES);
    }
}
