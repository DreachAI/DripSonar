//! One scrape: cache, single-flight, fetch, classify, render.

mod change;
pub use change::ChangeTrackingOptions;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::Duration,
};

use futures::future::{BoxFuture, FutureExt, Shared};
use reqwest::header::{HeaderName, HeaderValue};
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio::time::{Instant, timeout_at};
use url::Url;

use crate::{
    browser::{
        BrowserError, BrowserExtras, BrowserPlan, BrowserTier,
        policy::{self, Escalation},
    },
    config::Config,
    document::{self, DocumentError},
    net::{FetchError, Fetched, Fetcher, Route, Target},
    page::{self, RenderOptions, Rendered},
    store::{self, Store},
};

/// Firecrawl's default scrape timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

static RENDER_SLOTS: OnceLock<Semaphore> = OnceLock::new();

/// Set the process-wide CPU budget before rendering starts, including CLI exports.
pub fn configure_rendering(threads: usize) {
    RENDER_SLOTS.get_or_init(|| Semaphore::new(threads.clamp(1, Semaphore::MAX_PERMITS)));
}

#[derive(Clone, Debug)]
pub struct ScrapeRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub deadline: Instant,
    /// Newest acceptable cached capture; zero always fetches.
    pub max_age: Duration,
    pub render: RenderOptions,
    pub raw_html: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Document {
    pub capture_id: Option<i64>,
    pub blob_sha256: Option<String>,
    #[serde(with = "stored_route")]
    pub route: Route,
    /// Final URL after redirects.
    pub url: Url,
    pub status: u16,
    pub content_type: Option<String>,
    /// Set when served from a stored capture: that capture's fetch time.
    pub cached_at: Option<OffsetDateTime>,
    pub rendered: Rendered,
    pub raw_html: Option<String>,
    /// Firecrawl's `warning`: why a successful document carries less than requested.
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_tracking: Option<serde_json::Value>,
    /// Screenshot URL and action results from an explicit browser visit; never persisted,
    /// since jobs do not run browser plans.
    #[serde(skip)]
    pub browser: BrowserExtras,
}

