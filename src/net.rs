//! The only network path: destination guard, redirects, deadlines, byte caps, decoding.

use crate::config::Config;

pub struct Target {
    pub url: url::Url,
    pub headers: Vec<(String, String)>,
    pub deadline: tokio::time::Instant,
    pub max_bytes: usize,
}

pub struct Fetched {
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

    /// Body decoded with the declared charset; stub: lossy UTF-8.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Debug, thiserror::Error)]
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
    #[error("http protocol error")]
    Protocol,
}

#[derive(Clone)]
pub struct Fetcher {
    _client: reqwest::Client,
}

impl Fetcher {
    pub fn new(cfg: &Config) -> Result<Self, FetchError> {
        let _client = reqwest::Client::builder()
            .user_agent(cfg.user_agent.as_str())
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| FetchError::Protocol)?;
        Ok(Self { _client })
    }

    pub async fn fetch(&self, _target: &Target) -> Result<Fetched, FetchError> {
        Err(FetchError::Protocol)
    }
}
