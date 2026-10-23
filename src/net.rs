//! The only network path: destination guard, redirects, deadlines, byte caps, decoding.

mod gate;

#[cfg(any(test, feature = "test-support"))]
use std::{collections::HashMap, net::IpAddr};
use std::{io, sync::Arc, time::Duration};

use bytes::BytesMut;
use encoding_rs::{Encoding, UTF_8};
use tokio::sync::Semaphore;
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

use self::gate::{Gate, Pass};
pub use crate::egress::Route;
use crate::{
    config::Config,
    egress::{DnsFailure, FetchGuard as Guard, GuardError, GuardedResolver, Refusal, RouteConfig},
};

/// Redirects followed per fetch; one more is `TooManyRedirects`.
const MAX_REDIRECTS: usize = 5;

pub struct Target {
    pub url: url::Url,
    pub headers: Vec<(String, String)>,
    pub deadline: tokio::time::Instant,
    pub max_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct Fetched {
    pub route: Route,
    pub requested_url: url::Url,
    pub final_url: url::Url,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub body: bytes::Bytes,
    pub fetched_at: time::OffsetDateTime,
}

impl Fetched {
    /// First header value with this name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Body decoded with the header charset, then `<meta charset>`, then lossy UTF-8.
    /// A byte-order mark overrides both, as browsers do.
    pub fn text(&self) -> String {
        let label = self
            .content_type
            .as_deref()
            .and_then(charset_param)
            .or_else(|| meta_charset(&self.body));
        let encoding = label
            .and_then(|label| Encoding::for_label(label.as_bytes()))
            .unwrap_or(UTF_8);
        encoding.decode(&self.body).0.into_owned()
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum FetchError {
    #[error("destination refused: {0}")]
    Refused(String),
    #[error("dns resolution failed")]
    Dns,
    #[error("tls failure")]
    Tls,
    #[error("connection failed")]
    Connect,
    #[error("too many redirects")]
    TooManyRedirects,
    #[error("body exceeds limit")]
    TooLarge,
    #[error("deadline exceeded")]
    Timeout,
    #[error("server overloaded: no fetch slot before the deadline")]
    Overloaded,
    #[error("http protocol error")]
    Protocol,
}

/// One global and one per-host fetch slot, held for a whole browser visit.
pub struct Admission {
    _pass: Pass,
}

#[derive(Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    proxy_client: Option<reqwest::Client>,
    routes: RouteConfig,
    guard: Arc<Guard>,
    gate: Arc<Gate>,
    webhook_slots: Arc<Semaphore>,
}

impl Fetcher {
    pub fn new(cfg: &Config) -> Result<Self, FetchError> {
        Self::build(cfg, Guard::new(&cfg.extra_ports))
    }

    /// Test-only: names resolve from `hosts` alone and loopback addresses are allowed.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(
        cfg: &Config,
        hosts: HashMap<String, Vec<IpAddr>>,
    ) -> Result<Self, FetchError> {
        let mut guard = Guard::new(&cfg.extra_ports);
        guard.test_hosts = Some(hosts);
        Self::build(cfg, guard)
    }

    fn build(cfg: &Config, guard: Guard) -> Result<Self, FetchError> {
        let guard = Arc::new(guard);
        let routes = RouteConfig::from_config(cfg)
            .map_err(|error| FetchError::Refused(error.to_string()))?;
        let builder = || {
            reqwest::Client::builder()
                .user_agent(cfg.user_agent.as_str())
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(Arc::new(GuardedResolver(Arc::clone(&guard))))
        };
        let client = builder().build().map_err(|_| FetchError::Protocol)?;
        let proxy_client = routes
            .reqwest_proxy()
            .map_err(|error| FetchError::Refused(error.to_string()))?
            .map(|proxy| {
                builder()
                    .proxy(proxy)
                    .build()
                    .map_err(|_| FetchError::Protocol)
            })
            .transpose()?;
        Ok(Self {
            client,
            proxy_client,
            routes,
            guard,
            gate: Arc::new(Gate::new(cfg)),
            webhook_slots: Arc::new(Semaphore::new(4)),
        })
    }

