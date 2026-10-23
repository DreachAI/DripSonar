//! Browser tier: one supervised Chrome behind the guarded forward proxy, a fresh isolated
//! context per visit, Firecrawl actions, screenshot artifacts, and short sessions.
//!
//! Disabled unless `SONAR_CHROME_PATH` (child Chrome, development) or `SONAR_CHROME_WS`
//! (externally confined Chrome, production) is set. No chromiumoxide type leaves this module.

mod actions;
mod network;
pub mod policy;
mod pool;
mod sessions;
mod types;

use std::{
    future::Future,
    sync::{Arc, Weak},
    time::Duration,
};

use time::OffsetDateTime;
use tokio::{
    task::JoinHandle,
    time::{Instant, sleep_until, timeout_at},
};
use url::Url;

pub use self::types::{
    Action, ActionsOutput, BrowserExtras, BrowserOutput, BrowserPlan, Direction, JsReturn,
    PageScrape, PlanError, Shot, validate_plan,
};
use self::{
    actions::Captured,
    network::TabPolicy,
    pool::{Chrome, Pool, PoolConfig, Tab},
    sessions::Sessions,
};
use crate::{
    config::Config,
    egress::{ForwardProxy, Route, RouteConfig},
    net::{FetchError, Fetched, Fetcher},
    store::{self, Store, StoreError},
};

