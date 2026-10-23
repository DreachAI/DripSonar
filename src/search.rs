//! Search providers: SearXNG, then DuckDuckGo, Mojeek, and Brave HTML.

use std::{collections::HashSet, future::Future, sync::Arc, time::Duration};

use crate::{
    config::Config,
    net::{FetchError, Fetched, Fetcher, Target},
};
use dom_query::Document;
use tokio::time::{Instant, timeout_at};
use url::Url;

/// Largest search response read, from any provider.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Same-origin redirects followed for the operator's own SearXNG.
const TRUSTED_MAX_REDIRECTS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Hit {
    pub url: String,
    pub title: String,
    pub description: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("all search providers failed")]
    AllProvidersFailed,
    #[error("deadline exceeded")]
    Timeout,
}

pub fn parse_searxng(body: &[u8]) -> Result<Vec<Hit>, SearchError> {
    searxng_page(body).map(|(hits, _)| hits)
}

fn searxng_page(body: &[u8]) -> Result<(Vec<Hit>, bool), SearchError> {
    #[derive(serde::Deserialize)]
    struct Page {
        results: Vec<serde_json::Value>,
    }
    let page: Page = serde_json::from_slice(body).map_err(|_| SearchError::AllProvidersFailed)?;
    let has_results = !page.results.is_empty();
    let hits = page.results.iter().filter_map(|result| {
        Some(Hit {
            url: result.get("url")?.as_str()?.to_owned(),
            title: result
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            description: result
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
        })
    });
    Ok((normalize(hits), has_results))
}

pub fn parse_duckduckgo(html: &str) -> Vec<Hit> {
    let document = Document::from(html);
    duckduckgo_hits(&document)
}

fn duckduckgo_hits(document: &Document) -> Vec<Hit> {
    let hits = document.select("a.result__a").iter().filter_map(|anchor| {
        let ancestors = anchor.ancestors(None);
        if anchor.has_class("result--ad") || ancestors.is(".result--ad") {
            return None;
        }
        let href = anchor.attr("href")?;
        let href = if href.starts_with("//") {
            format!("https:{href}")
        } else {
            href.to_string()
        };
        let mut url = Url::parse(&href).ok()?;
        if matches!(
            url.host_str(),
            Some("duckduckgo.com" | "www.duckduckgo.com")
        ) {
            if url.path() == "/y.js" {
                return None;
            }
            if url.path() == "/l/" {
                let destination = url
                    .query_pairs()
                    .find(|(key, _)| key == "uddg")?
                    .1
                    .into_owned();
                url = Url::parse(&destination).ok()?;
            }
        }
        Some(Hit {
            url: url.to_string(),
            title: anchor.text().to_string(),
            description: ancestors
                .filter(".result")
                .first()
                .select(".result__snippet")
                .text()
                .to_string(),
        })
    });
    normalize(hits)
}

fn duckduckgo_page(html: &str) -> Result<Vec<Hit>, SearchError> {
    let document = Document::from(html);
    if html.contains("anomaly-modal")
        || document.select("form[action*='/anomaly.js']").exists()
        || !document
            .select("a.result__a, .no-results, .result--no-result")
            .exists()
    {
        return Err(SearchError::AllProvidersFailed);
    }
    Ok(duckduckgo_hits(&document))
}

// Mojeek: markers are best-effort and checked against its markup of 2026-10.
const MOJEEK_URL: &str = "https://www.mojeek.com/search";
const MOJEEK_ITEM: &str = "ul.results-standard > li";
const MOJEEK_LINK: &str = "a.title";
const MOJEEK_TITLE: &str = "a.title";
const MOJEEK_SNIPPET: &str = "p.s";
const MOJEEK_EMPTY: &str = ".no-results, .noresults";

// Brave: markers are best-effort and checked against its markup of 2026-10.
const BRAVE_URL: &str = "https://search.brave.com/search";
const BRAVE_ITEM: &str = "div.snippet[data-type='web']";
const BRAVE_LINK: &str = "a[href^='http']";
const BRAVE_TITLE: &str = ".search-snippet-title";
const BRAVE_SNIPPET: &str = ".generic-snippet .content";
const BRAVE_EMPTY: &str = ".no-results, #no-results";

/// CSS markers of one HTML results page.
struct Markers {
    item: &'static str,
    link: &'static str,
    title: &'static str,
    snippet: &'static str,
    empty: &'static str,
}

