//! Browser-wide request interception: proxy authentication, per-tab request policy, and
//! popup refusal; plus each tab's main-frame commits and document responses.
//!
//! `Fetch` is enabled on the browser session rather than per page, so its events reach only
//! this module: chromiumoxide's per-page network manager would otherwise answer every auth
//! challenge itself and could hand the proxy token to a site. Every paused request gets
//! exactly one continue or fail; a request Chrome cannot be made to release fails the instance.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use chromiumoxide::{
    Browser, Command, Page,
    cdp::browser_protocol::{
        fetch::{
            AuthChallengeResponse, AuthChallengeResponseResponse, AuthChallengeSource,
            ContinueRequestParams, ContinueWithAuthParams, EnableParams, EventAuthRequired,
            EventRequestPaused, FailRequestParams, HeaderEntry, RequestPattern,
        },
        network::{ErrorReason, EventResponseReceived, ResourceType},
        page::EventFrameNavigated,
        target::{CloseTargetParams, EventTargetCreated},
    },
    error::CdpError,
};
use futures::StreamExt;
use tokio::{sync::RwLock, task::JoinHandle, time::timeout};
use url::{Origin, Url};

/// Redirects followed per top-level navigation, as for plain fetches.
const MAX_REDIRECTS: usize = 5;
/// Remembered proxy-auth attempts; a second challenge for the same request is cancelled.
const MAX_AUTH_ATTEMPTS: usize = 1024;
/// How long Chrome gets to accept one interception reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Main-frame document responses remembered per tab; older ones can no longer commit.
const MAX_RESPONSES: usize = 8;
/// Caller headers Chrome manages itself or that would address our proxy; never forwarded.
const UNSENDABLE: [&str; 10] = [
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

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
    document: Mutex<Document>,
}

#[derive(Default)]
struct Document {
    url: Option<Url>,
    redirects: usize,
    /// Top-level document requests seen, including redirect hops.
    requests: u64,
    /// Loader ID of the newest main-frame commit.
    committed: Option<String>,
    /// Recent main-frame document responses, newest last.
    responses: VecDeque<Response>,
}

struct Response {
    loader: String,
    status: u16,
    headers: Vec<(String, String)>,
}

/// Where the tab's navigation stands; any change means a navigation started.
#[derive(Clone, PartialEq)]
pub(super) struct Mark {
    requests: u64,
    pub committed: Option<String>,
}

impl TabNet {
    pub(super) fn mark(&self) -> Mark {
        let document = self.lock();
        Mark {
            requests: document.requests,
            committed: document.committed.clone(),
        }
    }

    /// Status and headers of the committed main-frame document, when Chrome reported them.
    pub(super) fn response(&self) -> Option<(u16, Vec<(String, String)>)> {
        let document = self.lock();
        let committed = document.committed.as_deref()?;
        let response = document
            .responses
            .iter()
            .rev()
            .find(|response| response.loader == committed)?;
        Some((response.status, response.headers.clone()))
    }

    /// Records main-frame document responses and commits from the page's own events until the
    /// page goes away. Subscribed before any navigation, so none is missed.
    pub(super) async fn listen(
        self: &Arc<Self>,
        page: &Page,
    ) -> chromiumoxide::Result<JoinHandle<()>> {
        let frame = page.target_id().as_ref().to_owned();
        let mut responses = page.event_listener::<EventResponseReceived>().await?;
        let mut commits = page.event_listener::<EventFrameNavigated>().await?;
        let this = Arc::clone(self);
        Ok(tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = responses.next() => {
                        if event.r#type == ResourceType::Document
                            && event.frame_id.as_ref().is_some_and(|id| id.as_ref() == frame)
                        {
                            this.on_response(&event);
                        }
                    }
                    Some(event) = commits.next() => {
                        if event.frame.parent_id.is_none() && event.frame.id.as_ref() == frame {
                            this.lock().committed = Some(event.frame.loader_id.as_ref().to_owned());
                        }
                    }
                    else => break,
                }
            }
        }))
    }

    fn on_response(&self, event: &EventResponseReceived) {
        // Chrome joins repeated headers with newlines.
        let headers = event
            .response
            .headers
            .inner()
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(name, value)| Some((name.to_ascii_lowercase(), value.as_str()?)))
            .flat_map(|(name, value)| {
                value
                    .split('\n')
                    .map(move |value| (name.clone(), value.to_owned()))
            })
            .collect();
        let mut document = self.lock();
        if document.responses.len() >= MAX_RESPONSES {
            document.responses.pop_front();
        }
        document.responses.push_back(Response {
            loader: event.loader_id.as_ref().to_owned(),
            // An out-of-range status stays unknown (0), which fails the visit.
            status: u16::try_from(event.response.status).unwrap_or(0),
            headers,
        });
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
        document.requests += 1;
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
    /// until the browser connection ends. `on_lost` runs when a paused request may stay stuck.
    pub(super) async fn start(
        self: &Arc<Self>,
        browser: Arc<RwLock<Browser>>,
        on_lost: Arc<dyn Fn() + Send + Sync>,
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
                let (browser, on_lost) = (Arc::clone(&browser), Arc::clone(&on_lost));
                tokio::spawn(async move {
                    let outcome = match reply {
                        Reply::Continue(params) => {
                            let id = params.request_id.clone();
                            match send(&browser, params).await {
                                Outcome::Done => Outcome::Done,
                                // Refused (say, a header Chrome will not send) or unanswered:
                                // fail it instead, so the page never waits on it.
                                _ => {
                                    let fail = FailRequestParams::new(id, ErrorReason::Failed);
                                    send(&browser, fail).await
                                }
                            }
                        }
                        Reply::Fail(params) => send(&browser, params).await,
                        Reply::Auth(params) => send(&browser, params).await,
                        // A popup may close itself first; one left open has no caller headers.
                        Reply::Close(params) => {
                            let _ = send(&browser, params).await;
                            Outcome::Done
                        }
                    };
                    if outcome == Outcome::Lost {
                        on_lost();
                    }
                });
            }
        }))
    }

    pub(super) fn register(&self, target_id: &str, mut policy: TabPolicy) -> Arc<TabNet> {
        if !policy.block_assets {
            self.visual_tabs.fetch_add(1, Ordering::AcqRel);
        }
        policy
            .headers
            .retain(|(name, _)| !UNSENDABLE.iter().any(|h| name.eq_ignore_ascii_case(h)));
        let net = Arc::new(TabNet {
            policy,
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

#[derive(PartialEq)]
enum Outcome {
    Done,
    /// Chrome answered with an error: the request is gone or the reply was invalid.
    Refused,
    /// No answer in time, or the connection failed.
    Lost,
}

async fn send<C: Command>(browser: &RwLock<Browser>, params: C) -> Outcome {
    let sent = timeout(REPLY_TIMEOUT, async {
        browser.read().await.execute(params).await
    })
    .await;
    match sent {
        Ok(Ok(_)) => Outcome::Done,
        Ok(Err(CdpError::Chrome(_))) => Outcome::Refused,
        _ => Outcome::Lost,
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
