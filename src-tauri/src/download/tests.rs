//! Offline fixtures only: no HTTP listener, internet request, or real media download.

use super::network::{self, AcquisitionReceipt, Body, PinnedTarget, Reply, Transport};
use super::*;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, LOCATION};
use std::collections::VecDeque;
use std::net::IpAddr;

const START: &str =
    "https://player.vimeo.com/progressive_redirect/download/one.mp4?token=PRIVATE_SECRET";
const MP4: &[u8] = b"\0\0\0\x18ftypisom00000000";

struct MockTransport {
    dns: Mutex<VecDeque<Vec<IpAddr>>>,
    replies: Mutex<VecDeque<Reply>>,
    pins: Mutex<Vec<PinnedTarget>>,
    lookups: Mutex<Vec<String>>,
}

impl MockTransport {
    fn new(answers: &[&[&str]], replies: Vec<Reply>) -> Self {
        Self {
            dns: Mutex::new(
                answers
                    .iter()
                    .map(|ips| ips.iter().map(|ip| ip.parse().unwrap()).collect())
                    .collect(),
            ),
            replies: Mutex::new(replies.into()),
            pins: Mutex::new(Vec::new()),
            lookups: Mutex::new(Vec::new()),
        }
    }
}

impl Transport for MockTransport {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        self.lookups.lock().unwrap().push(host.into());
        Ok(self
            .dns
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected DNS lookup"))
    }
    async fn get(&self, target: &PinnedTarget) -> Result<Reply, String> {
        self.pins.lock().unwrap().push(target.clone());
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected connection"))
    }
}

fn redirect(status: u16, location: &str) -> Reply {
    let mut headers = HeaderMap::new();
    headers.insert(LOCATION, location.parse().unwrap());
    Reply {
        status,
        headers,
        body: Body::Mock(VecDeque::from([Err(
            "redirect body must never be read".into()
        )])),
    }
}

fn video(chunks: Vec<Result<Vec<u8>, String>>, total: Option<u64>, mime: &str) -> Reply {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, mime.parse().unwrap());
    if let Some(total) = total {
        headers.insert(CONTENT_LENGTH, total.into());
    }
    Reply {
        status: 200,
        headers,
        body: Body::Mock(chunks.into()),
    }
}

fn complete() -> Reply {
    video(vec![Ok(MP4.to_vec())], Some(MP4.len() as u64), "video/mp4")
}

async fn transfer(
    mock: &MockTransport,
    directory: &Path,
    limit: u64,
) -> Result<network::StagedVideo, String> {
    network::transfer(
        reqwest::Url::parse(START).unwrap(),
        directory,
        mock,
        limit,
        |_, _| {},
    )
    .await
}

#[test]
fn command_rejects_every_remote_source_regardless_of_confirmations() {
    for url in [
        START,
        "https://www.youtube.com/watch?v=abcdefghijk",
        "https://youtu.be/abcdefghijk",
        "https://127.0.0.1/PRIVATE_SECRET",
        "not a URL",
    ] {
        for (owns_media, official_vimeo_link) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let error = reject_remote_request(&DownloadVideoRequest {
                url: url.into(),
                owns_media,
                official_vimeo_link,
            })
            .unwrap_err();
            assert_eq!(error, REMOTE_DOWNLOAD_UNAVAILABLE);
            assert!(!error.contains("PRIVATE_SECRET"));
        }
    }
    let minimal =
        serde_json::from_value::<DownloadVideoRequest>(serde_json::json!({"url": START})).unwrap();
    assert!(reject_remote_request(&minimal).is_err());
    assert!(
        serde_json::from_value::<DownloadVideoRequest>(
            serde_json::json!({"url": START, "cookies": "secret"})
        )
        .is_err()
    );
}

#[test]
fn retired_command_preserves_existing_source_receipt_and_export() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = [
        ("existing.mp4", b"original-source".as_slice()),
        ("existing.mp4.source.json", b"original-receipt".as_slice()),
        ("export.mp4", b"reviewed-export".as_slice()),
    ];
    for (name, bytes) in fixtures {
        std::fs::write(directory.path().join(name), bytes).unwrap();
    }
    for url in [START, "https://www.youtube.com/watch?v=abcdefghijk"] {
        assert!(
            reject_remote_request(&DownloadVideoRequest {
                url: url.into(),
                owns_media: true,
                official_vimeo_link: true,
            })
            .is_err()
        );
    }
    for (name, bytes) in fixtures {
        assert_eq!(std::fs::read(directory.path().join(name)).unwrap(), bytes);
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 3);
}

