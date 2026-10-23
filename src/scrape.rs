//! One scrape: cache, single-flight, fetch, classify, render.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use futures::future::{BoxFuture, FutureExt, Shared};
use reqwest::header::{HeaderName, HeaderValue};
use time::OffsetDateTime;
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

pub struct Document {
    pub route: Route,
    /// Final URL after redirects.
    pub url: Url,
    pub status: u16,
    pub content_type: Option<String>,
    /// Set when served from a stored capture: that capture's fetch time.
    pub cached_at: Option<OffsetDateTime>,
    pub rendered: Rendered,
    pub raw_html: Option<String>,
    /// Screenshot URL and action results from an explicit browser visit.
    pub browser: BrowserExtras,
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
}

type SharedFetch = Shared<BoxFuture<'static, Result<Fetched, FetchError>>>;
type SharedRender = Shared<BoxFuture<'static, Result<Fetched, BrowserError>>>;

#[derive(Clone)]
pub struct Scraper {
    fetcher: Fetcher,
    store: Store,
    max_bytes: usize,
    inflight: Arc<Mutex<HashMap<String, SharedFetch>>>,
    browser: BrowserTier,
    rendering: Arc<Mutex<HashMap<String, SharedRender>>>,
}

impl Scraper {
    pub fn new(fetcher: Fetcher, store: Store, cfg: &Config) -> Self {
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
    /// or do not help fall back to the HTTP result.
    pub async fn scrape_with(
        &self,
        req: &ScrapeRequest,
        plan: &BrowserPlan,
    ) -> Result<Document, ScrapeError> {
        let url = normalize(&req.url)?;
        let headers = request_headers(&req.headers)?;
        if plan.is_explicit() {
            return self.render_explicit(&url, &headers, req, plan).await;
        }
        let origin = url.origin().ascii_serialization();
        if self.browser.prefers(&origin)
            && let Some(doc) = self.render_auto(&url, &headers, req, None).await
        {
            return Ok(doc);
        }
        let (fetched, hit) = self.fetch_http(url.clone(), headers.clone(), req).await?;
        if self.browser.is_enabled()
            && let Escalation::Render { visible } = policy::classify_http(&fetched)
            && let Some(doc) = self.render_auto(&url, &headers, req, Some(visible)).await
        {
            return Ok(doc);
        }
        finish(fetched, req, hit).await
    }

    /// The HTTP capture: a fresh enough cached one, or a shared fetch.
    async fn fetch_http(
        &self,
        url: Url,
        headers: Vec<(String, String)>,
        req: &ScrapeRequest,
    ) -> Result<(Fetched, bool), ScrapeError> {
        let mut key_material = url.to_string();
        for (name, value) in &headers {
            key_material.push_str(&format!("\n{name}: {value}"));
        }
        // Hashed so header values (possibly credentials) never reach the database.
        let key = store::sha256_hex(key_material.as_bytes());
        if !req.max_age.is_zero() {
            match self.store.cached(&key, req.max_age).await {
                Ok(Some(capture)) => return Ok((capture, true)),
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
        Ok((self.fetch_once(key, target).await?, false))
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
        if let Err(error) = self.store.put_rendered(&key, &output.capture).await {
            tracing::error!(%error, "rendered capture not stored");
        }
        let mut doc = finish(output.capture, req, false).await?;
        doc.browser = output.extras;
        Ok(doc)
    }

    /// An automatic render from the rendered cache or a shared visit. It is used only when it
    /// shows real text (and at least twice the HTTP text when escalating); success remembers
    /// the origin, failure forgets it.
    async fn render_auto(
        &self,
        url: &Url,
        headers: &[(String, String)],
        req: &ScrapeRequest,
        http_visible: Option<usize>,
    ) -> Option<Document> {
        let origin = url.origin().ascii_serialization();
        let key = self
            .browser
            .rendered_key(url, headers, &BrowserPlan::default());
        let cached = if req.max_age.is_zero() {
            None
        } else {
            self.store.cached(&key, req.max_age).await.ok().flatten()
        };
        let (fetched, hit) = match cached {
            Some(capture) => (capture, true),
            None => {
                let render = self.render_once(key, url.clone(), headers.to_vec(), req.deadline);
                match timeout_at(req.deadline, render).await {
                    Ok(Ok(capture)) => (capture, false),
                    _ => {
                        self.browser.forget(&origin);
                        return None;
                    }
                }
            }
        };
        let visible = policy::visible_chars(&fetched.text());
        if !policy::improves(http_visible.unwrap_or(0), visible) {
            self.browser.forget(&origin);
            return None;
        }
        match finish(fetched, req, hit).await {
            Ok(doc) => {
                self.browser.remember(&origin);
                Some(doc)
            }
            Err(_) => {
                self.browser.forget(&origin);
                None
            }
        }
    }

    /// Identical concurrent automatic renders share one visit, stored before it resolves. Each
    /// waiter still stops at its own deadline.
    fn render_once(
        &self,
        key: String,
        url: Url,
        headers: Vec<(String, String)>,
        deadline: Instant,
    ) -> SharedRender {
        let mut rendering = self
            .rendering
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(render) = rendering.get(&key) {
            return render.clone();
        }
        let this = self.clone();
        let task_key = key.clone();
        let task = tokio::spawn(async move {
            let result = this
                .browser
                .render(&url, &headers, deadline, &BrowserPlan::default())
                .await
                .map(|output| output.capture);
            if let Ok(capture) = &result
                && let Err(error) = this.store.put_rendered(&task_key, capture).await
            {
                tracing::error!(%error, "rendered capture not stored");
            }
            let mut rendering = this
                .rendering
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            rendering.remove(&task_key);
            result
        });
        let render = async move {
            task.await
                .unwrap_or(Err(BrowserError::Unavailable("render task failed")))
        }
        .boxed()
        .shared();
        rendering.insert(key, render.clone());
        render
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
            let result = this.fetcher.fetch(&target).await;
            if let Ok(fetched) = &result
                && let Err(error) = this.store.put(&task_key, fetched).await
            {
                tracing::error!(%error, "capture not stored");
            }
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
fn request_headers(headers: &[(String, String)]) -> Result<Vec<(String, String)>, ScrapeError> {
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

async fn finish(fetched: Fetched, req: &ScrapeRequest, hit: bool) -> Result<Document, ScrapeError> {
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
            route: fetched.route,
            url: fetched.final_url,
            status: fetched.status,
            content_type: Some("application/pdf".into()),
            cached_at: hit.then_some(fetched.fetched_at),
            rendered,
            raw_html: None,
            browser: BrowserExtras::default(),
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
        route: fetched.route,
        url: fetched.final_url,
        status: fetched.status,
        content_type: fetched.content_type,
        cached_at: hit.then_some(fetched.fetched_at),
        rendered,
        raw_html: req.raw_html.then_some(text),
        browser: BrowserExtras::default(),
    })
}
