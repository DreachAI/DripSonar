//! Shared destination guard, explicit proxy routes, and a guarded browser forward proxy.

mod forward;
mod guard;

use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use time::OffsetDateTime;
use url::Url;

use crate::config::Config;

pub use forward::ForwardProxy;
pub use guard::FetchGuard;
pub(crate) use guard::{DnsFailure, GuardError, GuardedResolver, Refusal};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Route {
    #[default]
    Direct,
    Proxy,
}

impl Route {
    pub fn proxy_used(self) -> &'static str {
        match self {
            Self::Direct => "basic",
            Self::Proxy => "stealth",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    #[error("{0}")]
    Config(&'static str),
    #[error("proxy budget exhausted")]
    BudgetExhausted,
}

/// Clones share the byte budget. Obtain this from the fetcher when starting Chrome.
/// Counts decoded reqwest response bodies and both directions of browser tunnel payloads,
/// not SOCKS/TLS overhead or provider-billed wire bytes.
#[derive(Clone)]
pub struct RouteConfig {
    upstream: Option<Arc<Upstream>>,
    hosts: Arc<[String]>,
    budget: Arc<Budget>,
    concurrency: usize,
    idle_timeout: Duration,
}

impl fmt::Debug for RouteConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteConfig")
            .field("upstream", &self.upstream.as_ref().map(|_| "<redacted>"))
            .field("hosts", &self.hosts)
            .field("max_bytes_per_day", &self.budget.limit)
            .field("concurrency", &self.concurrency)
            .field("idle_timeout", &self.idle_timeout)
            .finish()
    }
}

impl RouteConfig {
    pub fn from_config(cfg: &Config) -> Result<Self, EgressError> {
        let upstream = cfg
            .proxy
            .as_deref()
            .map(Upstream::parse)
            .transpose()?
            .map(Arc::new);
        let mut hosts = Vec::new();
        for host in &cfg.proxy_hosts {
            let host = host.trim().trim_start_matches('.').trim_end_matches('.');
            if host == "*" {
                hosts.push(host.to_owned());
            } else {
                if host.contains('*') {
                    return Err(EgressError::Config(
                        "SONAR_PROXY_HOSTS accepts suffixes or * only",
                    ));
                }
                let host = url::Host::parse(host)
                    .map_err(|_| EgressError::Config("invalid SONAR_PROXY_HOSTS suffix"))?;
                hosts.push(host.to_string().to_ascii_lowercase());
            }
        }
        if !hosts.is_empty() && upstream.is_none() {
            return Err(EgressError::Config(
                "SONAR_PROXY_HOSTS requires SONAR_PROXY",
            ));
        }
        if cfg.forward_proxy_concurrency == 0
            || cfg.forward_proxy_concurrency > tokio::sync::Semaphore::MAX_PERMITS
            || cfg.forward_proxy_idle_timeout_ms == 0
            || cfg.forward_proxy_idle_timeout_ms > 86_400_000
        {
            return Err(EgressError::Config(
                "forward proxy concurrency must be supported and idle timeout must be 1..=86400000 ms",
            ));
        }
        let budget = Budget::load(
            cfg.data_dir.join("proxy-usage.json"),
            cfg.proxy_max_bytes_per_day,
        )?;
        if upstream.is_some() {
            let budget = Arc::downgrade(&budget);
            std::thread::Builder::new()
                .name("proxy-usage".into())
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_secs(5));
                    let Some(budget) = budget.upgrade() else { break };
                    let mut state = budget.state.lock().unwrap_or_else(PoisonError::into_inner);
                    if budget.flush(&mut state).is_err() {
                        tracing::error!("proxy usage could not be persisted; proxy traffic refused until recovery");
                    }
                })
                .map_err(|_| EgressError::Config("cannot start proxy usage writer"))?;
        }
        Ok(Self {
            upstream,
            hosts: hosts.into(),
            budget,
            concurrency: cfg.forward_proxy_concurrency,
            idle_timeout: Duration::from_millis(cfg.forward_proxy_idle_timeout_ms),
        })
    }

    pub fn route_for(&self, host: &str) -> Route {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if self.upstream.is_some()
            && self.hosts.iter().any(|suffix| {
                suffix == "*"
                    || host == *suffix
                    || host
                        .strip_suffix(suffix.as_str())
                        .is_some_and(|prefix| prefix.ends_with('.'))
            })
        {
            Route::Proxy
        } else {
            Route::Direct
        }
    }

    pub(crate) fn reqwest_proxy(&self) -> Result<Option<reqwest::Proxy>, EgressError> {
        self.upstream
            .as_ref()
            .map(|upstream| {
                reqwest::Proxy::all(upstream.url.as_str())
                    .map_err(|_| EgressError::Config("invalid SONAR_PROXY SOCKS5 endpoint"))
            })
            .transpose()
    }

    pub(crate) fn check_budget(&self, route: Route) -> Result<(), EgressError> {
        self.charge(route, 0)
    }

    pub(crate) fn charge(&self, route: Route, bytes: usize) -> Result<(), EgressError> {
        if route == Route::Direct {
            return Ok(());
        }
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let day = utc_day();
        if state.usage.utc_day != day {
            state.usage = DailyUsage {
                utc_day: day,
                bytes: 0,
            };
        }
        let previous = state.usage.bytes;
        state.usage.bytes = state.usage.bytes.saturating_add(bytes as u64);
        if state.failed
            || state.usage.utc_day != state.saved.utc_day
            || state.usage.bytes.saturating_sub(state.saved.bytes) >= 8 * 1024 * 1024
        {
            self.budget.flush(&mut state)?;
        }
        if self
            .budget
            .limit
            .is_some_and(|limit| previous >= limit || state.usage.bytes > limit)
        {
            return Err(EgressError::BudgetExhausted);
        }
        Ok(())
    }
}

