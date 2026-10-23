//! Firecrawl v1 job wire shapes. Durable storage keeps domain documents, not API JSON.

use super::*;
use crate::{
    crawl::{self, MapRequest, Scope, ScopeOptions},
    jobs::{JobError, JobId, JobKind, JobSpec, Jobs, Seed, Task},
    scrape::ScrapeTemplate,
};
use axum::{
    extract::{Path, Query},
    http::HeaderMap,
};
use serde::de::DeserializeOwned;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/map", post(map))
        .route("/v1/crawl", post(crawl_start))
        .route("/v1/crawl/{id}", get(crawl_status).delete(crawl_cancel))
        .route("/v1/crawl/{id}/errors", get(crawl_errors))
        .route("/v1/batch/scrape", post(batch_start))
        .route(
            "/v1/batch/scrape/{id}",
            get(batch_status).delete(batch_cancel),
        )
        .route("/v1/batch/scrape/{id}/errors", get(batch_errors))
}

impl From<JobError> for ApiError {
    fn from(error: JobError) -> Self {
        let (status, code) = match &error {
            JobError::Invalid(_) => (StatusCode::BAD_REQUEST, "BAD_REQUEST"),
            JobError::Overloaded => (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMIT_EXCEEDED"),
            JobError::InsufficientStorage => {
                (StatusCode::INSUFFICIENT_STORAGE, "INSUFFICIENT_STORAGE")
            }
            JobError::Stopped => (StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "JOB_ERROR"),
        };
        // Database paths and diagnostics stay in server logs, not the HTTP response.
        let message = if matches!(error, JobError::Store(_) | JobError::Locked) {
            tracing::error!(%error, "job request failed");
            "Job storage unavailable".into()
        } else {
            error.to_string()
        };
        Self {
            status,
            code,
            message,
        }
    }
}

fn jobs(state: &AppState) -> Result<&Jobs, ApiError> {
    state.jobs.as_ref().ok_or_else(|| JobError::Stopped.into())
}
fn not_found() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "NOT_FOUND",
        message: "Job not found".into(),
    }
}

fn reject_extraction(formats: &[Format]) -> Result<(), ApiError> {
    if formats.contains(&Format::Json) || formats.contains(&Format::Summary) {
        return Err(ApiError::bad_request(
            "json/summary formats are not supported for jobs",
        ));
    }
    Ok(())
}
fn id(value: &str) -> Result<JobId, ApiError> {
    JobId::parse_str(value).map_err(|_| not_found())
}

fn public_url(state: &AppState, headers: &HeaderMap, path: &str) -> Result<String, ApiError> {
    if let Some(base) = &state.job_config.public_url {
        if !matches!(base.scheme(), "http" | "https")
            || !base.has_host()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ApiError::bad_request(
                "SONAR_PUBLIC_URL must be an http(s) URL without credentials",
            ));
        }
        return Ok(format!("{}{path}", base.as_str().trim_end_matches('/')));
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| ApiError::bad_request("Host header is required"))?;
    let base = url::Url::parse(&format!("http://{host}"))
        .map_err(|_| ApiError::bad_request("invalid Host header"))?;
    if base.path() != "/"
        || base.query().is_some()
        || base.fragment().is_some()
        || !base.has_host()
        || !base.username().is_empty()
        || base.password().is_some()
    {
        return Err(ApiError::bad_request("invalid Host header"));
    }
    Ok(format!("{}{path}", base.as_str().trim_end_matches('/')))
}

