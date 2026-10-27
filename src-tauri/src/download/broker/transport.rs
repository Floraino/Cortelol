use std::net::{IpAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_LENGTH};

use super::policy::{self, BrokerError, Budget, Method, PinnedRequest, RequestSpec, ResponseMeta};

struct DenyDns;

impl reqwest::dns::Resolve for DenyDns {
    fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async { Err(std::io::Error::other("unvalidated DNS lookup denied").into()) })
    }
}

/// Resolve all A/AAAA answers before creating a client. A mixed or oversized answer fails.
/// Call again for each distinct request or redirect; never reuse a prior answer as authority.
pub async fn resolve_and_pin(
    request: policy::ValidatedRequest,
    budget: &Budget,
) -> Result<PinnedRequest, BrokerError> {
    let host = request
        .url
        .host_str()
        .ok_or(BrokerError::UnsafeDestination)?
        .to_owned();
    let remaining = budget.remaining_time()?;
    let lookup = tokio::task::spawn_blocking(move || {
        (host.as_str(), 443)
            .to_socket_addrs()
            .map(|addresses| {
                addresses
                    .take(65)
                    .map(|address| address.ip())
                    .collect::<Vec<IpAddr>>()
            })
            .map_err(|_| BrokerError::InvalidDns)
    });
    let answers = tokio::time::timeout(remaining.min(Duration::from_secs(10)), lookup)
        .await
        .map_err(|_| BrokerError::LimitExceeded)?
        .map_err(|_| BrokerError::InvalidDns)??;
    policy::pin(request, &answers)
}

fn client(target: &PinnedRequest, budget: &Budget) -> Result<Client, BrokerError> {
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
        .read_timeout(Duration::from_secs(15))
        .timeout(budget.remaining_time()?)
        .pool_max_idle_per_host(0)
        .dns_resolver(Arc::new(DenyDns))
        .resolve(
            target
                .request
                .url
                .host_str()
                .ok_or(BrokerError::UnsafeDestination)?,
            target.address,
        )
        .build()
        .map_err(|_| BrokerError::NetworkFailure)
}

/// Performs exactly one HTTPS hop. 3xx is returned without a body or Location following;
/// the caller must validate a new RequestSpec, re-resolve, and consume another request slot.
/// No transparent retry, proxy, resolver fallback, or connection reuse is permitted.
pub async fn send_one_hop(
    spec: RequestSpec,
    budget: &mut Budget,
) -> Result<ResponseMeta, BrokerError> {
    let request = policy::validate_request(spec)?;
    budget.begin_request(request.body.len())?;
    let target = resolve_and_pin(request, budget).await?;
    let client = client(&target, budget)?;
    let method = match target.request.method {
        Method::Get => reqwest::Method::GET,
        Method::Head => reqwest::Method::HEAD,
        Method::Post => reqwest::Method::POST,
    };
    let mut builder = client
        .request(method, target.request.url.clone())
        .headers(target.request.headers.clone())
        .header(ACCEPT_ENCODING, "identity");
    if !target.request.body.is_empty() {
        builder = builder.body(target.request.body.clone());
    }
    let mut response = builder
        .send()
        .await
        .map_err(|_| BrokerError::NetworkFailure)?;
    if response.remote_addr() != Some(target.address) {
        return Err(BrokerError::UnsafeDestination);
    }
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or(BrokerError::InvalidRequest)?;
        let next = policy::validate_redirect(&target.request, status, location)?;
        return Ok(ResponseMeta {
            status,
            headers: vec![("location".into(), next.to_string())],
            body: Vec::new(),
        });
    }
    if response
        .headers()
        .get("content-encoding")
        .is_some_and(|v| v != "identity")
    {
        return Err(BrokerError::InvalidRequest);
    }
    if response.headers().len() > 64 {
        return Err(BrokerError::LimitExceeded);
    }
    let mut headers = Vec::new();
    let mut header_bytes = 0usize;
    for (name, value) in response.headers() {
        header_bytes = header_bytes
            .checked_add(name.as_str().len() + value.as_bytes().len())
            .ok_or(BrokerError::LimitExceeded)?;
        if header_bytes > 16 * 1024 {
            return Err(BrokerError::LimitExceeded);
        }
        if [
            "content-type",
            "content-length",
            "content-range",
            "accept-ranges",
            "location",
        ]
        .contains(&name.as_str())
        {
            headers.push((
                name.as_str().to_owned(),
                value
                    .to_str()
                    .map_err(|_| BrokerError::InvalidRequest)?
                    .to_owned(),
            ));
        }
    }
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > policy::MAX_RESPONSE_BODY_BYTES as u64)
    {
        return Err(BrokerError::LimitExceeded);
    }
    let mut body = Vec::new();
    while let Some(chunk) = tokio::time::timeout(budget.remaining_time()?, response.chunk())
        .await
        .map_err(|_| BrokerError::LimitExceeded)?
        .map_err(|_| BrokerError::NetworkFailure)?
    {
        let next = body
            .len()
            .checked_add(chunk.len())
            .ok_or(BrokerError::LimitExceeded)?;
        if next > policy::MAX_RESPONSE_BODY_BYTES {
            return Err(BrokerError::LimitExceeded);
        }
        budget.add_bytes(chunk.len())?;
        body.extend_from_slice(&chunk);
    }
    Ok(ResponseMeta {
        status,
        headers,
        body,
    })
}