    /// Share the same destination policy with the browser forward proxy.
    pub fn guard(&self) -> Arc<Guard> {
        Arc::clone(&self.guard)
    }

    /// Cloning preserves the shared daily byte budget.
    pub fn route_config(&self) -> RouteConfig {
        self.routes.clone()
    }

    /// Admits a browser visit: the guard and DNS policy first, so refusals and DNS failures
    /// classify as they do for fetches, then the same global and per-host slots. Subresources
    /// go through the forward proxy and never take a slot, so a page cannot deadlock itself.
    pub async fn admit(&self, url: &Url, deadline: Instant) -> Result<Admission, FetchError> {
        timeout_at(deadline, self.guard.resolve_url(url))
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(|error| match error {
                GuardError::Refused(refusal) => FetchError::Refused(refusal.0),
                GuardError::Dns(_) => FetchError::Dns,
            })?;
        let pass = self
            .gate
            .enter(url.host_str().unwrap_or_default(), deadline)
            .await?;
        Ok(Admission { _pass: pass })
    }

    /// One guarded exchange; `target.deadline` covers queueing, redirects, retries, and body.
    pub async fn fetch(&self, target: &Target) -> Result<Fetched, FetchError> {
        self.fetch_with_route(target, None).await
    }

    /// A request-local override applies to redirects too, without changing host routing.
    pub async fn fetch_with_route(
        &self,
        target: &Target,
        route: Option<Route>,
    ) -> Result<Fetched, FetchError> {
        if route == Some(Route::Proxy) && self.proxy_client.is_none() {
            return Err(FetchError::Refused("proxy not configured".into()));
        }
        timeout_at(target.deadline, self.follow(target, route))
            .await
            .unwrap_or(Err(FetchError::Timeout))
    }

