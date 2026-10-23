//! Durable event delivery. Destinations use the page fetcher's guard, never the trusted clients.

use super::*;
use hmac::{Hmac, KeyInit, Mac};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    Started,
    Page,
    Completed,
    Failed,
}

impl Event {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Page => "page",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, from = "WebhookInput")]
pub struct Webhook {
    pub url: Url,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default = "all_events")]
    pub events: Vec<Event>,
}

impl std::fmt::Debug for Webhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Webhook(<redacted>)")
    }
}

fn all_events() -> Vec<Event> {
    vec![Event::Started, Event::Page, Event::Completed, Event::Failed]
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WebhookInput {
    Url(Url),
    Options(WebhookOptions),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookOptions {
    url: Url,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default = "all_events")]
    events: Vec<Event>,
}

impl From<WebhookInput> for Webhook {
    fn from(input: WebhookInput) -> Self {
        match input {
            WebhookInput::Url(url) => Self {
                url,
                headers: BTreeMap::new(),
                metadata: BTreeMap::new(),
                events: all_events(),
            },
            WebhookInput::Options(options) => Self {
                url: options.url,
                headers: options.headers,
                metadata: options.metadata,
                events: options.events,
            },
        }
    }
}

impl Webhook {
    pub(super) fn validate(&self, fetcher: &Fetcher) -> Result<(), String> {
        fetcher
            .guard()
            .check_url(&self.url)
            .map_err(|_| "webhook.url is not an allowed public HTTP(S) destination".to_owned())?;
        let headers: Vec<_> = self.headers.clone().into_iter().collect();
        crate::scrape::request_headers(&headers)
            .map_err(|_| "invalid webhook.headers".to_owned())?;
        for name in self.headers.keys() {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "content-type"
                    | "x-sonar-signature"
            ) {
                return Err(format!("webhook.headers.{name} is not allowed"));
            }
        }
        Ok(())
    }
}

pub(super) struct Delivery {
    pub id: i64,
    pub attempt: u32,
    pub webhook: Webhook,
    pub body: String,
}

async fn send(inner: &Inner, delivery: &Delivery) -> bool {
    let mut headers: Vec<_> = delivery.webhook.headers.clone().into_iter().collect();
    if let Some(secret) = &inner.cfg.webhook_secret {
        let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
            return false;
        };
        mac.update(delivery.body.as_bytes());
        let signature: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        headers.push(("X-Sonar-Signature".into(), format!("sha256={signature}")));
    }
    matches!(
        inner
            .fetcher
            .post_json(
                &delivery.webhook.url,
                &headers,
                delivery.body.as_bytes(),
                Instant::now() + Duration::from_secs(10)
            )
            .await,
        Ok(200..=299)
    )
}

/// Four in-flight deliveries bound memory and slow receivers. Claims are persisted before
/// sending; a restart can redeliver an uncertain attempt, so receivers must tolerate duplicates.
pub(super) async fn deliver(inner: std::sync::Weak<Inner>) {
    let mut workers = JoinSet::new();
    loop {
        let Some(inner) = inner.upgrade() else { return };
        if inner.stopped.load(Ordering::Acquire) {
            workers.abort_all();
            return;
        }
        while workers.len() < 4 {
            match db::claim_webhook(&inner.store).await {
                Ok(Some(delivery)) => {
                    let inner = inner.clone();
                    workers.spawn(async move {
                        let success = send(&inner, &delivery).await;
                        if let Err(error) =
                            db::webhook_result(&inner.store, delivery.id, delivery.attempt, success)
                                .await
                        {
                            tracing::error!(%error, "webhook result not stored");
                        }
                    });
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::error!(%error, "webhook outbox unavailable");
                    break;
                }
            }
        }
        drop(inner);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                if result.is_err() { tracing::error!("webhook delivery worker failed"); }
            }
        }
    }
}
