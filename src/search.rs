//! Search providers: SearXNG, then DuckDuckGo HTML.

use crate::{config::Config, net::Fetcher};

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

/// Stub: always fails until the search module is written.
pub fn parse_searxng(_body: &[u8]) -> Result<Vec<Hit>, SearchError> {
    Err(SearchError::AllProvidersFailed)
}

/// Stub: returns no hits until the search module is written.
pub fn parse_duckduckgo(_html: &str) -> Vec<Hit> {
    Vec::new()
}

#[derive(Clone)]
pub struct Searcher {
    _fetcher: Fetcher,
    _searxng: Option<url::Url>,
}

impl Searcher {
    pub fn new(fetcher: Fetcher, cfg: &Config) -> Self {
        Self {
            _fetcher: fetcher,
            _searxng: cfg.searxng_endpoint.clone(),
        }
    }

    pub async fn search(
        &self,
        _query: &str,
        _limit: usize,
        _lang: &str,
        _deadline: tokio::time::Instant,
    ) -> Result<Vec<Hit>, SearchError> {
        Err(SearchError::AllProvidersFailed)
    }
}