const MOJEEK: Markers = Markers {
    item: MOJEEK_ITEM,
    link: MOJEEK_LINK,
    title: MOJEEK_TITLE,
    snippet: MOJEEK_SNIPPET,
    empty: MOJEEK_EMPTY,
};
const BRAVE: Markers = Markers {
    item: BRAVE_ITEM,
    link: BRAVE_LINK,
    title: BRAVE_TITLE,
    snippet: BRAVE_SNIPPET,
    empty: BRAVE_EMPTY,
};

/// Hits of a results page. A page with neither results nor the engine's no-results marker is a
/// block, captcha, or layout change, and fails instead of reading as zero results.
fn marked_page(html: &str, m: &Markers) -> Result<Vec<Hit>, SearchError> {
    let document = Document::from(html);
    if !document.select(m.item).exists() && !document.select(m.empty).exists() {
        return Err(SearchError::AllProvidersFailed);
    }
    let hits = document.select(m.item).iter().filter_map(|item| {
        Some(Hit {
            url: item.select(m.link).first().attr("href")?.to_string(),
            title: item.select(m.title).first().text().to_string(),
            description: item.select(m.snippet).first().text().to_string(),
        })
    });
    Ok(normalize(hits))
}

fn normalize(hits: impl IntoIterator<Item = Hit>) -> Vec<Hit> {
    let mut seen = HashSet::new();
    hits.into_iter()
        .filter_map(|mut hit| {
            let mut url = Url::parse(&hit.url).ok()?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return None;
            }
            url.set_fragment(None);
            let path = url.path().trim_end_matches('/').to_owned();
            url.set_path(&path);
            hit.url = url.to_string();
            if !seen.insert(hit.url.clone()) {
                return None;
            }
            hit.title = clean_text(&hit.title, 500);
            hit.description = clean_text(&hit.description, 1000);
            Some(hit)
        })
        .collect()
}

fn clean_text(text: &str, cap: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(cap)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

#[derive(Clone)]
pub struct Searcher {
    fetcher: Fetcher,
    /// The operator's SearXNG and the unguarded client that reaches it.
    searxng: Option<(Url, Arc<reqwest::Client>)>,
}

impl Searcher {
    pub fn new(fetcher: Fetcher, cfg: &Config) -> Self {
        let searxng = cfg.searxng_endpoint.clone().and_then(|endpoint| {
            match trusted_client(&endpoint, &cfg.user_agent) {
                Ok(client) => Some((endpoint, Arc::new(client))),
                Err(error) => {
                    tracing::error!(%error, "searxng disabled: client not built");
                    None
                }
            }
        });
        Self { fetcher, searxng }
    }

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        lang: &str,
        deadline: Instant,
    ) -> Result<Vec<Hit>, SearchError> {
        let searxng = self.searxng.as_ref();
        run_chain(
            searxng.map(|(endpoint, _)| endpoint),
            query,
            limit,
            lang,
            deadline,
            |target| async move {
                match searxng {
                    Some((endpoint, client)) if target.url.origin() == endpoint.origin() => {
                        fetch_trusted(client, &target).await
                    }
                    _ => self.fetcher.fetch(&target).await,
                }
            },
        )
        .await
    }
}

/// A client for the operator-configured SearXNG origin only. SearXNG usually runs on a loopback
/// or private address, which the guarded fetcher refuses. Redirects stay on that origin.
fn trusted_client(endpoint: &Url, user_agent: &str) -> Result<reqwest::Client, reqwest::Error> {
    let origin = endpoint.origin();
    reqwest::Client::builder()
        .user_agent(user_agent)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() <= TRUSTED_MAX_REDIRECTS && attempt.url().origin() == origin
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
}