pub(super) async fn map(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    let req: MapRequest = parse(body)?;
    if req.limit > state.job_config.max_crawl_limit as usize {
        return Err(ApiError::bad_request("limit exceeds SONAR_MAX_CRAWL_LIMIT"));
    }
    let deadline = Instant::now() + Duration::from_millis(state.job_config.map_timeout_ms);
    let links = tokio::time::timeout_at(
        deadline,
        crawl::map(&state.fetcher, &state.scraper, &req, deadline),
    )
    .await
    .map_err(|_| ScrapeError::Fetch(FetchError::Timeout))??;
    Ok(Json(json!({"success": true, "links": links})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CrawlBody {
    url: String,
    limit: Option<u32>,
    max_depth: Option<u32>,
    max_discovery_depth: Option<u32>,
    #[serde(default)]
    include_paths: Vec<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    #[serde(default)]
    allow_backward_links: bool,
    #[serde(default)]
    crawl_entire_domain: bool,
    #[serde(default)]
    allow_external_links: bool,
    #[serde(default)]
    allow_subdomains: bool,
    #[serde(default)]
    ignore_sitemap: bool,
    #[serde(default)]
    ignore_query_parameters: bool,
    delay: Option<f64>,
    #[serde(default)]
    scrape_options: ScrapeOptions,
    #[serde(rename = "maxConcurrency")]
    _max_concurrency: Option<u32>,
    #[serde(rename = "deduplicateSimilarURLs")]
    _deduplicate_similar_urls: Option<bool>,
    #[serde(default)]
    regex_on_full_url: bool,
    #[serde(rename = "origin")]
    _origin: Option<String>,
    #[serde(rename = "integration")]
    _integration: Option<String>,
}

pub(super) async fn crawl_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    let body: CrawlBody = parse(body)?;
    if body.regex_on_full_url {
        return Err(ApiError::bad_request("regexOnFullURL is not supported"));
    }
    if body.scrape_options.url.is_some() {
        return Err(ApiError::bad_request("scrapeOptions.url is not supported"));
    }
    let seed = scrape::normalize(&body.url)?;
    let scope = Scope::new(
        &seed,
        ScopeOptions {
            include_paths: body.include_paths,
            exclude_paths: body.exclude_paths,
            max_depth: body.max_depth.unwrap_or(10),
            max_discovery_hops: body.max_discovery_depth.unwrap_or(u32::MAX),
            allow_backward_links: body.allow_backward_links,
            crawl_entire_domain: body.crawl_entire_domain,
            allow_external_links: body.allow_external_links,
            allow_subdomains: body.allow_subdomains,
            ignore_query_parameters: body.ignore_query_parameters,
        },
    )
    .map_err(ApiError::bad_request)?;
    let delay_ms = match body.delay {
        Some(delay) if delay.is_finite() && (0.0..=60.0).contains(&delay) => {
            Some((delay * 1000.0).ceil() as u64)
        }
        Some(_) => {
            return Err(ApiError::bad_request(
                "delay must be between 0 and 60 seconds",
            ));
        }
        None => None,
    };
    let (request, formats) = state.plan(&body.scrape_options, &[Format::Markdown])?;
    reject_extraction(&formats)?;
    let spec = JobSpec {
        kind: JobKind::Crawl,
        scrape: ScrapeTemplate::from(&request),
        scope: Some(scope),
        limit: body.limit.unwrap_or(10_000),
        delay_ms,
        seed_origin: Some(seed.origin().ascii_serialization()),
    };
    let mut seeds = vec![Seed {
        url: seed.clone(),
        task: Task::Page,
    }];
    if !body.ignore_sitemap {
        let mut root = seed;
        root.set_query(None);
        root.set_fragment(None);
        root.set_path("/robots.txt");
        seeds.push(Seed {
            url: root.clone(),
            task: Task::Robots,
        });
        root.set_path("/sitemap.xml");
        seeds.push(Seed {
            url: root,
            task: Task::Sitemap,
        });
    }
    // Validate the response URL before persisting a job.
    let base = public_url(&state, &headers, "/v1/crawl")?;
    let id = jobs(&state)?
        .submit(spec, json!({"formats": formats}), seeds)
        .await?;
    Ok(Json(
        json!({"success":true,"id":id.to_string(),"url":format!("{base}/{id}")}),
    ))
}

fn take<T: DeserializeOwned>(
    body: &mut Map<String, Value>,
    name: &str,
) -> Result<Option<T>, ApiError> {
    body.remove(name)
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| ApiError::bad_request(format!("{name}: {e}")))
}

pub(super) async fn batch_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    let mut body: Map<String, Value> = parse(body)?;
    let urls: Vec<String> =
        take(&mut body, "urls")?.ok_or_else(|| ApiError::bad_request("urls is required"))?;
    let ignore_invalid = take::<bool>(&mut body, "ignoreInvalidURLs")?.unwrap_or(false);
    let _ = take::<u32>(&mut body, "maxConcurrency")?;
    let _ = take::<bool>(&mut body, "deduplicateSimilarURLs")?;
    let opts: ScrapeOptions = serde_json::from_value(Value::Object(body))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if opts.url.is_some() {
        return Err(ApiError::bad_request("use urls, not url, for batch scrape"));
    }
    if urls.is_empty() || urls.len() > state.job_config.max_crawl_limit as usize {
        return Err(ApiError::bad_request(
            "urls must be nonempty and within SONAR_MAX_CRAWL_LIMIT",
        ));
    }
    let (request, formats) = state.plan(&opts, &[Format::Markdown])?;
    reject_extraction(&formats)?;
    let mut seeds = Vec::new();
    let mut invalid = Vec::new();
    for input in urls {
        match scrape::normalize(&input) {
            Ok(url)
                if matches!(url.scheme(), "http" | "https")
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none() =>
            {
                seeds.push(Seed {
                    url,
                    task: Task::Page,
                })
            }
            _ if ignore_invalid => invalid.push(input),
            _ => return Err(ApiError::bad_request("invalid URL in urls")),
        }
    }
    if seeds.is_empty() {
        return Err(ApiError::bad_request("no valid URLs"));
    }
    let spec = JobSpec {
        kind: JobKind::Batch,
        scrape: ScrapeTemplate::from(&request),
        scope: None,
        limit: seeds.len() as u32,
        delay_ms: None,
        seed_origin: None,
    };
    let base = public_url(&state, &headers, "/v1/batch/scrape")?;
    let id = jobs(&state)?
        .submit(spec, json!({"formats": formats}), seeds)
        .await?;
    Ok(Json(
        json!({"success":true,"id":id.to_string(),"url":format!("{base}/{id}"),"invalidURLs":invalid}),
    ))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pagination {
    skip: Option<u64>,
    limit: Option<usize>,
}

async fn status(
    state: AppState,
    headers: HeaderMap,
    raw_id: String,
    query: Pagination,
    kind: JobKind,
) -> Result<Json<Value>, ApiError> {
    let id = id(&raw_id)?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::bad_request("limit must be between 1 and 1000"));
    }
    let page = jobs(&state)?
        .status(id, kind, query.skip.unwrap_or(0), limit, 10 * 1024 * 1024)
        .await?
        .ok_or_else(not_found)?;
    let formats: Vec<Format> = serde_json::from_value(
        page.request
            .get("formats")
            .cloned()
            .unwrap_or_else(|| json!(["markdown"])),
    )
    .map_err(|_| ApiError::from(JobError::Store(StoreError::Corrupt)))?;
    let mut data = Vec::new();
    let mut bytes = 4096; // Reserve the response envelope; always allow at least one document.
    for (url, doc) in &page.docs {
        let wire = document_json(doc, &formats, url);
        let size = serde_json::to_vec(&wire)
            .map_err(|_| ApiError::from(JobError::Store(StoreError::Corrupt)))?
            .len();
        if !data.is_empty() && bytes + size > 10 * 1024 * 1024 {
            break;
        }
        bytes += size;
        data.push(wire);
    }
    let next_skip = if data.len() < page.docs.len() {
        Some(query.skip.unwrap_or(0).saturating_add(data.len() as u64))
    } else {
        page.next_skip
    };
    let expires = page
        .expires_at
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| ApiError::from(JobError::Store(StoreError::Corrupt)))?;
    let mut result = json!({"success":true,"status":page.status,"completed":page.completed,"total":page.total,"creditsUsed":0,"expiresAt":expires,"data":data});
    if let Some(skip) = next_skip {
        let path = match kind {
            JobKind::Crawl => format!("/v1/crawl/{id}"),
            JobKind::Batch => format!("/v1/batch/scrape/{id}"),
        };
        result["next"] = json!(format!(
            "{}?skip={skip}&limit={limit}",
            public_url(&state, &headers, &path)?
        ));
    }
    Ok(Json(result))
}

