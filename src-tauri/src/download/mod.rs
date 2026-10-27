//! Local YouTube acquisition candidate. The worker is not yet OS isolated or brokered.
mod job;
#[cfg(test)]
mod network;
mod official_help;
pub mod youtube_url;
pub use official_help::AcquisitionHelp;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
#[cfg(test)]
use std::{path::Path, time::Duration};
use tauri::Manager;
use tokio_util::sync::CancellationToken;
use youtube_url::YoutubeVideoId;

#[tauri::command]
pub fn open_acquisition_help(topic: AcquisitionHelp) -> Result<(), String> {
    official_help::open_acquisition_help(topic)
}

#[derive(Default)]
pub struct DownloadRuntime {
    inner: Mutex<RuntimeState>,
}
#[derive(Default)]
struct RuntimeState {
    active: Option<CancellationToken>,
    status: DownloadStatus,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadStatus {
    pub state: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DownloadVideoRequest {
    pub url: String,
    #[serde(default)]
    pub owns_media: bool,
    #[serde(default)]
    pub official_vimeo_link: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadedVideo {
    pub path: String,
    pub receipt_path: String,
    pub sha256: String,
    pub file_size_bytes: u64,
    pub source_site: &'static str,
    pub acquisition_method: &'static str,
}

impl DownloadRuntime {
    fn begin(&self) -> Result<CancellationToken, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The download state is unavailable.")?;
        if inner.active.is_some() {
            return Err("A video download is already running.".into());
        }
        let token = CancellationToken::new();
        inner.active = Some(token.clone());
        inner.status = DownloadStatus {
            state: "downloading".into(),
            ..Default::default()
        };
        Ok(token)
    }
    fn progress(&self, bytes: u64, total: Option<u64>) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.status.state == "downloading" {
                inner.status.downloaded_bytes = bytes.min(job::MAX_BYTES);
                inner.status.total_bytes = total.filter(|n| *n <= job::MAX_BYTES);
            }
        }
    }
    fn finish(&self, result: &Result<DownloadedVideo, String>, token: &CancellationToken) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.status.state = if result.is_ok() {
                "completed"
            } else if token.is_cancelled() {
                "canceled"
            } else {
                "failed"
            }
            .into();
            inner.status.error = result.as_ref().err().cloned();
            inner.active = None;
        }
    }
}

#[tauri::command]
pub fn get_download_status(
    runtime: tauri::State<'_, DownloadRuntime>,
) -> Result<DownloadStatus, String> {
    let inner = runtime
        .inner
        .lock()
        .map_err(|_| "The download state is unavailable.")?;
    let mut status = inner.status.clone();
    if status.state.is_empty() {
        status.state = "idle".into();
    }
    Ok(status)
}

#[tauri::command]
pub fn cancel_download_video(runtime: tauri::State<'_, DownloadRuntime>) -> Result<(), String> {
    let inner = runtime
        .inner
        .lock()
        .map_err(|_| "The download state is unavailable.")?;
    if let Some(token) = &inner.active {
        token.cancel();
    }
    Ok(())
}

#[tauri::command]
pub async fn download_video(
    app: tauri::AppHandle,
    runtime: tauri::State<'_, DownloadRuntime>,
    request: DownloadVideoRequest,
) -> Result<DownloadedVideo, String> {
    if !request.owns_media {
        return Err("Confirm that you have rights to use this video before downloading.".into());
    }
    if request.official_vimeo_link {
        return Err("Vimeo links are not supported.".into());
    }
    let id = YoutubeVideoId::parse(&request.url).map_err(str::to_owned)?;
    let token = runtime.begin()?;
    let result = async {
        let root = app
            .path()
            .app_data_dir()
            .map_err(|_| "The download folder is unavailable.")?
            .join("downloads");
        job::run(&root, &id, &token, |bytes, total| {
            runtime.progress(bytes, total)
        })
        .await
    }
    .await;
    runtime.finish(&result, &token);
    result
}

// Retained transport fixtures exercise the retired Vimeo route without enabling it.
#[cfg(test)]
const REMOTE_DOWNLOAD_UNAVAILABLE: &str = "Remote video downloads are unavailable. Public YouTube page links cannot be downloaded here. Download your own uploads through YouTube Studio or Google Takeout, then import the local file. Vimeo downloads are no longer supported.";
#[cfg(test)]
fn reject_remote_request(_: &DownloadVideoRequest) -> Result<DownloadedVideo, String> {
    Err(REMOTE_DOWNLOAD_UNAVAILABLE.into())
}
#[cfg(test)]
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
#[cfg(test)]
async fn wait_for_transfer<T>(
    transfer: impl std::future::Future<Output = Result<T, String>>,
    token: &CancellationToken,
    timeout: std::time::Duration,
) -> Result<T, String> {
    tokio::select! {
        biased;
        _ = token.cancelled() => Err("The download was canceled.".into()),
        result = tokio::time::timeout(timeout, transfer) =>
            result.map_err(|_| "The download exceeded the 30-minute time limit.".to_owned())?,
    }
}
