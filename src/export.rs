//! Streaming capture-history export, independent of job retention and request credentials.

use crate::{
    page::RenderOptions,
    scrape::{self, ScrapeTemplate},
    search::Hit,
    store::{Store, sha256_hex},
};
use serde_json::json;
use std::{
    collections::HashSet,
    error::Error,
    io::{self, BufWriter, Write},
    path::Path,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;

/// Which records an export emits; pages first when both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportKind {
    #[default]
    Pages,
    Search,
    All,
}

pub async fn export_kind(
    store: &Store,
    kind: ExportKind,
    since: Option<&str>,
    host: Option<&str>,
    out: Option<&Path>,
) -> Result<u64, Box<dyn Error>> {
    let since = since
        .map(|s| OffsetDateTime::parse(s, &Rfc3339))
        .transpose()?;
    let host = host.map(|s| s.trim_start_matches('.').to_ascii_lowercase());
    if host
        .as_ref()
        .is_some_and(|h| h.is_empty() || h.contains(['/', ':', '@']))
    {
        return Err("host must be a hostname suffix".into());
    }
    let writer: Box<dyn Write> = match out {
        Some(path) => Box::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?,
        ),
        None => Box::new(io::stdout()),
    };
    let mut writer = BufWriter::new(writer);
    let mut count = 0;
    if kind != ExportKind::Search {
        count += pages(store, since, host.as_deref(), &mut writer).await?;
    }
    if kind != ExportKind::Pages {
        count += searches(store, since, host.as_deref(), &mut writer).await?;
    }
    writer.flush()?;
    Ok(count)
}

/// Whether `candidate` is `suffix` or one of its subdomains.
fn host_matches(candidate: &str, suffix: &str) -> bool {
    candidate == suffix
        || candidate
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Search records; with a host filter, only results on that host, and only records keeping one.
async fn searches(
    store: &Store,
    since: Option<OffsetDateTime>,
    host: Option<&str>,
    writer: &mut impl Write,
) -> Result<u64, Box<dyn Error>> {
    let (mut after, mut count) = (0, 0);
    loop {
        let rows = store.searches_after(after, 100).await?;
        let Some(last) = rows.last() else {
            return Ok(count);
        };
        after = last.id;
        for search in rows {
            if since.is_some_and(|since| search.fetched_at < since) {
                continue;
            }
            let mut results: Vec<Hit> = serde_json::from_str(&search.hits)?;
            if let Some(suffix) = host {
                results.retain(|hit| {
                    Url::parse(&hit.url)
                        .is_ok_and(|url| url.host_str().is_some_and(|h| host_matches(h, suffix)))
                });
                if results.is_empty() {
                    continue;
                }
            }
            let record = json!({"query":search.query,"provider":search.provider,"results":results,
                "fetched_at":search.fetched_at.format(&Rfc3339)?,"capture_id":search.capture_id});
            serde_json::to_writer(&mut *writer, &record)?;
            writer.write_all(b"\n")?;
            count += 1;
        }
    }
}

async fn pages(
    store: &Store,
    since: Option<OffsetDateTime>,
    host: Option<&str>,
    writer: &mut impl Write,
) -> Result<u64, Box<dyn Error>> {
    let template = ScrapeTemplate {
        proxy: Default::default(),
        headers: Vec::new(),
        timeout_ms: 30_000,
        max_age_ms: 0,
        raw_html: false,
        render: RenderOptions {
            only_main_content: true,
            remove_base64_images: true,
            markdown: true,
            ..RenderOptions::default()
        },
    };
    let mut seen = HashSet::new();
    let (mut after, mut count) = (0, 0);
    loop {
        let (captures, next) = store.captures_after(after, 8).await?;
        if next == after {
            return Ok(count);
        }
        after = next;
        for capture in captures {
            let tier = capture.tier.as_str();
            let fetched = capture.fetched;
            if !(200..300).contains(&fetched.status)
                || since.is_some_and(|since| fetched.fetched_at < since)
            {
                continue;
            }
            if host.is_some_and(|suffix| {
                !host_matches(fetched.final_url.host_str().unwrap_or_default(), suffix)
            }) {
                continue;
            }
            let requested_url = fetched.requested_url.to_string();
            let fetched_at = fetched.fetched_at.format(&Rfc3339)?;
            let request = template.request(&fetched.final_url);
            let doc = match scrape::render_capture(fetched, &request, false).await {
                Ok(doc) => doc,
                Err(
                    scrape::ScrapeError::Blocked
                    | scrape::ScrapeError::Unsupported(_)
                    | scrape::ScrapeError::Pdf(_),
                ) => continue,
                Err(error) => return Err(error.into()),
            };
            let markdown = doc.rendered.markdown.as_deref().unwrap_or_default();
            if markdown.trim().is_empty() {
                continue;
            }
            let content_sha256 = sha256_hex(markdown.as_bytes());
            if !seen.insert(content_sha256.clone()) {
                continue;
            }
            let meta = doc.rendered.metadata;
            let record = json!({"url":requested_url,"final_url":doc.url,"status":doc.status,
                "content_type":doc.content_type,"fetched_at":fetched_at,"published_time":meta.published_time,
                "modified_time":meta.modified_time,"title":meta.title,"language":meta.language,"markdown":markdown,
                "content_sha256":content_sha256,"blob_sha256":capture.blob_sha256,"capture_id":capture.id,
                "collector":concat!("DripSonar/",env!("CARGO_PKG_VERSION")),
                "route":doc.route.as_str(),"tier":tier});
            serde_json::to_writer(&mut *writer, &record)?;
            writer.write_all(b"\n")?;
            count += 1;
        }
    }
}
