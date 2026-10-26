//! TikTok Content Posting API adapter. Only documented endpoints are used.
//! Credentials and upload URLs never leave Rust or appear in errors.

use reqwest::{Client, StatusCode, Url, header};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{path::Path, time::Duration};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
};
use tokio_util::sync::CancellationToken;

use super::{Capabilities, TikTokPrivacy, TikTokSettings};

const BASE: &str = "https://open.tiktokapis.com";
const MAX_RESPONSE_BYTES: usize = 128 * 1024;
const CHUNK_SIZE: u64 = 10_000_000;
const MAX_VIDEO_BYTES: u64 = 4_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Cancelled,
    Authentication,
    Permission,
    RateLimited,
    Unavailable,
    InvalidResponse,
    UploadRejected,
    InvalidMedia,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Cancelled => "The TikTok upload was cancelled.",
            Self::Authentication => "TikTok authorization expired. Reconnect this account.",
            Self::Permission => {
                "TikTok has not granted this app permission to publish to this account."
            }
            Self::RateLimited => "TikTok is limiting publishing requests. Try again later.",
            Self::Unavailable => "TikTok is temporarily unavailable. Try again later.",
            Self::InvalidResponse => "TikTok returned an unusable response.",
            Self::UploadRejected => {
                "TikTok rejected this video upload. Check the exported MP4 and account limits."
            }
            Self::InvalidMedia => "The exported MP4 cannot be uploaded to TikTok.",
        };
        f.write_str(message)
    }
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    error: ApiError,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: String,
}

#[derive(Debug, Deserialize)]
struct CreatorInfo {
    creator_username: String,
    creator_nickname: String,
    privacy_level_options: Vec<TikTokPrivacy>,
    comment_disabled: bool,
    duet_disabled: bool,
    stitch_disabled: bool,
    max_video_post_duration_sec: u64,
}

#[derive(Debug, Deserialize)]
struct InitData {
    publish_id: String,
    upload_url: String,
}

#[derive(Debug, Deserialize)]
struct StatusData {
    status: String,
    fail_reason: Option<String>,
    #[serde(default, rename = "publicaly_available_post_id")]
    public_post_ids: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostStatus {
    Processing,
    Published { public_post_id: Option<String> },
    Failed { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkPlan {
    chunk_size: u64,
    count: u64,
}

fn chunk_plan(size: u64) -> Result<ChunkPlan, Error> {
    if size == 0 || size > MAX_VIDEO_BYTES {
        return Err(Error::InvalidMedia);
    }
    let chunk_size = size.min(CHUNK_SIZE);
    let count = (size / chunk_size).max(1);
    if count > 1000 {
        return Err(Error::InvalidMedia);
    }
    Ok(ChunkPlan { chunk_size, count })
}

fn chunk_range(plan: ChunkPlan, index: u64, total: u64) -> (u64, u64) {
    let start = index * plan.chunk_size;
    let end_exclusive = if index + 1 == plan.count {
        total
    } else {
        start + plan.chunk_size
    };
    (start, end_exclusive)
}

fn classify(status: StatusCode, code: Option<&str>) -> Error {
    match (status.as_u16(), code) {
        (401, _) | (_, Some("access_token_invalid")) => Error::Authentication,
        (
            _,
            Some("scope_not_authorized" | "unaudited_client_can_only_post_to_private_accounts"),
        ) => Error::Permission,
        (429, _)
        | (
            _,
            Some("rate_limit_exceeded" | "spam_risk_too_many_posts" | "reached_active_user_cap"),
        ) => Error::RateLimited,
        (500..=599, _) => Error::Unavailable,
        _ => Error::InvalidResponse,
    }
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(Error::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn api_post<T: for<'de> Deserialize<'de>>(
    client: &Client,
    token: &str,
    path: &str,
    body: serde_json::Value,
    cancel: &CancellationToken,
) -> Result<T, Error> {
    if token.trim().is_empty() {
        return Err(Error::Authentication);
    }
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(Error::Cancelled),
        result = client.post(format!("{BASE}{path}")).bearer_auth(token).json(&body).send() => result.map_err(|_| Error::Unavailable)?,
    };
    let status = response.status();
    let bytes = tokio::select! {
        _ = cancel.cancelled() => return Err(Error::Cancelled),
        result = bounded_body(response) => result?,
    };
    let envelope: Envelope<T> =
        serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)?;
    if !status.is_success() || envelope.error.code != "ok" {
        return Err(classify(status, Some(&envelope.error.code)));
    }
    envelope.data.ok_or(Error::InvalidResponse)
}

pub fn client() -> Result<Client, Error> {
    Client::builder()
        .timeout(Duration::from_secs(45))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::Unavailable)
}

pub async fn query_capabilities(
    client: &Client,
    token: &str,
    cancel: &CancellationToken,
) -> Result<Capabilities, Error> {
    let data: CreatorInfo = api_post(
        client,
        token,
        "/v2/post/publish/creator_info/query/",
        json!({}),
        cancel,
    )
    .await?;
    if data.creator_username.is_empty()
        || data.creator_nickname.trim().is_empty()
        || data.max_video_post_duration_sec == 0
        || data.privacy_level_options.is_empty()
    {
        return Err(Error::InvalidResponse);
    }
    Ok(Capabilities::TikTok {
        account_id: data.creator_username,
        display_name: data.creator_nickname,
        can_post: true,
        has_publish_scope: true,
        // No audit result is exposed by creator_info. Default to private posting.
        app_audited: false,
        allowed_privacy: data.privacy_level_options,
        max_duration_ms: data.max_video_post_duration_sec.saturating_mul(1000),
        max_caption_utf16_units: 2200,
        comments_disabled: data.comment_disabled,
        duet_disabled: data.duet_disabled,
        stitch_disabled: data.stitch_disabled,
    })
}

#[derive(Serialize)]
struct PostInfo<'a> {
    title: &'a str,
    privacy_level: TikTokPrivacy,
    disable_comment: bool,
    disable_duet: bool,
    disable_stitch: bool,
    brand_content_toggle: bool,
    brand_organic_toggle: bool,
}

fn post_info<'a>(
    caption: &'a str,
    privacy: TikTokPrivacy,
    settings: &TikTokSettings,
) -> PostInfo<'a> {
    PostInfo {
        title: caption,
        privacy_level: privacy,
        disable_comment: !settings.allow_comments,
        disable_duet: !settings.allow_duet,
        disable_stitch: !settings.allow_stitch,
        brand_content_toggle: settings.branded_content,
        brand_organic_toggle: settings.promotes_own_brand,
    }
}

