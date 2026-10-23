//! One scrape: cache, single-flight, fetch, classify, render.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use futures::future::{BoxFuture, FutureExt, Shared};
use reqwest::header::{HeaderName, HeaderValue};
use time::OffsetDateTime;
use tokio::time::Instant;
use url::Url;

use crate::{
    config::Config,
    document::{self, DocumentError},
    net::{FetchError, Fetched, Fetcher, Route, Target},
    page::{self, RenderOptions, Rendered},
    store::{self, Store},
};

/// Firecrawl's default scrape timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

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
}

mod stored_route {
    use super::Route;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(route: &Route, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match route {
            Route::Direct => "direct",
            Route::Proxy => "proxy",
        })
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Route, D::Error> {
        match String::deserialize(d)?.as_str() {
            "direct" => Ok(Route::Direct),
            "proxy" => Ok(Route::Proxy),
            _ => Err(serde::de::Error::custom("invalid capture route")),
        }
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
}

impl ScrapeError {
    /// One error-code vocabulary for interactive responses and persisted job failures.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "BAD_REQUEST",
            Self::Blocked => "SCRAPE_ALL_ENGINES_FAILED",
            Self::Unsupported(_) => "SCRAPE_UNSUPPORTED_FILE_ERROR",
            Self::Pdf(DocumentError::Timeout) | Self::Fetch(FetchError::Timeout) => {
                "SCRAPE_TIMEOUT"
            }
            Self::Pdf(DocumentError::NoTextLayer) => "SCRAPE_PDF_OCR_REQUIRED",
            Self::Pdf(DocumentError::Encrypted | DocumentError::Failed) => {
                "SCRAPE_UNSUPPORTED_FILE_ERROR"
            }
            Self::Fetch(FetchError::Dns) => "SCRAPE_DNS_RESOLUTION_ERROR",
            Self::Fetch(FetchError::Tls) => "SCRAPE_SSL_ERROR",
            Self::Fetch(FetchError::Overloaded) => "CONCURRENCY_QUEUE_TIMEOUT",
            Self::Fetch(_) => "SCRAPE_SITE_ERROR",
        }
    }
}

type SharedFetch = Shared<BoxFuture<'static, Result<(Fetched, i64), FetchError>>>;

#[derive(Clone)]
pub struct Scraper {
    fetcher: Fetcher,
    store: Store,
    max_bytes: usize,
    inflight: Arc<Mutex<HashMap<String, SharedFetch>>>,
}

impl Scraper {
    pub fn new(fetcher: Fetcher, store: Store, cfg: &Config) -> Self {
        Self {
            fetcher,
            store,
            max_bytes: cfg.max_body_bytes,
            inflight: Arc::default(),
        }
    }

    pub async fn scrape(&self, req: &ScrapeRequest) -> Result<Document, ScrapeError> {
        let url = normalize(&req.url)?;
        let headers = request_headers(&req.headers)?;
        let mut key_material = url.to_string();
        for (name, value) in &headers {
            key_material.push_str(&format!("\n{name}: {value}"));
        }
        // Hashed so header values (possibly credentials) never reach the database.
        let key = store::sha256_hex(key_material.as_bytes());
        if !req.max_age.is_zero() {
            match self.store.cached_capture(&key, req.max_age).await {
                Ok(Some(capture)) => {
                    let mut doc = render_capture(capture.fetched, req, true).await?;
                    doc.capture_id = Some(capture.id);
                    return Ok(doc);
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "capture cache unavailable"),
            }
        }
        let target = Target {
            url,
            headers,
            deadline: req.deadline,
            max_bytes: self.max_bytes,
        };
        let (fetched, id) = self.fetch_once(key, target).await?;
        let mut doc = render_capture(fetched, req, false).await?;
        doc.capture_id = Some(id);
        Ok(doc)
    }

    /// Identical concurrent misses share one fetch. It runs as its own task, so it completes
    /// (and is stored) even if every waiting caller goes away, and it ends by its deadline.
    fn fetch_once(&self, key: String, target: Target) -> SharedFetch {
        let mut inflight = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(fetch) = inflight.get(&key) {
            return fetch.clone();
        }
        let this = self.clone();
        let task_key = key.clone();
        let task = tokio::spawn(async move {
            let result = async {
                let fetched = this.fetcher.fetch(&target).await?;
                let id = this.store.put(&task_key, &fetched).await.map_err(|error| {
                    tracing::error!(%error, "capture not stored");
                    FetchError::Protocol
                })?;
                Ok((fetched, id))
            }
            .await;
            let mut inflight = this.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            inflight.remove(&task_key);
            result
        });
        let fetch = async move { task.await.unwrap_or(Err(FetchError::Protocol)) }
            .boxed()
            .shared();
        inflight.insert(key, fetch.clone());
        fetch
    }
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

pub async fn render_capture(
    fetched: Fetched,
    req: &ScrapeRequest,
    hit: bool,
) -> Result<Document, ScrapeError> {
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
        let pdf = document::pdf_to_markdown(&fetched.body, req.deadline).await?;
        let mut rendered = Rendered {
            markdown: req.render.markdown.then_some(pdf.markdown),
            ..Rendered::default()
        };
        rendered.metadata.title = pdf.title;
        rendered
            .metadata
            .extra
            .insert("numPages".into(), pdf.pages.to_string());
        return Ok(Document {
            capture_id: None,
            blob_sha256,
            route: fetched.route,
            url: fetched.final_url,
            status: fetched.status,
            content_type: Some("application/pdf".into()),
            cached_at: hit.then_some(fetched.fetched_at),
            rendered,
            raw_html: None,
        });
    }
    let text = fetched.text();
    if page::is_challenge(fetched.status, &fetched.headers, &text) {
        return Err(ScrapeError::Blocked);
    }
    let rendered = match mime.as_deref() {
        None | Some("text/html" | "application/xhtml+xml") => {
            page::render(&text, &fetched.final_url, &req.render)
        }
        Some(m) if m.starts_with("text/") || m == "application/json" || m.ends_with("+json") => {
            Rendered {
                markdown: req.render.markdown.then(|| text.clone()),
                ..Rendered::default()
            }
        }
        Some(m) => return Err(ScrapeError::Unsupported(m.to_owned())),
    };
    Ok(Document {
        capture_id: None,
        blob_sha256,
        route: fetched.route,
        url: fetched.final_url,
        status: fetched.status,
        content_type: fetched.content_type,
        cached_at: hit.then_some(fetched.fetched_at),
        rendered,
        raw_html: req.raw_html.then_some(text),
    })
}
