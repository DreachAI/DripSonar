//! Markdown comparison against the capture history; never substitutes fetch failures for pages.

use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum ChangeMode {
    #[serde(rename = "git-diff")]
    GitDiff,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeTrackingOptions {
    #[serde(default)]
    pub modes: Vec<ChangeMode>,
}

impl Scraper {
    pub async fn track_changes(
        &self,
        doc: &Document,
        req: &ScrapeRequest,
        options: &ChangeTrackingOptions,
    ) -> Result<Value, ScrapeError> {
        let id = doc.capture_id.ok_or(ScrapeError::Storage)?;
        let mut cursor = None;
        let (at, old) = loop {
            let previous = timeout_at(req.deadline, self.store.previous_capture(id, cursor))
                .await
                .map_err(|_| FetchError::Timeout)?
                .map_err(|_| ScrapeError::Storage)?;
            let Some(previous) = previous else {
                let status = if matches!(doc.status, 404 | 410) {
                    "removed"
                } else {
                    "new"
                };
                return Ok(
                    json!({"previousScrapeAt": null, "changeStatus": status, "visibility": "visible"}),
                );
            };
            cursor = Some(previous.id);
            let at = previous
                .fetched
                .fetched_at
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| ScrapeError::Storage)?;
            match render_capture(previous.fetched, req, true).await {
                Ok(old) => break (at, old),
                Err(
                    ScrapeError::Blocked
                    | ScrapeError::Unsupported(_)
                    | ScrapeError::Pdf(
                        DocumentError::Failed
                        | DocumentError::Encrypted
                        | DocumentError::NoTextLayer,
                    ),
                ) => continue,
                Err(error) => return Err(error),
            }
        };
        let before = old.rendered.markdown.ok_or(ScrapeError::Storage)?;
        let after = doc.rendered.markdown.clone().ok_or(ScrapeError::Storage)?;
        let status = if matches!(doc.status, 404 | 410) {
            "removed"
        } else if before == after {
            "same"
        } else {
            "changed"
        };
        let mut tracking =
            json!({"previousScrapeAt": at, "changeStatus": status, "visibility": "visible"});
        if !options.modes.is_empty() && status == "changed" {
            let text = render_blocking(req.deadline, move || {
                Ok(similar::TextDiff::configure()
                    .algorithm(similar::Algorithm::Patience)
                    .timeout(Duration::from_secs(1))
                    .diff_lines(&before, &after)
                    .unified_diff()
                    .header("previous", "current")
                    .to_string())
            })
            .await?;
            tracking["diff"] = json!({"text": text});
        }
        Ok(tracking)
    }
}