fn validated_upload_url(raw: &str) -> Result<Url, Error> {
    let url = Url::parse(raw).map_err(|_| Error::InvalidResponse)?;
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || url.port().is_some()
        || !url.host_str().is_some_and(|host| {
            host == "open-upload.tiktokapis.com"
                || (host.starts_with("upload.") && host.ends_with(".tiktokapis.com"))
        })
    {
        return Err(Error::InvalidResponse);
    }
    Ok(url)
}

pub async fn upload_video(
    client: &Client,
    token: &str,
    path: &Path,
    caption: &str,
    privacy: TikTokPrivacy,
    settings: &TikTokSettings,
    cancel: &CancellationToken,
) -> Result<String, Error> {
    let file = File::open(path).await.map_err(|_| Error::InvalidMedia)?;
    let size = file
        .metadata()
        .await
        .map_err(|_| Error::InvalidMedia)?
        .len();
    let plan = chunk_plan(size)?;
    let info = post_info(caption, privacy, settings);
    let data: InitData = api_post(
        client,
        token,
        "/v2/post/publish/video/init/",
        json!({
            "post_info": info,
            "source_info": { "source": "FILE_UPLOAD", "video_size": size,
                "chunk_size": plan.chunk_size, "total_chunk_count": plan.count }
        }),
        cancel,
    )
    .await?;
    if data.publish_id.is_empty() || data.publish_id.len() > 256 {
        return Err(Error::InvalidResponse);
    }
    let upload_url = validated_upload_url(&data.upload_url)?;
    let mut file = file;
    for index in 0..plan.count {
        let (start, end) = chunk_range(plan, index, size);
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|_| Error::InvalidMedia)?;
        let mut chunk = vec![0; (end - start) as usize];
        file.read_exact(&mut chunk)
            .await
            .map_err(|_| Error::InvalidMedia)?;
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            result = client.put(upload_url.clone())
                .header(header::CONTENT_TYPE, "video/mp4")
                .header(header::CONTENT_RANGE, format!("bytes {start}-{}/{size}", end - 1))
                .header(header::CONTENT_LENGTH, chunk.len())
                .body(chunk).send() => result.map_err(|_| Error::Unavailable)?,
        };
        let expected = if index + 1 == plan.count {
            StatusCode::CREATED
        } else {
            StatusCode::PARTIAL_CONTENT
        };
        if response.status() != expected {
            return Err(Error::UploadRejected);
        }
    }
    Ok(data.publish_id)
}

