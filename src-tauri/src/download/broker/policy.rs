use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

pub const MAX_FRAME_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
pub const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REQUESTS: u32 = 64;
const MAX_URL_BYTES: usize = 8192;
const MAX_HEADER_COUNT: usize = 24;
const MAX_HEADER_BYTES: usize = 8192;
const MAX_DNS_ANSWERS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerError {
    InvalidRequest,
    UnsafeDestination,
    InvalidDns,
    LimitExceeded,
    NetworkFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    Head,
    Post,
}

impl Method {
    pub fn parse(raw: &str) -> Result<Self, BrokerError> {
        match raw {
            "GET" => Ok(Self::Get),
            "HEAD" => Ok(Self::Head),
            "POST" => Ok(Self::Post),
            _ => Err(BrokerError::InvalidRequest),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RequestSpec {
    pub url: String,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct ValidatedRequest {
    pub url: Url,
    pub method: Method,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct PinnedRequest {
    pub request: ValidatedRequest,
    pub address: SocketAddr,
}

#[derive(Clone, Debug)]
pub struct ResponseMeta {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct Budget {
    started: Instant,
    pub requests: u32,
    pub transferred_bytes: u64,
    pub max_transferred_bytes: u64,
    pub max_duration: Duration,
}

impl Budget {
    pub fn new(max_transferred_bytes: u64, max_duration: Duration) -> Self {
        Self {
            started: Instant::now(),
            requests: 0,
            transferred_bytes: 0,
            max_transferred_bytes,
            max_duration,
        }
    }

    pub fn remaining_time(&self) -> Result<Duration, BrokerError> {
        self.max_duration
            .checked_sub(self.started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(BrokerError::LimitExceeded)
    }

    pub fn begin_request(&mut self, body_bytes: usize) -> Result<(), BrokerError> {
        self.remaining_time()?;
        if self.requests >= MAX_REQUESTS || body_bytes > MAX_REQUEST_BODY_BYTES {
            return Err(BrokerError::LimitExceeded);
        }
        self.requests += 1;
        self.add_bytes(body_bytes)?;
        Ok(())
    }

    pub fn add_bytes(&mut self, bytes: usize) -> Result<(), BrokerError> {
        self.remaining_time()?;
        let next = self
            .transferred_bytes
            .checked_add(bytes as u64)
            .ok_or(BrokerError::LimitExceeded)?;
        if next > self.max_transferred_bytes {
            return Err(BrokerError::LimitExceeded);
        }
        self.transferred_bytes = next;
        Ok(())
    }
}

fn permitted_host(host: &str) -> bool {
    // Exact service domains and their subdomains. A suffix requires a dot boundary.
    [
        "youtube.com",
        "youtube-nocookie.com",
        "youtu.be",
        "youtubei.googleapis.com",
        "googlevideo.com",
        "ytimg.com",
    ]
    .iter()
    .any(|domain| {
        host == *domain
            || (host.ends_with(domain)
                && host.as_bytes().get(host.len() - domain.len() - 1) == Some(&b'.'))
    })
}

pub fn validate_request(spec: RequestSpec) -> Result<ValidatedRequest, BrokerError> {
    if spec.url.is_empty()
        || spec.url.len() > MAX_URL_BYTES
        || spec
            .url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || spec.body.len() > MAX_REQUEST_BODY_BYTES
        || spec.headers.len() > MAX_HEADER_COUNT
    {
        return Err(BrokerError::InvalidRequest);
    }
    let url = Url::parse(&spec.url).map_err(|_| BrokerError::InvalidRequest)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err(BrokerError::UnsafeDestination);
    }
    let host = url.host_str().ok_or(BrokerError::UnsafeDestination)?;
    if host.parse::<IpAddr>().is_ok() || !permitted_host(host) {
        return Err(BrokerError::UnsafeDestination);
    }
    if spec.method == Method::Head && !spec.body.is_empty()
        || spec.method == Method::Get && !spec.body.is_empty()
    {
        return Err(BrokerError::InvalidRequest);
    }
    let mut headers = HeaderMap::new();
    let mut size = 0usize;
    let mut seen = HashSet::new();
    for (name, value) in spec.headers {
        size = size
            .checked_add(name.len() + value.len())
            .ok_or(BrokerError::LimitExceeded)?;
        if size > MAX_HEADER_BYTES {
            return Err(BrokerError::LimitExceeded);
        }
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| BrokerError::InvalidRequest)?;
        if !seen.insert(name.clone()) || !permitted_header(name.as_str(), &value) {
            return Err(BrokerError::InvalidRequest);
        }
        let value = HeaderValue::from_str(&value).map_err(|_| BrokerError::InvalidRequest)?;
        headers.insert(name, value);
    }
    if spec.method == Method::Post {
        let mime = headers
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .ok_or(BrokerError::InvalidRequest)?;
        if !["application/json", "application/x-www-form-urlencoded"].contains(&mime) {
            return Err(BrokerError::InvalidRequest);
        }
        if host != "youtubei.googleapis.com" {
            return Err(BrokerError::UnsafeDestination);
        }
    }
    Ok(ValidatedRequest {
        url,
        method: spec.method,
        headers,
        body: spec.body,
    })
}

fn permitted_header(name: &str, value: &str) -> bool {
    if value.len() > 2048 || value.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    match name {
        "accept" | "accept-language" | "user-agent" => true,
        "origin" | "referer" => Url::parse(value).ok().is_some_and(|url| {
            url.scheme() == "https"
                && url.port_or_known_default() == Some(443)
                && url.username().is_empty()
                && url.password().is_none()
                && url.host_str().is_some_and(permitted_host)
        }),
        "content-type" => {
            ["application/json", "application/x-www-form-urlencoded"].contains(&value)
        }
        "range" => valid_range(value),
        "x-youtube-client-name" | "x-youtube-client-version" | "x-goog-visitor-id" => {
            value.len() <= 256
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-=+/".contains(&b))
        }
        _ => false,
    }
}

fn valid_range(value: &str) -> bool {
    let Some(range) = value.strip_prefix("bytes=") else {
        return false;
    };
    let Some((start, end)) = range.split_once('-') else {
        return false;
    };
    !start.is_empty()
        && start.bytes().all(|b| b.is_ascii_digit())
        && start.parse::<u64>().is_ok()
        && (end.is_empty()
            || end
                .parse::<u64>()
                .is_ok_and(|end| start.parse::<u64>().is_ok_and(|start| end >= start)))
}

/// Inspect Location without following it. The caller must issue a fresh request,
/// which consumes another budget slot and repeats URL and DNS validation.
pub fn validate_redirect(
    current: &ValidatedRequest,
    status: u16,
    location: &str,
) -> Result<Url, BrokerError> {
    if ![301, 302, 303, 307, 308].contains(&status)
        || current.method == Method::Post
        || location.is_empty()
        || location.len() > MAX_URL_BYTES
        || location
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(BrokerError::InvalidRequest);
    }
    let next = current
        .url
        .join(location)
        .map_err(|_| BrokerError::InvalidRequest)?;
    validate_request(RequestSpec {
        url: next.to_string(),
        method: current.method,
        headers: vec![],
        body: vec![],
    })
    .map(|v| v.url)
}

pub fn pin(request: ValidatedRequest, addresses: &[IpAddr]) -> Result<PinnedRequest, BrokerError> {
    if addresses.is_empty()
        || addresses.len() > MAX_DNS_ANSWERS
        || !addresses.iter().copied().all(public_ip)
    {
        return Err(BrokerError::InvalidDns);
    }
    Ok(PinnedRequest {
        request,
        address: SocketAddr::new(addresses[0], 443),
    })
}

pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_ipv4(ip),
        IpAddr::V6(ip) => {
            let s = ip.segments();
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && (s[1] <= 0x01ff || s[1] == 0x0db8))
                && s[0] != 0x2002
        }
    }
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !matches!(a, 0 | 10 | 127 | 169 | 224..=255)
        && !(a == 100 && (64..=127).contains(&b))
        && !(a == 172 && (16..=31).contains(&b))
        && !(a == 192 && (b == 168 || b == 0 || b == 88 && c == 99))
        && !(a == 198 && (b == 18 || b == 19 || b == 51 && c == 100))
        && !(a == 203 && b == 0 && c == 113)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(url: &str) -> RequestSpec {
        RequestSpec {
            url: url.into(),
            method: Method::Get,
            headers: vec![],
            body: vec![],
        }
    }

    #[test]
    fn hostile_urls_never_validate() {
        for url in [
            "http://youtube.com/watch",
            "https://youtube.com:444/watch",
            "https://user:pass@youtube.com/",
            "https://youtube.com.evil.test/",
            "https://evil-youtube.com/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://youtube.com/#fragment",
            "https://youtube.com/\n",
        ] {
            assert!(validate_request(spec(url)).is_err(), "{url}");
        }
        assert!(validate_request(spec("https://r1---sn.googlevideo.com/videoplayback")).is_ok());
    }

    #[test]
    fn denies_credentials_and_header_smuggling() {
        for (name, value) in [
            ("Cookie", "session=x"),
            ("Authorization", "Bearer x"),
            ("Proxy-Authorization", "x"),
            ("Host", "127.0.0.1"),
            ("Connection", "keep-alive"),
            ("Referer", "https://evil.test/"),
            ("Accept", "ok\r\nHost: evil"),
        ] {
            let mut request = spec("https://www.youtube.com/watch");
            request.headers.push((name.into(), value.into()));
            assert!(validate_request(request).is_err(), "{name}");
        }
    }

    #[test]
    fn post_only_to_youtubei_with_small_typed_body() {
        let mut request = spec("https://youtubei.googleapis.com/youtubei/v1/player");
        request.method = Method::Post;
        request.body = b"{}".to_vec();
        request
            .headers
            .push(("Content-Type".into(), "application/json".into()));
        assert!(validate_request(request.clone()).is_ok());
        request.url = "https://youtube.com/api".into();
        assert!(validate_request(request).is_err());
    }

    #[test]
    fn mixed_dns_and_special_ranges_fail() {
        let request = validate_request(spec("https://youtube.com/watch")).unwrap();
        let good: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(pin(request.clone(), &[good]).is_ok());
        for bad in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2001:db8::1",
            "2002:0808:0808::1",
        ] {
            let bad: IpAddr = bad.parse().unwrap();
            assert!(!public_ip(bad), "{bad}");
            assert!(pin(request.clone(), &[good, bad]).is_err());
        }
    }

