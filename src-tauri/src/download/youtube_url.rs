//! Canonicalize a single public YouTube video identity without performing I/O.

use reqwest::Url;
use serde::Serialize;

const MAX_INPUT_BYTES: usize = 2048;
const INVALID_LINK: &str = "Enter a single HTTPS YouTube video link.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct YoutubeVideoId(String);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeVideoLink {
    pub video_id: String,
    pub canonical_url: String,
}

#[tauri::command]
pub fn validate_youtube_video_url(url: String) -> Result<YoutubeVideoLink, String> {
    let id = YoutubeVideoId::parse(&url).map_err(str::to_owned)?;
    Ok(YoutubeVideoLink {
        video_id: id.as_str().to_owned(),
        canonical_url: id.canonical_url(),
    })
}

impl YoutubeVideoId {
    pub(super) fn parse(input: &str) -> Result<Self, &'static str> {
        if input.is_empty()
            || input.len() > MAX_INPUT_BYTES
            || input
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(INVALID_LINK);
        }

        let url = Url::parse(input).map_err(|_| INVALID_LINK)?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.fragment().is_some()
        {
            return Err(INVALID_LINK);
        }

        let id = match (url.host_str(), url.path()) {
            (Some("youtube.com" | "www.youtube.com" | "m.youtube.com"), "/watch") => {
                let mut video_id = None;
                for (key, value) in url.query_pairs() {
                    match key.as_ref() {
                        "v" if video_id.is_none() => video_id = Some(value.into_owned()),
                        "v" | "list" => return Err(INVALID_LINK),
                        _ => {}
                    }
                }
                video_id.ok_or(INVALID_LINK)?
            }
            (Some("youtube.com" | "www.youtube.com" | "m.youtube.com"), path)
                if path.starts_with("/shorts/") && share_query_only(&url) =>
            {
                path.strip_prefix("/shorts/").unwrap().to_owned()
            }
            (Some("youtu.be" | "www.youtu.be"), path) if share_query_only(&url) => {
                path.strip_prefix('/').unwrap_or("").to_owned()
            }
            _ => return Err(INVALID_LINK),
        };

        if id.len() != 11
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(INVALID_LINK);
        }
        Ok(Self(id))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn canonical_url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.0)
    }
}

fn share_query_only(url: &Url) -> bool {
    url.query_pairs()
        .all(|(key, _)| matches!(key.as_ref(), "si" | "t" | "feature"))
}

#[cfg(test)]
mod tests {
    use super::YoutubeVideoId;

    #[test]
    fn canonicalizes_supported_single_video_links() {
        let id = "YE7VzlLtp-4";
        for link in [
            format!("https://www.youtube.com/watch?v={id}"),
            format!("https://youtube.com/watch?feature=share&v={id}&t=120"),
            format!("https://m.youtube.com/watch?v={id}&si=tracker"),
            format!("https://youtu.be/{id}"),
            format!("https://youtu.be/{id}?si=tracker"),
            format!("https://www.youtube.com/shorts/{id}"),
            format!("https://www.youtube.com/shorts/{id}?feature=share"),
        ] {
            let parsed = YoutubeVideoId::parse(&link).unwrap();
            assert_eq!(parsed.as_str(), id);
            assert_eq!(
                parsed.canonical_url(),
                format!("https://www.youtube.com/watch?v={id}")
            );
        }
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_links_before_network() {
        let id = "YE7VzlLtp-4";
        for link in [
            format!("http://www.youtube.com/watch?v={id}"),
            format!("https://user:pass@www.youtube.com/watch?v={id}"),
            format!("https://www.youtube.com:444/watch?v={id}"),
            format!("https://www.youtube.com/watch?v={id}#fragment"),
            format!("https://www.youtube.com/watch?v={id}&v={id}"),
            format!("https://www.youtube.com/watch?v={id}&list=playlist"),
            format!("https://www.youtube.com/watch?v={id}/extra"),
            format!("https://www.youtube.com/shorts/{id}/extra"),
            format!("https://youtu.be/{id}/extra"),
            format!("https://youtu.be/{id}?list=playlist"),
            format!("https://youtube.com.evil.test/watch?v={id}"),
            format!("https://www.youtube.com/redirect?q=https://127.0.0.1/&v={id}"),
            format!("https://127.0.0.1/watch?v={id}"),
            format!("https://www.youtube.com/watch?v={id}%0a"),
            format!("https://www.youtube.com/watch?v={id}\n"),
            "not a URL".to_owned(),
        ] {
            assert!(YoutubeVideoId::parse(&link).is_err(), "accepted {link}");
        }
    }
}
