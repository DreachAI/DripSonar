//! Browser-wide request interception: proxy authentication, per-tab request policy, and
//! popup refusal.
//!
//! `Fetch` is enabled on the browser session rather than per page, so its events reach only
//! this module: chromiumoxide's per-page network manager would otherwise answer every auth
//! challenge itself and could hand the proxy token to a site. Every paused request gets
//! exactly one continue or fail.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use chromiumoxide::{
    Browser,
    cdp::browser_protocol::{
        fetch::{
            AuthChallengeResponse, AuthChallengeResponseResponse, AuthChallengeSource,
            ContinueRequestParams, ContinueWithAuthParams, EnableParams, EventAuthRequired,
            EventRequestPaused, FailRequestParams, HeaderEntry, RequestPattern,
        },
        network::{ErrorReason, ResourceType},
        target::{CloseTargetParams, EventTargetCreated},
    },
};
use futures::StreamExt;
use tokio::{sync::RwLock, task::JoinHandle};
use url::{Origin, Url};

/// Redirects followed per top-level navigation, as for plain fetches.
const MAX_REDIRECTS: usize = 5;
/// Remembered proxy-auth attempts; a second challenge for the same request is cancelled.
const MAX_AUTH_ATTEMPTS: usize = 1024;

/// Request policy for one tab's main frame.
pub(super) struct TabPolicy {
    /// Caller headers are sent only to this origin.
    pub origin: Origin,
    pub headers: Vec<(String, String)>,
    /// Text-only visits refuse images, fonts, and media.
    pub block_assets: bool,
}

pub(super) struct TabNet {
    policy: TabPolicy,
    /// Top-level document requests seen, so actions can tell when they started a navigation.
    navigations: AtomicU64,
    document: Mutex<Document>,
}

#[derive(Default)]
struct Document {
    url: Option<Url>,
    redirects: usize,
}

impl TabNet {
    pub(super) fn navigations(&self) -> u64 {
        self.navigations.load(Ordering::Acquire)
    }

