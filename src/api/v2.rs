//! Firecrawl v2 wire translation; collection, job ownership, and errors stay shared with v1.

use super::*;
use axum::{
    extract::{Path, Query, rejection::QueryRejection},
    http::HeaderMap,
};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/scrape", post(scrape))
        .route("/v2/search", post(search))
        .route("/v2/map", post(map))
        .route("/v2/crawl", post(crawl_start))
        .route(
            "/v2/crawl/{id}",
            get(crawl_status).delete(jobs::crawl_cancel),
        )
        .route("/v2/crawl/{id}/errors", get(jobs::crawl_errors))
        .route("/v2/batch/scrape", post(batch_start))
        .route(
            "/v2/batch/scrape/{id}",
            get(batch_status).delete(jobs::batch_cancel),
        )
        .route("/v2/batch/scrape/{id}/errors", get(jobs::batch_errors))
}

fn decode<T: DeserializeOwned>(value: Value, field: &str) -> Result<T, ApiError> {
    serde_json::from_value(value)
        .map_err(|error| ApiError::bad_request(format!("{field}: {error}")))
}

fn take<T: DeserializeOwned>(
    body: &mut Map<String, Value>,
    field: &str,
) -> Result<Option<T>, ApiError> {
    body.remove(field).map(|v| decode(v, field)).transpose()
}

fn reject(body: &Map<String, Value>, fields: &[&str]) -> Result<(), ApiError> {
    for field in fields {
        if body.contains_key(*field) {
            return Err(ApiError::bad_request(format!("{field} is not supported")));
        }
    }
    Ok(())
}

/// String or object variants use one strict object validator, preserving field names in errors.
fn variant(value: Value, field: &str) -> Result<(String, Map<String, Value>), ApiError> {
    match value {
        Value::String(name) => Ok((name, Map::new())),
        Value::Object(mut object) => {
            let name = take(&mut object, "type")?
                .ok_or_else(|| ApiError::bad_request(format!("{field}.type is required")))?;
            Ok((name, object))
        }
        _ => Err(ApiError::bad_request(format!(
            "{field} must be a string or object"
        ))),
    }
}

fn no_fields(object: &Map<String, Value>, field: &str) -> Result<(), ApiError> {
    if let Some(key) = object.keys().next() {
        return Err(ApiError::bad_request(format!(
            "{field}.{key} is not supported"
        )));
    }
    Ok(())
}

/// Translate only supported v2 differences; the v1 schema remains the shared option validator.
fn scrape_options(
    body: &mut Map<String, Value>,
) -> Result<(ScrapeOptions, Option<Shot>), ApiError> {
    reject(body, &["parsePDF", "jsonOptions"])?;
    let mut shot = None;
    if let Some(formats) = take::<Vec<Value>>(body, "formats")? {
        let mut translated = Vec::new();
        for format in formats {
            let (name, mut options) = variant(format, "formats")?;
            let format = match name.as_str() {
                "markdown" | "html" | "rawHtml" | "links" => name,
                "json" => {
                    if body.contains_key("jsonOptions") {
                        return Err(ApiError::bad_request(
                            "formats: only one json format is allowed",
                        ));
                    }
                    let schema: Option<Value> = take(&mut options, "schema")?;
                    let prompt: Option<String> = take(&mut options, "prompt")?;
                    no_fields(&options, "formats.json")?;
                    let mut json_options = Map::new();
                    if let Some(schema) = schema {
                        json_options.insert("schema".into(), schema);
                    }
                    if let Some(prompt) = prompt {
                        json_options.insert("prompt".into(), json!(prompt));
                    }
                    body.insert("jsonOptions".into(), Value::Object(json_options));
                    name
                }
                "summary" => name,
                "screenshot" => {
                    if shot.is_some() {
                        return Err(ApiError::bad_request(
                            "formats: only one screenshot is allowed",
                        ));
                    }
                    let full_page = take(&mut options, "fullPage")?.unwrap_or(false);
                    let quality: Option<u8> = take(&mut options, "quality")?;
                    if quality.is_some_and(|quality| !(1..=100).contains(&quality)) {
                        return Err(ApiError::bad_request(
                            "formats.screenshot.quality must be between 1 and 100",
                        ));
                    }
                    // The browser owns a fixed 1280x800 viewport; do not pretend to resize it.
                    no_fields(&options, "formats.screenshot")?;
                    shot = Some(Shot { full_page, quality });
                    if full_page {
                        "screenshot@fullPage"
                    } else {
                        "screenshot"
                    }
                    .into()
                }
                _ => {
                    return Err(ApiError::bad_request(format!(
                        "formats.{name} is not supported"
                    )));
                }
            };
            no_fields(&options, "formats")?;
            translated.push(format);
        }
        body.insert("formats".into(), json!(translated));
    }
    if let Some(parsers) = take::<Vec<Value>>(body, "parsers")? {
        let mut pdf = false;
        for parser in parsers {
            let (name, options) = variant(parser, "parsers")?;
            if name != "pdf" {
                return Err(ApiError::bad_request(format!(
                    "parsers.{name} is not supported"
                )));
            }
            no_fields(&options, "parsers.pdf")?;
            pdf = true;
        }
        body.insert("parsePDF".into(), json!(pdf));
    }
    let opts = decode(Value::Object(body.clone()), "scrapeOptions")?;
    Ok((opts, shot))
}

