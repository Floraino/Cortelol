//! Review gate and local state for official social publishing adapters.
//!
//! No network request is made here. Callers must fetch live account capabilities,
//! show the export and editable metadata, then call `review` from a user action.
//! `begin_publish` is a second, explicit user action and is the only transition
//! that permits an adapter to upload media. Persist the job between transitions.

use crate::media::probe_media;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod jobs;
pub mod tiktok;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    #[serde(rename = "tiktok")]
    TikTok {
        privacy: TikTokPrivacy,
        settings: TikTokSettings,
    },
    Instagram {
        share_to_feed: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TikTokPrivacy {
    PublicToEveryone,
    MutualFollowFriends,
    FollowerOfCreator,
    SelfOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TikTokSettings {
    pub allow_comments: bool,
    pub allow_duet: bool,
    pub allow_stitch: bool,
    pub promotes_own_brand: bool,
    pub branded_content: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum Capabilities {
    #[serde(rename = "tiktok")]
    TikTok {
        account_id: String,
        display_name: String,
        can_post: bool,
        has_publish_scope: bool,
        app_audited: bool,
        allowed_privacy: Vec<TikTokPrivacy>,
        max_duration_ms: u64,
        max_caption_utf16_units: usize,
        comments_disabled: bool,
        duet_disabled: bool,
        stitch_disabled: bool,
    },
    Instagram {
        account_id: String,
        professional_account: bool,
        has_publish_permission: bool,
        min_reel_duration_ms: u64,
        max_reel_duration_ms: u64,
        max_caption_characters: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishMetadata {
    pub account_id: String,
    pub caption: String,
    pub destination: Destination,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfirmation {
    pub watched_export: bool,
    pub reviewed_caption: bool,
    pub reviewed_destination: bool,
    pub consent_to_transfer: bool,
    pub accepted_platform_terms: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum PublishState {
    Draft,
    Reviewed,
    Uploading,
    Processing {
        provider_reference: String,
    },
    Published {
        provider_reference: String,
        post_id: Option<String>,
    },
    Failed {
        message: String,
    },
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublishJob {
    export_path: PathBuf,
    metadata: PublishMetadata,
    state: PublishState,
}

impl PublishJob {
    pub fn new(export_path: PathBuf, metadata: PublishMetadata) -> Self {
        Self {
            export_path,
            metadata,
            state: PublishState::Draft,
        }
    }

    pub fn export_path(&self) -> &std::path::Path {
        &self.export_path
    }

    pub fn metadata(&self) -> &PublishMetadata {
        &self.metadata
    }

    pub fn state(&self) -> &PublishState {
        &self.state
    }

    pub fn edit_metadata(&mut self, metadata: PublishMetadata) -> Result<(), String> {
        if self.state != PublishState::Draft {
            return Err("Return to draft before changing publishing details.".into());
        }
        self.metadata = metadata;
        Ok(())
    }

    /// A caller must invoke this only after showing the actual rendered file,
    /// account, caption, platform controls, transfer notice, and platform terms.
    pub fn review(
        &mut self,
        confirmation: ReviewConfirmation,
        capabilities: &Capabilities,
    ) -> Result<(), String> {
        if self.state != PublishState::Draft {
            return Err("This publication must be in draft before review.".into());
        }
        if !confirmation.watched_export
            || !confirmation.reviewed_caption
            || !confirmation.reviewed_destination
            || !confirmation.consent_to_transfer
            || !confirmation.accepted_platform_terms
        {
            return Err(
                "Review the clip, caption, destination, transfer, and platform terms.".into(),
            );
        }
        self.validate(capabilities)?;
        self.state = PublishState::Reviewed;
        Ok(())
    }

    /// Call only from the explicit Publish control. Recheck live capabilities
    /// before this call; a stale or changed account must return to draft.
    pub fn begin_publish(&mut self, capabilities: &Capabilities) -> Result<(), String> {
        if self.state != PublishState::Reviewed {
            return Err("Review this publication before publishing.".into());
        }
        self.validate(capabilities)?;
        self.state = PublishState::Uploading;
        Ok(())
    }

    pub fn mark_processing(&mut self, provider_reference: String) -> Result<(), String> {
        if self.state != PublishState::Uploading {
            return Err("The upload has not started.".into());
        }
        validate_identifier(&provider_reference, "provider reference")?;
        self.state = PublishState::Processing { provider_reference };
        Ok(())
    }

    pub fn mark_published(&mut self, post_id: String) -> Result<(), String> {
        validate_identifier(&post_id, "post ID")?;
        self.mark_publish_complete(Some(post_id))
    }

    pub fn mark_publish_complete(&mut self, post_id: Option<String>) -> Result<(), String> {
        let PublishState::Processing { provider_reference } = &self.state else {
            return Err("The platform has not confirmed processing.".into());
        };
        if let Some(post_id) = &post_id {
            validate_identifier(post_id, "post ID")?;
        }
        self.state = PublishState::Published {
            provider_reference: provider_reference.clone(),
            post_id,
        };
        Ok(())
    }

    pub fn fail(&mut self, message: String) -> Result<(), String> {
        if !matches!(
            self.state,
            PublishState::Uploading | PublishState::Processing { .. }
        ) {
            return Err("No publication is in progress.".into());
        }
        let message = message.trim();
        if message.is_empty() || message.len() > 500 || message.chars().any(char::is_control) {
            return Err("Enter a short, actionable publishing error.".into());
        }
        self.state = PublishState::Failed {
            message: message.into(),
        };
        Ok(())
    }

    /// A retry must be reviewed again and never touches the local export.
    pub fn retry(&mut self) -> Result<(), String> {
        if !matches!(
            self.state,
            PublishState::Failed { .. } | PublishState::Cancelled
        ) {
            return Err("Only failed or cancelled publications can be retried.".into());
        }
        self.state = PublishState::Draft;
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), String> {
        if !matches!(
            self.state,
            PublishState::Draft | PublishState::Reviewed | PublishState::Uploading
        ) {
            return Err("This publication cannot be cancelled at its current stage.".into());
        }
        self.state = PublishState::Cancelled;
        Ok(())
    }

    fn validate(&self, capabilities: &Capabilities) -> Result<(), String> {
        let export = self
            .export_path
            .canonicalize()
            .map_err(|_| "The local export is unavailable.".to_string())?;
        if !export.is_file()
            || !export
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
            || export
                .metadata()
                .map_err(|_| "The local export could not be read.".to_string())?
                .len()
                == 0
        {
            return Err("Choose a nonempty MP4 export.".into());
        }
        let export_str = export
            .to_str()
            .ok_or_else(|| "The export path is unsupported.".to_string())?;
        let media = probe_media(export_str.to_owned())?;
        if !media.has_video || !media.has_audio {
            return Err("The export must contain video and audio.".into());
        }
        let duration_ms = media.duration_seconds * 1_000.0;
        if !duration_ms.is_finite() || duration_ms <= 0.0 {
            return Err("The export duration is invalid.".into());
        }
        validate_identifier(&self.metadata.account_id, "account ID")?;
        if self
            .metadata
            .caption
            .chars()
            .any(|c| c.is_control() && c != '\n')
        {
            return Err("The caption contains unsupported control characters.".into());
        }
        match (&self.metadata.destination, capabilities) {
            (
                Destination::TikTok { privacy, settings },
                Capabilities::TikTok {
                    account_id,
                    display_name: _,
                    can_post,
                    has_publish_scope,
                    app_audited,
                    allowed_privacy,
                    max_duration_ms,
                    max_caption_utf16_units,
                    comments_disabled,
                    duet_disabled,
                    stitch_disabled,
                },
            ) => {
                if account_id != &self.metadata.account_id || !can_post || !has_publish_scope {
                    return Err("This TikTok account is not eligible for direct posting.".into());
                }
                if duration_ms > *max_duration_ms as f64 || !allowed_privacy.contains(privacy) {
                    return Err("The video duration or privacy choice is unavailable for this TikTok account.".into());
                }
                if !app_audited && *privacy != TikTokPrivacy::SelfOnly {
                    return Err("This TikTok app has not been audited for nonprivate posts.".into());
                }
                if (settings.allow_comments && *comments_disabled)
                    || (settings.allow_duet && *duet_disabled)
                    || (settings.allow_stitch && *stitch_disabled)
                {
                    return Err("An interaction option is disabled for this TikTok account.".into());
                }
                if settings.branded_content && *privacy == TikTokPrivacy::SelfOnly {
                    return Err("Branded content cannot be posted privately on TikTok.".into());
                }
                if self.metadata.caption.encode_utf16().count() > *max_caption_utf16_units {
                    return Err("The TikTok caption exceeds the configured limit.".into());
                }
            }
            (
                Destination::Instagram { .. },
                Capabilities::Instagram {
                    account_id,
                    professional_account,
                    has_publish_permission,
                    min_reel_duration_ms,
                    max_reel_duration_ms,
                    max_caption_characters,
                },
            ) => {
                if account_id != &self.metadata.account_id
                    || !professional_account
                    || !has_publish_permission
                {
                    return Err("This Instagram account is not eligible for publishing.".into());
                }
                if duration_ms < *min_reel_duration_ms as f64
                    || duration_ms > *max_reel_duration_ms as f64
                {
                    return Err(
                        "The Reel duration is outside the configured platform range.".into(),
                    );
                }
                if self.metadata.caption.chars().count() > *max_caption_characters {
                    return Err("The Instagram caption exceeds the configured limit.".into());
                }
            }
            _ => return Err("The account capabilities do not match the destination.".into()),
        }
        Ok(())
    }
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(format!("The {label} is invalid."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::{Command, Stdio};

    fn fixture() -> (PublishJob, Capabilities) {
        let path = std::env::temp_dir().join(format!(
            "cortelol-publish-{}-{:?}.mp4",
            std::process::id(),
            std::thread::current().id()
        ));
        let output = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=160x284:r=24",
            ])
            .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"])
            .args(["-t", "10", "-c:v", "mpeg4", "-c:a", "aac", "-y"])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(output.success());
        (
            PublishJob::new(
                path,
                PublishMetadata {
                    account_id: "creator-1".into(),
                    caption: "Reviewed #clip".into(),
                    destination: Destination::TikTok {
                        privacy: TikTokPrivacy::SelfOnly,
                        settings: TikTokSettings {
                            allow_comments: false,
                            allow_duet: false,
                            allow_stitch: false,
                            promotes_own_brand: false,
                            branded_content: false,
                        },
                    },
                },
            ),
            Capabilities::TikTok {
                account_id: "creator-1".into(),
                display_name: "Creator".into(),
                can_post: true,
                has_publish_scope: true,
                app_audited: false,
                allowed_privacy: vec![TikTokPrivacy::SelfOnly],
                max_duration_ms: 60_000,
                max_caption_utf16_units: 2200,
                comments_disabled: false,
                duet_disabled: false,
                stitch_disabled: false,
            },
        )
    }

    fn confirmation() -> ReviewConfirmation {
        ReviewConfirmation {
            watched_export: true,
            reviewed_caption: true,
            reviewed_destination: true,
            consent_to_transfer: true,
            accepted_platform_terms: true,
        }
    }

    #[test]
    fn explicit_review_and_publish_gate() {
        let (mut job, caps) = fixture();
        assert!(job.begin_publish(&caps).is_err());
        let mut incomplete = confirmation();
        incomplete.consent_to_transfer = false;
        assert!(job.review(incomplete, &caps).is_err());
        job.review(confirmation(), &caps).unwrap();
        job.begin_publish(&caps).unwrap();
        job.mark_processing("publish-123".into()).unwrap();
        job.mark_published("post-456".into()).unwrap();
        assert!(matches!(job.state, PublishState::Published { .. }));
        fs::remove_file(job.export_path).unwrap();
    }

    #[test]
    fn failure_and_retry_preserve_export_and_require_new_review() {
        let (mut job, caps) = fixture();
        job.review(confirmation(), &caps).unwrap();
        job.begin_publish(&caps).unwrap();
        job.fail("The platform rejected this upload.".into())
            .unwrap();
        assert!(job.export_path.exists());
        job.retry().unwrap();
        assert!(job.begin_publish(&caps).is_err());
        fs::remove_file(job.export_path).unwrap();
    }

    #[test]
    fn stale_capabilities_and_private_branded_content_are_rejected() {
        let (mut job, mut caps) = fixture();
        if let Capabilities::TikTok {
            ref mut account_id, ..
        } = caps
        {
            *account_id = "different-account".into();
        }
        assert!(job.review(confirmation(), &caps).is_err());
        if let Capabilities::TikTok {
            ref mut account_id, ..
        } = caps
        {
            *account_id = "creator-1".into();
        }
        if let Destination::TikTok {
            ref mut settings, ..
        } = job.metadata.destination
        {
            settings.branded_content = true;
        }
        assert!(job.review(confirmation(), &caps).is_err());
        fs::remove_file(job.export_path).unwrap();
    }

    #[test]
    fn missing_export_blocks_publish_after_review() {
        let (mut job, caps) = fixture();
        job.review(confirmation(), &caps).unwrap();
        fs::remove_file(&job.export_path).unwrap();
        assert!(job.begin_publish(&caps).is_err());
        assert_eq!(job.state, PublishState::Reviewed);
    }

    #[test]
    fn fake_mp4_and_live_duration_limit_block_review() {
        let (mut job, mut caps) = fixture();
        if let Capabilities::TikTok {
            ref mut max_duration_ms,
            ..
        } = caps
        {
            *max_duration_ms = 5_000;
        }
        assert!(job.review(confirmation(), &caps).is_err());
        fs::write(&job.export_path, b"not an mp4").unwrap();
        if let Capabilities::TikTok {
            ref mut max_duration_ms,
            ..
        } = caps
        {
            *max_duration_ms = 60_000;
        }
        assert!(job.review(confirmation(), &caps).is_err());
        fs::remove_file(job.export_path).unwrap();
    }

    #[test]
    fn tiktok_wire_name_matches_webview_contract() {
        let metadata = PublishMetadata {
            account_id: "creator".into(),
            caption: "Clip".into(),
            destination: Destination::TikTok {
                privacy: TikTokPrivacy::SelfOnly,
                settings: TikTokSettings {
                    allow_comments: false,
                    allow_duet: false,
                    allow_stitch: false,
                    promotes_own_brand: false,
                    branded_content: false,
                },
            },
        };
        let wire = serde_json::to_value(&metadata).unwrap();
        assert_eq!(wire["destination"]["tiktok"]["privacy"], "SELF_ONLY");
        assert_eq!(
            serde_json::from_value::<PublishMetadata>(wire).unwrap(),
            metadata
        );
        let capabilities = serde_json::to_value(Capabilities::TikTok {
            account_id: "creator".into(),
            display_name: "Creator".into(),
            can_post: true,
            has_publish_scope: true,
            app_audited: false,
            allowed_privacy: vec![TikTokPrivacy::SelfOnly],
            max_duration_ms: 60_000,
            max_caption_utf16_units: 2200,
            comments_disabled: false,
            duet_disabled: false,
            stitch_disabled: false,
        })
        .unwrap();
        assert_eq!(capabilities["platform"], "tiktok");
    }
}