pub async fn fetch_status(
    client: &Client,
    token: &str,
    publish_id: &str,
    cancel: &CancellationToken,
) -> Result<PostStatus, Error> {
    if publish_id.is_empty() || publish_id.len() > 256 {
        return Err(Error::InvalidResponse);
    }
    let data: StatusData = api_post(
        client,
        token,
        "/v2/post/publish/status/fetch/",
        json!({"publish_id": publish_id}),
        cancel,
    )
    .await?;
    match data.status.as_str() {
        "PROCESSING_UPLOAD" | "PROCESSING_DOWNLOAD" => Ok(PostStatus::Processing),
        "PUBLISH_COMPLETE" => Ok(PostStatus::Published {
            public_post_id: data.public_post_ids.first().map(ToString::to_string),
        }),
        "FAILED" => Ok(PostStatus::Failed {
            reason: map_failure_reason(data.fail_reason.as_deref()),
        }),
        _ => Err(Error::InvalidResponse),
    }
}

fn map_failure_reason(reason: Option<&str>) -> String {
    match reason {
        Some("file_format_check_failed") => {
            "TikTok rejected the video format. Export an MP4 using H.264 and AAC."
        }
        Some("duration_check_failed") => {
            "TikTok rejected the video duration. Trim the clip to this account's limit."
        }
        Some("frame_rate_check_failed") => {
            "TikTok rejected the frame rate. Export between 23 and 60 FPS."
        }
        Some("picture_size_check_failed") => {
            "TikTok rejected the video dimensions. Export at least 360 pixels on each side."
        }
        Some("auth_removed") => {
            "TikTok access was removed. Reconnect the account before trying again."
        }
        Some("spam_risk_too_many_posts") => {
            "TikTok's daily posting limit was reached for this account."
        }
        _ => {
            "TikTok could not publish this video. Review the account and clip before trying again."
        }
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_follow_tiktok_trailing_bytes_rule() {
        let plan = chunk_plan(50_000_123).unwrap();
        assert_eq!(
            plan,
            ChunkPlan {
                chunk_size: 10_000_000,
                count: 5
            }
        );
        assert_eq!(chunk_range(plan, 4, 50_000_123), (40_000_000, 50_000_123));
        assert_eq!(chunk_plan(4_194_304).unwrap().count, 1);
    }

    #[test]
    fn upload_url_stays_on_tiktok_https() {
        assert!(
            validated_upload_url(
                "https://open-upload.tiktokapis.com/video/?upload_id=1&upload_token=abc"
            )
            .is_ok()
        );
        assert!(validated_upload_url("https://evil.example/video").is_err());
        assert!(validated_upload_url("https://open.tiktokapis.com/video/").is_err());
        assert!(validated_upload_url("http://open-upload.tiktokapis.com/video/").is_err());
        assert!(
            validated_upload_url("https://open-upload.tiktokapis.com.evil.example/video/").is_err()
        );
    }

    #[test]
    fn provider_status_and_error_are_restricted() {
        let raw = br#"{"data":{"status":"PUBLISH_COMPLETE","publicaly_available_post_id":[]},"error":{"code":"ok"}}"#;
        let envelope: Envelope<StatusData> = serde_json::from_slice(raw).unwrap();
        assert_eq!(envelope.data.unwrap().status, "PUBLISH_COMPLETE");
        assert_eq!(
            map_failure_reason(Some("auth_removed")),
            "TikTok access was removed. Reconnect the account before trying again."
        );
    }

    #[test]
    fn post_metadata_maps_interaction_and_commercial_choices() {
        let settings = TikTokSettings {
            allow_comments: true,
            allow_duet: false,
            allow_stitch: false,
            promotes_own_brand: true,
            branded_content: false,
        };
        let value = serde_json::to_value(post_info(
            "Reviewed #clip",
            TikTokPrivacy::SelfOnly,
            &settings,
        ))
        .unwrap();
        assert_eq!(value["title"], "Reviewed #clip");
        assert_eq!(value["privacy_level"], "SELF_ONLY");
        assert_eq!(value["disable_comment"], false);
        assert_eq!(value["disable_duet"], true);
        assert_eq!(value["brand_organic_toggle"], true);
        assert_eq!(value["brand_content_toggle"], false);
    }
}