async fn scrape_document(
    state: &AppState,
    opts: &ScrapeOptions,
    shot: Option<Shot>,
    url: &str,
    deadline: Option<Instant>,
) -> Result<Map<String, Value>, ApiError> {
    let (mut request, formats) = state.plan(opts, &[Format::Markdown])?;
    request.url = url.into();
    if let Some(deadline) = deadline {
        request.deadline = request.deadline.min(deadline);
    }
    let mut plan = browser_plan(opts, &formats)?;
    plan.screenshot = shot;
    let doc = state.scraper.scrape_with(&request, &plan).await?;
    state
        .extracted_document(&doc, &formats, &request.url, opts)
        .await
}

async fn scrape(State(state): State<AppState>, body: Result<Bytes, BytesRejection>) -> Reply {
    let mut body = parse(body)?;
    let (opts, shot) = scrape_options(&mut body)?;
    let url = opts
        .url
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("url is required"))?;
    let data = scrape_document(&state, &opts, shot, url, None).await?;
    Ok(Json(json!({"success": true, "data": data})))
}

fn sitemap(body: &mut Map<String, Value>) -> Result<String, ApiError> {
    let mode = take::<String>(body, "sitemap")?.unwrap_or_else(|| "include".into());
    if !matches!(mode.as_str(), "skip" | "include" | "only") {
        return Err(ApiError::bad_request(
            "sitemap must be skip, include, or only",
        ));
    }
    body.insert("ignoreSitemap".into(), json!(mode == "skip"));
    Ok(mode)
}

fn wire(body: Map<String, Value>) -> Result<Bytes, BytesRejection> {
    // A JSON value contains no fallible serializers.
    Ok(Bytes::from(Value::Object(body).to_string()))
}

async fn map(State(state): State<AppState>, body: Result<Bytes, BytesRejection>) -> Reply {
    let mut body = parse(body)?;
    reject(&body, &["ignoreSitemap", "sitemapOnly"])?;
    let mode = sitemap(&mut body)?;
    body.insert("sitemapOnly".into(), json!(mode == "only"));
    let Json(mut response) = jobs::map(State(state), wire(body)).await?;
    if let Some(Value::Array(links)) = response.get_mut("links") {
        for link in links {
            *link = json!({"url": link});
        }
    }
    Ok(Json(response))
}

fn job_options(body: &mut Map<String, Value>) -> Result<(), ApiError> {
    let (opts, shot) = scrape_options(body)?;
    if shot.is_some() {
        return Err(ApiError::bad_request(
            "formats.screenshot is not supported for jobs",
        ));
    }
    if !opts.actions.is_empty() {
        return Err(ApiError::bad_request("actions is not supported for jobs"));
    }
    if opts.wait_for != 0 {
        return Err(ApiError::bad_request("waitFor is not supported for jobs"));
    }
    Ok(())
}

async fn crawl_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Reply {
    let mut body = parse(body)?;
    reject(
        &body,
        &[
            "ignoreSitemap",
            "sitemapOnly",
            "maxDepth",
            "allowBackwardLinks",
            "regexOnFullURL",
        ],
    )?;
    let mode = sitemap(&mut body)?;
    // Sitemap pages enter at hop zero; only page-to-page discovery increments this counter.
    if mode == "only" {
        let _ = take::<u32>(&mut body, "maxDiscoveryDepth")?;
        body.insert("maxDiscoveryDepth".into(), json!(0));
    }
    body.insert("maxDepth".into(), json!(100));
    let mut opts = take::<Map<String, Value>>(&mut body, "scrapeOptions")?.unwrap_or_default();
    job_options(&mut opts)?;
    body.insert("scrapeOptions".into(), Value::Object(opts));
    versioned(jobs::crawl_start(State(state), headers, wire(body)).await?)
}

async fn batch_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Reply {
    let mut body: Map<String, Value> = parse(body)?;
    let mut job = Map::new();
    for field in [
        "urls",
        "ignoreInvalidURLs",
        "maxConcurrency",
        "deduplicateSimilarURLs",
    ] {
        if let Some(value) = body.remove(field) {
            job.insert(field.into(), value);
        }
    }
    job_options(&mut body)?;
    body.extend(job);
    versioned(jobs::batch_start(State(state), headers, wire(body)).await?)
}

/// Public URLs and pagination stay on v2, including a configured base URL with a path prefix.
fn versioned(Json(mut response): Json<Value>) -> Reply {
    for field in ["url", "next"] {
        if let Some(url) = response.get(field).and_then(Value::as_str)
            && let Some((base, path)) = url.rsplit_once("/v1/")
        {
            response[field] = json!(format!("{base}/v2/{path}"));
        }
    }
    Ok(Json(response))
}

