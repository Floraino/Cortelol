//! The only network path for app downloads: one fresh, pinned HTTPS client per hop.

use std::future::Future;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_REDIRECTS: usize = 5;
const MAX_URL_LENGTH: usize = 8192;

pub(super) fn validate_url(value: &str) -> Result<Url, String> {
    let url = validate_network_url(value)?;
    let host = url.host_str().unwrap_or_default();
    if !["vimeo.com", "vimeocdn.com"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    {
        return Err("Start with the original link from Vimeo's official download option. Import the saved file manually if its host is unsupported.".into());
    }
    Ok(url)
}

fn validate_network_url(value: &str) -> Result<Url, String> {
    if value.len() > MAX_URL_LENGTH
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(
            "Enter one HTTPS Vimeo download link without spaces or control characters.".into(),
        );
    }
    let url = Url::parse(value).map_err(|_| "Enter one valid HTTPS Vimeo download link.")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.fragment().is_some()
    {
        return Err(
            "Use an HTTPS Vimeo download link without credentials, fragments, or a custom port."
                .into(),
        );
    }
    let host = url.host_str().ok_or("The file host is invalid.")?;
    if host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|address| !public_ip(address))
    {
        return Err("The file redirect targets a private or reserved network address.".into());
    }
    Ok(url)
}

/// Conservative global-unicast policy. Reject special-purpose and transition ranges,
/// including mapped IPv4, NAT64, 6to4, Teredo, documentation, and future allocation space.
pub(super) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_multicast()
                || ip.is_unspecified()
                || a == 0
                || a >= 240
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19)))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && s[1] <= 0x01ff)
                && !(s[0] == 0x2001 && s[1] == 0x0db8)
                && s[0] != 0x2002
                && s[0] != 0x3ffe
                && s[0] & 0xfff0 != 0x3ff0
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct PinnedTarget {
    pub url: Url,
    pub address: SocketAddr,
}

impl PinnedTarget {
    pub(super) fn new(url: Url, addresses: Vec<IpAddr>) -> Result<Self, String> {
        // Reject the entire answer if even one A/AAAA address is unsafe.
        if addresses.is_empty() || addresses.len() > 64 || !addresses.iter().copied().all(public_ip)
        {
            return Err(
                "The file host resolves to a private, reserved, or unsupported network address."
                    .into(),
            );
        }
        Ok(Self {
            url,
            address: SocketAddr::new(addresses[0], 443),
        })
    }

    pub fn client(&self) -> Result<Client, String> {
        Client::builder()
            .https_only(true)
            .http1_only()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .timeout(super::DOWNLOAD_TIMEOUT)
            .pool_max_idle_per_host(0)
            // Unknown names can never fall back to system DNS inside the HTTP client.
            .dns_resolver(Arc::new(DenyDns))
            .resolve(
                self.url.host_str().ok_or("The file host is invalid.")?,
                self.address,
            )
            .build()
            .map_err(|_| "The secure download client could not be created.".into())
    }
}

pub(super) struct DenyDns;
impl reqwest::dns::Resolve for DenyDns {
    fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async { Err(std::io::Error::other("Unpinned DNS resolution blocked").into()) })
    }
}

pub(super) trait Transport: Sync {
    fn resolve(&self, host: &str) -> impl Future<Output = Result<Vec<IpAddr>, String>> + Send;
    fn get(&self, target: &PinnedTarget) -> impl Future<Output = Result<Reply, String>> + Send;
}

pub(super) struct DirectTransport;
impl Transport for DirectTransport {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        let host = host.to_owned();
        tokio::task::spawn_blocking(move || {
            (host.as_str(), 443)
                .to_socket_addrs()
                .map(|addresses| addresses.take(65).map(|address| address.ip()).collect())
                .map_err(|_| "The Vimeo file host could not be resolved.".into())
        })
        .await
        .map_err(|_| "The address lookup stopped unexpectedly.")?
    }

    async fn get(&self, target: &PinnedTarget) -> Result<Reply, String> {
        let response = target
            .client()?
            .get(target.url.clone())
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(
                |_| "The secure file request failed. Check the official link and try again.",
            )?;
        // Defense in depth: verify the connected peer before consuming any body.
        if response.remote_addr() != Some(target.address) {
            return Err("The file connection did not match its validated public address.".into());
        }
        Ok(Reply {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            body: Body::Live(response),
        })
    }
}

