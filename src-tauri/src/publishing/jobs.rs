//! Review and command boundary for one official-platform publication.
//! A social access token can only be placed in the OS credential store by an
//! official OAuth flow; the webview never receives or supplies it here.

use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::Mutex,
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

use crate::{
    credentials::{self, SocialPlatform},
    export::jobs::{latest_completed_export, latest_completed_export_with_sha256},
};

use super::{
    Capabilities, Destination, PublishJob, PublishMetadata, PublishState, ReviewConfirmation,
    tiktok,
};

#[derive(Default)]
pub struct PublishRuntime {
    inner: Mutex<RuntimeInner>,
}

#[derive(Default)]
struct RuntimeInner {
    job: Option<PublishJob>,
    reviewed_hash: Option<[u8; 32]>,
    cancellation: Option<CancellationToken>,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    #[serde(rename = "tiktok")]
    TikTok,
    Instagram,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Eligibility {
    available: bool,
    reason: Option<String>,
    capabilities: Option<Capabilities>,
}

impl Eligibility {
    fn unavailable(reason: &str) -> Self {
        Self {
            available: false,
            reason: Some(reason.into()),
            capabilities: None,
        }
    }
}

fn token() -> Result<Option<String>, String> {
    credentials::get_social_token(SocialPlatform::TikTok)
}

#[tauri::command]
pub async fn get_publish_status(app: AppHandle) -> Result<Option<PublishJob>, String> {
    let current_export = latest_completed_export_async(app.clone()).await?;
    let runtime = app.state::<PublishRuntime>();
    let inner = runtime
        .inner
        .lock()
        .map_err(|_| "The publishing runtime is unavailable.")?;
    Ok(inner
        .job
        .as_ref()
        .filter(|job| Some(job.export_path().to_path_buf()) == current_export)
        .cloned())
}

#[tauri::command]
pub async fn get_publish_capabilities(platform: Platform) -> Result<Eligibility, String> {
    match platform {
        Platform::Instagram => Ok(Eligibility::unavailable(
            "Instagram Reels requires a professional account, approved app permissions, and a public video URL. Cortelol has no media hosting configured; use the local MP4 manually.",
        )),
        Platform::TikTok => {
            let Some(token) = token()? else {
                return Ok(Eligibility::unavailable(
                    "Connect TikTok through an approved app's desktop OAuth flow before direct posting. The local MP4 is ready for manual upload.",
                ));
            };
            let client = tiktok::client().map_err(|error| error.to_string())?;
            let cancel = CancellationToken::new();
            match tiktok::query_capabilities(&client, &token, &cancel).await {
                Ok(capabilities) => Ok(Eligibility {
                    available: true,
                    reason: None,
                    capabilities: Some(capabilities),
                }),
                Err(error) => Ok(Eligibility::unavailable(&error.to_string())),
            }
        }
    }
}

async fn latest_completed_export_async(app: AppHandle) -> Result<Option<PathBuf>, String> {
    tokio::task::spawn_blocking(move || latest_completed_export(&app))
        .await
        .map_err(|_| "The reviewed export could not be checked.".to_string())?
}

async fn reviewed_export_async(app: AppHandle) -> Result<Option<(PathBuf, [u8; 32])>, String> {
    tokio::task::spawn_blocking(move || latest_completed_export_with_sha256(&app))
        .await
        .map_err(|_| "The reviewed export could not be verified.".to_string())?
}

async fn matching_export(app: AppHandle, requested: String) -> Result<(PathBuf, [u8; 32]), String> {
    let (approved, hash) = reviewed_export_async(app)
        .await?
        .ok_or_else(|| "Export a reviewed clip before preparing publication.".to_string())?;
    let requested = PathBuf::from(requested)
        .canonicalize()
        .map_err(|_| "The local export is unavailable.".to_string())?;
    let approved = approved
        .canonicalize()
        .map_err(|_| "The reviewed export is unavailable.".to_string())?;
    if requested != approved {
        return Err("Only the latest reviewed export can be published.".into());
    }
    Ok((approved, hash))
}

fn private_snapshot(
    app: &AppHandle,
    export_path: &std::path::Path,
    expected_hash: [u8; 32],
) -> Result<tempfile::TempDir, String> {
    let cache = app
        .path()
        .app_cache_dir()
        .map_err(|_| "The private publishing directory is unavailable.".to_string())?;
    std::fs::create_dir_all(&cache)
        .map_err(|_| "The private publishing directory could not be created.".to_string())?;
    let directory = tempfile::Builder::new()
        .prefix("publish-")
        .tempdir_in(cache)
        .map_err(|_| "The private publishing directory could not be created.".to_string())?;
    let mut source = std::fs::File::open(export_path)
        .map_err(|_| "The reviewed export could not be read.".to_string())?;
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.path().join("reviewed.mp4"))
        .map_err(|_| "The private publishing copy could not be created.".to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = source
            .read(&mut buffer)
            .map_err(|_| "The reviewed export could not be copied.".to_string())?;
        if count == 0 {
            break;
        }
        destination
            .write_all(&buffer[..count])
            .map_err(|_| "The private publishing copy could not be written.".to_string())?;
        hasher.update(&buffer[..count]);
    }
    destination
        .sync_all()
        .map_err(|_| "The private publishing copy could not be completed.".to_string())?;
    let copied_hash: [u8; 32] = hasher.finalize().into();
    if copied_hash != expected_hash {
        return Err(
            "The exported MP4 changed after review. Review and export it again before publishing."
                .into(),
        );
    }
    Ok(directory)
}

