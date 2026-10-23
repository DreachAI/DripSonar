//! Crawl scope and URL identity, plus bounded single-page and sitemap discovery.

pub mod sitemap;

use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::time::{Instant, timeout_at};
use url::{Host, Url};

use crate::{
    net::{FetchError, Fetcher, Target},
    page::RenderOptions,
    scrape::{ScrapeError, ScrapeRequest, Scraper},
};
use sitemap::SitemapEntries;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ScopeOptions {
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub max_depth: u32,
    pub max_discovery_hops: u32,
    pub allow_backward_links: bool,
    pub crawl_entire_domain: bool,
    pub allow_external_links: bool,
    pub allow_subdomains: bool,
    pub ignore_query_parameters: bool,
}

impl Default for ScopeOptions {
    fn default() -> Self {
        Self {
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            max_depth: 10,
            max_discovery_hops: u32::MAX,
            allow_backward_links: false,
            crawl_entire_domain: false,
            allow_external_links: false,
            allow_subdomains: false,
            ignore_query_parameters: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Scope {
    seed: String,
    options: ScopeOptions,
    #[serde(skip)]
    include: Vec<Regex>,
    #[serde(skip)]
    exclude: Vec<Regex>,
    #[serde(skip)]
    host: String,
    #[serde(skip)]
    domain_host: bool,
    #[serde(skip)]
    prefix: String,
}

impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct StoredScope {
            seed: String,
            options: ScopeOptions,
        }
        let stored = StoredScope::deserialize(deserializer)?;
        let seed = Url::parse(&stored.seed).map_err(serde::de::Error::custom)?;
        Self::new(&seed, stored.options).map_err(serde::de::Error::custom)
    }
}

impl Scope {
    pub fn options(&self) -> &ScopeOptions {
        &self.options
    }

    pub fn new(seed: &Url, opts: ScopeOptions) -> Result<Self, String> {
        if !matches!(seed.scheme(), "http" | "https")
            || !seed.has_host()
            || !seed.username().is_empty()
            || seed.password().is_some()
        {
            return Err("scope seed must be an http(s) URL without credentials".into());
        }
        if opts.max_depth > 100 || path_depth(seed) > opts.max_depth as usize {
            return Err("maxDepth must cover the seed path and be at most 100".into());
        }
        if opts.include_paths.len() + opts.exclude_paths.len() > 64 {
            return Err("at most 64 path patterns are allowed".into());
        }
        let compile = |patterns: &[String]| {
            patterns
                .iter()
                .map(|pattern| {
                    if pattern.len() > 1024 {
                        return Err("path patterns must be at most 1 KiB".into());
                    }
                    RegexBuilder::new(pattern)
                        .size_limit(1024 * 1024)
                        .build()
                        .map_err(|e| format!("invalid path pattern: {e}"))
                })
                .collect::<Result<Vec<_>, String>>()
        };
        Ok(Self {
            seed: seed.to_string(),
            include: compile(&opts.include_paths)?,
            exclude: compile(&opts.exclude_paths)?,
            host: canonical_host(seed).to_owned(),
            domain_host: matches!(seed.host(), Some(Host::Domain(_))),
            prefix: seed.path().trim_end_matches('/').to_owned(),
            options: opts,
        })
    }

