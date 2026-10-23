//! Settings from flags and environment.

use std::{fmt, net::SocketAddr, path::PathBuf};

#[derive(Clone, clap::Args)]
pub struct Config {
    #[arg(long, env = "SONAR_BIND", default_value = "127.0.0.1:3002")]
    pub bind: SocketAddr,
    #[arg(long, env = "SONAR_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,
    #[arg(long, env = "SONAR_API_KEY")]
    pub api_key: Option<String>,
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
    pub proxy: Option<String>,
    #[arg(long, env = "SONAR_PROXY_HOSTS", value_delimiter = ',')]
    pub proxy_hosts: Vec<String>,
    #[arg(long, env = "SONAR_PROXY_MAX_BYTES_PER_DAY")]
    pub proxy_max_bytes_per_day: Option<u64>,
    #[arg(long, env = "SONAR_FORWARD_PROXY_CONCURRENCY", default_value_t = 32, value_parser = at_least_one())]
    pub forward_proxy_concurrency: usize,
    #[arg(long, env = "SONAR_FORWARD_PROXY_IDLE_TIMEOUT_MS", default_value_t = 30_000, value_parser = clap::value_parser!(u64).range(1..=86_400_000))]
    pub forward_proxy_idle_timeout_ms: u64,
    #[arg(long, env = "SONAR_MAX_BODY_BYTES", default_value_t = 10_485_760, value_parser = at_least_one())]
    pub max_body_bytes: usize,
    #[arg(long, env = "SONAR_GLOBAL_CONCURRENCY", default_value_t = 256, value_parser = at_least_one())]
    pub global_concurrency: usize,
    #[arg(long, env = "SONAR_PER_HOST_CONCURRENCY", default_value_t = 2, value_parser = at_least_one())]
    pub per_host_concurrency: usize,
    #[arg(long, env = "SONAR_PER_HOST_SPACING_MS", default_value_t = 250)]
    pub per_host_spacing_ms: u64,
    #[arg(long, env = "SONAR_MAX_TIMEOUT_MS", default_value_t = 120_000, value_parser = clap::value_parser!(u64).range(1000..))]
    pub max_timeout_ms: u64,
    #[arg(long, env = "SONAR_DEFAULT_MAX_AGE_MS", default_value_t = 86_400_000)]
    pub default_max_age_ms: u64,
}

fn at_least_one() -> clap::builder::RangedU64ValueParser<usize> {
    clap::builder::RangedU64ValueParser::new().range(1..)
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("data_dir", &self.data_dir)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("user_agent", &self.user_agent)
            .field("searxng_endpoint", &self.searxng_endpoint)
            .field("extra_ports", &self.extra_ports)
            .field("proxy", &self.proxy.as_ref().map(|_| "<redacted>"))
            .field("proxy_hosts", &self.proxy_hosts)
            .field("proxy_max_bytes_per_day", &self.proxy_max_bytes_per_day)
            .field("forward_proxy_concurrency", &self.forward_proxy_concurrency)
            .field(
                "forward_proxy_idle_timeout_ms",
                &self.forward_proxy_idle_timeout_ms,
            )
            .field("max_body_bytes", &self.max_body_bytes)
            .field("global_concurrency", &self.global_concurrency)
            .field("per_host_concurrency", &self.per_host_concurrency)
            .field("per_host_spacing_ms", &self.per_host_spacing_ms)
            .field("max_timeout_ms", &self.max_timeout_ms)
            .field("default_max_age_ms", &self.default_max_age_ms)
            .finish()
    }
}
