//! One supervised Chrome: launch or connect, a fresh isolated context per tab, retirement,
//! crash backoff, and a memory ceiling.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, PoisonError, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use chromiumoxide::{
    Browser, BrowserConfig, Page,
    cdp::browser_protocol::{
        browser::{
            BrowserContextId, CloseParams, SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
        },
        target::{CreateBrowserContextParams, CreateTargetParams},
    },
    handler::{HandlerConfig, viewport::Viewport},
};
use futures::StreamExt;
use tokio::{
    sync::{Mutex, Notify, OnceCell, OwnedSemaphorePermit, RwLock, Semaphore},
    task::JoinHandle,
    time::{Instant, sleep, timeout, timeout_at},
};

use super::{
    BrowserError,
    network::{Interceptor, TabNet, TabPolicy},
};
use crate::net::FetchError;

/// Requests allowed to wait for a tab; one more is refused at once.
const QUEUE: usize = 16;
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
/// chromiumoxide's own command eviction; ours is tighter, except navigation, which may take
/// the whole request deadline.
const LIBRARY_TIMEOUT: Duration = Duration::from_secs(120);
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const FAILURE_LIMIT: usize = 3;
const COOLDOWN: Duration = Duration::from_secs(30);
const CONNECT_RETRY: Duration = Duration::from_millis(250);
const MEMORY_SAMPLE: Duration = Duration::from_secs(1);
pub(super) const VIEWPORT: (u32, u32) = (1280, 800);

pub(super) enum Chrome {
    /// Launch this executable as a child process.
    Launch(PathBuf),
    /// Connect to an external Chrome's DevTools endpoint.
    Connect(String),
}

pub(super) struct PoolConfig {
    pub chrome: Chrome,
    pub tabs: usize,
    pub recycle_after: usize,
    pub memory_limit: u64,
    pub user_agent: String,
    pub proxy: SocketAddr,
    pub token: String,
}

pub(super) struct Pool {
    cfg: PoolConfig,
    slots: Arc<Semaphore>,
    queue: Arc<Semaphore>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    instance: Option<Arc<Instance>>,
    failures: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
}

/// One live Chrome connection. Tabs and sessions hold it; replacing it never touches them.
struct Instance {
    browser: Arc<RwLock<Browser>>,
    net: Arc<Interceptor>,
    /// Drives the DevTools connection; it ends when Chrome closes the connection.
    handler: std::sync::Mutex<Option<JoinHandle<()>>>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
    user_data_dir: Option<PathBuf>,
    served: AtomicUsize,
    /// One-shot visits in flight; retirement waits for them, not for sessions.
    active: AtomicUsize,
    retiring: AtomicBool,
    dead: AtomicBool,
    /// Died on its own (connection, memory, cleanup) rather than by retirement or shutdown.
    crashed: AtomicBool,
    /// Set once teardown finishes; every closer waits for it.
    closed: OnceCell<()>,
    idle: Notify,
}

impl Instance {
    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Marks the instance failed and tears it down; affected visits fail, none is replayed.
    fn fail(self: &Arc<Self>, reason: &'static str) {
        if !self.dead.swap(true, Ordering::AcqRel) {
            self.crashed.store(true, Ordering::Release);
            tracing::warn!(reason, "browser instance failed");
            tokio::spawn(Arc::clone(self).close(false));
        }
    }

    /// Idempotent; every caller returns only after teardown finishes. Graceful asks Chrome to
    /// exit first (an external one is then restarted by systemd); a child Chrome is killed
    /// regardless, an external one only disconnected.
    async fn close(self: Arc<Self>, graceful: bool) {
        self.closed.get_or_init(|| self.teardown(graceful)).await;
    }

