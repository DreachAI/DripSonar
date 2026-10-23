//! Search providers: SearXNG, then DuckDuckGo HTML.

use std::{collections::HashSet, future::Future};

use crate::{
    config::Config,
    net::{FetchError, Fetched, Fetcher, Target},
};
use dom_query::Document;
use tokio::time::{Instant, timeout_at};
use url::Url;

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
    searxng: Option<Url>,
}

impl Searcher {
    pub fn new(fetcher: Fetcher, cfg: &Config) -> Self {
        Self {
            fetcher,
            searxng: cfg.searxng_endpoint.clone(),
        }
    }

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        lang: &str,
        deadline: Instant,
    ) -> Result<Vec<Hit>, SearchError> {
        run_chain(
            self.searxng.as_ref(),
            query,
            limit,
            lang,
            deadline,
            |target| async move { self.fetcher.fetch(&target).await },
        )
        .await
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
    for endpoint in searxng.into_iter().map(Some).chain(std::iter::once(None)) {
        let mut hits = Vec::new();
        let mut seen = HashSet::new();
        let pages = if endpoint.is_some() { 5 } else { 1 };
        for page in 1..=pages {
            if Instant::now() >= deadline {
                return Err(SearchError::Timeout);
            }
            let mut url = match endpoint {
                Some(endpoint) => endpoint.clone(),
                None => Url::parse("https://html.duckduckgo.com/html/")
                    .map_err(|_| SearchError::AllProvidersFailed)?,
            };
            url.set_query(None);
            url.set_fragment(None);
            if endpoint.is_some() {
                url.set_path(&format!("{}/search", url.path().trim_end_matches('/')));
            }
            {
                let mut params = url.query_pairs_mut();
                params.append_pair("q", query);
                if endpoint.is_some() {
                    params
                        .append_pair("format", "json")
                        .append_pair("language", lang)
                        .append_pair("pageno", &page.to_string());
                }
            }
            let target = Target {
                url,
                headers: Vec::new(),
                deadline,
                max_bytes: 2 * 1024 * 1024,
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
            let parsed = if endpoint.is_some() {
                searxng_page(&fetched.body)
            } else {
                duckduckgo_page(&fetched.text()).map(|hits| (hits, false))
            };
            let Ok((page_hits, has_results)) = parsed else {
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
            (None, "<div class='anomaly-modal'>captcha</div>", None, 2),
            (None, "<form action='/anomaly.js?sv=html'></form>", None, 2),
            (None, "not a search page", None, 2),
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
                    assert_eq!(target.max_bytes, 2 * 1024 * 1024);
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
                        assert_eq!(target.url.host_str(), Some("html.duckduckgo.com"));
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
