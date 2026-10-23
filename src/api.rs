//! HTTP routes, wire shapes, validation, auth, and error mapping.
//!
//! `/v1/scrape` and `/v1/search` speak Firecrawl v1; `/v1/web/*` speak context.dev.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, MatchedPath, Request, State, rejection::BytesRejection},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tokio::time::Instant;
use tracing::Instrument;

use crate::{
    config::Config,
    net::{FetchError, Fetcher},
    page::RenderOptions,
    scrape::{self, Document, ScrapeError, ScrapeRequest, Scraper},
    search::{Hit, SearchError, Searcher},
    store::{Store, StoreError},
};

const BODY_LIMIT: usize = 1 << 20;
const SCRAPE_TIMEOUT_MS: u64 = scrape::DEFAULT_TIMEOUT.as_millis() as u64;
/// Firecrawl's default search timeout.
const SEARCH_TIMEOUT_MS: u64 = 60_000;
const SEARCH_SCRAPE_CONCURRENCY: usize = 4;

#[derive(Clone)]
pub struct AppState {
    scraper: Scraper,
    searcher: Searcher,
    api_key: Option<Arc<str>>,
    max_timeout_ms: u64,
    default_max_age_ms: u64,
}

impl AppState {
    pub fn new(cfg: &Config, fetcher: Fetcher) -> Result<Self, StoreError> {
        let store = Store::open(&cfg.data_dir)?;
        Ok(Self {
            scraper: Scraper::new(fetcher.clone(), store, cfg),
            searcher: Searcher::new(fetcher, cfg),
            api_key: cfg.api_key.as_deref().map(Arc::from),
            max_timeout_ms: cfg.max_timeout_ms,
            default_max_age_ms: cfg.default_max_age_ms,
        })
    }

    /// The requested timeout (at least one second), clamped to the server maximum.
    fn timeout(&self, requested_ms: Option<u64>, default_ms: u64) -> Result<Duration, ApiError> {
        match requested_ms.unwrap_or(default_ms) {
            ms if ms < 1000 => Err(ApiError::bad_request("timeout must be at least 1000 ms")),
            ms => Ok(Duration::from_millis(ms.min(self.max_timeout_ms))),
        }
    }

    /// Validates Firecrawl scrape options into a request; an absent `url` is left empty.
    fn plan(
        &self,
        opts: &ScrapeOptions,
        default_formats: &[Format],
    ) -> Result<(ScrapeRequest, Vec<Format>), ApiError> {
        let timeout = self.timeout(opts.timeout, SCRAPE_TIMEOUT_MS)?;
        let requested_ms = opts.timeout.unwrap_or(SCRAPE_TIMEOUT_MS);
        if opts.wait_for > 60_000 || opts.wait_for > requested_ms / 2 {
            let message = "waitFor must not exceed half of timeout or 60000 ms";
            return Err(ApiError::bad_request(message));
        }
        let formats = opts
            .formats
            .clone()
            .unwrap_or_else(|| default_formats.to_vec());
        let request = ScrapeRequest {
            url: opts.url.clone().unwrap_or_default(),
            headers: opts.headers.clone().into_iter().collect(),
            deadline: Instant::now() + timeout,
            max_age: Duration::from_millis(opts.max_age.unwrap_or(self.default_max_age_ms)),
            render: RenderOptions {
                only_main_content: opts.only_main_content.unwrap_or(true),
                include_tags: opts.include_tags.clone(),
                exclude_tags: opts.exclude_tags.clone(),
                remove_base64_images: opts.remove_base64_images.unwrap_or(true),
                markdown: formats.contains(&Format::Markdown),
                html: formats.contains(&Format::Html),
                links: formats.contains(&Format::Links),
            },
            raw_html: formats.contains(&Format::RawHtml),
        };
        Ok((request, formats))
    }
}

pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/v1/scrape", post(scrape))
        .route("/v1/search", post(search))
        .route("/v1/web/scrape", post(web_scrape))
        .route("/v1/web/search", post(web_search))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(v1)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn(observe))
        .with_state(state)
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Format {
    Markdown,
    Html,
    RawHtml,
    Links,
}