    async fn teardown(&self, graceful: bool) {
        self.dead.store(true, Ordering::Release);
        self.idle.notify_waiters();
        let handler = self
            .handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut handler) = handler {
            if graceful {
                let browser = self.browser.read().await;
                let _ = timeout(CLEANUP_TIMEOUT, browser.execute(CloseParams::default())).await;
                drop(browser);
                // Chrome drops the connection as it exits; waiting for that keeps the next
                // connection from reaching a dying external Chrome.
                let _ = timeout(CLEANUP_TIMEOUT, &mut handler).await;
            }
            // Dropping the handler fails every pending command at once, releasing read locks.
            handler.abort();
        }
        for task in self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
        if let Ok(mut browser) = timeout(CLEANUP_TIMEOUT, self.browser.write()).await
            && browser.get_mut_child().is_some()
        {
            let _ = browser.kill().await;
            let _ = browser.wait().await;
        }
        if let Some(dir) = self.user_data_dir.clone() {
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        }
    }
}

/// A leased tab in its own browser context. Dropping it disposes the context within the
/// cleanup bound and only then releases the slot; a stuck cleanup fails the instance.
pub(super) struct Tab {
    pub page: Page,
    pub net: Arc<TabNet>,
    lease: Lease,
}

impl Tab {
    pub(super) fn is_dead(&self) -> bool {
        self.lease.instance.is_dead()
    }
}

struct Lease {
    instance: Arc<Instance>,
    /// A context was requested but Chrome's reply never arrived: one may exist unknown to us.
    context_pending: bool,
    context: Option<BrowserContextId>,
    target: Option<String>,
    events: Option<JoinHandle<()>>,
    slot: Option<OwnedSemaphorePermit>,
    one_shot: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(events) = self.events.take() {
            events.abort();
        }
        let (instance, context_pending, context, target, slot, one_shot) = (
            Arc::clone(&self.instance),
            self.context_pending,
            self.context.take(),
            self.target.take(),
            self.slot.take(),
            self.one_shot,
        );
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if context_pending {
                // Only disconnecting disposes a context we cannot name (dispose on detach).
                instance.fail("browser context creation abandoned");
            }
            if let Some(target) = target {
                instance.net.unregister(&target);
            }
            if let Some(context) = context {
                let disposed = timeout(CLEANUP_TIMEOUT, async {
                    instance
                        .browser
                        .read()
                        .await
                        .dispose_browser_context(context)
                        .await
                })
                .await;
                if !matches!(disposed, Ok(Ok(()))) && !instance.is_dead() {
                    instance.fail("browser context cleanup failed");
                }
            }
            drop(slot);
            if one_shot && instance.active.fetch_sub(1, Ordering::AcqRel) == 1 {
                instance.idle.notify_waiters();
                if instance.retiring.load(Ordering::Acquire) {
                    instance.close(true).await;
                }
            }
        });
    }
}

