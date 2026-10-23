//! Page operations: navigation, readiness, Firecrawl actions in order, screenshots, and DOM
//! snapshots. Nothing here retries an action.

use std::{fmt, time::Duration};

use chromiumoxide::{
    Page,
    cdp::{
        browser_protocol::{
            input::{DispatchKeyEventParams, DispatchKeyEventType, InsertTextParams},
            page::{CaptureScreenshotFormat, NavigateParams, Viewport},
        },
        js_protocol::runtime::{EvaluateParams, RemoteObject},
    },
    layout::Point,
    page::ScreenshotParams,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep_until, timeout_at};
use url::Url;

use super::{
    BrowserError,
    network::{Mark, TabNet},
    pool::command,
    types::{Action, Direction, JsReturn, PageScrape, Shot, parse_key},
};
use crate::net::FetchError;

/// After the document loads (or `LOAD_WAIT` passes), scripts get this long to render.
const SETTLE_MS: u64 = 500;
const LOAD_WAIT_MS: u64 = 3000;
/// How long an action may take to start a navigation, then to commit it.
const NAVIGATION_START: Duration = Duration::from_millis(300);
const NAVIGATION_COMMIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);
const MAX_CLICKS: i64 = 100;
const MAX_PIXELS: f64 = 16_000_000.0;
const MAX_SCREENSHOT_BYTES: usize = 8 << 20;
const MAX_SCREENSHOTS_BYTES: usize = 32 << 20;
const MAX_ACTION_CONTENT_BYTES: usize = 16 << 20;

/// `(selector, index)`: the count of visible matches when `index < 0`, else the viewport
/// center of the index-th visible match after scrolling it into view, or `null`.
const VISIBLE: &str = "(s,i)=>{const v=[...document.querySelectorAll(s)].filter(e=>{const r=e.getBoundingClientRect(),c=getComputedStyle(e);return r.width>0&&r.height>0&&c.visibility!=='hidden'&&c.display!=='none'});if(i<0)return v.length;const e=v[i];if(!e)return null;e.scrollIntoView({block:'center',inline:'center'});const r=e.getBoundingClientRect();return{x:r.left+r.width/2,y:r.top+r.height/2}}";
/// `(selector, up)`: scrolls the page, or the selected container, by one of its heights.
const SCROLL: &str = "(s,up)=>{const t=s?document.querySelector(s):null;if(s&&!t)throw new Error('no element matches the selector');const h=t?t.clientHeight:innerHeight;(t||window).scrollBy(0,up?-h:h)}";
/// The current document's URL and DOM; its status comes from Chrome's response events.
const SNAPSHOT: &str =
    "({url:location.href,html:document.documentElement?document.documentElement.outerHTML:''})";

#[derive(Deserialize)]
pub(super) struct Snapshot {
    pub url: String,
    pub html: String,
}

/// Action outputs in order, within the per-visit byte limits.
#[derive(Default)]
pub(super) struct Captured {
    pub screenshots: Vec<(Vec<u8>, &'static str)>,
    pub scrapes: Vec<PageScrape>,
    pub javascript_returns: Vec<JsReturn>,
    screenshot_bytes: usize,
    content_bytes: usize,
}

impl Captured {
    pub(super) fn add_screenshot(
        &mut self,
        shot: (Vec<u8>, &'static str),
    ) -> Result<(), BrowserError> {
        self.screenshot_bytes += shot.0.len();
        if self.screenshot_bytes > MAX_SCREENSHOTS_BYTES {
            return Err(BrowserError::Limit("screenshots exceed 32 MiB per request"));
        }
        self.screenshots.push(shot);
        Ok(())
    }

    fn add_content(&mut self, bytes: usize) -> Result<(), BrowserError> {
        self.content_bytes += bytes;
        if self.content_bytes > MAX_ACTION_CONTENT_BYTES {
            return Err(BrowserError::Limit("action results exceed 16 MiB"));
        }
        Ok(())
    }
}

/// Why one action failed: the page refused it, or the browser failed.
enum StepError {
    Page(String),
    Browser(BrowserError),
}

impl From<BrowserError> for StepError {
    fn from(error: BrowserError) -> Self {
        Self::Browser(error)
    }
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Page(reason) => f.write_str(reason),
            Self::Browser(error) => error.fmt(f),
        }
    }
}

