//! Settings from flags and environment.

use std::{fmt, net::SocketAddr, path::PathBuf};

#[derive(Clone)]
pub struct Secret(String);

impl std::str::FromStr for Secret {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.to_owned()))
    }
}

impl std::ops::Deref for Secret {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

pub(crate) fn default_render_threads() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

#[derive(Clone, clap::Args)]
pub struct Config {
    #[arg(long, env = "SONAR_BIND", default_value = "127.0.0.1:3002")]
    pub bind: SocketAddr,
    #[arg(long, env = "SONAR_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,
    #[arg(long, env = "SONAR_API_KEY", hide_env_values = true)]
    pub api_key: Option<Secret>,
    #[arg(long, env = "SONAR_WEBHOOK_SECRET", hide_env_values = true)]
    pub webhook_secret: Option<Secret>,
    // Kept out of Config's Debug output, including the provider credential.
    #[command(flatten)]
    pub llm: crate::extract::LlmConfig,
    #[arg(
        long,
        env = "SONAR_USER_AGENT",
        default_value = concat!("DripSonar/", env!("CARGO_PKG_VERSION"), " (+https://github.com/DreachAI/DripSonar)")
    )]
    pub user_agent: String,
    #[arg(long, env = "SEARXNG_ENDPOINT")]
    pub searxng_endpoint: Option<url::Url>,
    /// Ports allowed besides 80 and 443.
    #[arg(long, env = "SONAR_EXTRA_PORTS", value_delimiter = ',')]
    pub extra_ports: Vec<u16>,
    /// Upstream proxy; only SOCKS5 with locally guarded DNS is accepted.
    #[arg(long, env = "SONAR_PROXY", hide_env_values = true)]
    pub proxy: Option<Secret>,
    #[arg(long, env = "SONAR_PROXY_HOSTS", value_delimiter = ',')]
    pub proxy_hosts: Vec<String>,
    #[arg(long, env = "SONAR_PROXY_MAX_BYTES_PER_DAY")]
    pub proxy_max_bytes_per_day: Option<u64>,
    /// Retry blocked GET pages through the proxy and browser tiers.
    #[arg(long, env = "SONAR_BLOCK_RECOVERY", default_value_t = true, action = clap::ArgAction::Set)]
    pub block_recovery: bool,
    #[arg(long, env = "SONAR_FORWARD_PROXY_CONCURRENCY", default_value_t = 32, value_parser = at_least_one())]
    pub forward_proxy_concurrency: usize,
    #[arg(long, env = "SONAR_FORWARD_PROXY_IDLE_TIMEOUT_MS", default_value_t = 30_000, value_parser = clap::value_parser!(u64).range(1..=86_400_000))]
    pub forward_proxy_idle_timeout_ms: u64,
    #[arg(long, env = "SONAR_MAX_BODY_BYTES", default_value_t = 10_485_760, value_parser = at_least_one())]
    pub max_body_bytes: usize,
    #[arg(long, env = "SONAR_GLOBAL_CONCURRENCY", default_value_t = 256, value_parser = at_least_one())]
    pub global_concurrency: usize,
    #[arg(long, env = "SONAR_RENDER_THREADS", default_value_t = default_render_threads(), value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=tokio::sync::Semaphore::MAX_PERMITS as u64))]
    pub render_threads: usize,
    #[arg(long, env = "SONAR_PER_HOST_CONCURRENCY", default_value_t = 2, value_parser = at_least_one())]
    pub per_host_concurrency: usize,
    #[arg(long, env = "SONAR_PER_HOST_SPACING_MS", default_value_t = 250)]
    pub per_host_spacing_ms: u64,
    #[arg(long, env = "SONAR_MAX_TIMEOUT_MS", default_value_t = 120_000, value_parser = clap::value_parser!(u64).range(1000..))]
    pub max_timeout_ms: u64,
    #[arg(long, env = "SONAR_DEFAULT_MAX_AGE_MS", default_value_t = 86_400_000)]
    pub default_max_age_ms: u64,
    #[arg(long, env = "SONAR_JOB_WORKERS", default_value_t = 192, value_parser = at_least_one())]
    pub job_workers: usize,
    #[arg(long, env = "SONAR_MAX_ACTIVE_JOBS", default_value_t = 100, value_parser = at_least_one())]
    pub max_active_jobs: usize,
    #[arg(long, env = "SONAR_MAX_CRAWL_LIMIT", default_value_t = 100_000, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_crawl_limit: u32,
    #[arg(long, env = "SONAR_JOB_RETENTION_HOURS", default_value_t = 24, value_parser = clap::value_parser!(u64).range(1..=87600))]
    pub job_retention_hours: u64,
    #[arg(long, env = "SONAR_MAP_TIMEOUT_MS", default_value_t = 60_000, value_parser = clap::value_parser!(u64).range(1000..=600_000))]
    pub map_timeout_ms: u64,
    #[arg(long, env = "SONAR_PUBLIC_URL")]
    pub public_url: Option<url::Url>,
    #[arg(long, env = "SONAR_MIN_FREE_DISK_MB", default_value_t = 2048)]
    pub min_free_disk_mb: u64,
    // Browser tier: disabled unless one of the two Chrome settings is present.
    /// Launch a child Chrome from this binary (development; no OS egress confinement).
    #[arg(long, env = "SONAR_CHROME_PATH", conflicts_with = "chrome_ws")]
    pub chrome_path: Option<PathBuf>,
    /// DevTools endpoint of an externally confined Chrome, e.g. `http://127.0.0.1:9222`.
    #[arg(long, env = "SONAR_CHROME_WS")]
    pub chrome_ws: Option<url::Url>,
    #[arg(long, env = "SONAR_BROWSER_TABS", default_value_t = 4, value_parser = at_least_one())]
    pub browser_tabs: usize,
    /// Chrome process-tree memory ceiling; enforced only for a child Chrome on Linux.
    #[arg(long, env = "SONAR_BROWSER_MEMORY_MB", default_value_t = 2048, value_parser = clap::value_parser!(u64).range(1..))]
    pub browser_memory_mb: u64,
    #[arg(long, env = "SONAR_BROWSER_RECYCLE_PAGES", default_value_t = 200, value_parser = at_least_one())]
    pub browser_recycle_pages: usize,
    /// Multi-step sessions; capped at one less than the tab count.
    #[arg(long, env = "SONAR_BROWSER_SESSIONS", default_value_t = 2)]
    pub browser_sessions: usize,
    /// Fixed loopback port of the browser forward proxy when Chrome is external.
    #[arg(long, env = "SONAR_FORWARD_PROXY_PORT", default_value_t = 3003, value_parser = clap::value_parser!(u16).range(1..))]
    pub forward_proxy_port: u16,
    /// Characters per MCP tool result; longer text is cut with a note.
    #[arg(long, env = "SONAR_MCP_MAX_CHARS", default_value_t = 40_000, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1000..))]
    pub mcp_max_chars: usize,
}