    /// One guarded POST, without redirects or transport retries. The durable outbox owns
    /// retries; a timeout can mean the receiver accepted the event already.
    pub async fn post_json(
        &self,
        url: &Url,
        headers: &[(String, String)],
        body: &[u8],
        deadline: Instant,
    ) -> Result<u16, FetchError> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(10));
        timeout_at(deadline, async {
            self.guard
                .check_url(url)
                .map_err(|refusal| FetchError::Refused(refusal.0))?;
            let _permit = self
                .webhook_slots
                .acquire()
                .await
                .map_err(|_| FetchError::Protocol)?;
            let route = Route::Direct;
            let mut request = self
                .client
                .post(url.clone())
                .timeout(deadline.saturating_duration_since(Instant::now()))
                .header(reqwest::header::CONTENT_TYPE, "application/json");
            for (name, value) in headers {
                request = request.header(name, value);
            }
            self.routes
                .charge(route, body.len())
                .map_err(|error| FetchError::Refused(error.to_string()))?;
            let response = request
                .body(body.to_vec())
                .send()
                .await
                .map_err(|error| classify(&error, url))?;
            // No response body is needed; dropping it bounds hostile response sizes.
            Ok(response.status().as_u16())
        })
        .await
        .unwrap_or(Err(FetchError::Timeout))
    }

    async fn follow(
        &self,
        target: &Target,
        override_route: Option<Route>,
    ) -> Result<Fetched, FetchError> {
        let mut url = target.url.clone();
        for _ in 0..=MAX_REDIRECTS {
            self.guard
                .check_url(&url)
                .map_err(|refusal| FetchError::Refused(refusal.0))?;
            // Caller headers may carry credentials; they never follow a redirect to another origin.
            let headers = if url.origin() == target.url.origin() {
                &target.headers[..]
            } else {
                &[]
            };
            let route = override_route
                .unwrap_or_else(|| self.routes.route_for(url.host_str().unwrap_or_default()));
            let (response, pass) = self.send(&url, headers, target.deadline, route).await?;
            match redirect_location(&response) {
                Some(location) => url = next_hop(&url, location)?,
                None => return read(response, pass, target, url, route, &self.routes).await,
            }
        }
        Err(FetchError::TooManyRedirects)
    }

    /// Sends one request, retrying once on a connect failure or reset and once on a
    /// `429`/`503` whose `Retry-After` fits within the deadline.
    async fn send(
        &self,
        url: &Url,
        headers: &[(String, String)],
        deadline: Instant,
        route: Route,
    ) -> Result<(reqwest::Response, Pass), FetchError> {
        let host = url.host_str().unwrap_or_default();
        let (mut retried_error, mut retried_status) = (false, false);
        loop {
            let pass = self.gate.enter(host, deadline).await?;
            self.routes
                .check_budget(route)
                .map_err(|error| FetchError::Refused(error.to_string()))?;
            let client = match route {
                Route::Direct => &self.client,
                Route::Proxy => self.proxy_client.as_ref().ok_or(FetchError::Protocol)?,
            };
            let mut request = client.get(url.clone());
            for (name, value) in headers {
                request = request.header(name, value);
            }
            match request.send().await {
                Ok(response) => {
                    let retry_after = retry_after(&response);
                    if matches!(response.status().as_u16(), 429 | 503) {
                        self.gate.penalize(host, retry_after);
                    } else if response.status().is_success() {
                        self.gate.reward(host);
                    }
                    if !retried_status
                        && matches!(response.status().as_u16(), 429 | 503)
                        && let Some(wait) = retry_after
                        && wait < deadline.saturating_duration_since(Instant::now())
                        && self.gate.next_start(host) < deadline
                    {
                        retried_status = true;
                        drop((response, pass));
                        sleep(wait).await;
                        continue;
                    }
                    return Ok((response, pass));
                }
                Err(error) => match classify(&error, url) {
                    FetchError::Connect if !retried_error => retried_error = true,
                    error => return Err(error),
                },
            }
        }
    }
}

fn redirect_location(response: &reqwest::Response) -> Option<&str> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    response
        .headers()
        .get(reqwest::header::LOCATION)?
        .to_str()
        .ok()
}

fn next_hop(from: &Url, location: &str) -> Result<Url, FetchError> {
    let next = from.join(location).map_err(|_| FetchError::Protocol)?;
    if from.scheme() == "https" && next.scheme() == "http" {
        return Err(FetchError::Refused("redirect from https to http".into()));
    }
    Ok(next)
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let value = value.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse() {
        return Some(Duration::from_secs(seconds));
    }
    let at =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc2822).ok()?;
    Some(
        (at - time::OffsetDateTime::now_utc())
            .try_into()
            .unwrap_or_default(),
    )
}