#[tauri::command]
pub async fn prepare_publish(
    app: AppHandle,
    export_path: String,
    metadata: PublishMetadata,
    confirmation: ReviewConfirmation,
) -> Result<PublishJob, String> {
    let (export_path, reviewed_hash) = matching_export(app.clone(), export_path).await?;
    if !matches!(metadata.destination, Destination::TikTok { .. }) {
        return Err(
            "Instagram direct publishing is unavailable until a public media URL is configured."
                .into(),
        );
    }
    let token = token()?
        .ok_or_else(|| "Connect TikTok through an approved app before publishing.".to_string())?;
    let client = tiktok::client().map_err(|error| error.to_string())?;
    let capabilities = tiktok::query_capabilities(&client, &token, &CancellationToken::new())
        .await
        .map_err(|error| error.to_string())?;
    let mut job = PublishJob::new(export_path, metadata);
    job.review(confirmation, &capabilities)?;
    let runtime = app.state::<PublishRuntime>();
    let mut inner = runtime
        .inner
        .lock()
        .map_err(|_| "The publishing runtime is unavailable.")?;
    if inner.cancellation.is_some()
        || inner
            .job
            .as_ref()
            .is_some_and(|existing| matches!(existing.state(), PublishState::Processing { .. }))
    {
        return Err("A publication is already in progress.".into());
    }
    inner.job = Some(job.clone());
    inner.reviewed_hash = Some(reviewed_hash);
    Ok(job)
}