impl Pool {
    pub(super) fn new(cfg: PoolConfig) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(cfg.tabs)),
            queue: Arc::new(Semaphore::new(QUEUE)),
            state: Mutex::default(),
            cfg,
        }
    }

    /// Waits for a tab slot within the deadline (queueing consumes it), then opens a tab in a
    /// fresh context. One-shot tabs count toward retirement draining; session tabs do not.
    pub(super) async fn lease(
        &self,
        policy: TabPolicy,
        deadline: Instant,
        one_shot: bool,
    ) -> Result<Tab, BrowserError> {
        let slot = {
            let _queued = Arc::clone(&self.queue)
                .try_acquire_owned()
                .map_err(|_| FetchError::Overloaded)?;
            timeout_at(deadline, Arc::clone(&self.slots).acquire_owned())
                .await
                .map_err(|_| FetchError::Overloaded)?
                .map_err(|_| BrowserError::Unavailable("browser tier stopped"))?
        };
        let instance = self.instance(deadline).await?;
        if instance.served.fetch_add(1, Ordering::AcqRel) + 1 >= self.cfg.recycle_after {
            instance.retiring.store(true, Ordering::Release);
        }
        if one_shot {
            instance.active.fetch_add(1, Ordering::AcqRel);
        }
        let mut lease = Lease {
            instance: Arc::clone(&instance),
            context_pending: true,
            context: None,
            target: None,
            events: None,
            slot: Some(slot),
            one_shot,
        };
        let browser = &instance.browser;
        // The outer error is a timeout; dropping this future mid-reply leaves the lease pending.
        let created = command(deadline, async {
            let params = CreateBrowserContextParams {
                dispose_on_detach: Some(true),
                ..CreateBrowserContextParams::default()
            };
            Ok(browser.read().await.create_browser_context(params).await)
        })
        .await?;
        lease.context_pending = false;
        let context = created.map_err(|_| BrowserError::Unavailable("browser command failed"))?;
        lease.context = Some(context.clone());
        let page = command(deadline, async {
            let browser = browser.read().await;
            browser
                .execute(SetDownloadBehaviorParams {
                    behavior: SetDownloadBehaviorBehavior::Deny,
                    browser_context_id: Some(context.clone()),
                    download_path: None,
                    events_enabled: None,
                })
                .await?;
            let mut target = CreateTargetParams::new("about:blank");
            target.browser_context_id = Some(context);
            let page = browser.new_page(target).await?;
            // Set per tab, so an external Chrome declares the same identity as plain fetches.
            page.set_user_agent(self.cfg.user_agent.as_str()).await?;
            Ok(page)
        })
        .await?;
        let target: &str = page.target_id().as_ref();
        lease.target = Some(target.to_owned());
        let net = instance.net.register(target, policy);
        lease.events = Some(command(deadline, net.listen(&page)).await?);
        Ok(Tab { page, net, lease })
    }

    /// The ready instance, after draining a retiring one or replacing a dead one.
    async fn instance(&self, deadline: Instant) -> Result<Arc<Instance>, BrowserError> {
        let mut state = timeout_at(deadline, self.state.lock())
            .await
            .map_err(|_| FetchError::Overloaded)?;
        if let Some(current) = state.instance.clone() {
            if !current.is_dead() && !current.retiring.load(Ordering::Acquire) {
                return Ok(current);
            }
            if current.crashed.load(Ordering::Acquire) {
                record_failure(&mut state);
            }
            // Retirement drains one-shot visits first; the last one to finish closes it too.
            loop {
                let idle = current.idle.notified();
                tokio::pin!(idle);
                idle.as_mut().enable();
                if current.is_dead() || current.active.load(Ordering::Acquire) == 0 {
                    break;
                }
                timeout_at(deadline, idle)
                    .await
                    .map_err(|_| FetchError::Overloaded)?;
            }
            Arc::clone(&current).close(true).await;
            state.instance = None;
        }
        let now = Instant::now();
        if state.cooldown_until.is_some_and(|until| until > now) {
            return Err(BrowserError::Unavailable(
                "browser restarting after repeated failures",
            ));
        }
        match self
            .start_instance(deadline.min(now + LAUNCH_TIMEOUT))
            .await
        {
            Ok(instance) => {
                state.instance = Some(Arc::clone(&instance));
                Ok(instance)
            }
            Err(error) => {
                record_failure(&mut state);
                Err(error)
            }
        }
    }

    async fn start_instance(&self, deadline: Instant) -> Result<Arc<Instance>, BrowserError> {
        let handler_cfg = HandlerConfig {
            ignore_https_errors: false,
            viewport: Some(viewport()),
            request_timeout: LIBRARY_TIMEOUT,
            ..HandlerConfig::default()
        };
        let (mut browser, mut handler, user_data_dir) = match &self.cfg.chrome {
            Chrome::Launch(path) => {
                let dir = std::env::temp_dir()
                    .join(format!("sonar-chrome-{}", uuid::Uuid::new_v4().simple()));
                let config = chrome_flags(self.cfg.proxy)
                    .into_iter()
                    .fold(
                        BrowserConfig::builder(),
                        |builder, (key, value)| match value {
                            Some(value) => builder.arg((key, value.as_str())),
                            None => builder.arg(key),
                        },
                    )
                    .chrome_executable(path)
                    .new_headless_mode()
                    .respect_https_errors()
                    .user_data_dir(&dir)
                    .window_size(VIEWPORT.0, VIEWPORT.1)
                    .viewport(viewport())
                    .launch_timeout(LAUNCH_TIMEOUT)
                    .request_timeout(LIBRARY_TIMEOUT)
                    .build()
                    .map_err(|_| BrowserError::Unavailable("invalid Chrome configuration"))?;
                // A dropped launch kills its child (chromiumoxide sets kill-on-drop).
                let launched = match timeout_at(deadline, Browser::launch(config)).await {
                    Ok(Ok(launched)) => Ok(launched),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "Chrome launch failed");
                        Err(BrowserError::Unavailable("Chrome failed to launch"))
                    }
                    Err(_) => Err(BrowserError::Unavailable("Chrome launch timed out")),
                };
                let (browser, handler) = match launched {
                    Ok(launched) => launched,
                    Err(error) => {
                        let _ =
                            tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
                        return Err(error);
                    }
                };
                (browser, handler, Some(dir))
            }
            Chrome::Connect(endpoint) => loop {
                let attempt = timeout_at(
                    deadline,
                    Browser::connect_with_config(endpoint.clone(), handler_cfg.clone()),
                )
                .await
                .map_err(|_| BrowserError::Unavailable("Chrome DevTools endpoint unreachable"))?;
                match attempt {
                    Ok((browser, handler)) => break (browser, handler, None),
                    // External Chrome may be restarting under systemd; retry until the deadline.
                    Err(_) => sleep(CONNECT_RETRY).await,
                }
            },
        };
        let pid = browser
            .get_mut_child()
            .and_then(|child| child.as_mut_inner().id());
        let external = user_data_dir.is_none();
        let proxy_origin = format!("http://{}", self.cfg.proxy);
        let instance = Arc::new(Instance {
            browser: Arc::new(RwLock::new(browser)),
            net: Arc::new(Interceptor::new(proxy_origin, self.cfg.token.clone())),
            handler: std::sync::Mutex::default(),
            tasks: std::sync::Mutex::default(),
            user_data_dir,
            served: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            retiring: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            crashed: AtomicBool::new(false),
            closed: OnceCell::new(),
            idle: Notify::new(),
        });
        let weak = Arc::downgrade(&instance);
        let connection = tokio::spawn(async move {
            // Any handler error is a broken connection; unknown events are ignored inside it.
            while let Some(Ok(())) = handler.next().await {}
            if let Some(instance) = weak.upgrade() {
                instance.fail("browser connection lost");
            }
        });
        *instance
            .handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(connection);
        if let Some(pid) = pid {
            let watch = watch_memory(Arc::downgrade(&instance), pid, self.cfg.memory_limit);
            instance
                .tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(tokio::spawn(watch));
        }
        let failing = Arc::downgrade(&instance);
        let on_lost = Arc::new(move || {
            if let Some(instance) = failing.upgrade() {
                instance.fail("browser left a request paused");
            }
        });
        let started = timeout_at(
            deadline,
            instance.net.start(Arc::clone(&instance.browser), on_lost),
        )
        .await;
        match started {
            Ok(Ok(task)) => {
                instance
                    .tasks
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(task);
                tracing::info!(external, "browser instance ready");
                Ok(instance)
            }
            _ => {
                Arc::clone(&instance).close(false).await;
                Err(BrowserError::Unavailable(
                    "browser interception setup failed",
                ))
            }
        }
    }

    /// Fails waiting requests and stops Chrome; an external Chrome keeps running.
    pub(super) async fn shutdown(&self) {
        self.slots.close();
        let instance = self.state.lock().await.instance.take();
        if let Some(instance) = instance {
            instance.close(false).await;
        }
    }
}