pub(super) struct Reply {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Body,
}

pub(super) enum Body {
    Live(reqwest::Response),
    #[cfg(test)]
    Mock(std::collections::VecDeque<Result<Vec<u8>, String>>),
    #[cfg(test)]
    Pending(Option<Vec<u8>>),
}

impl Body {
    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        match self {
            Self::Live(response) => response
                .chunk()
                .await
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                .map_err(|_| {
                    "The file transfer was interrupted. Try a fresh official download link.".into()
                }),
            #[cfg(test)]
            Self::Mock(chunks) => chunks.pop_front().transpose(),
            #[cfg(test)]
            Self::Pending(first) => match first.take() {
                Some(bytes) => Ok(Some(bytes)),
                None => std::future::pending().await,
            },
        }
    }
}

pub(super) struct StagedVideo {
    file: tempfile::NamedTempFile,
    extension: &'static str,
    size: u64,
    sha256: String,
    rights_confirmed_at_unix_ms: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct AcquisitionReceipt {
    pub schema_version: u32,
    pub file_name: String,
    pub file_size_bytes: u64,
    pub sha256: String,
    pub acquired_at_unix_ms: u64,
    pub rights_confirmed_at_unix_ms: u64,
    pub owns_media_confirmed: bool,
    pub official_vimeo_link_confirmed: bool,
    pub source_site: String,
    pub acquisition_method: String,
}

fn now_unix_ms() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| "The system clock is invalid.".into())
}

impl StagedVideo {
    pub fn publish(self) -> Result<super::DownloadedVideo, String> {
        // The random staging name is generated locally; ignore Content-Disposition and URL names.
        let destination = self.file.path().with_extension(self.extension);
        let receipt_path = destination.with_file_name(format!(
            "{}.source.json",
            destination
                .file_name()
                .ok_or("The completed filename is invalid.")?
                .to_string_lossy()
        ));
        let receipt = AcquisitionReceipt {
            schema_version: 1,
            file_name: destination
                .file_name()
                .ok_or("The completed filename is invalid.")?
                .to_string_lossy()
                .into_owned(),
            file_size_bytes: self.size,
            sha256: self.sha256.clone(),
            acquired_at_unix_ms: now_unix_ms()?,
            rights_confirmed_at_unix_ms: self.rights_confirmed_at_unix_ms,
            owns_media_confirmed: true,
            official_vimeo_link_confirmed: true,
            source_site: "Vimeo".into(),
            acquisition_method: "official-vimeo-direct-file".into(),
        };
        let mut staged_receipt = tempfile::NamedTempFile::new_in(
            destination
                .parent()
                .ok_or("The download folder is invalid.")?,
        )
        .map_err(|_| "The acquisition receipt could not be created.")?;
        serde_json::to_writer_pretty(&mut staged_receipt, &receipt)
            .map_err(|_| "The acquisition receipt could not be written.")?;
        staged_receipt
            .as_file()
            .sync_all()
            .map_err(|_| "The acquisition receipt could not be saved to disk.")?;
        // Publish the receipt first, then the media as the transaction's commit point.
        // A crash can leave an orphan receipt, never a completed video without a receipt.
        staged_receipt
            .persist_noclobber(&receipt_path)
            .map_err(|_| "The acquisition receipt already exists or could not be saved.")?;
        if self.file.persist_noclobber(&destination).is_err() {
            let _ = std::fs::remove_file(&receipt_path); // Only the receipt just created by this transaction.
            return Err(
                "The completed file could not be saved without overwriting an existing file."
                    .into(),
            );
        }
        Ok(super::DownloadedVideo {
            path: destination.to_string_lossy().into_owned(),
            file_size_bytes: self.size,
            receipt_path: receipt_path.to_string_lossy().into_owned(),
            sha256: self.sha256,
            source_site: "Vimeo",
            acquisition_method: "official-vimeo-direct-file",
        })
    }
}

fn redirect_url(current: &Url, location: &str) -> Result<Url, String> {
    if location.len() > MAX_URL_LENGTH
        || location
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err("The file redirect contains an invalid URL.".into());
    }
    let next = current
        .join(location)
        .map_err(|_| "The file redirect contains an invalid URL.")?;
    validate_network_url(next.as_str())
}