/// Reads the body, refusing it once its decoded size passes `target.max_bytes`.
async fn read(
    mut response: reqwest::Response,
    _pass: Pass,
    target: &Target,
    url: Url,
    route: Route,
    routes: &RouteConfig,
) -> Result<Fetched, FetchError> {
    // Under content decoding reqwest reports no length, so this only rejects identity bodies.
    if response
        .content_length()
        .is_some_and(|len| len > target.max_bytes as u64)
    {
        return Err(FetchError::TooLarge);
    }
    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_owned())))
        .collect();
    let mut body = BytesMut::new();
    // shortcut: reqwest exposes decoded bodies; use metered transport for complete wire-byte billing.
    while let Some(chunk) = response.chunk().await.map_err(|e| classify(&e, &url))? {
        routes
            .charge(route, chunk.len())
            .map_err(|error| FetchError::Refused(error.to_string()))?;
        if body.len() + chunk.len() > target.max_bytes {
            return Err(FetchError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    let content_type = headers
        .iter()
        .find(|(name, _)| name == "content-type")
        .map(|(_, value)| value.clone());
    Ok(Fetched {
        route,
        requested_url: target.url.clone(),
        final_url: url,
        status,
        headers,
        content_type,
        body: body.freeze(),
        fetched_at: time::OffsetDateTime::now_utc(),
    })
}

fn classify(error: &reqwest::Error, url: &Url) -> FetchError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(cause) = source {
        if let Some(refusal) = cause.downcast_ref::<Refusal>() {
            return FetchError::Refused(refusal.0.clone());
        }
        if cause.is::<DnsFailure>() {
            return FetchError::Dns;
        }
        if error.is_connect() && url.scheme() == "https" && is_invalid_data(cause) {
            return FetchError::Tls;
        }
        source = cause.source();
    }
    if error.is_timeout() {
        FetchError::Timeout
    } else if error.is_connect() || error.is_request() || error.is_body() {
        FetchError::Connect
    } else {
        FetchError::Protocol
    }
}

/// tokio-rustls reports handshake and certificate failures as `InvalidData`. Wrapped
/// `io::Error`s skip their inner error in `source()`, so this unwraps them directly.
fn is_invalid_data(cause: &(dyn std::error::Error + 'static)) -> bool {
    let mut io = cause.downcast_ref::<io::Error>();
    while let Some(error) = io {
        if error.kind() == io::ErrorKind::InvalidData {
            return true;
        }
        io = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<io::Error>());
    }
    false
}

fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        let value = value.trim().trim_matches(['"', '\'']);
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.to_owned())
    })
}