/// One exchange with the trusted client, bounded by the target's deadline and byte cap.
async fn fetch_trusted(client: &reqwest::Client, target: &Target) -> Result<Fetched, FetchError> {
    let remaining = target
        .deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    let exchange = async {
        let mut response = client
            .get(target.url.clone())
            .timeout(remaining)
            .send()
            .await
            .map_err(trusted_error)?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(trusted_error)? {
            if body.len() + chunk.len() > target.max_bytes {
                return Err(FetchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Fetched {
            requested_url: target.url.clone(),
            final_url: response.url().clone(),
            status: response.status().as_u16(),
            headers: Vec::new(),
            content_type: response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            body: body.into(),
            fetched_at: time::OffsetDateTime::now_utc(),
        })
    };
    timeout_at(target.deadline, exchange)
        .await
        .unwrap_or(Err(FetchError::Timeout))
}

fn trusted_error(error: reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Connect
    }
}

#[derive(Clone, Copy)]
enum Provider<'a> {
    Searxng(&'a Url),
    DuckDuckGo,
    Mojeek,
    Brave,
}

impl Provider<'_> {
    /// Result pages requested: only SearXNG pages through its JSON.
    fn pages(self) -> u32 {
        if matches!(self, Self::Searxng(_)) {
            5
        } else {
            1
        }
    }

    fn url(self, query: &str, lang: &str, page: u32) -> Result<Url, SearchError> {
        let mut url = match self {
            Self::Searxng(endpoint) => {
                let mut url = endpoint.clone();
                url.set_query(None);
                url.set_fragment(None);
                url.set_path(&format!("{}/search", url.path().trim_end_matches('/')));
                Ok(url)
            }
            Self::DuckDuckGo => Url::parse("https://html.duckduckgo.com/html/"),
            Self::Mojeek => Url::parse(MOJEEK_URL),
            Self::Brave => Url::parse(BRAVE_URL),
        }
        .map_err(|_| SearchError::AllProvidersFailed)?;
        let mut params = url.query_pairs_mut();
        params.append_pair("q", query);
        match self {
            Self::Searxng(_) => {
                params
                    .append_pair("format", "json")
                    .append_pair("language", lang)
                    .append_pair("pageno", &page.to_string());
            }
            Self::Brave => {
                params.append_pair("source", "web");
            }
            Self::DuckDuckGo | Self::Mojeek => {}
        }
        drop(params);
        Ok(url)
    }

    /// Parsed hits and whether the engine reported any results at all (SearXNG paging).
    fn parse(self, fetched: &Fetched) -> Result<(Vec<Hit>, bool), SearchError> {
        match self {
            Self::Searxng(_) => searxng_page(&fetched.body),
            Self::DuckDuckGo => duckduckgo_page(&fetched.text()).map(|hits| (hits, false)),
            Self::Mojeek => marked_page(&fetched.text(), &MOJEEK).map(|hits| (hits, false)),
            Self::Brave => marked_page(&fetched.text(), &BRAVE).map(|hits| (hits, false)),
        }
    }
}