fn record_failure(state: &mut State) {
    let now = Instant::now();
    state.failures.push_back(now);
    while state
        .failures
        .front()
        .is_some_and(|at| now.duration_since(*at) > FAILURE_WINDOW)
    {
        state.failures.pop_front();
    }
    if state.failures.len() >= FAILURE_LIMIT {
        tracing::warn!("repeated browser failures; cooling down");
        state.failures.clear();
        state.cooldown_until = Some(now + COOLDOWN);
    }
}

/// Runs one DevTools exchange within the command timeout and the request deadline.
pub(super) async fn command<T>(
    deadline: Instant,
    exchange: impl Future<Output = chromiumoxide::Result<T>>,
) -> Result<T, BrowserError> {
    let limit = deadline.min(Instant::now() + COMMAND_TIMEOUT);
    match timeout_at(limit, exchange).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(BrowserError::Unavailable("browser command failed")),
        Err(_) if limit == deadline => Err(FetchError::Timeout.into()),
        Err(_) => Err(BrowserError::Unavailable("browser command timed out")),
    }
}

fn viewport() -> Viewport {
    Viewport {
        width: VIEWPORT.0,
        height: VIEWPORT.1,
        ..Viewport::default()
    }
}

/// Flags shared by the child Chrome and the documented external unit
/// (`deploy/sonar-chrome.service`): every connection goes through the guarded forward proxy,
/// name resolution inside Chrome fails, and QUIC and non-proxied WebRTC UDP are off. The user
/// agent is set per tab instead.
fn chrome_flags(proxy: SocketAddr) -> Vec<(&'static str, Option<String>)> {
    let value = |v: &str| Some(v.to_owned());
    vec![
        ("proxy-server", Some(format!("http://{proxy}"))),
        ("proxy-bypass-list", value("<-loopback>")),
        (
            "host-resolver-rules",
            value("MAP * ~NOTFOUND , EXCLUDE 127.0.0.1"),
        ),
        (
            "force-webrtc-ip-handling-policy",
            value("disable_non_proxied_udp"),
        ),
        ("disable-quic", None),
        ("disable-component-update", None),
        ("disable-domain-reliability", None),
        ("no-pings", None),
        ("no-default-browser-check", None),
        (
            "disable-features",
            value("Translate,MediaRouter,OptimizationHints,AutofillServerCommunication"),
        ),
    ]
}