    #[test]
    fn budgets_bound_requests_bytes_and_time() {
        let mut budget = Budget::new(10, Duration::from_secs(1));
        budget.begin_request(5).unwrap();
        budget.add_bytes(5).unwrap();
        assert!(budget.add_bytes(1).is_err());
        let mut budget = Budget::new(100, Duration::from_secs(1));
        for _ in 0..MAX_REQUESTS {
            budget.begin_request(0).unwrap();
        }
        assert!(budget.begin_request(0).is_err());
        assert!(Budget::new(10, Duration::ZERO).begin_request(0).is_err());
    }

    #[test]
    fn redirects_require_fresh_validation_and_reject_post() {
        let get = validate_request(spec("https://youtube.com/watch?v=abc")).unwrap();
        assert!(validate_redirect(&get, 302, "/next").is_ok());
        for location in [
            "http://youtube.com/",
            "https://127.0.0.1/",
            "https://youtube.com.evil.test/",
            "https://youtube.com:8443/",
            "//localhost/",
            "https://youtube.com/\r\nX: y",
        ] {
            assert!(
                validate_redirect(&get, 302, location).is_err(),
                "{location}"
            );
        }
        let mut post = spec("https://youtubei.googleapis.com/youtubei/v1/player");
        post.method = Method::Post;
        post.body = b"{}".to_vec();
        post.headers
            .push(("content-type".into(), "application/json".into()));
        assert!(validate_redirect(&validate_request(post).unwrap(), 307, "/again").is_err());
    }
}
