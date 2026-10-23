//! When a plain HTTP page should be rendered, and per-origin memory of renders that helped.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
    time::Duration,
};

use dom_query::Document;
use tokio::time::Instant;

use crate::net::Fetched;

/// Below this many visible non-whitespace characters a page may be a script shell.
const MIN_VISIBLE: usize = 200;
const MAX_HINTS: usize = 4096;
const HINT_TTL: Duration = Duration::from_secs(600);

/// Cheap substring checks on the lowercased body; only a hit pays for an HTML parse.
const APP_ROOT_MARKERS: [&str; 9] = [
    "id=\"root\"",
    "id='root'",
    "id=\"app\"",
    "id='app'",
    "id=\"__next\"",
    "id=\"__nuxt\"",
    "id=\"___gatsby\"",
    "id=\"svelte\"",
    "<app-root",
];
const APP_ROOTS: &str = "#root, #app, #__next, #__nuxt, #___gatsby, #svelte, app-root";
const JS_NOTICES: [&str; 7] = [
    "enable javascript",
    "javascript is required",
    "javascript is disabled",
    "requires javascript",
    "javascript enabled",
    "turn on javascript",
    "need javascript",
];
const INVISIBLE: [&str; 4] = ["script", "style", "noscript", "template"];

#[derive(Debug, PartialEq, Eq)]
pub enum Escalation {
    Keep,
    /// A script shell; `visible` is its visible character count, for judging the render.
    Render {
        visible: usize,
    },
}

/// Escalates only a successful HTML page with almost no visible text and a concrete shell
/// marker: an empty app root plus external or module scripts, or a JavaScript-required
/// notice. Challenges and error statuses never escalate.
pub(crate) fn classify_http(
    fetched: &Fetched,
    text: &str,
    doc: &Document,
    visible: usize,
) -> Escalation {
    let html = fetched.content_type.as_deref().is_none_or(|value| {
        let essence = value.split(';').next().unwrap_or_default().trim();
        essence.eq_ignore_ascii_case("text/html")
            || essence.eq_ignore_ascii_case("application/xhtml+xml")
    });
    if !(200..300).contains(&fetched.status) || !html {
        return Escalation::Keep;
    }
    let lower = text.to_ascii_lowercase();
    let root_marker = APP_ROOT_MARKERS.iter().any(|m| lower.contains(m));
    let notice = JS_NOTICES.iter().any(|m| lower.contains(m));
    if !(root_marker || notice) {
        return Escalation::Keep;
    }
    let empty_root = root_marker
        && doc.select("script[src], script[type=module]").exists()
        && doc
            .select(APP_ROOTS)
            .iter()
            .any(|root| root.text().trim().is_empty());
    if visible < MIN_VISIBLE && (empty_root || notice) {
        Escalation::Render { visible }
    } else {
        Escalation::Keep
    }
}

/// Visible non-whitespace characters of the body, ignoring scripts, styles, and templates.
pub(crate) fn visible_chars(doc: &Document) -> usize {
    let mut nodes = doc.select("body").nodes().to_vec();
    let mut visible = 0;
    while let Some(node) = nodes.pop() {
        if node.is_text() {
            visible += node.text().chars().filter(|c| !c.is_whitespace()).count();
        } else if !node
            .node_name()
            .is_some_and(|name| INVISIBLE.contains(&name.as_ref()))
        {
            nodes.extend(node.children_it(false));
        }
    }
    visible
}

/// A render helped when it shows real text and at least twice what plain HTTP showed.
pub fn improves(http_visible: usize, rendered_visible: usize) -> bool {
    rendered_visible >= MIN_VISIBLE && rendered_visible >= http_visible.saturating_mul(2)
}

/// Origins whose pages needed the browser recently; they go browser-first until expiry.
#[derive(Default)]
pub struct Hints {
    expiry: Mutex<HashMap<String, Instant>>,
}

impl Hints {
    pub fn prefers(&self, origin: &str) -> bool {
        let expiry = self.expiry.lock().unwrap_or_else(PoisonError::into_inner);
        expiry.get(origin).is_some_and(|at| *at > Instant::now())
    }

    pub fn remember(&self, origin: &str) {
        let now = Instant::now();
        let mut expiry = self.expiry.lock().unwrap_or_else(PoisonError::into_inner);
        if expiry.len() >= MAX_HINTS && !expiry.contains_key(origin) {
            expiry.retain(|_, at| *at > now);
            let oldest = expiry
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(o, _)| o.clone());
            if expiry.len() >= MAX_HINTS
                && let Some(oldest) = oldest
            {
                expiry.remove(&oldest);
            }
        }
        expiry.insert(origin.to_owned(), now + HINT_TTL);
    }

    pub fn forget(&self, origin: &str) {
        let mut expiry = self.expiry.lock().unwrap_or_else(PoisonError::into_inner);
        expiry.remove(origin);
    }
}