/// Kills the instance when Chrome's process tree passes the memory ceiling. Stops quietly
/// where `/proc` is unavailable.
async fn watch_memory(instance: Weak<Instance>, pid: u32, limit: u64) {
    loop {
        sleep(MEMORY_SAMPLE).await;
        let Some(current) = instance.upgrade() else {
            return;
        };
        if current.is_dead() {
            return;
        }
        let sampled = tokio::task::spawn_blocking(move || tree_rss(pid)).await;
        match sampled {
            Ok(Some(bytes)) if bytes > limit => {
                current.fail("browser memory ceiling exceeded");
                return;
            }
            Ok(Some(_)) => {}
            _ => return,
        }
    }
}

/// Resident memory of `root` and its descendants. Shared pages count once per process, so
/// this overestimates; that errs toward restarting early.
fn tree_rss(root: u32) -> Option<u64> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The command name may contain spaces or parentheses; fields resume after the last ')'.
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|ppid| ppid.parse().ok());
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(pid);
        }
    }
    let (mut total, mut pending) = (0, vec![root]);
    while let Some(pid) = pending.pop() {
        total += resident_bytes(pid).unwrap_or(0);
        pending.extend(children.get(&pid).into_iter().flatten());
    }
    Some(total)
}

fn resident_bytes(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?;
    Some(kib.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()? * 1024)
}
