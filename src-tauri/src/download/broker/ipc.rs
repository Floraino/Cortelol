//! Pure v1 framing/shape adapter for scripts/youtube_worker.py.
//! No reader, subprocess, DNS lookup, or HTTP call is performed here.

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::policy::{
    self, BrokerError, Budget, Method, RequestSpec, ResponseMeta, ValidatedRequest,
};

const MAX_IPC_URL_BYTES: usize = 4096;
const MAX_TOTAL_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpRequest {
    v: u8,
    kind: String,
    seq: u32,
    url: String,
    method: String,
    headers: StrictHeaders,
    body_b64: String,
}

struct StrictHeaders(Vec<(String, String)>);

impl<'de> Deserialize<'de> for StrictHeaders {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HeaderVisitor;
        impl<'de> Visitor<'de> for HeaderVisitor {
            type Value = StrictHeaders;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map of unique string headers")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut headers = Vec::new();
                let mut names = HashSet::new();
                while let Some((name, value)) = map.next_entry::<String, String>()? {
                    if headers.len() >= 32 || !names.insert(name.to_ascii_lowercase()) {
                        return Err(serde::de::Error::custom("duplicate or excess header"));
                    }
                    headers.push((name, value));
                }
                Ok(StrictHeaders(headers))
            }
        }
        deserializer.deserialize_map(HeaderVisitor)
    }
}

#[derive(Serialize)]
struct HttpResponse<'a> {
    v: u8,
    kind: &'static str,
    seq: u32,
    url: &'a str,
    status: u16,
    headers: std::collections::BTreeMap<&'a str, &'a str>,
    body_b64: String,
}

#[derive(Debug, PartialEq)]
pub struct MetadataResult {
    pub video_id: String,
    pub duration_seconds: f64,
    pub format_count: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMetadata {
    v: u8,
    kind: String,
    video_id: String,
    duration_seconds: f64,
    format_count: u8,
}

pub struct IpcSession {
    video_id: String,
    next_seq: u32,
    pending: Option<(u32, String)>,
    completed: bool,
    budget: Budget,
}

impl IpcSession {
    pub fn new(video_id: &str, max_duration: Duration) -> Result<Self, BrokerError> {
        if video_id.len() != 11
            || !video_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(BrokerError::InvalidRequest);
        }
        Ok(Self {
            video_id: video_id.into(),
            next_seq: 1,
            pending: None,
            completed: false,
            budget: Budget::new(MAX_TOTAL_RESPONSE_BYTES, max_duration),
        })
    }

    /// Consume exactly one complete length-prefixed worker frame and enforce seq.
    pub fn accept_request(&mut self, frame: &[u8]) -> Result<(u32, ValidatedRequest), BrokerError> {
        if self.pending.is_some() || self.completed {
            return Err(BrokerError::InvalidRequest);
        }
        let wire: HttpRequest =
            serde_json::from_slice(payload(frame)?).map_err(|_| BrokerError::InvalidRequest)?;
        if wire.v != 1
            || wire.kind != "http_request"
            || wire.seq != self.next_seq
            || wire.url.len() > MAX_IPC_URL_BYTES
            || wire.body_b64.len() > ((policy::MAX_REQUEST_BODY_BYTES + 2) / 3 * 4)
        {
            return Err(BrokerError::InvalidRequest);
        }
        let body = STANDARD
            .decode(wire.body_b64.as_bytes())
            .map_err(|_| BrokerError::InvalidRequest)?;
        let request = policy::validate_request(RequestSpec {
            url: wire.url,
            method: Method::parse(&wire.method)?,
            headers: wire.headers.0,
            body,
        })?;
        self.budget.begin_request(request.body.len())?;
        self.next_seq += 1;
        self.pending = Some((wire.seq, request.url.to_string()));
        Ok((wire.seq, request))
    }