impl StepError {
    fn into_browser(self) -> BrowserError {
        match self {
            Self::Page(_) => BrowserError::Unavailable("page script failed"),
            Self::Browser(error) => error,
        }
    }
}

/// Navigates the tab and waits for the document to be ready.
pub(super) async fn navigate(
    page: &Page,
    url: &Url,
    deadline: Instant,
) -> Result<(), BrowserError> {
    // Only the request deadline bounds this: a slow first byte is not a stuck browser.
    let reply = timeout_at(deadline, page.execute(NavigateParams::new(url.as_str())))
        .await
        .map_err(|_| FetchError::Timeout)?
        .map_err(|_| BrowserError::Unavailable("navigation command failed"))?;
    if let Some(error) = reply.result.error_text {
        tracing::info!(%error, "navigation failed");
        return Err(net_error(&error).into());
    }
    wait_ready(page, deadline).await
}

/// Load (at most `LOAD_WAIT_MS`) plus `SETTLE_MS`; no network-idle waits. A document replaced
/// mid-wait is waited on again.
pub(super) async fn wait_ready(page: &Page, deadline: Instant) -> Result<(), BrowserError> {
    let ready = format!(
        "new Promise(r=>{{const s=()=>setTimeout(r,{SETTLE_MS});if(document.readyState==='complete')s();else{{addEventListener('load',s,{{once:true}});setTimeout(s,{LOAD_WAIT_MS})}}}})"
    );
    let give_up = deadline.min(Instant::now() + NAVIGATION_COMMIT);
    loop {
        match evaluate(page, ready.clone(), true, deadline).await {
            Ok(_) => return Ok(()),
            Err(StepError::Browser(error @ BrowserError::Fetch(FetchError::Timeout))) => {
                return Err(error);
            }
            Err(error) if Instant::now() >= give_up => return Err(error.into_browser()),
            Err(_) => sleep_until(give_up.min(Instant::now() + POLL)).await,
        }
    }
}

/// Runs actions in order. A failure reports the action's index and type; selectors, text, and
/// scripts are never logged.
pub(super) async fn run(
    page: &Page,
    net: &TabNet,
    actions: &[Action],
    deadline: Instant,
    captured: &mut Captured,
) -> Result<(), BrowserError> {
    for (index, action) in actions.iter().enumerate() {
        step(page, net, action, deadline, captured)
            .await
            .map_err(|error| match error {
                StepError::Browser(error @ (BrowserError::Fetch(_) | BrowserError::Limit(_))) => {
                    error
                }
                other => BrowserError::Action {
                    index,
                    kind: action.kind(),
                    reason: other.to_string(),
                },
            })?;
    }
    Ok(())
}