/// The first `charset` declared in a `<meta>` tag within the first 1024 bytes.
fn meta_charset(body: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&body[..body.len().min(1024)]).to_ascii_lowercase();
    head.split("<meta").skip(1).find_map(|tag| {
        let tag = tag.split('>').next()?;
        let rest = &tag[tag.find("charset")? + "charset".len()..];
        let rest = rest.trim_start().strip_prefix('=')?.trim_start();
        let rest = rest.trim_start_matches(['"', '\'']);
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || "-_:.".contains(c)))
            .unwrap_or(rest.len());
        (end > 0).then(|| rest[..end].to_owned())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use axum::{
        Router,
        body::Body,
        extract::Path,
        http::{StatusCode, header},
        response::{IntoResponse, Redirect},
        routing::get,
    };
    use futures::StreamExt;

    use super::*;
    use crate::testkit;

    fn target(url: &str, timeout_ms: u64) -> Target {
        Target {
            url: url.parse().unwrap(),
            headers: Vec::new(),
            deadline: Instant::now() + Duration::from_millis(timeout_ms),
            max_bytes: 1000,
        }
    }

    #[tokio::test]
    async fn redirects_are_reguarded_and_capped() {
        let app = Router::new()
            .route(
                "/hop/{n}",
                get(|Path(n): Path<u32>| async move {
                    match n {
                        0 => "done".into_response(),
                        n => Redirect::to(&format!("/hop/{}", n - 1)).into_response(),
                    }
                }),
            )
            .route(
                "/literal",
                get(|| async { Redirect::to("http://10.0.0.1/") }),
            )
            .route(
                "/named",
                get(|| async { Redirect::to("http://private.test/") }),
            )
            .route(
                "/mixed",
                get(|| async { Redirect::to("http://mixed.test/") }),
            );
        let (fetcher, base) = testkit::fetcher_for(app).await;
        let named = base.replace("127.0.0.1", "site.test");

        let done = fetcher
            .fetch(&target(&format!("{named}/hop/5"), 5000))
            .await
            .unwrap();
        assert_eq!((done.status, done.final_url.path()), (200, "/hop/0"));
        let sixth = fetcher.fetch(&target(&format!("{base}/hop/6"), 5000)).await;
        assert!(matches!(sixth, Err(FetchError::TooManyRedirects)));
        for path in ["literal", "named", "mixed"] {
            let refused = fetcher
                .fetch(&target(&format!("{base}/{path}"), 5000))
                .await;
            assert!(matches!(refused, Err(FetchError::Refused(_))), "{path}");
        }
        let downgrade = next_hop(&"https://a.example/".parse().unwrap(), "http://a.example/");
        assert!(matches!(downgrade, Err(FetchError::Refused(_))));
    }

    #[tokio::test]
    async fn limits_fail_closed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/big", get(|| async { "x".repeat(2000) }))
            .route(
                "/stream",
                get(|| async {
                    let chunks = futures::stream::iter(0..20)
                        .map(|_| Ok::<_, io::Error>(bytes::Bytes::from(vec![b'x'; 100])));
                    Body::from_stream(chunks)
                }),
            )
            .route(
                "/slow",
                get(|| async {
                    let chunks = futures::stream::iter(0..2).then(|_| async {
                        sleep(Duration::from_secs(2)).await;
                        Ok::<_, io::Error>(bytes::Bytes::from_static(b"x"))
                    });
                    Body::from_stream(chunks)
                }),
            )
            .route(
                "/hold",
                get(|| async {
                    sleep(Duration::from_millis(800)).await;
                    "held"
                }),
            )
            .route(
                "/retry",
                get(move || async move {
                    match calls.fetch_add(1, Ordering::SeqCst) {
                        0 => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [(header::RETRY_AFTER, "0")],
                        )
                            .into_response(),
                        _ => "ok".into_response(),
                    }
                }),
            )
            .route(
                "/busy",
                get(|| async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "3600")],
                    )
                }),
            );
        let (fetcher, base) = testkit::fetcher_for(app).await;
        let fetch = |path: &str, timeout_ms| {
            let target = target(&format!("{base}{path}"), timeout_ms);
            let fetcher = fetcher.clone();
            async move { fetcher.fetch(&target).await }
        };

        assert!(matches!(
            fetch("/big", 5000).await,
            Err(FetchError::TooLarge)
        ));
        assert!(matches!(
            fetch("/stream", 5000).await,
            Err(FetchError::TooLarge)
        ));
        assert!(matches!(
            fetch("/slow", 300).await,
            Err(FetchError::Timeout)
        ));
        let https = target(&base.replace("http:", "https:"), 5000);
        assert!(matches!(fetcher.fetch(&https).await, Err(FetchError::Tls)));
        assert_eq!(fetch("/retry", 5000).await.unwrap().status, 200);
        assert_eq!(fetch("/busy", 5000).await.unwrap().status, 429);

        // testkit allows one global slot: a second fetch queued past its deadline is overload.
        let holder = tokio::spawn({
            let fetcher = fetcher.clone();
            let url = format!("{base}/hold");
            async move { fetcher.fetch(&target(&url, 5000)).await.map(|f| f.status) }
        });
        sleep(Duration::from_millis(100)).await;
        assert!(matches!(
            fetch("/hold", 300).await,
            Err(FetchError::Overloaded)
        ));
        assert_eq!(holder.await.unwrap().unwrap(), 200);
    }

    #[test]
    fn charset_decoding() {
        let page = |content_type: Option<&str>, body: &[u8]| Fetched {
            route: Route::Direct,
            requested_url: "http://a.example/".parse().unwrap(),
            final_url: "http://a.example/".parse().unwrap(),
            status: 200,
            headers: Vec::new(),
            content_type: content_type.map(str::to_owned),
            body: bytes::Bytes::copy_from_slice(body),
            fetched_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(
            page(Some("text/html; charset=\"ISO-8859-1\""), b"caf\xe9").text(),
            "café"
        );
        let meta = b"<meta http-equiv=Content-Type content='text/html; charset=windows-1252'>\xe9";
        assert!(page(Some("text/html"), meta).text().ends_with('é'));
        assert_eq!(
            page(None, b"<meta charset=\"latin1\">\xe9").text(),
            "<meta charset=\"latin1\">é"
        );
        assert_eq!(page(None, b"caf\xc3\xa9 \xff").text(), "café \u{fffd}");
    }
}