fn at_least_one() -> clap::builder::RangedU64ValueParser<usize> {
    clap::builder::RangedU64ValueParser::new().range(1..)
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("data_dir", &self.data_dir)
            .field("api_key", &self.api_key)
            .field("user_agent", &self.user_agent)
            .field("searxng_endpoint", &self.searxng_endpoint)
            .field("extra_ports", &self.extra_ports)
            .field("proxy", &self.proxy)
            .field("proxy_hosts", &self.proxy_hosts)
            .field("proxy_max_bytes_per_day", &self.proxy_max_bytes_per_day)
            .field("forward_proxy_concurrency", &self.forward_proxy_concurrency)
            .field(
                "forward_proxy_idle_timeout_ms",
                &self.forward_proxy_idle_timeout_ms,
            )
            .field("max_body_bytes", &self.max_body_bytes)
            .field("global_concurrency", &self.global_concurrency)
            .field("render_threads", &self.render_threads)
            .field("per_host_concurrency", &self.per_host_concurrency)
            .field("per_host_spacing_ms", &self.per_host_spacing_ms)
            .field("max_timeout_ms", &self.max_timeout_ms)
            .field("default_max_age_ms", &self.default_max_age_ms)
            .field("job_workers", &self.job_workers)
            .field("max_active_jobs", &self.max_active_jobs)
            .field("max_crawl_limit", &self.max_crawl_limit)
            .field("job_retention_hours", &self.job_retention_hours)
            .field("map_timeout_ms", &self.map_timeout_ms)
            .field(
                "public_url",
                &self.public_url.as_ref().map(|_| "<configured>"),
            )
            .field("min_free_disk_mb", &self.min_free_disk_mb)
            .field("chrome_path", &self.chrome_path)
            .field("chrome_ws", &self.chrome_ws)
            .field("browser_tabs", &self.browser_tabs)
            .field("browser_memory_mb", &self.browser_memory_mb)
            .field("browser_recycle_pages", &self.browser_recycle_pages)
            .field("browser_sessions", &self.browser_sessions)
            .field("forward_proxy_port", &self.forward_proxy_port)
            .field("mcp_max_chars", &self.mcp_max_chars)
            .finish()
    }
}