async fn step(
    page: &Page,
    net: &TabNet,
    action: &Action,
    deadline: Instant,
    captured: &mut Captured,
) -> Result<(), StepError> {
    let before = net.mark();
    match action {
        Action::Wait {
            milliseconds: Some(ms),
            ..
        } => pause(Duration::from_millis(*ms), deadline).await?,
        Action::Wait {
            selector: Some(selector),
            ..
        } => {
            while visible(page, selector, -1, deadline).await?.as_i64() == Some(0) {
                pause(POLL, deadline).await?;
            }
        }
        Action::Wait { .. } => return Err(StepError::Page("invalid wait".into())),
        Action::Click { selector, all } => {
            let count = if *all {
                visible(page, selector, -1, deadline)
                    .await?
                    .as_i64()
                    .unwrap_or(0)
                    .min(MAX_CLICKS)
            } else {
                1
            };
            let mut clicked = 0;
            for index in 0..count {
                let Ok(point) = serde_json::from_value::<Center>(
                    visible(page, selector, index, deadline).await?,
                ) else {
                    break;
                };
                let point = Point {
                    x: point.x,
                    y: point.y,
                };
                command(deadline, async { page.click(point).await.map(drop) }).await?;
                clicked += 1;
            }
            if clicked == 0 {
                return Err(StepError::Page(
                    "no visible element matches the selector".into(),
                ));
            }
            settle(page, net, &before, deadline).await?;
        }
        Action::Screenshot { full_page, quality } => {
            let shot = Shot {
                full_page: *full_page,
                quality: *quality,
            };
            captured.add_screenshot(screenshot(page, shot, deadline).await?)?;
        }
        Action::Write { text } => {
            command(deadline, page.execute(InsertTextParams::new(text.clone()))).await?;
        }
        Action::Press { key } => {
            press(page, key, deadline).await?;
            settle(page, net, &before, deadline).await?;
        }
        Action::Scroll {
            direction,
            selector,
        } => {
            let up = *direction == Direction::Up;
            evaluate(
                page,
                call(SCROLL, &[json!(selector), json!(up)]),
                false,
                deadline,
            )
            .await?;
        }
        Action::Scrape => {
            let page_now = snapshot(page, deadline).await?;
            captured.add_content(page_now.url.len() + page_now.html.len())?;
            captured.scrapes.push(PageScrape {
                url: page_now.url,
                html: page_now.html,
            });
        }
        Action::ExecuteJavascript { script } => {
            let result = evaluate(page, script.clone(), true, deadline).await?;
            let value = result.value.unwrap_or(Value::Null);
            captured.add_content(value.to_string().len())?;
            captured.javascript_returns.push(JsReturn {
                kind: result.r#type.as_ref().to_owned(),
                value,
            });
            settle(page, net, &before, deadline).await?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Center {
    x: f64,
    y: f64,
}

/// Format or action screenshot: PNG, or JPEG when a quality is given. A full page larger than
/// 16 megapixels fails rather than being cropped.
pub(super) async fn screenshot(
    page: &Page,
    shot: Shot,
    deadline: Instant,
) -> Result<(Vec<u8>, &'static str), BrowserError> {
    let mut params = ScreenshotParams::builder();
    let media_type = match shot.quality {
        Some(quality) => {
            params = params
                .format(CaptureScreenshotFormat::Jpeg)
                .quality(quality);
            "image/jpeg"
        }
        None => {
            params = params.format(CaptureScreenshotFormat::Png);
            "image/png"
        }
    };
    if shot.full_page {
        let size = command(deadline, page.layout_metrics())
            .await?
            .css_content_size;
        if size.width * size.height > MAX_PIXELS {
            return Err(BrowserError::Limit(
                "full-page screenshot exceeds 16 megapixels",
            ));
        }
        let clip = Viewport {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: size.height,
            scale: 1.0,
        };
        params = params.clip(clip).capture_beyond_viewport(true);
    }
    let bytes = command(deadline, page.screenshot(params.build())).await?;
    if bytes.len() > MAX_SCREENSHOT_BYTES {
        return Err(BrowserError::Limit("screenshot exceeds 8 MiB"));
    }
    Ok((bytes, media_type))
}

pub(super) async fn snapshot(page: &Page, deadline: Instant) -> Result<Snapshot, BrowserError> {
    let object = evaluate(page, SNAPSHOT.into(), false, deadline)
        .await
        .map_err(StepError::into_browser)?;
    serde_json::from_value(object.value.unwrap_or_default())
        .map_err(|_| BrowserError::Unavailable("page snapshot unreadable"))
}

/// The page's current URL, when it can be read.
pub(super) async fn location(page: &Page, deadline: Instant) -> Option<Url> {
    let object = evaluate(page, "location.href".into(), false, deadline)
        .await
        .ok()?;
    Url::parse(object.value?.as_str()?).ok()
}

/// After an action that may navigate: if it started a top-level navigation, wait for a new
/// main-frame document to commit, then to become ready. One that never commits (a 204, a
/// refused download) fails the action.
async fn settle(
    page: &Page,
    net: &TabNet,
    before: &Mark,
    deadline: Instant,
) -> Result<(), StepError> {
    sleep_until(deadline.min(Instant::now() + NAVIGATION_START)).await;
    if net.mark() == *before {
        return Ok(());
    }
    let give_up = deadline.min(Instant::now() + NAVIGATION_COMMIT);
    while net.mark().committed == before.committed {
        if Instant::now() >= give_up {
            return Err(if give_up == deadline {
                BrowserError::from(FetchError::Timeout).into()
            } else {
                StepError::Page("the navigation it started did not commit".into())
            });
        }
        sleep_until(give_up.min(Instant::now() + POLL)).await;
    }
    Ok(wait_ready(page, deadline).await?)
}

async fn press(page: &Page, spec: &str, deadline: Instant) -> Result<(), StepError> {
    let (key, modifiers) =
        parse_key(spec).ok_or_else(|| StepError::Page("unsupported key".into()))?;
    // Control and Meta chords are shortcuts, not text input.
    let text = (modifiers & 0b0110 == 0)
        .then(|| {
            key.text
                .or((key.key.chars().count() == 1).then_some(key.key))
        })
        .flatten();
    let event = DispatchKeyEventParams::builder()
        .key(key.key)
        .code(key.code)
        .windows_virtual_key_code(key.key_code)
        .native_virtual_key_code(key.key_code)
        .modifiers(modifiers);
    let down = match text {
        Some(text) => event
            .clone()
            .r#type(DispatchKeyEventType::KeyDown)
            .text(text),
        None => event.clone().r#type(DispatchKeyEventType::RawKeyDown),
    };
    let invalid = |_| StepError::Page("unsupported key".into());
    let (down, up) = (
        down.build().map_err(invalid)?,
        event
            .r#type(DispatchKeyEventType::KeyUp)
            .build()
            .map_err(invalid)?,
    );
    command(deadline, page.execute(down)).await?;
    command(deadline, page.execute(up)).await?;
    Ok(())
}

async fn visible(
    page: &Page,
    selector: &str,
    index: i64,
    deadline: Instant,
) -> Result<Value, StepError> {
    let object = evaluate(
        page,
        call(VISIBLE, &[json!(selector), json!(index)]),
        false,
        deadline,
    )
    .await?;
    Ok(object.value.unwrap_or_default())
}

async fn pause(duration: Duration, deadline: Instant) -> Result<(), BrowserError> {
    let until = Instant::now() + duration;
    if until > deadline {
        sleep_until(deadline).await;
        return Err(FetchError::Timeout.into());
    }
    sleep_until(until).await;
    Ok(())
}

/// `(function)(args…)`; JSON values are valid JavaScript literals.
fn call(function: &str, args: &[Value]) -> String {
    let args: Vec<String> = args.iter().map(Value::to_string).collect();
    format!("({function})({})", args.join(","))
}

/// Runs a script in the page's main world, awaiting a returned promise when asked. A thrown
/// exception is the page's refusal, not a browser failure.
async fn evaluate(
    page: &Page,
    expression: String,
    await_promise: bool,
    deadline: Instant,
) -> Result<RemoteObject, StepError> {
    let mut params = EvaluateParams::new(expression);
    params.return_by_value = Some(true);
    params.await_promise = Some(await_promise);
    params.user_gesture = Some(true);
    let reply = command(deadline, page.execute(params)).await?.result;
    match reply.exception_details {
        Some(details) => Err(StepError::Page(
            details
                .exception
                .and_then(|exception| exception.description)
                .unwrap_or(details.text),
        )),
        None => Ok(reply.result),
    }
}

/// Chrome's network error for a failed top-level navigation, as a fetch error.
fn net_error(text: &str) -> FetchError {
    match text.trim_start_matches("net::") {
        "ERR_NAME_NOT_RESOLVED" | "ERR_NAME_RESOLUTION_FAILED" => FetchError::Dns,
        "ERR_TIMED_OUT" | "ERR_CONNECTION_TIMED_OUT" => FetchError::Timeout,
        "ERR_TOO_MANY_REDIRECTS" => FetchError::TooManyRedirects,
        // An error status with an empty body, which Chrome replaces with its own error page.
        "ERR_HTTP_RESPONSE_CODE_FAILURE" => FetchError::Protocol,
        "ERR_BLOCKED_BY_CLIENT" => FetchError::Refused("request refused by browser policy".into()),
        code if code.starts_with("ERR_CERT_") || code.starts_with("ERR_SSL_") => FetchError::Tls,
        _ => FetchError::Connect,
    }
}