async fn crawl_status(
    state: State<AppState>,
    headers: HeaderMap,
    id: Path<String>,
    query: Result<Query<jobs::Pagination>, QueryRejection>,
) -> Reply {
    let query = query.map_err(|error| ApiError::bad_request(error.body_text()))?;
    versioned(jobs::crawl_status(state, headers, id, query).await?)
}

async fn batch_status(
    state: State<AppState>,
    headers: HeaderMap,
    id: Path<String>,
    query: Result<Query<jobs::Pagination>, QueryRejection>,
) -> Reply {
    let query = query.map_err(|error| ApiError::bad_request(error.body_text()))?;
    versioned(jobs::batch_status(state, headers, id, query).await?)
}

/// Domain restrictions apply before scraping, matching exact hosts or their subdomains.
fn domains(body: &mut Map<String, Value>, field: &str) -> Result<Vec<String>, ApiError> {
    let domains = take::<Vec<String>>(body, field)?.unwrap_or_default();
    domains
        .into_iter()
        .map(|domain| {
            let domain = domain.trim().to_ascii_lowercase();
            if domain.len() > 253
                || !domain.contains('.')
                || domain.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
            {
                return Err(ApiError::bad_request(format!(
                    "{field} must contain hostnames without protocol or path"
                )));
            }
            Ok(domain)
        })
        .collect()
}

fn matches_domain(host: &str, domains: &[String]) -> bool {
    domains.iter().any(|domain| {
        host == domain
            || host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

async fn search(State(state): State<AppState>, body: Result<Bytes, BytesRejection>) -> Reply {
    let mut body: Map<String, Value> = parse(body)?;
    reject(&body, &["tbs", "location", "filter"])?;
    if let Some(sources) = take::<Vec<Value>>(&mut body, "sources")? {
        if sources.is_empty() {
            return Err(ApiError::bad_request("sources must contain web"));
        }
        for source in sources {
            let (name, options) = variant(source, "sources")?;
            if name != "web" {
                return Err(ApiError::bad_request(format!(
                    "sources.{name} is not supported"
                )));
            }
            no_fields(&options, "sources.web")?;
        }
    }
    let include = domains(&mut body, "includeDomains")?;
    let exclude = domains(&mut body, "excludeDomains")?;
    if !include.is_empty() && !exclude.is_empty() {
        return Err(ApiError::bad_request(
            "includeDomains and excludeDomains cannot both be specified",
        ));
    }
    let mut options = take::<Map<String, Value>>(&mut body, "scrapeOptions")?.unwrap_or_default();
    let (mut opts, shot) = scrape_options(&mut options)?;
    if opts.url.is_some() {
        return Err(ApiError::bad_request("scrapeOptions.url is not supported"));
    }
    // Validate the scrape plan before issuing the search, even if no hits are returned.
    let (_, formats) = state.plan(&opts, &[])?;
    let _ = browser_plan(&opts, &formats)?;
    let timeout = body
        .get("timeout")
        .cloned()
        .map(|value| decode(value, "timeout"))
        .transpose()?;
    let deadline = Instant::now() + state.timeout(timeout, SEARCH_TIMEOUT_MS)?;
    body.entry("limit").or_insert(json!(10));
    let Json(response) = super::search(State(state.clone()), wire(body)).await?;
    let hits = response["data"].as_array().cloned().unwrap_or_default();
    let hits: Vec<_> = hits
        .into_iter()
        .filter(|hit| {
            hit["url"]
                .as_str()
                .and_then(|url| url::Url::parse(url).ok())
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
                .is_some_and(|host| {
                    (include.is_empty() || matches_domain(&host, &include))
                        && !matches_domain(&host, &exclude)
                })
        })
        .collect();
    opts.formats = Some(formats.clone());
    let data: Vec<_> = futures::stream::iter(hits.into_iter().enumerate())
        .map(|(index, hit)| {
            let opts = &opts;
            let state = &state;
            let scrape = !formats.is_empty();
            async move {
                let mut item = hit.as_object().cloned().unwrap_or_default();
                item.insert("position".into(), json!(index + 1));
                if scrape && let Some(url) = hit["url"].as_str() {
                    match scrape_document(state, opts, shot, url, Some(deadline)).await {
                        Ok(doc) => item.extend(doc),
                        Err(error) => {
                            item.insert(
                                "warning".into(),
                                json!(format!("Scrape failed: {}", error.code)),
                            );
                        }
                    }
                }
                item
            }
        })
        .buffered(SEARCH_SCRAPE_CONCURRENCY)
        .collect()
        .await;
    let mut result = json!({"success": true, "data": {"web": data}});
    if result["data"]["web"].as_array().is_some_and(Vec::is_empty) {
        result["warning"] = json!("No search results found");
    }
    Ok(Json(result))
}