    /// Serialize a bounded response for the pending request. URLs and headers are
    /// restricted to the worker's response schema; no redirect body is forwarded.
    pub fn response_frame(
        &mut self,
        seq: u32,
        response: &ResponseMeta,
    ) -> Result<Vec<u8>, BrokerError> {
        if self.completed {
            return Err(BrokerError::InvalidRequest);
        }
        let (pending_seq, url) = self.pending.as_ref().ok_or(BrokerError::InvalidRequest)?;
        if *pending_seq != seq
            || !(200..=599).contains(&response.status)
            || (300..400).contains(&response.status)
            || response.body.len() > policy::MAX_RESPONSE_BODY_BYTES
            || response.headers.len() > 32
        {
            return Err(BrokerError::InvalidRequest);
        }
        let mut headers = std::collections::BTreeMap::new();
        let mut bytes = 0usize;
        for (name, value) in &response.headers {
            bytes = bytes
                .checked_add(name.len() + value.len())
                .ok_or(BrokerError::LimitExceeded)?;
            if bytes > 16 * 1024
                || value.len() > 2048
                || value.bytes().any(|b| b < 32 || b == 127)
                || ![
                    "content-type",
                    "content-length",
                    "content-range",
                    "accept-ranges",
                    "date",
                    "etag",
                    "last-modified",
                    "cache-control",
                ]
                .contains(&name.to_ascii_lowercase().as_str())
            {
                return Err(BrokerError::InvalidRequest);
            }
            if headers
                .insert(name.to_ascii_lowercase(), value.as_str())
                .is_some()
            {
                return Err(BrokerError::InvalidRequest);
            }
        }
        self.budget.add_bytes(response.body.len())?;
        let wire = HttpResponse {
            v: 1,
            kind: "http_response",
            seq,
            url,
            status: response.status,
            headers: headers.iter().map(|(k, v)| (k.as_str(), *v)).collect(),
            body_b64: STANDARD.encode(&response.body),
        };
        let frame = encode_frame(&wire)?;
        self.pending = None;
        Ok(frame)
    }

    /// Accept the single terminal success frame after all requests are answered.
    pub fn metadata_result(&mut self, frame: &[u8]) -> Result<MetadataResult, BrokerError> {
        if self.pending.is_some() || self.completed {
            return Err(BrokerError::InvalidRequest);
        }
        let wire: WireMetadata =
            serde_json::from_slice(payload(frame)?).map_err(|_| BrokerError::InvalidRequest)?;
        if wire.v != 1
            || wire.kind != "metadata_result"
            || wire.video_id != self.video_id
            || !wire.duration_seconds.is_finite()
            || !(0.0..=21600.0).contains(&wire.duration_seconds)
            || wire.duration_seconds == 0.0
            || wire.format_count > 100
        {
            return Err(BrokerError::InvalidRequest);
        }
        self.completed = true;
        Ok(MetadataResult {
            video_id: wire.video_id,
            duration_seconds: wire.duration_seconds,
            format_count: wire.format_count,
        })
    }
}

fn payload(frame: &[u8]) -> Result<&[u8], BrokerError> {
    if frame.len() < 4 {
        return Err(BrokerError::InvalidRequest);
    }
    let size = u32::from_be_bytes(
        frame[..4]
            .try_into()
            .map_err(|_| BrokerError::InvalidRequest)?,
    ) as usize;
    if size == 0 || size > policy::MAX_FRAME_BYTES || frame.len() != size + 4 {
        return Err(BrokerError::InvalidRequest);
    }
    Ok(&frame[4..])
}

fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, BrokerError> {
    let body = serde_json::to_vec(value).map_err(|_| BrokerError::InvalidRequest)?;
    if body.is_empty() || body.len() > policy::MAX_FRAME_BYTES {
        return Err(BrokerError::LimitExceeded);
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(value: serde_json::Value) -> Vec<u8> {
        encode_frame(&value).unwrap()
    }
    fn request(seq: u32) -> serde_json::Value {
        json!({"v":1,"kind":"http_request","seq":seq,"url":"https://www.youtube.com/watch?v=YE7VzlLtp-4","method":"GET","headers":{"accept":"*/*"},"body_b64":""})
    }
    fn session() -> IpcSession {
        IpcSession::new("YE7VzlLtp-4", Duration::from_secs(30)).unwrap()
    }

    #[test]
    fn python_schema_round_trip() {
        let mut ipc = session();
        let (seq, req) = ipc.accept_request(&frame(request(1))).unwrap();
        assert_eq!(seq, 1);
        assert_eq!(req.url.host_str(), Some("www.youtube.com"));
        let response = ResponseMeta {
            status: 200,
            headers: vec![("content-type".into(), "text/html".into())],
            body: b"ok".to_vec(),
        };
        let encoded = ipc.response_frame(seq, &response).unwrap();
        let decoded: serde_json::Value =
            serde_json::from_slice(payload(&encoded).unwrap()).unwrap();
        assert_eq!(decoded["kind"], "http_response");
        assert_eq!(decoded["seq"], 1);
        assert_eq!(decoded["body_b64"], "b2s=");
        let metadata = frame(
            json!({"v":1,"kind":"metadata_result","video_id":"YE7VzlLtp-4","duration_seconds":123.5,"format_count":3}),
        );
        assert_eq!(ipc.metadata_result(&metadata).unwrap().format_count, 3);
        assert!(ipc.metadata_result(&metadata).is_err());
        assert!(ipc.accept_request(&frame(request(2))).is_err());
    }

    #[test]
    fn frames_shape_sequence_and_base64_fail_closed() {
        let mut ipc = session();
        let mut cases = vec![
            vec![],
            vec![0, 0, 0, 0],
            vec![0, 0, 0, 8, b'{'],
            (policy::MAX_FRAME_BYTES as u32 + 1).to_be_bytes().to_vec(),
            frame(json!([])),
            frame(json!({"v":2,"kind":"http_request"})),
        ];
        let mut extra = request(1);
        extra["extra"] = json!(true);
        cases.push(frame(extra));
        for bad in cases {
            assert!(ipc.accept_request(&bad).is_err());
        }
        for seq in [0, 2, 65] {
            assert!(ipc.accept_request(&frame(request(seq))).is_err());
        }
        let mut bad = request(1);
        bad["body_b64"] = json!("**");
        assert!(ipc.accept_request(&frame(bad)).is_err());
        let mut bad = request(1);
        bad["headers"] = json!({"cookie":"x"});
        assert!(ipc.accept_request(&frame(bad)).is_err());
        ipc.accept_request(&frame(request(1))).unwrap();
        assert!(ipc.accept_request(&frame(request(2))).is_err());
    }

    #[test]
    fn duplicate_headers_and_fields_are_rejected() {
        let raw = br#"{"v":1,"kind":"http_request","seq":1,"url":"https://youtube.com/","method":"GET","headers":{"Accept":"a","accept":"b"},"body_b64":""}"#;
        let mut framed = (raw.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(raw);
        assert!(session().accept_request(&framed).is_err());
        let raw = br#"{"v":1,"kind":"http_request","seq":1,"seq":1,"url":"https://youtube.com/","method":"GET","headers":{},"body_b64":""}"#;
        let mut framed = (raw.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(raw);
        assert!(session().accept_request(&framed).is_err());
    }

    #[test]
    fn response_and_terminal_limits_are_enforced() {
        let mut ipc = session();
        ipc.accept_request(&frame(request(1))).unwrap();
        let mut response = ResponseMeta {
            status: 200,
            headers: vec![],
            body: vec![],
        };
        assert!(ipc.response_frame(2, &response).is_err());
        response.status = 302;
        assert!(ipc.response_frame(1, &response).is_err());
        response.status = 200;
        response.headers.push(("set-cookie".into(), "x=y".into()));
        assert!(ipc.response_frame(1, &response).is_err());
        response.headers.clear();
        response.body = vec![0; policy::MAX_RESPONSE_BODY_BYTES + 1];
        assert!(ipc.response_frame(1, &response).is_err());
        let wrong = frame(
            json!({"v":1,"kind":"metadata_result","video_id":"other","duration_seconds":123,"format_count":3}),
        );
        assert!(ipc.metadata_result(&wrong).is_err());
        ipc.response_frame(
            1,
            &ResponseMeta {
                status: 200,
                headers: vec![],
                body: vec![],
            },
        )
        .unwrap();
        for value in [
            json!({"v":1,"kind":"metadata_result","video_id":"YE7VzlLtp-4","duration_seconds":0,"format_count":3}),
            json!({"v":1,"kind":"metadata_result","video_id":"YE7VzlLtp-4","duration_seconds":1,"format_count":101}),
            json!({"v":1,"kind":"error","code":"worker_failed"}),
        ] {
            assert!(ipc.metadata_result(&frame(value)).is_err());
        }
    }

    #[test]
    fn ipc_request_count_and_body_limit_match_worker() {
        let mut ipc = session();
        let empty = ResponseMeta {
            status: 200,
            headers: vec![],
            body: vec![],
        };
        for seq in 1..=policy::MAX_REQUESTS {
            ipc.accept_request(&frame(request(seq))).unwrap();
            ipc.response_frame(seq, &empty).unwrap();
        }
        assert!(
            ipc.accept_request(&frame(request(policy::MAX_REQUESTS + 1)))
                .is_err()
        );

        let mut oversized = request(1);
        oversized["url"] = json!("https://youtubei.googleapis.com/youtubei/v1/player");
        oversized["method"] = json!("POST");
        oversized["headers"] = json!({"content-type":"application/json"});
        oversized["body_b64"] =
            json!(STANDARD.encode(vec![b'x'; policy::MAX_REQUEST_BODY_BYTES + 1]));
        assert!(session().accept_request(&frame(oversized)).is_err());
    }
}