async fn run_chain<F, Fut>(
    searxng: Option<&Url>,
    query: &str,
    limit: usize,
    lang: &str,
    deadline: Instant,
    mut fetch: F,
) -> Result<Vec<Hit>, SearchError>
where
    F: FnMut(Target) -> Fut,
    Fut: Future<Output = Result<Fetched, FetchError>>,
{
    if Instant::now() >= deadline {
        return Err(SearchError::Timeout);
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    let providers = searxng.map(Provider::Searxng).into_iter().chain([
        Provider::DuckDuckGo,
        Provider::Mojeek,
        Provider::Brave,
    ]);
    for provider in providers {
        let mut hits = Vec::new();
        let mut seen = HashSet::new();
        let pages = provider.pages();
        for page in 1..=pages {
            if Instant::now() >= deadline {
                return Err(SearchError::Timeout);
            }
            let target = Target {
                url: provider.url(query, lang, page)?,
                headers: Vec::new(),
                deadline,
                max_bytes: MAX_RESPONSE_BYTES,
            };
            let response = timeout_at(deadline, fetch(target)).await;
            if Instant::now() >= deadline || response.is_err() {
                return Err(SearchError::Timeout);
            }
            let fetched = match response {
                Ok(Ok(fetched)) if (200..300).contains(&fetched.status) => fetched,
                Ok(Err(FetchError::Timeout)) => return Err(SearchError::Timeout),
                _ => break,
            };
            let Ok((page_hits, has_results)) = provider.parse(&fetched) else {
                break;
            };
            hits.extend(
                page_hits
                    .into_iter()
                    .filter(|hit| seen.insert(hit.url.clone())),
            );
            if Instant::now() >= deadline {
                return Err(SearchError::Timeout);
            }
            if hits.len() >= limit || !has_results || page == pages {
                hits.truncate(limit);
                return Ok(hits);
            }
        }
    }
    Err(SearchError::AllProvidersFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::ready, time::Duration};

    fn response(target: Target, status: u16, body: &str) -> Fetched {
        Fetched {
            requested_url: target.url.clone(),
            final_url: target.url,
            status,
            headers: Vec::new(),
            content_type: None,
            body: bytes::Bytes::copy_from_slice(body.as_bytes()),
            fetched_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[tokio::test]
    async fn provider_chain_table() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint = Url::parse("https://search.example/base/")?;
        let ddg = include_str!("../tests/fixtures/search/duckduckgo.html");
        for (first, second, expected_len, expected_calls) in [
            (None, ddg, Some(2), 2),
            (Some((503, "unavailable")), ddg, Some(2), 2),
            (Some((200, "not JSON")), ddg, Some(2), 2),
            (Some((200, r#"{"results":[]}"#)), ddg, Some(0), 1),
            (None, "<div class='no-results'>No results</div>", Some(0), 2),
            (None, "<div class='anomaly-modal'>captcha</div>", None, 4),
            (None, "<form action='/anomaly.js?sv=html'></form>", None, 4),
            (None, "not a search page", None, 4),
        ] {
            let mut calls = 0;
            let deadline = Instant::now() + Duration::from_secs(5);
            let result = run_chain(
                Some(&endpoint),
                "company & jobs",
                10,
                "en",
                deadline,
                |target| {
                    calls += 1;
                    assert_eq!(target.deadline, deadline);
                    assert_eq!(target.max_bytes, MAX_RESPONSE_BYTES);
                    assert_eq!(
                        target
                            .url
                            .query_pairs()
                            .find(|(key, _)| key == "q")
                            .map(|(_, value)| value.into_owned()),
                        Some("company & jobs".to_owned())
                    );
                    let result = if calls == 1 {
                        assert_eq!(target.url.path(), "/base/search");
                        assert!(
                            target
                                .url
                                .query_pairs()
                                .any(|(key, value)| key == "language" && value == "en")
                        );
                        first.map_or(Err(FetchError::Connect), |(status, body)| {
                            Ok(response(target, status, body))
                        })
                    } else {
                        let hosts = ["html.duckduckgo.com", "www.mojeek.com", "search.brave.com"];
                        assert_eq!(target.url.host_str(), Some(hosts[calls - 2]));
                        Ok(response(target, 200, second))
                    };
                    ready(result)
                },
            )
            .await;
            assert_eq!(calls, expected_calls);
            match expected_len {
                Some(len) => assert_eq!(result?.len(), len),
                None => assert!(matches!(result, Err(SearchError::AllProvidersFailed))),
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn pagination_is_unique_ranked_and_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint = Url::parse("https://search.example")?;
        for (limit, empty_page, expected_calls, expected_len) in
            [(2, 0, 2, 2), (10, 3, 3, 2), (10, 0, 5, 5)]
        {
            let mut calls = 0;
            let hits = run_chain(
                Some(&endpoint),
                "test",
                limit,
                "en",
                Instant::now() + Duration::from_secs(5),
                |target| {
                    calls += 1;
                    assert_eq!(
                        target
                            .url
                            .query_pairs()
                            .find(|(key, _)| key == "pageno")
                            .map(|(_, value)| value.into_owned()),
                        Some(calls.to_string())
                    );
                    let body = if calls == empty_page {
                        r#"{"results":[]}"#.to_owned()
                    } else {
                        serde_json::json!({"results": [
                            {"url": "https://example.com/1/#duplicate", "title": "First"},
                            {"url": format!("https://example.com/{calls}")}
                        ]})
                        .to_string()
                    };
                    ready(Ok(response(target, 200, &body)))
                },
            )
            .await?;
            assert_eq!(calls, expected_calls);
            assert_eq!(hits.len(), expected_len);
            assert_eq!(hits[0].title, "First");
            assert_eq!(hits[1].url, "https://example.com/2");
        }
        Ok(())
    }

    #[tokio::test]
    async fn deadline_and_zero_limit_stop_work() {
        for (limit, deadline, timeout) in [
            (10, Instant::now(), true),
            (0, Instant::now() + Duration::from_secs(5), false),
        ] {
            let result = run_chain(None, "test", limit, "en", deadline, |_| {
                panic!("no fetch should start");
                #[allow(unreachable_code)]
                ready(Err(FetchError::Connect))
            })
            .await;
            if timeout {
                assert!(matches!(result, Err(SearchError::Timeout)));
            } else {
                assert!(result.is_ok_and(|hits| hits.is_empty()));
            }
        }
        let result = run_chain(
            None,
            "test",
            10,
            "en",
            Instant::now() + Duration::from_millis(5),
            |_| std::future::pending::<Result<Fetched, FetchError>>(),
        )
        .await;
        assert!(matches!(result, Err(SearchError::Timeout)));
    }
}