#[tauri::command]
pub async fn confirm_publish(app: AppHandle) -> Result<PublishJob, String> {
    let (approved_export, approved_hash) = reviewed_export_async(app.clone())
        .await?
        .ok_or_else(|| "The reviewed export is unavailable.".to_string())?;
    let token = token()?
        .ok_or_else(|| "TikTok authorization is unavailable. Reconnect the account.".to_string())?;
    let client = tiktok::client().map_err(|error| error.to_string())?;
    let capabilities = tiktok::query_capabilities(&client, &token, &CancellationToken::new())
        .await
        .map_err(|error| error.to_string())?;
    let runtime = app.state::<PublishRuntime>();
    let (job, cancel) = {
        let mut inner = runtime
            .inner
            .lock()
            .map_err(|_| "The publishing runtime is unavailable.")?;
        if inner.cancellation.is_some() {
            return Err("A publication is already in progress.".into());
        }
        let reviewed_hash = inner.reviewed_hash;
        let job = inner
            .job
            .as_mut()
            .ok_or_else(|| "Review an export before publishing.".to_string())?;
        if job.export_path() != approved_export || reviewed_hash != Some(approved_hash) {
            return Err("Only the latest reviewed export can be published.".into());
        }
        job.begin_publish(&capabilities)?;
        let job = job.clone();
        let cancel = CancellationToken::new();
        inner.cancellation = Some(cancel.clone());
        (job, cancel)
    };
    let snapshot_result = {
        let app = app.clone();
        let path = job.export_path().to_path_buf();
        tokio::task::spawn_blocking(move || private_snapshot(&app, &path, approved_hash))
            .await
            .map_err(|_| "The private publishing copy could not be prepared.".to_string())?
    };
    let snapshot = match snapshot_result {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let mut inner = runtime
                .inner
                .lock()
                .map_err(|_| "The publishing runtime is unavailable.")?;
            inner.cancellation = None;
            if let Some(current) = inner.job.as_mut() {
                current.fail(error.clone())?;
            }
            return Err(error);
        }
    };
    let result = match &job.metadata().destination {
        Destination::TikTok { privacy, settings } => {
            tiktok::upload_video(
                &client,
                &token,
                &snapshot.path().join("reviewed.mp4"),
                &job.metadata().caption,
                *privacy,
                settings,
                &cancel,
            )
            .await
        }
        Destination::Instagram { .. } => return Err("Instagram publishing is unavailable.".into()),
    };
    let mut inner = runtime
        .inner
        .lock()
        .map_err(|_| "The publishing runtime is unavailable.")?;
    inner.cancellation = None;
    let current = inner
        .job
        .as_mut()
        .ok_or_else(|| "The publishing job is unavailable.".to_string())?;
    match result {
        Ok(reference) => current.mark_processing(reference)?,
        Err(tiktok::Error::Cancelled) => {
            current.cancel()?;
        }
        Err(error) => {
            current.fail(error.to_string())?;
        }
    }
    Ok(current.clone())
}

#[tauri::command]
pub fn cancel_publish(app: AppHandle) -> Result<(), String> {
    let runtime = app.state::<PublishRuntime>();
    let mut inner = runtime
        .inner
        .lock()
        .map_err(|_| "The publishing runtime is unavailable.")?;
    if let Some(cancel) = &inner.cancellation {
        cancel.cancel();
    } else if let Some(job) = &mut inner.job {
        job.cancel()?;
    }
    Ok(())
}

#[tauri::command]
pub async fn refresh_publish_status(app: AppHandle) -> Result<Option<PublishJob>, String> {
    let runtime = app.state::<PublishRuntime>();
    let reference = {
        let inner = runtime
            .inner
            .lock()
            .map_err(|_| "The publishing runtime is unavailable.")?;
        match inner.job.as_ref().map(PublishJob::state) {
            Some(PublishState::Processing { provider_reference }) => provider_reference.clone(),
            _ => return Ok(inner.job.clone()),
        }
    };
    let token = token()?
        .ok_or_else(|| "TikTok authorization is unavailable. Reconnect the account.".to_string())?;
    let client = tiktok::client().map_err(|error| error.to_string())?;
    let result = tiktok::fetch_status(&client, &token, &reference, &CancellationToken::new()).await;
    let mut inner = runtime
        .inner
        .lock()
        .map_err(|_| "The publishing runtime is unavailable.")?;
    let job = inner
        .job
        .as_mut()
        .ok_or_else(|| "The publishing job is unavailable.".to_string())?;
    if !matches!(job.state(), PublishState::Processing { provider_reference } if provider_reference == &reference)
    {
        return Ok(Some(job.clone()));
    }
    match result {
        Ok(tiktok::PostStatus::Published { public_post_id }) => {
            job.mark_publish_complete(public_post_id)?
        }
        Ok(tiktok::PostStatus::Failed { reason }) => job.fail(reason)?,
        Ok(tiktok::PostStatus::Processing) => {}
        Err(error) => return Err(error.to_string()),
    }
    Ok(Some(job.clone()))
}