    pub fn admits(&self, url: &Url, hops: u32) -> bool {
        let opts = &self.options;
        if !matches!(url.scheme(), "http" | "https")
            || !url.has_host()
            || !url.username().is_empty()
            || url.password().is_some()
            || hops > opts.max_discovery_hops
            || path_depth(url) > opts.max_depth as usize
        {
            return false;
        }
        let host = canonical_host(url);
        let same_host = host.eq_ignore_ascii_case(&self.host);
        let subdomain = opts.allow_subdomains
            && self.domain_host
            && matches!(url.host(), Some(Host::Domain(_)))
            && host
                .strip_suffix(&self.host)
                .is_some_and(|prefix| prefix.ends_with('.'));
        if !same_host && !subdomain && !opts.allow_external_links {
            return false;
        }
        let path = url.path();
        if !opts.allow_backward_links
            && !opts.crawl_entire_domain
            && !(path == self.prefix
                || path
                    .strip_prefix(&self.prefix)
                    .is_some_and(|p| p.starts_with('/')))
        {
            return false;
        }
        !self.exclude.iter().any(|pattern| pattern.is_match(path))
            && (self.include.is_empty()
                || self.include.iter().any(|pattern| pattern.is_match(path)))
    }
}

fn path_depth(url: &Url) -> usize {
    url.path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count()
}

fn canonical_host(url: &Url) -> &str {
    let host = url.host_str().unwrap_or_default();
    host.strip_prefix("www.").unwrap_or(host)
}

pub fn dedup_key(url: &Url, ignore_query: bool) -> String {
    let mut normalized = url.clone();
    normalized.set_fragment(None);
    if ignore_query {
        normalized.set_query(None);
    }
    if matches!(url.host(), Some(Host::Domain(_))) {
        let _ = normalized.set_host(Some(canonical_host(url)));
    }
    if matches!(
        (normalized.scheme(), normalized.port()),
        ("http", Some(80)) | ("https", Some(443))
    ) {
        let _ = normalized.set_port(None);
    }
    let path = normalized.path().trim_end_matches('/').to_owned();
    normalized.set_path(if path.is_empty() { "/" } else { &path });
    normalized.to_string()
}

pub fn normalize_link(base: &Url, href: &str) -> Option<Url> {
    let mut url = base.join(href.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.set_fragment(None);
    Some(url)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct MapRequest {
    pub url: String,
    pub search: Option<String>,
    pub limit: usize,
    pub include_subdomains: bool,
    pub ignore_sitemap: bool,
    pub sitemap_only: bool,
    pub ignore_query_parameters: bool,
}

impl Default for MapRequest {
    fn default() -> Self {
        Self {
            url: String::new(),
            search: None,
            limit: 5000,
            include_subdomains: true,
            ignore_sitemap: false,
            sitemap_only: false,
            ignore_query_parameters: true,
        }
    }
}

pub async fn map(
    fetcher: &Fetcher,
    scraper: &Scraper,
    req: &MapRequest,
    deadline: Instant,
) -> Result<Vec<String>, ScrapeError> {
    let seed = crate::scrape::normalize(&req.url)?;
    if req.limit == 0 || req.limit > 100_000 || (req.ignore_sitemap && req.sitemap_only) {
        return Err(ScrapeError::Invalid(
            "invalid map limit or conflicting sitemap flags".into(),
        ));
    }
    let scope = Scope::new(
        &seed,
        ScopeOptions {
            max_depth: 100,
            max_discovery_hops: 1,
            allow_backward_links: true,
            crawl_entire_domain: true,
            allow_subdomains: req.include_subdomains,
            ignore_query_parameters: req.ignore_query_parameters,
            ..ScopeOptions::default()
        },
    )
    .map_err(ScrapeError::Invalid)?;
    let search = req.search.as_ref().map(|s| s.to_lowercase());
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |mut url: Url| {
        url.set_fragment(None);
        if results.len() < req.limit
            && scope.admits(&url, 1)
            && search
                .as_ref()
                .is_none_or(|search| url.as_str().to_lowercase().contains(search))
            && seen.insert(dedup_key(&url, req.ignore_query_parameters))
        {
            results.push(url.to_string());
        }
        results.len() >= req.limit
    };
    let mut discovery_succeeded = false;
    let mut last_error = None;
    if !req.ignore_sitemap {
        let mut queue = VecDeque::new();
        let mut sitemap_seen = HashSet::new();
        let mut root = seed.clone();
        root.set_path("/robots.txt");
        root.set_query(None);
        root.set_fragment(None);
        match fetcher
            .fetch(&Target {
                url: root.clone(),
                headers: Vec::new(),
                deadline,
                max_bytes: 1024 * 1024,
            })
            .await
        {
            Ok(robots) if (200..300).contains(&robots.status) => {
                // Keep one of the twenty slots for the conventional root sitemap.
                for url in sitemap::robots_sitemaps(&robots.text())
                    .into_iter()
                    .take(19)
                {
                    if sitemap_seen.insert(dedup_key(&url, false)) {
                        queue.push_back((url, 0));
                    }
                }
            }
            Err(error) => last_error = Some(ScrapeError::Fetch(error)),
            _ => {}
        }
        root.set_path("/sitemap.xml");
        if sitemap_seen.insert(dedup_key(&root, false)) {
            queue.push_back((root, 0));
        }
        let mut fetched_files = 0;
        'sitemaps: while let Some((url, depth)) = queue.pop_front() {
            if fetched_files == 20 {
                break;
            }
            fetched_files += 1;
            let fetched = fetcher
                .fetch(&Target {
                    url,
                    headers: Vec::new(),
                    deadline,
                    max_bytes: 10 * 1024 * 1024,
                })
                .await;
            match fetched {
                Ok(fetched) if (200..300).contains(&fetched.status) => {
                    match sitemap::parse(&fetched.body, 50 * 1024 * 1024, 50_000) {
                        Ok(SitemapEntries::Urls(urls)) => {
                            discovery_succeeded = true;
                            for url in urls {
                                if add(url) {
                                    break 'sitemaps;
                                }
                            }
                        }
                        Ok(SitemapEntries::Sitemaps(urls)) => {
                            discovery_succeeded |= urls.is_empty();
                            if depth < 3 {
                                for url in urls {
                                    if sitemap_seen.len() >= 20 {
                                        break;
                                    }
                                    if sitemap_seen.insert(dedup_key(&url, false)) {
                                        queue.push_back((url, depth + 1));
                                    }
                                }
                            }
                        }
                        Err(_) => last_error = Some(ScrapeError::Fetch(FetchError::Protocol)),
                    }
                }
                Ok(_) => last_error = Some(ScrapeError::Fetch(FetchError::Protocol)),
                Err(error) => last_error = Some(ScrapeError::Fetch(error)),
            }
        }
    }
    if !req.sitemap_only {
        let request = ScrapeRequest {
            url: seed.to_string(),
            proxy: Default::default(),
            headers: Vec::new(),
            deadline,
            max_age: Duration::from_secs(86400),
            raw_html: false,
            render: RenderOptions {
                links: true,
                ..RenderOptions::default()
            },
        };
        match timeout_at(deadline, scraper.scrape(&request)).await {
            Ok(Ok(doc)) => {
                discovery_succeeded = true;
                for href in doc.rendered.links.into_iter().flatten() {
                    if let Some(url) = normalize_link(&doc.url, &href) {
                        add(url);
                    }
                }
            }
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => return Err(FetchError::Timeout.into()),
        }
    }
    if Instant::now() >= deadline {
        return Err(FetchError::Timeout.into());
    }
    if !discovery_succeeded {
        return Err(last_error.unwrap_or_else(|| ScrapeError::Fetch(FetchError::Protocol)));
    }
    Ok(results)
}