#[test]
fn url_policy_rejects_private_literals_non_https_and_non_vimeo_sources() {
    for url in [
        "http://player.vimeo.com/file.mp4",
        "file:///etc/passwd",
        "https://user:secret@vimeo.com/file",
        "https://vimeo.com:444/file",
        "https://vimeo.com/file#secret",
        "https://vimeo.com.evil.test/file",
        "https://notvimeo.com/file",
        "https://youtube.com/watch?v=x",
        "https://127.0.0.1/file",
        "https://2130706433/file",
        "https://0x7f000001/file",
        "https://[::1]/file",
        "https://[::ffff:127.0.0.1]/file",
        "https://vimeo.com./file",
        "https://player.vimeo.com/\nfile",
    ] {
        assert!(network::validate_url(url).is_err(), "accepted {url}");
    }
    assert!(network::validate_url("https://vod-progressive.akamaized.net/file.mp4").is_err());
    assert!(network::validate_url("https://vod-adaptive.vimeocdn.com/file.mp4").is_ok());
}

#[test]
fn blocks_special_ipv4_and_ipv6_ranges() {
    for value in [
        "0.0.0.0",
        "0.5.1.2",
        "10.0.0.1",
        "100.64.0.1",
        "100.127.255.255",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.0.0.9",
        "192.0.2.1",
        "192.88.99.1",
        "192.168.1.1",
        "198.18.0.1",
        "198.19.255.255",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "240.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::ffff:8.8.8.8",
        "::ffff:127.0.0.1",
        "64:ff9b::7f00:1",
        "64:ff9b:1::1",
        "100::1",
        "2001::1",
        "2001:20::1",
        "2001:db8::1",
        "2002:7f00:1::1",
        "3fff::1",
        "fc00::1",
        "fd00::1",
        "fe80::1",
        "ff02::1",
    ] {
        assert!(
            !network::public_ip(value.parse().unwrap()),
            "accepted {value}"
        );
    }
    for value in [
        "8.8.8.8",
        "1.1.1.1",
        "100.128.0.1",
        "172.32.0.1",
        "2606:4700:4700::1111",
        "2a04:4e42::1",
    ] {
        assert!(
            network::public_ip(value.parse().unwrap()),
            "blocked {value}"
        );
    }
}