    /// The newest top-level document URL requested, including redirect hops.
    pub(super) fn document_url(&self) -> Option<Url> {
        self.lock().url.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Document> {
        self.document.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Refuses a sixth redirect and HTTPS-to-HTTP downgrades.
    fn on_document(&self, url: &Url, redirected: bool) -> bool {
        let mut document = self.lock();
        if redirected {
            document.redirects += 1;
            let downgrade = document
                .url
                .as_ref()
                .is_some_and(|from| from.scheme() == "https" && url.scheme() == "http");
            if document.redirects > MAX_REDIRECTS || downgrade {
                return false;
            }
        } else {
            document.redirects = 0;
        }
        document.url = Some(url.clone());
        self.navigations.fetch_add(1, Ordering::AcqRel);
        true
    }
}

enum Reply {
    Continue(ContinueRequestParams),
    Fail(FailRequestParams),
    Auth(ContinueWithAuthParams),
    Close(CloseTargetParams),
}

pub(super) struct Interceptor {
    proxy_origin: String,
    token: String,
    /// Keyed by target ID, which is also the tab's main frame ID.
    tabs: Mutex<HashMap<String, Arc<TabNet>>>,
    visual_tabs: AtomicUsize,
    auth_attempts: Mutex<HashSet<String>>,
}

impl Interceptor {
    pub(super) fn new(proxy_origin: String, token: String) -> Self {
        Self {
            proxy_origin,
            token,
            tabs: Mutex::default(),
            visual_tabs: AtomicUsize::new(0),
            auth_attempts: Mutex::default(),
        }
    }

    /// Subscribes before enabling interception, so no event is missed, then answers events
    /// until the browser connection ends.
    pub(super) async fn start(
        self: &Arc<Self>,
        browser: Arc<RwLock<Browser>>,
    ) -> chromiumoxide::Result<JoinHandle<()>> {
        let (mut paused, mut auth, mut created) = {
            let handle = browser.read().await;
            let streams = (
                handle.event_listener::<EventRequestPaused>().await?,
                handle.event_listener::<EventAuthRequired>().await?,
                handle.event_listener::<EventTargetCreated>().await?,
            );
            let pattern = RequestPattern {
                url_pattern: Some("*".into()),
                ..RequestPattern::default()
            };
            handle
                .execute(EnableParams {
                    patterns: Some(vec![pattern]),
                    handle_auth_requests: Some(true),
                })
                .await?;
            streams
        };
        let this = Arc::clone(self);
        Ok(tokio::spawn(async move {
            loop {
                let reply = tokio::select! {
                    Some(event) = paused.next() => this.on_paused(&event),
                    Some(event) = auth.next() => this.on_auth(&event),
                    Some(event) = created.next() => match on_created(&event) {
                        Some(reply) => reply,
                        None => continue,
                    },
                    else => break,
                };
                let browser = Arc::clone(&browser);
                // A failed reply means the target or connection is gone; nothing waits on it.
                tokio::spawn(async move {
                    let browser = browser.read().await;
                    let _ = match reply {
                        Reply::Continue(params) => browser.execute(params).await.map(drop),
                        Reply::Fail(params) => browser.execute(params).await.map(drop),
                        Reply::Auth(params) => browser.execute(params).await.map(drop),
                        Reply::Close(params) => browser.execute(params).await.map(drop),
                    };
                });
            }
        }))
    }

    pub(super) fn register(&self, target_id: &str, policy: TabPolicy) -> Arc<TabNet> {
        if !policy.block_assets {
            self.visual_tabs.fetch_add(1, Ordering::AcqRel);
        }
        let net = Arc::new(TabNet {
            policy,
            navigations: AtomicU64::new(0),
            document: Mutex::default(),
        });
        self.lock_tabs()
            .insert(target_id.to_owned(), Arc::clone(&net));
        net
    }

    pub(super) fn unregister(&self, target_id: &str) {
        let removed = self.lock_tabs().remove(target_id);
        if removed.is_some_and(|net| !net.policy.block_assets) {
            self.visual_tabs.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn lock_tabs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<TabNet>>> {
        self.tabs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn on_paused(&self, event: &EventRequestPaused) -> Reply {
        let id = event.request_id.clone();
        let fail = || {
            Reply::Fail(FailRequestParams::new(
                id.clone(),
                ErrorReason::BlockedByClient,
            ))
        };
        let Ok(url) = Url::parse(&event.request.url) else {
            return fail();
        };
        if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
            return fail();
        }
        let asset = matches!(
            event.resource_type,
            ResourceType::Image | ResourceType::Font | ResourceType::Media
        );
        let tab = self.lock_tabs().get(event.frame_id.as_ref()).cloned();
        let Some(tab) = tab else {
            // Subframes, workers, and refused popups: no caller headers; assets only while a
            // visual visit runs, since a screenshot may include a subframe.
            if asset && self.visual_tabs.load(Ordering::Acquire) == 0 {
                return fail();
            }
            return Reply::Continue(ContinueRequestParams::new(id));
        };
        if event.resource_type == ResourceType::Document
            && !tab.on_document(&url, event.redirected_request_id.is_some())
        {
            return fail();
        }
        if asset && tab.policy.block_assets {
            return fail();
        }
        let mut params = ContinueRequestParams::new(id);
        if !tab.policy.headers.is_empty() && url.origin() == tab.policy.origin {
            params.headers = Some(merge_headers(
                event.request.headers.inner(),
                &tab.policy.headers,
            ));
        }
        Reply::Continue(params)
    }

    /// Answers only our own proxy's challenge, once per request; site auth is cancelled.
    fn on_auth(&self, event: &EventAuthRequired) -> Reply {
        let challenge = &event.auth_challenge;
        let ours = challenge.source == Some(AuthChallengeSource::Proxy)
            && challenge.origin == self.proxy_origin
            && {
                let mut attempts = self
                    .auth_attempts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if attempts.len() >= MAX_AUTH_ATTEMPTS {
                    attempts.clear();
                }
                attempts.insert(event.request_id.as_ref().to_owned())
            };
        let response = if ours {
            AuthChallengeResponse {
                response: AuthChallengeResponseResponse::ProvideCredentials,
                username: Some("sonar".into()),
                password: Some(self.token.clone()),
            }
        } else {
            AuthChallengeResponse::new(AuthChallengeResponseResponse::CancelAuth)
        };
        Reply::Auth(ContinueWithAuthParams::new(
            event.request_id.clone(),
            response,
        ))
    }
}

/// Pages opened by a page (popups, `target=_blank`) are closed; every visit has one tab.
fn on_created(event: &EventTargetCreated) -> Option<Reply> {
    let info = &event.target_info;
    (info.r#type == "page" && info.opener_id.is_some())
        .then(|| Reply::Close(CloseTargetParams::new(info.target_id.clone())))
}

/// The request's own headers with caller headers replacing any of the same name.
fn merge_headers(original: &serde_json::Value, caller: &[(String, String)]) -> Vec<HeaderEntry> {
    let overridden = |name: &str| caller.iter().any(|(c, _)| c.eq_ignore_ascii_case(name));
    let mut headers: Vec<HeaderEntry> = original
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| !overridden(name))
        .filter_map(|(name, value)| Some(HeaderEntry::new(name.clone(), value.as_str()?)))
        .collect();
    headers.extend(
        caller
            .iter()
            .map(|(name, value)| HeaderEntry::new(name.clone(), value.clone())),
    );
    headers
}