/// Bump when rendering behavior changes, so older rendered captures stop matching.
const RENDER_POLICY_VERSION: u32 = 1;
const ARTIFACT_TTL: Duration = Duration::from_secs(24 * 3600);
const ARTIFACT_QUOTA_BYTES: u64 = 1 << 30;
const ARTIFACT_SWEEP_EVERY: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("browser tier not configured")]
    Disabled,
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("browser unavailable: {0}")]
    Unavailable(&'static str),
    #[error("action {index} ({kind}) failed: {reason}")]
    Action {
        index: usize,
        kind: &'static str,
        reason: String,
    },
    #[error("{0}")]
    Limit(&'static str),
    #[error("screenshot could not be stored")]
    Storage,
    #[error("browser session not found or expired")]
    SessionNotFound,
    #[error("all browser sessions are in use")]
    SessionLimit,
}

/// Cheap to clone; a disabled tier answers every visit with [`BrowserError::Disabled`].
#[derive(Clone, Default)]
pub struct BrowserTier(Option<Arc<Tier>>);

struct Tier {
    pool: Pool,
    fetcher: Fetcher,
    routes: RouteConfig,
    store: Store,
    artifact_base: Url,
    hints: policy::Hints,
    sessions: Sessions,
    user_agent: String,
    max_bytes: usize,
    tasks: Vec<JoinHandle<()>>,
}

impl BrowserTier {
    /// Starts the forward proxy and the session reaper; Chrome itself starts on first use.
    /// Always starts the artifact sweeper, since stored screenshots outlive a configuration.
    pub async fn start(
        cfg: &Config,
        fetcher: &Fetcher,
        store: Store,
    ) -> Result<Self, BrowserError> {
        spawn_sweeper(store.clone());
        let (chrome, proxy_port) = match (&cfg.chrome_path, &cfg.chrome_ws) {
            (Some(path), _) if path.is_file() => (Chrome::Launch(path.clone()), 0),
            (Some(_), _) => {
                return Err(BrowserError::Unavailable("SONAR_CHROME_PATH is not a file"));
            }
            (None, Some(endpoint)) => (
                Chrome::Connect(endpoint.to_string()),
                cfg.forward_proxy_port,
            ),
            (None, None) => return Ok(Self::default()),
        };
        let routes = fetcher.route_config();
        let (proxy, token, proxy_task) = ForwardProxy::start(
            proxy_port,
            fetcher.guard(),
            routes.clone(),
        )
        .await
        .map_err(|error| {
            tracing::error!(%error, port = proxy_port, "browser forward proxy failed to start");
            BrowserError::Unavailable("forward proxy failed to start")
        })?;
        // A separate listener fixes the route for an isolated Chrome context, including
        // workers and subframes, without changing routing for any concurrent visit.
        let (proxy_override, proxy_override_task) = if routes.has_proxy() {
            let (address, token, task) =
                ForwardProxy::start(0, fetcher.guard(), routes.proxy_only())
                    .await
                    .map_err(|_| {
                        proxy_task.abort();
                        BrowserError::Unavailable("proxy override failed to start")
                    })?;
            (Some((address, token)), Some(task))
        } else {
            (None, None)
        };
        let mut base = cfg
            .public_url
            .clone()
            .or_else(|| Url::parse(&format!("http://{}", cfg.bind)).ok())
            .ok_or(BrowserError::Unavailable("invalid SONAR_PUBLIC_URL"))?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let artifact_base = base
            .join("v1/artifacts/")
            .map_err(|_| BrowserError::Unavailable("invalid SONAR_PUBLIC_URL"))?;
        let launched = matches!(chrome, Chrome::Launch(_));
        let pool = Pool::new(PoolConfig {
            chrome,
            tabs: cfg.browser_tabs,
            recycle_after: cfg.browser_recycle_pages,
            memory_limit: cfg.browser_memory_mb.saturating_mul(1 << 20),
            user_agent: cfg.user_agent.clone(),
            proxy,
            token,
            proxy_override,
        });
        let sessions = cfg.browser_sessions.min(cfg.browser_tabs.saturating_sub(1));
        let tier = Arc::new_cyclic(|weak: &Weak<Tier>| Tier {
            pool,
            fetcher: fetcher.clone(),
            routes,
            store,
            artifact_base,
            hints: policy::Hints::default(),
            sessions: Sessions::new(sessions),
            user_agent: cfg.user_agent.clone(),
            max_bytes: cfg.max_body_bytes,
            tasks: std::iter::once(proxy_task)
                .chain(proxy_override_task)
                .chain(std::iter::once(spawn_reaper(weak.clone())))
                .collect(),
        });
        tracing::info!(
            launched,
            tabs = cfg.browser_tabs,
            sessions,
            "browser tier enabled"
        );
        Ok(Self(Some(tier)))
    }

    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Stops Chrome (a child is killed; an external one is left running) and the proxy.
    pub async fn shutdown(&self) {
        if let Some(tier) = &self.0 {
            tier.sessions.clear();
            tier.pool.shutdown().await;
            for task in &tier.tasks {
                task.abort();
            }
        }
    }

    /// Navigates a fresh tab, then runs `plan`. Host admission is taken once for the visit.
    pub async fn render(
        &self,
        url: &Url,
        headers: &[(String, String)],
        deadline: Instant,
        plan: &BrowserPlan,
    ) -> Result<BrowserOutput, BrowserError> {
        self.render_with_proxy(url, headers, deadline, plan, false)
            .await
    }

    /// The override is per context, never a process-wide proxy setting.
    pub(crate) async fn render_with_proxy(
        &self,
        url: &Url,
        headers: &[(String, String)],
        deadline: Instant,
        plan: &BrowserPlan,
        force_proxy: bool,
    ) -> Result<BrowserOutput, BrowserError> {
        let tier = self.tier()?;
        if force_proxy && !tier.routes.has_proxy() {
            return Err(FetchError::Refused("proxy not configured".into()).into());
        }
        within(deadline, async {
            let _admission = tier.fetcher.admit(url, deadline).await?;
            let policy = TabPolicy {
                origin: url.origin(),
                headers: headers.to_vec(),
                block_assets: !plan.is_visual(),
                proxy_route: force_proxy,
            };
            let tab = tier.pool.lease(policy, deadline, true).await?;
            actions::navigate(&tab.page, url, deadline).await?;
            let mut output = tier.visit(&tab, url, plan, deadline).await?;
            if force_proxy {
                output.capture.route = Route::Proxy;
            }
            Ok(output)
        })
        .await
    }

    /// Cache key for a rendered capture: URL, headers, and everything that changes rendering.
    pub fn rendered_key(
        &self,
        url: &Url,
        headers: &[(String, String)],
        plan: &BrowserPlan,
    ) -> String {
        self.rendered_key_with_proxy(url, headers, plan, false)
    }

    pub(crate) fn rendered_key_with_proxy(
        &self,
        url: &Url,
        headers: &[(String, String)],
        plan: &BrowserPlan,
        force_proxy: bool,
    ) -> String {
        let user_agent = self.0.as_ref().map_or("", |tier| tier.user_agent.as_str());
        let route = if force_proxy {
            Route::Proxy
        } else {
            self.0.as_ref().map_or(Route::Direct, |tier| {
                tier.routes.route_for(url.host_str().unwrap_or_default())
            })
        };
        let route = route.as_str();
        let assets = if plan.is_visual() { "visual" } else { "text" };
        let (width, height) = pool::VIEWPORT;
        let mut material = format!(
            "browser\n{RENDER_POLICY_VERSION}\n{user_agent}\n{width}x{height}\n{assets}\n{route}\n{url}"
        );
        if force_proxy {
            // Host-routed visits may redirect to a direct host; forced visits cannot.
            material.push_str("\noverride:proxy");
        }
        for (name, value) in headers {
            material.push_str(&format!("\n{name}: {value}"));
        }
        store::sha256_hex(material.as_bytes())
    }

    /// Whether this origin recently needed the browser (always false when disabled).
    pub fn prefers(&self, origin: &str) -> bool {
        self.0
            .as_ref()
            .is_some_and(|tier| tier.hints.prefers(origin))
    }

    pub fn remember(&self, origin: &str) {
        if let Some(tier) = &self.0 {
            tier.hints.remember(origin);
        }
    }

    pub fn forget(&self, origin: &str) {
        if let Some(tier) = &self.0 {
            tier.hints.forget(origin);
        }
    }

    /// Opens a session on `url` and returns its ID with the first view of the page.
    pub async fn open_session(
        &self,
        url: &Url,
        deadline: Instant,
        plan: &BrowserPlan,
    ) -> Result<(String, BrowserOutput), BrowserError> {
        let tier = self.tier()?;
        let permit = tier.sessions.reserve()?;
        within(deadline, async {
            let _admission = tier.fetcher.admit(url, deadline).await?;
            let policy = TabPolicy {
                origin: url.origin(),
                headers: Vec::new(),
                block_assets: false,
                proxy_route: false,
            };
            let tab = tier.pool.lease(policy, deadline, false).await?;
            actions::navigate(&tab.page, url, deadline).await?;
            let output = tier.visit(&tab, url, plan, deadline).await?;
            Ok((tier.sessions.insert(permit, tab), output))
        })
        .await
    }

    /// Runs one step in a session. Steps of one session run one at a time; host admission is
    /// taken for the page's current URL for the length of the step. A step that runs out of
    /// time leaves the page mid-action, so it ends the session.
    pub async fn session_step(
        &self,
        id: &str,
        plan: &BrowserPlan,
        deadline: Instant,
    ) -> Result<BrowserOutput, BrowserError> {
        let tier = self.tier()?;
        let (tab, _session) = tier.sessions.acquire(id, deadline).await?;
        let output = within(deadline, async {
            let current = actions::location(&tab.page, deadline)
                .await
                .ok_or(BrowserError::Unavailable("session page unreadable"))?;
            let _admission = tier.fetcher.admit(&current, deadline).await?;
            tier.visit(&tab, &current, plan, deadline).await
        })
        .await;
        if matches!(output, Err(BrowserError::Fetch(FetchError::Timeout))) {
            tier.sessions.remove(id);
        }
        output
    }

    pub fn close_session(&self, id: &str) -> Result<(), BrowserError> {
        if self.tier()?.sessions.remove(id) {
            Ok(())
        } else {
            Err(BrowserError::SessionNotFound)
        }
    }

    fn tier(&self) -> Result<&Tier, BrowserError> {
        self.0.as_deref().ok_or(BrowserError::Disabled)
    }
}

impl Tier {
    /// After navigation: `waitFor`, actions, the format screenshot, then the final DOM, in
    /// Firecrawl's order. Screenshots are stored as expiring artifacts.
    async fn visit(
        &self,
        tab: &Tab,
        requested: &Url,
        plan: &BrowserPlan,
        deadline: Instant,
    ) -> Result<BrowserOutput, BrowserError> {
        if !plan.wait_for.is_zero() {
            let until = Instant::now() + plan.wait_for;
            sleep_until(until.min(deadline)).await;
            if until > deadline {
                return Err(FetchError::Timeout.into());
            }
        }
        let mut captured = Captured::default();
        actions::run(&tab.page, &tab.net, &plan.actions, deadline, &mut captured).await?;
        let screenshot = match plan.screenshot {
            Some(shot) => Some(actions::screenshot(&tab.page, shot, deadline).await?),
            None => None,
        };
        let snapshot = actions::snapshot(&tab.page, deadline).await?;
        if snapshot.html.len() > self.max_bytes {
            return Err(FetchError::TooLarge.into());
        }
        // Never invent a status: an unknown one is a failed visit.
        let (status, headers) = tab
            .net
            .response()
            .filter(|(status, _)| *status != 0)
            .ok_or(BrowserError::Unavailable("navigation status unknown"))?;
        let final_url = Url::parse(&snapshot.url).unwrap_or_else(|_| requested.clone());
        let screenshot = match screenshot {
            Some((bytes, media_type)) => Some(self.publish(bytes, media_type).await?),
            None => None,
        };
        let actions = if plan.actions.is_empty() {
            None
        } else {
            let mut screenshots = Vec::with_capacity(captured.screenshots.len());
            for (bytes, media_type) in captured.screenshots {
                screenshots.push(self.publish(bytes, media_type).await?);
            }
            Some(ActionsOutput {
                screenshots,
                scrapes: captured.scrapes,
                javascript_returns: captured.javascript_returns,
                pdfs: Vec::new(),
            })
        };
        let capture = Fetched {
            route: self
                .routes
                .route_for(final_url.host_str().unwrap_or_default()),
            requested_url: requested.clone(),
            final_url,
            status,
            headers,
            content_type: Some("text/html; charset=utf-8".into()),
            body: snapshot.html.into(),
            fetched_at: OffsetDateTime::now_utc(),
        };
        Ok(BrowserOutput {
            capture,
            extras: BrowserExtras {
                screenshot,
                actions,
            },
        })
    }

    /// Stores a screenshot and returns its capability URL; the ID is never logged.
    async fn publish(
        &self,
        bytes: Vec<u8>,
        media_type: &'static str,
    ) -> Result<String, BrowserError> {
        let id = self
            .store
            .put_artifact(bytes, media_type, ARTIFACT_TTL, ARTIFACT_QUOTA_BYTES)
            .await
            .map_err(|error| match error {
                StoreError::Quota => BrowserError::Limit("screenshot storage quota exhausted"),
                error => {
                    tracing::error!(%error, "screenshot not stored");
                    BrowserError::Storage
                }
            })?;
        self.artifact_base
            .join(&id)
            .map(String::from)
            .map_err(|_| BrowserError::Unavailable("invalid artifact URL"))
    }
}

/// Bounds a whole browser workflow, storage included; dropping it on expiry releases its tab.
async fn within<T>(
    deadline: Instant,
    work: impl Future<Output = Result<T, BrowserError>>,
) -> Result<T, BrowserError> {
    timeout_at(deadline, work)
        .await
        .map_err(|_| FetchError::Timeout)?
}

fn spawn_reaper(tier: Weak<Tier>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(sessions::REAP_EVERY).await;
            let Some(tier) = tier.upgrade() else {
                return;
            };
            tier.sessions.reap();
        }
    })
}

fn spawn_sweeper(store: Store) {
    tokio::spawn(async move {
        loop {
            match store.sweep_artifacts().await {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "expired artifact files removed"),
                Err(error) => tracing::warn!(%error, "artifact sweep failed"),
            }
            tokio::time::sleep(ARTIFACT_SWEEP_EVERY).await;
        }
    });
}