fn media_extension(prefix: &[u8], mime: &str) -> Result<&'static str, String> {
    // This is a container gate, not a replacement for import_media's FFprobe validation.
    if prefix.len() >= 12 && &prefix[4..8] == b"ftyp" && mime != "video/webm" {
        Ok("mp4")
    } else if prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) && mime != "video/mp4" {
        Ok("webm")
    } else {
        Err("The link did not return a supported MP4 or WebM file. Use Vimeo's official Download option and import the saved file.".into())
    }
}

pub(super) async fn transfer<T: Transport>(
    mut url: Url,
    directory: &Path,
    transport: &T,
    max_bytes: u64,
    progress: impl Fn(u64, Option<u64>) + Send,
) -> Result<StagedVideo, String> {
    // This private path is called only after both consent fields pass validation.
    let rights_confirmed_at_unix_ms = now_unix_ms()?;
    url = validate_url(url.as_str())?;
    let mut visited = std::collections::HashSet::new();
    for hop in 0..=MAX_REDIRECTS {
        // Recheck even same-host redirects and resolve afresh for every new connection.
        url = validate_network_url(url.as_str())?;
        if !visited.insert(url.as_str().to_owned()) {
            return Err("The file redirect forms a loop.".into());
        }
        let addresses = transport
            .resolve(url.host_str().ok_or("The file host is invalid.")?)
            .await?;
        let target = PinnedTarget::new(url.clone(), addresses)?;
        let mut reply = transport.get(&target).await?;
        if [301, 302, 303, 307, 308].contains(&reply.status) {
            if hop == MAX_REDIRECTS {
                return Err("The file exceeded the redirect limit.".into());
            }
            let location = reply
                .headers
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or("The file redirect is missing a valid destination.")?;
            url = redirect_url(&url, location)?;
            // Never read a redirect body or follow URLs from manifests/HTML/metadata.
            continue;
        }
        if reply.status != 200 {
            return Err("Vimeo did not return a complete downloadable file. Use a fresh official link or import the file manually.".into());
        }
        if reply
            .headers
            .get(CONTENT_ENCODING)
            .is_some_and(|value| value != "identity")
        {
            return Err(
                "Encoded download responses are unsupported. Import the file manually.".into(),
            );
        }
        let mime = reply
            .headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !["video/mp4", "video/webm", "application/octet-stream"].contains(&mime.as_str()) {
            return Err("The link must point to an MP4 or WebM file, not a video page or streaming playlist.".into());
        }
        let total = reply
            .headers
            .get(CONTENT_LENGTH)
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or("The file has an invalid Content-Length.")
            })
            .transpose()?;
        if total.is_some_and(|size| size == 0 || size > max_bytes) {
            return Err("The file is empty or exceeds the 2 GiB download limit.".into());
        }
        let mut file = tempfile::Builder::new()
            .prefix("vimeo-")
            .suffix(".part")
            .tempfile_in(directory)
            .map_err(|_| "The temporary download file could not be created.")?;
        let mut size = 0_u64;
        let mut prefix = Vec::new();
        let mut digest = Sha256::new();
        progress(0, total);
        while let Some(chunk) = reply.body.chunk().await? {
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or("The download size is invalid.")?;
            if size > max_bytes || total.is_some_and(|total| size > total) {
                return Err(
                    "The file exceeds its declared size or the 2 GiB download limit.".into(),
                );
            }
            prefix.extend(chunk.iter().take(12_usize.saturating_sub(prefix.len())));
            if prefix.len() == 12 {
                media_extension(&prefix, &mime)?;
            }
            file.write_all(&chunk).map_err(
                |_| "The downloaded file could not be written. Check available disk space.",
            )?;
            digest.update(&chunk);
            progress(size, total);
        }
        if size == 0 || total.is_some_and(|total| size != total) {
            return Err("The file transfer ended before the complete file arrived.".into());
        }
        let extension = media_extension(&prefix, &mime)?;
        file.as_file()
            .sync_all()
            .map_err(|_| "The downloaded file could not be saved to disk.")?;
        return Ok(StagedVideo {
            file,
            extension,
            size,
            sha256: format!("{:x}", digest.finalize()),
            rights_confirmed_at_unix_ms,
        });
    }
    Err("The file exceeded the redirect limit.".into())
}