pub(super) async fn crawl_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<Pagination>,
) -> Result<Json<Value>, ApiError> {
    status(state, headers, id, query, JobKind::Crawl).await
}
pub(super) async fn batch_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<Pagination>,
) -> Result<Json<Value>, ApiError> {
    status(state, headers, id, query, JobKind::Batch).await
}
async fn cancel(state: AppState, raw_id: String, kind: JobKind) -> Result<Response, ApiError> {
    match jobs(&state)?.cancel(id(&raw_id)?, kind).await? {
        None => Err(not_found()),
        Some(false) => Ok((
            StatusCode::CONFLICT,
            Json(json!({"error":"Crawl is already completed"})),
        )
            .into_response()),
        Some(true) => Ok(Json(json!({"status":"cancelled"})).into_response()),
    }
}
pub(super) async fn crawl_cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    cancel(state, id, JobKind::Crawl).await
}
pub(super) async fn batch_cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    cancel(state, id, JobKind::Batch).await
}
async fn errors(state: AppState, raw_id: String, kind: JobKind) -> Result<Json<Value>, ApiError> {
    let errors = jobs(&state)?
        .errors(id(&raw_id)?, kind)
        .await?
        .ok_or_else(not_found)?;
    let errors = errors
        .into_iter()
        .map(|e| {
            e.at.format(&time::format_description::well_known::Rfc3339)
                .map(|at| json!({"id":e.id,"timestamp":at,"url":e.url,"error":e.code}))
                .map_err(|_| ApiError::from(JobError::Store(StoreError::Corrupt)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({"errors":errors,"robotsBlocked":[]})))
}

pub(super) async fn crawl_errors(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    errors(state, id, JobKind::Crawl).await
}

pub(super) async fn batch_errors(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    errors(state, id, JobKind::Batch).await
}
