//! Streaming capture-history export, independent of job retention and request credentials.

use crate::{
    net::Route,
    page::RenderOptions,
    scrape::{self, ScrapeTemplate},
    store::{Store, Tier, sha256_hex},
};
use serde_json::json;
use std::{
    collections::HashSet,
    error::Error,
    io::{self, BufWriter, Write},
    path::Path,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub async fn export(
    store: &Store,
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
    let template = ScrapeTemplate {
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
        let captures = store.captures_after(after, 8).await?;
        if captures.is_empty() {
            break;
        }
        for capture in captures {
            after = capture.id;
            let tier = match capture.tier {
                Tier::Http => "http",
                Tier::Browser => "browser",
            };
            let fetched = capture.fetched;
            if !(200..300).contains(&fetched.status)
                || since.is_some_and(|since| fetched.fetched_at < since)
            {
                continue;
            }
            if let Some(suffix) = &host {
                let candidate = fetched.final_url.host_str().unwrap_or_default();
                if candidate != suffix
                    && !candidate
                        .strip_suffix(suffix)
                        .is_some_and(|prefix| prefix.ends_with('.'))
                {
                    continue;
                }
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
                "route":match doc.route { Route::Direct => "direct", Route::Proxy => "proxy" },"tier":tier});
            serde_json::to_writer(&mut writer, &record)?;
            writer.write_all(b"\n")?;
            count += 1;
        }
    }
    writer.flush()?;
    Ok(count)
}