/// Firecrawl v1 scrape options. Unknown fields, including the known-but-unimplemented
/// `actions`, `jsonOptions`, `extract`, and `location`, are refused by name.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ScrapeOptions {
    url: Option<String>,
    formats: Option<Vec<Format>>,
    only_main_content: Option<bool>,
    #[serde(default)]
    include_tags: Vec<String>,
    #[serde(default)]
    exclude_tags: Vec<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    timeout: Option<u64>,
    #[serde(default)]
    wait_for: u64,
    remove_base64_images: Option<bool>,
    max_age: Option<u64>,
    // Accepted no-ops: no browser, ad blocker, or proxy tier yet; captures are always stored.
    #[serde(rename = "mobile")]
    _mobile: Option<bool>,
    #[serde(rename = "blockAds")]
    _block_ads: Option<bool>,
    #[serde(rename = "proxy")]
    _proxy: Option<String>,
    #[serde(rename = "storeInCache")]
    _store_in_cache: Option<bool>,
    #[serde(rename = "origin")]
    _origin: Option<String>,
    #[serde(rename = "integration")]
    _integration: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SearchBody {
    query: String,
    limit: Option<usize>,
    lang: Option<String>,
    timeout: Option<u64>,
    #[serde(default)]
    scrape_options: ScrapeOptions,
    // Accepted no-ops: providers take no region, and invalid URLs are never returned.
    #[serde(rename = "country")]
    _country: Option<String>,
    #[serde(rename = "ignoreInvalidURLs")]
    _ignore_invalid_urls: Option<bool>,
    #[serde(rename = "origin")]
    _origin: Option<String>,
    #[serde(rename = "integration")]
    _integration: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WebScrapeBody {
    url: String,
    formats: Option<WebFormats>,
    #[serde(default)]
    shared_params: SharedParams,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebFormats {
    markdown: bool,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SharedParams {
    main_content_only: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WebSearchBody {
    query: String,
    num_results: Option<usize>,
    /// Accepted no-op: one query is always one search.
    #[serde(rename = "queryFanout")]
    _query_fanout: Option<bool>,
}

#[derive(Clone)]
struct RequestId(String);

/// A failed request in Firecrawl's error body: `{success:false, code, error}`.
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        let (status, code) = (StatusCode::BAD_REQUEST, "BAD_REQUEST");
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl From<ScrapeError> for ApiError {
    fn from(error: ScrapeError) -> Self {
        let (status, code) = scrape_failure(&error);
        tracing::info!(code, "scrape failed");
        Self {
            status,
            code,
            message: error.to_string(),
        }
    }
}

impl From<SearchError> for ApiError {
    fn from(error: SearchError) -> Self {
        let (status, code) = match error {
            SearchError::Timeout => (StatusCode::REQUEST_TIMEOUT, "SCRAPE_TIMEOUT"),
            SearchError::AllProvidersFailed => (StatusCode::INTERNAL_SERVER_ERROR, "SEARCH_FAILED"),
        };
        tracing::info!(code, "search failed");
        Self {
            status,
            code,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"success": false, "code": self.code, "error": self.message});
        (self.status, Json(body)).into_response()
    }
}

type Reply = Result<Json<Value>, ApiError>;

async fn scrape(State(state): State<AppState>, body: Result<Bytes, BytesRejection>) -> Reply {
    let opts: ScrapeOptions = parse(body)?;
    if opts.url.is_none() {
        return Err(ApiError::bad_request("missing field `url`"));
    }
    let (request, formats) = state.plan(&opts, &[Format::Markdown])?;
    let doc = state.scraper.scrape(&request).await?;
    let data = document_json(&doc, &formats, &request.url);
    Ok(Json(json!({"success": true, "data": data})))
}

async fn search(State(state): State<AppState>, body: Result<Bytes, BytesRejection>) -> Reply {
    let body: SearchBody = parse(body)?;
    if body.scrape_options.url.is_some() {
        return Err(ApiError::bad_request(
            "unknown field `url` in scrapeOptions",
        ));
    }
    let limit = body.limit.unwrap_or(5);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::bad_request("limit must be between 1 and 100"));
    }
    let deadline = Instant::now() + state.timeout(body.timeout, SEARCH_TIMEOUT_MS)?;
    let (template, formats) = state.plan(&body.scrape_options, &[])?;
    let lang = body.lang.as_deref().unwrap_or("en");
    let hits = state
        .searcher
        .search(&body.query, limit, lang, deadline)
        .await?;
    if hits.is_empty() {
        let warning = "No search results found";
        return Ok(Json(
            json!({"success": true, "data": [], "warning": warning}),
        ));
    }
    if formats.is_empty() {
        return Ok(Json(json!({"success": true, "data": hits})));
    }
    let docs: Vec<Map<String, Value>> = futures::stream::iter(hits)
        .map(|hit| {
            let mut request = template.clone();
            request.url = hit.url.clone();
            request.deadline = request.deadline.min(deadline);
            let (scraper, formats) = (&state.scraper, &formats);
            async move {
                let mut item = hit_json(&hit);
                match scraper.scrape(&request).await {
                    Ok(doc) => item.extend(document_json(&doc, formats, &hit.url)),
                    Err(error) => {
                        tracing::info!(code = scrape_failure(&error).1, "hit not scraped");
                    }
                }
                item
            }
        })
        .buffered(SEARCH_SCRAPE_CONCURRENCY)
        .collect()
        .await;
    let with_content: Vec<_> = docs
        .iter()
        .filter(|doc| has_content(doc))
        .cloned()
        .collect();
    if with_content.is_empty() {
        let warning = "No content found in search results";
        return Ok(Json(
            json!({"success": true, "data": docs, "warning": warning}),
        ));
    }
    Ok(Json(json!({"success": true, "data": with_content})))
}

/// context.dev scrape: a failed page is still HTTP 200, reported inside `markdown`.
async fn web_scrape(
    State(state): State<AppState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    body: Result<Bytes, BytesRejection>,
) -> Reply {
    let body: WebScrapeBody = parse(body)?;
    if body.formats.is_some_and(|formats| !formats.markdown) {
        return Err(ApiError::bad_request(
            "only the markdown format is supported",
        ));
    }
    let opts = ScrapeOptions {
        url: Some(body.url.clone()),
        only_main_content: body.shared_params.main_content_only,
        ..ScrapeOptions::default()
    };
    let (request, _) = state.plan(&opts, &[Format::Markdown])?;
    let (markdown, metadata, cache) = match state.scraper.scrape(&request).await {
        Ok(doc) => {
            let age_ms = doc.cached_at.map_or(0, |at| {
                (OffsetDateTime::now_utc() - at).whole_milliseconds().max(0)
            });
            let status = if doc.cached_at.is_some() {
                "hit"
            } else {
                "miss"
            };
            let data = doc.rendered.markdown.clone().unwrap_or_default();
            (
                json!({"data": data, "requested": true, "success": true}),
                metadata_json(&doc, &body.url),
                json!({"age_ms": age_ms, "status": status}),
            )
        }
        Err(ScrapeError::Invalid(message)) => return Err(ApiError::bad_request(message)),
        Err(error) => {
            let code = match &error {
                ScrapeError::Blocked => "BLOCKED",
                other => scrape_failure(other).1,
            };
            tracing::info!(code, "scrape failed");
            let markdown = json!({"data": null, "requested": true, "success": false,
                                  "code": code, "error": error.to_string()});
            (markdown, json!({}), json!({"age_ms": 0, "status": "miss"}))
        }
    };
    Ok(Json(json!({
        "url": body.url,
        "request_id": request_id,
        "markdown": markdown,
        "metadata": metadata,
        "cache_metadata": cache,
    })))
}

/// context.dev search; this engine never spends credits.
async fn web_search(
    State(state): State<AppState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    body: Result<Bytes, BytesRejection>,
) -> impl IntoResponse {
    let reply = async {
        let body: WebSearchBody = parse(body)?;
        let limit = body.num_results.unwrap_or(10);
        if !(1..=100).contains(&limit) {
            return Err(ApiError::bad_request(
                "numResults must be between 1 and 100",
            ));
        }
        let deadline = Instant::now() + state.timeout(None, SEARCH_TIMEOUT_MS)?;
        let hits = state
            .searcher
            .search(&body.query, limit, "en", deadline)
            .await?;
        Ok(Json(json!({
            "query": body.query,
            "request_id": request_id,
            "results": hits,
            "cache_metadata": {"age_ms": 0, "status": "miss"},
        })))
    };
    ([("x-credits-used", "0")], reply.await)
}

/// Firecrawl `data` for one document: requested formats plus metadata.
fn document_json(doc: &Document, formats: &[Format], source_url: &str) -> Map<String, Value> {
    let rendered = &doc.rendered;
    let mut data = Map::new();
    for format in formats {
        let (name, value) = match format {
            Format::Markdown => (
                "markdown",
                json!(rendered.markdown.clone().unwrap_or_default()),
            ),
            Format::Html => ("html", json!(rendered.html.clone().unwrap_or_default())),
            Format::RawHtml => ("rawHtml", json!(doc.raw_html.clone().unwrap_or_default())),
            Format::Links => ("links", json!(rendered.links.clone().unwrap_or_default())),
        };
        data.insert(name.into(), value);
    }
    data.insert("metadata".into(), metadata_json(doc, source_url));
    data
}

fn metadata_json(doc: &Document, source_url: &str) -> Value {
    let mut meta = match serde_json::to_value(&doc.rendered.metadata) {
        Ok(Value::Object(meta)) => meta,
        _ => Map::new(),
    };
    meta.retain(|_, value| !value.is_null());
    meta.insert("sourceURL".into(), json!(source_url));
    meta.insert("url".into(), json!(doc.url.as_str()));
    meta.insert("statusCode".into(), json!(doc.status));
    if let Some(content_type) = &doc.content_type {
        meta.insert("contentType".into(), json!(content_type));
    }
    meta.insert("proxyUsed".into(), json!(doc.route.proxy_used()));
    let cache_state = if doc.cached_at.is_some() {
        "hit"
    } else {
        "miss"
    };
    meta.insert("cacheState".into(), json!(cache_state));
    let cached_at = doc.cached_at.and_then(|at| {
        at.format(&time::format_description::well_known::Rfc3339)
            .ok()
    });
    if let Some(cached_at) = cached_at {
        meta.insert("cachedAt".into(), json!(cached_at));
    }
    Value::Object(meta)
}

fn hit_json(hit: &Hit) -> Map<String, Value> {
    match serde_json::to_value(hit) {
        Ok(Value::Object(item)) => item,
        _ => Map::new(),
    }
}

fn has_content(doc: &Map<String, Value>) -> bool {
    ["markdown", "html", "rawHtml"].iter().any(|key| {
        doc.get(*key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    })
}

/// The Firecrawl error table in docs/ARCHITECTURE.md.
fn scrape_failure(error: &ScrapeError) -> (StatusCode, &'static str) {
    const FAILED: StatusCode = StatusCode::INTERNAL_SERVER_ERROR;
    match error {
        ScrapeError::Invalid(_) => (StatusCode::BAD_REQUEST, "BAD_REQUEST"),
        ScrapeError::Blocked => (FAILED, "SCRAPE_ALL_ENGINES_FAILED"),
        ScrapeError::Unsupported(_) => (FAILED, "SCRAPE_UNSUPPORTED_FILE_ERROR"),
        ScrapeError::Fetch(error) => match error {
            FetchError::Refused(_) => (StatusCode::FORBIDDEN, "SCRAPE_SITE_ERROR"),
            FetchError::Dns => (StatusCode::OK, "SCRAPE_DNS_RESOLUTION_ERROR"),
            FetchError::Timeout => (StatusCode::REQUEST_TIMEOUT, "SCRAPE_TIMEOUT"),
            FetchError::Tls => (FAILED, "SCRAPE_SSL_ERROR"),
            FetchError::Overloaded => (StatusCode::REQUEST_TIMEOUT, "CONCURRENCY_QUEUE_TIMEOUT"),
            FetchError::Connect
            | FetchError::TooManyRedirects
            | FetchError::TooLarge
            | FetchError::Protocol => (FAILED, "SCRAPE_SITE_ERROR"),
        },
    }
}

/// Strict JSON: unknown fields and unknown formats are a 400 that names them.
fn parse<T: DeserializeOwned>(body: Result<Bytes, BytesRejection>) -> Result<T, ApiError> {
    let body = body.map_err(|rejection| ApiError {
        status: rejection.status(),
        code: "BAD_REQUEST",
        message: rejection.body_text(),
    })?;
    serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))
}

async fn authorize(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(key) = &state.api_key {
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), key.as_bytes()) {
            let (status, code) = (StatusCode::UNAUTHORIZED, "UNAUTHORIZED");
            let message = "Unauthorized: invalid token".into();
            return ApiError {
                status,
                code,
                message,
            }
            .into_response();
        }
    }
    next.run(request).await
}

/// Compares without an early exit on the first differing byte; length is not secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Assigns a request ID and logs route, status, and timing; never bodies, queries, or keys.
async fn observe(mut request: Request, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_owned();
    let method = request.method().clone();
    request
        .extensions_mut()
        .insert(RequestId(request_id.clone()));
    let started = std::time::Instant::now();
    let span = tracing::info_span!("request", request_id = %request_id);
    let mut response = next.run(request).instrument(span.clone()).await;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    span.in_scope(|| {
        tracing::info!(
            %method,
            route,
            status = response.status().as_u16(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "handled"
        );
    });
    response
}