mod stored_route {
    use super::Route;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(route: &Route, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(route.as_str())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Route, D::Error> {
        Route::parse(&String::deserialize(d)?)
            .ok_or_else(|| serde::de::Error::custom("invalid capture route"))
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ScrapeTemplate {
    pub headers: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub max_age_ms: u64,
    pub render: RenderOptions,
    pub raw_html: bool,
}

impl ScrapeTemplate {
    pub fn request(&self, url: &Url) -> ScrapeRequest {
        ScrapeRequest {
            url: url.to_string(),
            headers: self.headers.clone(),
            deadline: Instant::now() + Duration::from_millis(self.timeout_ms),
            max_age: Duration::from_millis(self.max_age_ms),
            render: self.render.clone(),
            raw_html: self.raw_html,
        }
    }
}

impl From<&ScrapeRequest> for ScrapeTemplate {
    fn from(req: &ScrapeRequest) -> Self {
        Self {
            headers: req.headers.clone(),
            timeout_ms: req
                .deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1) as u64,
            max_age_ms: req.max_age.as_millis() as u64,
            render: req.render.clone(),
            raw_html: req.raw_html,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScrapeError {
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("the site served a challenge or block page")]
    Blocked,
    #[error("unsupported content type {0}")]
    Unsupported(String),
    #[error(transparent)]
    Pdf(#[from] DocumentError),
    #[error(transparent)]
    Browser(#[from] BrowserError),
    #[error("capture could not be stored")]
    Storage,
}

impl ScrapeError {
    /// One error-code vocabulary for interactive responses and persisted job failures.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "BAD_REQUEST",
            Self::Blocked => "SCRAPE_ALL_ENGINES_FAILED",
            Self::Unsupported(_) => "SCRAPE_UNSUPPORTED_FILE_ERROR",
            Self::Storage | Self::Browser(BrowserError::Storage) => "SCRAPE_STORAGE_ERROR",
            Self::Pdf(DocumentError::Timeout) => "SCRAPE_TIMEOUT",
            Self::Pdf(DocumentError::NoTextLayer) => "SCRAPE_PDF_OCR_REQUIRED",
            Self::Pdf(DocumentError::Encrypted | DocumentError::Failed) => {
                "SCRAPE_UNSUPPORTED_FILE_ERROR"
            }
            Self::Fetch(error) | Self::Browser(BrowserError::Fetch(error)) => match error {
                FetchError::Timeout => "SCRAPE_TIMEOUT",
                FetchError::Dns => "SCRAPE_DNS_RESOLUTION_ERROR",
                FetchError::Tls => "SCRAPE_SSL_ERROR",
                FetchError::Overloaded => "CONCURRENCY_QUEUE_TIMEOUT",
                _ => "SCRAPE_SITE_ERROR",
            },
            Self::Browser(BrowserError::Action { .. }) => "SCRAPE_ACTION_ERROR",
            Self::Browser(BrowserError::SessionNotFound) => "BROWSER_SESSION_NOT_FOUND",
            Self::Browser(BrowserError::SessionLimit) => "BROWSER_SESSION_LIMIT",
            Self::Browser(
                BrowserError::Disabled | BrowserError::Unavailable(_) | BrowserError::Limit(_),
            ) => "SCRAPE_ALL_ENGINES_FAILED",
        }
    }
}

/// What a shared fetch or render hands every waiter; `ScrapeError` itself is not `Clone`.
#[derive(Clone, Debug)]
enum FlightError {
    Fetch(FetchError),
    Browser(BrowserError),
    Storage,
}

impl From<FlightError> for ScrapeError {
    fn from(error: FlightError) -> Self {
        match error {
            FlightError::Fetch(error) => Self::Fetch(error),
            FlightError::Browser(error) => Self::Browser(error),
            FlightError::Storage => Self::Storage,
        }
    }
}

type Flight = Shared<BoxFuture<'static, Result<(Fetched, i64), FlightError>>>;
type Flights = Arc<Mutex<HashMap<String, Flight>>>;

/// Request validators: a response to them may be a bodiless 304.
const VALIDATORS: [&str; 2] = ["if-none-match", "if-modified-since"];

/// How the HTTP capture behind a document was obtained.
#[derive(Clone, Copy, PartialEq)]
enum Served {
    Fetched,
    Cached,
    /// The site answered 304; the newest prior capture for the same request stands in.
    NotModified,
}

#[derive(Clone)]
pub struct Scraper {
    fetcher: Fetcher,
    store: Store,
    max_bytes: usize,
    inflight: Flights,
    browser: BrowserTier,
    rendering: Flights,
}

impl Scraper {
    pub fn new(fetcher: Fetcher, store: Store, cfg: &Config) -> Self {
        configure_rendering(cfg.render_threads);
        Self {
            fetcher,
            store,
            max_bytes: cfg.max_body_bytes,
            inflight: Arc::default(),
            browser: BrowserTier::default(),
            rendering: Arc::default(),
        }
    }

    /// Adds the browser tier: explicit visits, and escalation of script-shell pages.
    pub fn with_browser(mut self, browser: BrowserTier) -> Self {
        self.browser = browser;
        self
    }

    pub async fn scrape(&self, req: &ScrapeRequest) -> Result<Document, ScrapeError> {
        self.scrape_with(req, &BrowserPlan::default()).await
    }

    /// An explicit plan (actions, waits, screenshots) goes straight to the browser without
    /// cache reads or shared execution. Otherwise plain HTTP, escalating script shells; an
    /// origin that recently needed the browser goes browser-first. Automatic renders that fail
    /// or do not help fall back to the HTTP result; a capture that cannot be stored fails.
    pub async fn scrape_with(
        &self,
        req: &ScrapeRequest,
        plan: &BrowserPlan,
    ) -> Result<Document, ScrapeError> {
        let url = normalize(&req.url)?;
        let mut headers = request_headers(&req.headers)?;
        if plan.is_explicit() {
            return self.render_explicit(&url, &headers, req, plan).await;
        }
        let origin = url.origin().ascii_serialization();
        if self.browser.prefers(&origin)
            && let Some(doc) = self.render_auto(&url, &headers, req, None).await?
        {
            return Ok(doc);
        }
        // This caller's deadline bounds its cache reads and its wait on a shared fetch; the
        // fetch itself carries on for other waiters.
        let mut request = req.clone();
        let mut refetched = false;
        let (prepared, served) = loop {
            let (fetched, id, served) =
                timeout_at(req.deadline, self.fetch_http(&url, &headers, &request))
                    .await
                    .map_err(|_| FetchError::Timeout)??;
            match render_stored(
                fetched,
                id,
                req,
                served != Served::Fetched,
                self.browser.is_enabled(),
            )
            .await
            {
                Err(ScrapeError::Blocked) if served != Served::Fetched && !refetched => {
                    // A stored challenge is a cache miss, including a 304's prior capture.
                    refetched = true;
                    request.max_age = Duration::ZERO;
                    headers.retain(|(name, _)| !VALIDATORS.contains(&name.as_str()));
                }
                result => break (result?, served),
            }
        };
        if self.browser.is_enabled()
            && let Escalation::Render { visible } = prepared.escalation
            && let Some(doc) = self.render_auto(&url, &headers, req, Some(visible)).await?
        {
            return Ok(doc);
        }
        let mut doc = prepared.doc;
        if served == Served::NotModified {
            let extra = &mut doc.rendered.metadata.extra;
            extra.insert("notModified".into(), true.into());
        }
        Ok(doc)
    }

    /// The HTTP capture for this request. A 304 answers the caller's validators and has no
    /// page to render: the newest prior capture for the same request stands in, or else the
    /// request is sent once more without validators.
    async fn fetch_http(
        &self,
        url: &Url,
        headers: &[(String, String)],
        req: &ScrapeRequest,
    ) -> Result<(Fetched, i64, Served), ScrapeError> {
        let key = http_key(url, headers);
        let capture = self
            .cached_or_fetched(key.clone(), url, headers, req)
            .await?;
        if capture.0.status != 304 {
            return Ok(capture);
        }
        if !headers
            .iter()
            .any(|(name, _)| VALIDATORS.contains(&name.as_str()))
        {
            // A 304 to an unconditional request is a protocol violation, never a cache hit.
            return Err(FetchError::Protocol.into());
        }
        match self.store.cached_capture(&key, Duration::MAX).await {
            Ok(Some(prior)) => return Ok((prior.fetched, prior.id, Served::NotModified)),
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "capture cache unavailable"),
        }
        let plain: Vec<_> = headers
            .iter()
            .filter(|(name, _)| !VALIDATORS.contains(&name.as_str()))
            .cloned()
            .collect();
        let key = http_key(url, &plain);
        let capture = self.cached_or_fetched(key, url, &plain, req).await?;
        if capture.0.status == 304 {
            return Err(FetchError::Protocol.into());
        }
        Ok(capture)
    }

    /// A fresh enough cached capture, or a shared fetch stored before it resolves.
    async fn cached_or_fetched(
        &self,
        key: String,
        url: &Url,
        headers: &[(String, String)],
        req: &ScrapeRequest,
    ) -> Result<(Fetched, i64, Served), ScrapeError> {
        if !req.max_age.is_zero() {
            match self.store.cached_capture(&key, req.max_age).await {
                Ok(Some(capture)) => return Ok((capture.fetched, capture.id, Served::Cached)),
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "capture cache unavailable"),
            }
        }
        let target = Target {
            url: url.clone(),
            headers: headers.to_vec(),
            deadline: req.deadline,
            max_bytes: self.max_bytes,
        };
        let (fetcher, store, flight_key) = (self.fetcher.clone(), self.store.clone(), key.clone());
        let work = async move {
            let fetched = fetcher.fetch(&target).await.map_err(FlightError::Fetch)?;
            let id = store.put(&flight_key, &fetched).await.map_err(not_stored)?;
            Ok((fetched, id))
        };
        let (fetched, id) = share(&self.inflight, key, work).await?;
        Ok((fetched, id, Served::Fetched))
    }

    /// A caller-driven visit. Captures with actions are stored for the record under a key no
    /// lookup produces, since the page they show may reflect the actions' side effects.
    async fn render_explicit(
        &self,
        url: &Url,
        headers: &[(String, String)],
        req: &ScrapeRequest,
        plan: &BrowserPlan,
    ) -> Result<Document, ScrapeError> {
        let output = self
            .browser
            .render(url, headers, req.deadline, plan)
            .await?;
        let key = if plan.actions.is_empty() {
            self.browser.rendered_key(url, headers, plan)
        } else {
            store::sha256_hex(format!("actions\n{}", uuid::Uuid::new_v4()).as_bytes())
        };
        let id = timeout_at(req.deadline, self.store.put_rendered(&key, &output.capture))
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(not_stored)?;
        let mut doc = render_stored(output.capture, id, req, false, false)
            .await?
            .doc;
        doc.browser = output.extras;
        Ok(doc)
    }

    /// An automatic render from the rendered cache or a shared visit, bounded by this caller's
    /// deadline. It is used only when it shows real text (and at least twice the HTTP text when
    /// escalating); success remembers the origin, failure forgets it. A render that cannot be
    /// stored fails the scrape rather than falling back.
    async fn render_auto(
        &self,
        url: &Url,
        headers: &[(String, String)],
        req: &ScrapeRequest,
        http_visible: Option<usize>,
    ) -> Result<Option<Document>, ScrapeError> {
        let origin = url.origin().ascii_serialization();
        let key = self
            .browser
            .rendered_key(url, headers, &BrowserPlan::default());
        let capture = timeout_at(req.deadline, async {
            if !req.max_age.is_zero()
                && let Ok(Some(capture)) = self.store.cached_capture(&key, req.max_age).await
            {
                match render_stored(capture.fetched, capture.id, req, true, true).await {
                    Ok(prepared) => return Ok(prepared),
                    Err(ScrapeError::Blocked) => {}
                    Err(error) => return Err(error),
                }
            }
            let (browser, store, flight_key) =
                (self.browser.clone(), self.store.clone(), key.clone());
            let (url, headers, deadline) = (url.clone(), headers.to_vec(), req.deadline);
            let work = async move {
                let output = browser
                    .render(&url, &headers, deadline, &BrowserPlan::default())
                    .await
                    .map_err(FlightError::Browser)?;
                let id = timeout_at(deadline, store.put_rendered(&flight_key, &output.capture))
                    .await
                    .map_err(|_| FlightError::Fetch(FetchError::Timeout))?
                    .map_err(not_stored)?;
                Ok((output.capture, id))
            };
            let (fetched, id) = share(&self.rendering, key.clone(), work)
                .await
                .map_err(ScrapeError::from)?;
            render_stored(fetched, id, req, false, true).await
        })
        .await;
        let prepared = match capture {
            Ok(Ok(capture)) => capture,
            Ok(Err(ScrapeError::Storage)) => return Err(ScrapeError::Storage),
            _ => {
                self.browser.forget(&origin);
                return Ok(None);
            }
        };
        if !policy::improves(http_visible.unwrap_or(0), prepared.visible) {
            self.browser.forget(&origin);
            return Ok(None);
        }
        self.browser.remember(&origin);
        Ok(Some(prepared.doc))
    }
}

/// Identical concurrent work shares one task. It runs on its own, so it completes (and stores
/// its capture) even if every waiter leaves, and it ends by its own deadline.
fn share(
    flights: &Flights,
    key: String,
    work: impl Future<Output = Result<(Fetched, i64), FlightError>> + Send + 'static,
) -> Flight {
    let mut map = flights.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(flight) = map.get(&key) {
        return flight.clone();
    }
    let (flights_ref, task_key) = (Arc::clone(flights), key.clone());
    let task = tokio::spawn(async move {
        // Built on first poll, so a task dropped unpolled never locks the map held here.
        let _landed = Landed {
            flights: flights_ref,
            key: task_key,
        };
        work.await
    });
    let flight = async move {
        task.await
            .unwrap_or(Err(FlightError::Fetch(FetchError::Protocol)))
    }
    .boxed()
    .shared();
    map.insert(key, flight.clone());
    flight
}

/// Removes its single-flight entry when the task ends, whether it returns, errors, or panics.
struct Landed {
    flights: Flights,
    key: String,
}

impl Drop for Landed {
    fn drop(&mut self) {
        let mut map = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
        map.remove(&self.key);
    }
}

/// Hashed so header values (possibly credentials) never reach the database.
fn http_key(url: &Url, headers: &[(String, String)]) -> String {
    let mut material = url.to_string();
    for (name, value) in headers {
        material.push_str(&format!("\n{name}: {value}"));
    }
    store::sha256_hex(material.as_bytes())
}

/// Missing scheme gets `http://`, as Firecrawl does; the fragment never reaches the server.
pub fn normalize(input: &str) -> Result<Url, ScrapeError> {
    let input = input.trim();
    let mut url = match Url::parse(input) {
        Ok(url) if url.has_host() => url,
        _ => Url::parse(&format!("http://{input}"))
            .map_err(|e| ScrapeError::Invalid(format!("invalid url: {e}")))?,
    };
    url.set_fragment(None);
    Ok(url)
}

/// Validated, lowercased, and sorted, so equal header sets share one cache key.
pub(crate) fn request_headers(
    headers: &[(String, String)],
) -> Result<Vec<(String, String)>, ScrapeError> {
    let mut valid = headers
        .iter()
        .map(|(name, value)| {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ScrapeError::Invalid(format!("invalid header name {name}")))?;
            HeaderValue::from_str(value)
                .map_err(|_| ScrapeError::Invalid(format!("invalid value for header {name}")))?;
            Ok((name.as_str().to_owned(), value.clone()))
        })
        .collect::<Result<Vec<_>, ScrapeError>>()?;
    valid.sort();
    Ok(valid)
}

/// A capture whose provenance is not recorded is never returned as a result.
fn not_stored(error: crate::store::StoreError) -> FlightError {
    tracing::error!(%error, "capture not stored");
    FlightError::Storage
}

/// Renders a stored capture and records which capture it came from.
async fn render_stored(
    fetched: Fetched,
    id: i64,
    req: &ScrapeRequest,
    hit: bool,
    inspect: bool,
) -> Result<Prepared, ScrapeError> {
    let mut prepared = prepare_capture(fetched, req, hit, inspect).await?;
    prepared.doc.capture_id = Some(id);
    Ok(prepared)
}

pub async fn render_capture(
    fetched: Fetched,
    req: &ScrapeRequest,
    hit: bool,
) -> Result<Document, ScrapeError> {
    Ok(prepare_capture(fetched, req, hit, false).await?.doc)
}

struct Prepared {
    doc: Document,
    escalation: Escalation,
    visible: usize,
}

enum ParsedCapture {
    Page(Prepared),
    Pdf(Document, bytes::Bytes),
}

/// Runs CPU-bound rendering off the async runtime, bounded by the process-wide render slots
/// and the caller's deadline.
pub(crate) async fn render_blocking<T: Send + 'static>(
    deadline: Instant,
    work: impl FnOnce() -> Result<T, ScrapeError> + Send + 'static,
) -> Result<T, ScrapeError> {
    timeout_at(deadline, async {
        let slots =
            RENDER_SLOTS.get_or_init(|| Semaphore::new(crate::config::default_render_threads()));
        let permit = slots.acquire().await.map_err(|_| FetchError::Protocol)?;
        tokio::task::spawn_blocking(move || {
            // Cancellation cannot stop a running parser; it must keep its CPU slot until done.
            let _permit = permit;
            if Instant::now() >= deadline {
                return Err(FetchError::Timeout.into());
            }
            work()
        })
        .await
        .map_err(|_| ScrapeError::Fetch(FetchError::Protocol))?
    })
    .await
    .map_err(|_| FetchError::Timeout)?
}

