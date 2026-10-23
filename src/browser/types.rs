//! Firecrawl v1 actions, validated browser plans, and their outputs.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::net::Fetched;

const MAX_ACTIONS: usize = 50;
/// Total of `waitFor` and wait actions, as Firecrawl counts it.
const MAX_WAIT_MS: u64 = 60_000;
/// Firecrawl charges a selector wait as one second of the wait budget.
const SELECTOR_WAIT_MS: u64 = 1000;
const MAX_SELECTOR_BYTES: usize = 2 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;

/// One Firecrawl v1 action; `pdf` is not supported and is refused as an unknown type.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Action {
    Wait {
        milliseconds: Option<u64>,
        selector: Option<String>,
    },
    Click {
        selector: String,
        #[serde(default)]
        all: bool,
    },
    Screenshot {
        #[serde(default, rename = "fullPage")]
        full_page: bool,
        quality: Option<u8>,
    },
    Write {
        text: String,
    },
    Press {
        key: String,
    },
    Scroll {
        #[serde(default)]
        direction: Direction,
        selector: Option<String>,
    },
    Scrape,
    ExecuteJavascript {
        script: String,
    },
}

impl Action {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Wait { .. } => "wait",
            Self::Click { .. } => "click",
            Self::Screenshot { .. } => "screenshot",
            Self::Write { .. } => "write",
            Self::Press { .. } => "press",
            Self::Scroll { .. } => "scroll",
            Self::Scrape => "scrape",
            Self::ExecuteJavascript { .. } => "executeJavascript",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    Up,
    #[default]
    Down,
}

/// A screenshot request: PNG by default, JPEG when a quality is given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shot {
    pub full_page: bool,
    pub quality: Option<u8>,
}

/// What the browser does after navigation: wait, run actions, take the format screenshot.
#[derive(Clone, Debug, Default)]
pub struct BrowserPlan {
    pub wait_for: Duration,
    pub actions: Vec<Action>,
    pub screenshot: Option<Shot>,
}

impl BrowserPlan {
    /// The caller asked for the browser itself, so caches and shared execution are skipped.
    pub fn is_explicit(&self) -> bool {
        !self.wait_for.is_zero() || !self.actions.is_empty() || self.screenshot.is_some()
    }

    /// Screenshots need images and fonts; text-only visits block them.
    pub(crate) fn is_visual(&self) -> bool {
        self.screenshot.is_some()
            || self
                .actions
                .iter()
                .any(|action| matches!(action, Action::Screenshot { .. }))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PlanError(String);

/// Checks everything that can fail before navigation, so a bad plan never touches the site.
pub fn validate_plan(
    actions: Vec<Action>,
    wait_for_ms: u64,
    screenshot: Option<Shot>,
) -> Result<BrowserPlan, PlanError> {
    let invalid = |index: usize, message: &str| PlanError(format!("actions[{index}]: {message}"));
    if actions.len() > MAX_ACTIONS {
        return Err(PlanError(format!(
            "Number of actions cannot exceed {MAX_ACTIONS}"
        )));
    }
    let mut waited = wait_for_ms;
    for (index, action) in actions.iter().enumerate() {
        let selector = match action {
            Action::Wait {
                milliseconds,
                selector,
            } => {
                waited = waited.saturating_add(match (milliseconds, selector) {
                    (Some(0), None) => return Err(invalid(index, "milliseconds must be positive")),
                    (Some(ms), None) => *ms,
                    (None, Some(_)) => SELECTOR_WAIT_MS,
                    _ => {
                        let message =
                            "Either 'milliseconds' or 'selector' must be provided, but not both.";
                        return Err(invalid(index, message));
                    }
                });
                selector.as_deref()
            }
            Action::Click { selector, .. } => Some(selector.as_str()),
            Action::Scroll { selector, .. } => selector.as_deref(),
            Action::Screenshot {
                quality: Some(quality),
                ..
            } if !(1..=100).contains(quality) => {
                return Err(invalid(index, "quality must be between 1 and 100"));
            }
            Action::Write { text } | Action::ExecuteJavascript { script: text }
                if text.len() > MAX_TEXT_BYTES =>
            {
                return Err(invalid(index, "text and scripts are limited to 64 KiB"));
            }
            Action::Press { key } if parse_key(key).is_none() => {
                return Err(invalid(index, "unsupported key"));
            }
            _ => None,
        };
        if selector.is_some_and(|s| s.is_empty() || s.len() > MAX_SELECTOR_BYTES) {
            return Err(invalid(index, "selector must be 1 to 2048 bytes"));
        }
    }
    if waited > MAX_WAIT_MS {
        return Err(PlanError(
            "Total wait time (waitFor + wait actions) cannot exceed 60 seconds".into(),
        ));
    }
    Ok(BrowserPlan {
        wait_for: Duration::from_millis(wait_for_ms),
        actions,
        screenshot,
    })
}

/// Parses `Key` or `Modifier+…+Key` (Alt, Control/Ctrl, Meta/Command/Cmd, Shift) into a US
/// keyboard key and a DevTools modifier mask.
pub(crate) fn parse_key(spec: &str) -> Option<(&'static chromiumoxide::keys::KeyDefinition, i64)> {
    let (modifiers, key) = match spec.strip_suffix("++") {
        Some(modifiers) => (modifiers, "+"),
        None if spec == "+" => ("", "+"),
        None => spec.rsplit_once('+').unwrap_or(("", spec)),
    };
    let mut mask = 0;
    for modifier in modifiers.split('+').filter(|m| !m.is_empty()) {
        mask |= match modifier {
            "Alt" => 1,
            "Control" | "Ctrl" => 2,
            "Meta" | "Command" | "Cmd" => 4,
            "Shift" => 8,
            _ => return None,
        };
    }
    Some((chromiumoxide::keys::get_key_definition(key)?, mask))
}

/// Firecrawl's `actions` result object; PDFs are never produced.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionsOutput {
    pub screenshots: Vec<String>,
    pub scrapes: Vec<PageScrape>,
    pub javascript_returns: Vec<JsReturn>,
    pub pdfs: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PageScrape {
    pub url: String,
    pub html: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct JsReturn {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: serde_json::Value,
}

/// Results beyond the page itself: screenshot URLs and action outputs.
#[derive(Clone, Debug, Default)]
pub struct BrowserExtras {
    pub screenshot: Option<String>,
    pub actions: Option<ActionsOutput>,
}

/// A browser visit: the final DOM as a UTF-8 HTML capture, plus extras.
#[derive(Clone, Debug)]
pub struct BrowserOutput {
    pub capture: Fetched,
    pub extras: BrowserExtras,
}