#[tokio::test]
async fn private_or_mixed_dns_answer_never_connects() {
    for ips in [
        vec!["127.0.0.1"],
        vec!["8.8.8.8", "10.0.0.1"],
        vec!["::ffff:127.0.0.1"],
        vec![],
    ] {
        let mock = MockTransport::new(&[&ips], vec![]);
        let dir = tempfile::tempdir().unwrap();
        assert!(transfer(&mock, dir.path(), 100).await.is_err());
        assert!(mock.pins.lock().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn redirects_to_local_reserved_or_insecure_urls_are_blocked_before_second_lookup() {
    for status in [301, 302, 303, 307, 308] {
        for destination in [
            "https://127.0.0.1/x",
            "https://169.254.169.254/x",
            "https://[::1]/x",
            "https://[::ffff:127.0.0.1]/x",
            "http://player.vimeo.com/x",
            "https://user:secret@player.vimeo.com/x",
            "https://vimeo.com:8443/x",
        ] {
            let mock = MockTransport::new(&[&["8.8.8.8"]], vec![redirect(status, destination)]);
            let dir = tempfile::tempdir().unwrap();
            let error = transfer(&mock, dir.path(), 100).await.err().unwrap();
            assert!(!error.contains("PRIVATE_SECRET"));
            assert_eq!(mock.pins.lock().unwrap().len(), 1);
            assert_eq!(mock.lookups.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn same_host_redirect_rebinds_to_private_and_never_makes_second_connection() {
    let mock = MockTransport::new(
        &[&["8.8.8.8"], &["127.0.0.1"]],
        vec![redirect(302, "/next.mp4")],
    );
    let dir = tempfile::tempdir().unwrap();
    assert!(transfer(&mock, dir.path(), 100).await.is_err());
    assert_eq!(
        *mock.lookups.lock().unwrap(),
        ["player.vimeo.com", "player.vimeo.com"]
    );
    assert_eq!(mock.pins.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cdn_redirect_rebinds_to_private_or_mixed_answer_and_is_blocked() {
    for answer in [vec!["10.0.0.1"], vec!["1.1.1.1", "fc00::1"]] {
        let mock = MockTransport::new(
            &[&["8.8.8.8"], &answer],
            vec![redirect(307, "https://vod.vimeocdn.com/next.mp4")],
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(transfer(&mock, dir.path(), 100).await.is_err());
        assert_eq!(mock.pins.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn public_external_cdn_is_allowed_only_after_original_official_url() {
    let mock = MockTransport::new(
        &[&["8.8.8.8"], &["1.1.1.1"]],
        vec![
            redirect(
                302,
                "https://vod-progressive.akamaized.net/file.mp4?signature=SECRET",
            ),
            complete(),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let staged = transfer(&mock, dir.path(), 100).await.unwrap();
    let pins = mock.pins.lock().unwrap();
    assert_eq!(pins.len(), 2);
    assert_eq!(pins[1].address.to_string(), "1.1.1.1:443");
    assert_eq!(
        pins[1].url.host_str(),
        Some("vod-progressive.akamaized.net")
    );
    drop(staged);
    assert!(network::validate_url("https://vod-progressive.akamaized.net/file.mp4").is_err());
}

#[tokio::test]
async fn external_cdn_private_dns_is_blocked_before_connection() {
    let mock = MockTransport::new(
        &[&["8.8.8.8"], &["169.254.169.254"]],
        vec![redirect(302, "https://cdn.example.test/file.mp4")],
    );
    let dir = tempfile::tempdir().unwrap();
    assert!(transfer(&mock, dir.path(), 100).await.is_err());
    assert_eq!(mock.pins.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pinned_request_uses_exact_validated_ip_without_second_dns_lookup() {
    let mock = MockTransport::new(&[&["8.8.8.8"], &["127.0.0.1"]], vec![complete()]);
    let dir = tempfile::tempdir().unwrap();
    let staged = transfer(&mock, dir.path(), 100).await.unwrap();
    assert_eq!(
        mock.dns.lock().unwrap().len(),
        1,
        "unvalidated second DNS answer was consulted"
    );
    let pins = mock.pins.lock().unwrap();
    assert_eq!(pins[0].address.to_string(), "8.8.8.8:443");
    assert_eq!(pins[0].url.host_str(), Some("player.vimeo.com"));
    assert!(pins[0].client().is_ok());
    drop(staged);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn unknown_http_client_dns_is_denied_without_system_resolution() {
    use reqwest::dns::Resolve;
    assert!(
        network::DenyDns
            .resolve("unvalidated.invalid".parse().unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn public_cdn_redirect_streams_and_receipt_survives_restart_without_url_secrets() {
    use sha2::{Digest, Sha256};
    let mock = MockTransport::new(
        &[&["8.8.8.8"], &["1.1.1.1"]],
        vec![
            redirect(
                302,
                "https://vod.vimeocdn.com/SECRET_PATH.mp4?token=OTHER_SECRET",
            ),
            complete(),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let staged = transfer(&mock, dir.path(), 100).await.unwrap();
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "media was published before receipt"
    );
    let result = staged.publish().unwrap();
    let saved_receipt_path = result.receipt_path.clone();
    let saved_media_path = result.path.clone();
    drop(result);
    drop(mock); // Simulate a new session reading only persisted files.
    let json = std::fs::read_to_string(saved_receipt_path).unwrap();
    for secret in [
        "PRIVATE_SECRET",
        "OTHER_SECRET",
        "SECRET_PATH",
        "https:",
        "?",
        "token=",
    ] {
        assert!(!json.contains(secret));
    }
    let receipt: AcquisitionReceipt = serde_json::from_str(&json).unwrap();
    assert_eq!(std::fs::read(&saved_media_path).unwrap(), MP4);
    assert_eq!(receipt.sha256, format!("{:x}", Sha256::digest(MP4)));
    assert!(receipt.owns_media_confirmed && receipt.official_vimeo_link_confirmed);
    assert_eq!(receipt.acquisition_method, "official-vimeo-direct-file");
    assert!(receipt.acquired_at_unix_ms >= receipt.rights_confirmed_at_unix_ms);
    assert_eq!(
        receipt.file_name,
        Path::new(&saved_media_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[tokio::test]
async fn rejects_loops_and_excessive_redirects() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockTransport::new(&[&["8.8.8.8"]], vec![redirect(302, START)]);
    assert!(
        transfer(&mock, dir.path(), 100)
            .await
            .err()
            .unwrap()
            .contains("loop")
    );
    let answers = vec![&["8.8.8.8"][..]; 6];
    let replies = (0..6)
        .map(|i| redirect(302, &format!("/next-{i}")))
        .collect();
    let mock = MockTransport::new(&answers, replies);
    assert!(
        transfer(&mock, dir.path(), 100)
            .await
            .err()
            .unwrap()
            .contains("redirect limit")
    );
    assert_eq!(mock.pins.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn rejects_pages_manifests_encoded_data_truncation_and_oversize_with_cleanup() {
    let mut encoded = complete();
    encoded
        .headers
        .insert(reqwest::header::CONTENT_ENCODING, "gzip".parse().unwrap());
    let mut forbidden = complete();
    forbidden.status = 403;
    for reply in [
        encoded,
        forbidden,
        video(vec![Ok(b"<html>login</html>".to_vec())], None, "text/html"),
        video(
            vec![Ok(b"#EXTM3U\nhttp://127.0.0.1/x".to_vec())],
            None,
            "application/vnd.apple.mpegurl",
        ),
        video(
            vec![Ok(b"<html>bad</html>".to_vec())],
            None,
            "application/octet-stream",
        ),
        video(vec![Ok(MP4.to_vec())], Some(101), "video/mp4"),
        video(vec![Ok(MP4.to_vec()), Ok(vec![0; 100])], None, "video/mp4"),
        video(
            vec![Ok(MP4[..8].to_vec())],
            Some(MP4.len() as u64),
            "video/mp4",
        ),
        video(vec![], Some(0), "video/mp4"),
        video(
            vec![Ok(MP4.to_vec()), Err("fixture interrupted".into())],
            None,
            "video/mp4",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mock = MockTransport::new(&[&["8.8.8.8"]], vec![reply]);
        assert!(transfer(&mock, dir.path(), 100).await.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn atomic_publication_never_overwrites_existing_media_or_receipt() {
    for collision_is_receipt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mock = MockTransport::new(&[&["8.8.8.8"]], vec![complete()]);
        let staged = transfer(&mock, dir.path(), 100).await.unwrap();
        let part = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let media = part.with_extension("mp4");
        let receipt = media.with_file_name(format!(
            "{}.source.json",
            media.file_name().unwrap().to_string_lossy()
        ));
        let collision = if collision_is_receipt {
            &receipt
        } else {
            &media
        };
        std::fs::write(collision, b"user-data").unwrap();
        assert!(staged.publish().is_err());
        assert_eq!(std::fs::read(collision).unwrap(), b"user-data");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn runtime_gates_concurrency_and_retains_finished_files_on_cancellation() {
    let runtime = DownloadRuntime::default();
    let token = runtime.begin().unwrap();
    assert!(runtime.begin().is_err());
    token.cancel();
    runtime.finish(&Err("The download was canceled.".into()), &token);
    assert_eq!(runtime.inner.lock().unwrap().status.state, "canceled");
    assert!(runtime.begin().is_ok());
}

#[tokio::test]
async fn cancellation_drops_partial_transfer_without_publishing_media_or_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let mut reply = complete();
    reply.headers.remove(CONTENT_LENGTH);
    reply.body = Body::Pending(Some(MP4.to_vec()));
    let mock = MockTransport::new(&[&["8.8.8.8"]], vec![reply]);
    let runtime = DownloadRuntime::default();
    let token = runtime.begin().unwrap();
    let receive = network::transfer(
        reqwest::Url::parse(START).unwrap(),
        dir.path(),
        &mock,
        100,
        |count, total| runtime.progress(count, total),
    );
    let cancel_after_partial = async {
        while runtime.inner.lock().unwrap().status.downloaded_bytes == 0 {
            tokio::task::yield_now().await;
        }
        token.cancel();
    };
    let (result, _) = tokio::join!(
        wait_for_transfer(receive, &token, Duration::from_secs(1)),
        cancel_after_partial
    );
    assert!(result.err().unwrap().contains("canceled"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn deadline_drops_stalled_transfer_without_publishing_media_or_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let mut reply = complete();
    reply.headers.remove(CONTENT_LENGTH);
    reply.body = Body::Pending(Some(MP4.to_vec()));
    let mock = MockTransport::new(&[&["8.8.8.8"]], vec![reply]);
    let token = CancellationToken::new();
    let receive = transfer(&mock, dir.path(), 100);
    let result = wait_for_transfer(receive, &token, Duration::from_millis(1)).await;
    assert!(result.err().unwrap().contains("time limit"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