async fn prepare_capture(
    fetched: Fetched,
    req: &ScrapeRequest,
    hit: bool,
    inspect: bool,
) -> Result<Prepared, ScrapeError> {
    let request = req.clone();
    let parsed = render_blocking(req.deadline, move || {
        parse_capture(fetched, &request, hit, inspect)
    })
    .await?;
    match parsed {
        ParsedCapture::Page(prepared) => Ok(prepared),
        ParsedCapture::Pdf(mut doc, body) => {
            if req.render.skip_pdf_parsing {
                doc.rendered.markdown = req.render.markdown.then(String::new);
                doc.warning = Some("PDF parsing disabled".to_owned());
            } else {
                let pdf = document::pdf_to_markdown(&body, req.deadline).await?;
                doc.rendered.markdown = req.render.markdown.then_some(pdf.markdown);
                doc.rendered.metadata.title = pdf.title;
                doc.rendered
                    .metadata
                    .extra
                    .insert("numPages".into(), pdf.pages.into());
            }
            Ok(Prepared {
                doc,
                escalation: Escalation::Keep,
                visible: 0,
            })
        }
    }
}

fn parse_capture(
    fetched: Fetched,
    req: &ScrapeRequest,
    hit: bool,
    inspect: bool,
) -> Result<ParsedCapture, ScrapeError> {
    let blob_sha256 = Some(store::sha256_hex(&fetched.body));
    let mime = fetched.content_type.as_deref().map(|value| {
        let essence = value.split_once(';').map_or(value, |(essence, _)| essence);
        essence.trim().to_ascii_lowercase()
    });
    if mime.as_deref() == Some("application/pdf")
        || (mime.as_deref() == Some("application/octet-stream")
            && fetched
                .final_url
                .path()
                .to_ascii_lowercase()
                .ends_with(".pdf"))
        || fetched.body.starts_with(b"%PDF-")
    {
        return Ok(ParsedCapture::Pdf(
            Document {
                capture_id: None,
                blob_sha256,
                route: fetched.route,
                url: fetched.final_url,
                status: fetched.status,
                content_type: Some("application/pdf".into()),
                cached_at: hit.then_some(fetched.fetched_at),
                rendered: Rendered::default(),
                raw_html: None,
                warning: None,
                change_tracking: None,
                browser: BrowserExtras::default(),
            },
            fetched.body,
        ));
    }
    let text = fetched.text();
    if page::is_challenge(fetched.status, &fetched.headers, &text) {
        return Err(ScrapeError::Blocked);
    }
    let (mut escalation, mut visible) = (Escalation::Keep, 0);
    let rendered = match mime.as_deref() {
        None | Some("text/html" | "application/xhtml+xml") => {
            let parsed = page::parse(&text);
            if inspect {
                // Decoding can exceed the render cap; classification historically sees all text.
                let full = (text.len() > page::MAX_INPUT_BYTES)
                    .then(|| dom_query::Document::from(text.as_str()));
                let inspected = full.as_ref().unwrap_or(&parsed);
                visible = policy::visible_chars(inspected);
                escalation = policy::classify_http(&fetched, &text, inspected, visible);
            }
            page::render_document(parsed, &fetched.final_url, &req.render)
        }
        Some(m) if m.starts_with("text/") || m == "application/json" || m.ends_with("+json") => {
            Rendered {
                markdown: req.render.markdown.then(|| text.clone()),
                ..Rendered::default()
            }
        }
        Some(m) => return Err(ScrapeError::Unsupported(m.to_owned())),
    };
    Ok(ParsedCapture::Page(Prepared {
        doc: Document {
            capture_id: None,
            blob_sha256,
            route: fetched.route,
            url: fetched.final_url,
            status: fetched.status,
            content_type: fetched.content_type,
            cached_at: hit.then_some(fetched.fetched_at),
            rendered,
            raw_html: req.raw_html.then_some(text),
            warning: None,
            change_tracking: None,
            browser: BrowserExtras::default(),
        },
        escalation,
        visible,
    }))
}