struct Budget {
    limit: Option<u64>,
    path: PathBuf,
    state: Mutex<BudgetState>,
}

struct BudgetState {
    usage: DailyUsage,
    saved: DailyUsage,
    failed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DailyUsage {
    utc_day: i64,
    bytes: u64,
}

impl Budget {
    fn load(path: PathBuf, limit: Option<u64>) -> Result<Arc<Self>, EgressError> {
        let usage = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| EgressError::Config("invalid proxy usage file"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DailyUsage {
                utc_day: utc_day(),
                bytes: 0,
            },
            Err(_) => return Err(EgressError::Config("cannot read proxy usage file")),
        };
        Ok(Arc::new(Self {
            limit,
            path,
            state: Mutex::new(BudgetState {
                usage,
                saved: usage,
                failed: false,
            }),
        }))
    }

    fn flush(&self, state: &mut BudgetState) -> Result<(), EgressError> {
        if state.usage == state.saved && !state.failed {
            return Ok(());
        }
        let write = || -> std::io::Result<()> {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temp = self.path.with_extension("json.tmp");
            std::fs::write(
                &temp,
                serde_json::to_vec(&state.usage).map_err(std::io::Error::other)?,
            )?;
            std::fs::rename(temp, &self.path)?;
            Ok(())
        };
        state.failed = write().is_err();
        if state.failed {
            return Err(EgressError::Config("cannot persist proxy usage"));
        }
        state.saved = state.usage;
        Ok(())
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if self.flush(&mut state).is_err() {
            tracing::error!("proxy usage could not be persisted at shutdown");
        }
    }
}

fn utc_day() -> i64 {
    OffsetDateTime::now_utc()
        .unix_timestamp()
        .div_euclid(86_400)
}

struct Upstream {
    url: Url,
    username: String,
    password: String,
}

impl Upstream {
    fn parse(input: &str) -> Result<Self, EgressError> {
        let invalid = || {
            EgressError::Config(
                "SONAR_PROXY must be a socks5:// endpoint; socks5h:// and HTTP proxies are refused",
            )
        };
        let mut url = Url::parse(input).map_err(|_| invalid())?;
        if url.scheme() != "socks5"
            || url.host().is_none()
            || url.port() == Some(0)
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid());
        }
        let username = decode_credential(url.username())?;
        let password = decode_credential(url.password().unwrap_or_default())?;
        if username.len() > 255
            || password.len() > 255
            || (url.password().is_some() && username.is_empty())
            || (!username.is_empty() && password.is_empty())
        {
            return Err(EgressError::Config(
                "invalid SONAR_PROXY SOCKS5 credentials",
            ));
        }
        if let Some(url::Host::Domain(host)) = url.host()
            && host.parse::<std::net::IpAddr>().is_err()
        {
            let mut destination = Url::parse("https://proxy.invalid/").map_err(|_| invalid())?;
            destination.set_host(Some(host)).map_err(|_| invalid())?;
            // Resolve on a startup-only runtime, even when the caller already runs in Tokio.
            let addresses = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| EgressError::Config("cannot start proxy DNS resolver"))?;
                runtime.block_on(async {
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        FetchGuard::new(&[]).resolve_url(&destination),
                    )
                    .await
                    .map_err(|_| EgressError::Config("proxy DNS resolution timed out"))?
                    .map_err(|_| {
                        EgressError::Config("proxy hostname must resolve to public addresses")
                    })
                })
            })
            .join()
            .map_err(|_| EgressError::Config("proxy DNS resolver failed"))??;
            let address = addresses
                .first()
                .ok_or(EgressError::Config("proxy hostname has no addresses"))?;
            url.set_ip_host(address.ip()).map_err(|_| invalid())?;
        }
        Ok(Self {
            url,
            username,
            password,
        })
    }
}

fn decode_credential(value: &str) -> Result<String, EgressError> {
    let invalid = || EgressError::Config("invalid SONAR_PROXY credential encoding");
    let mut decoded = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = char::from(bytes.next().ok_or_else(invalid)?)
                .to_digit(16)
                .ok_or_else(invalid)?;
            let low = char::from(bytes.next().ok_or_else(invalid)?)
                .to_digit(16)
                .ok_or_else(invalid)?;
            (high * 16 + low) as u8
        } else {
            byte
        });
    }
    String::from_utf8(decoded).map_err(|_| invalid())
}
