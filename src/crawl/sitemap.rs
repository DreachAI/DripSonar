//! Bounded, network-free sitemap and robots discovery parsers.

use std::io::Read;

use flate2::read::MultiGzDecoder;
use quick_xml::{Reader, events::Event};
use url::Url;

#[derive(Debug)]
pub enum SitemapEntries {
    Urls(Vec<Url>),
    Sitemaps(Vec<Url>),
}

#[derive(Debug, thiserror::Error)]
pub enum SitemapError {
    #[error("sitemap exceeds the byte limit")]
    TooLarge,
    #[error("sitemap exceeds the entry limit")]
    TooManyEntries,
    #[error("invalid sitemap: {0}")]
    Invalid(String),
}

pub fn parse(
    body: &[u8],
    max_inflated: usize,
    max_entries: usize,
) -> Result<SitemapEntries, SitemapError> {
    let inflated;
    let body = if body.starts_with(&[0x1f, 0x8b]) {
        let mut output = Vec::new();
        MultiGzDecoder::new(body)
            .take((max_inflated as u64).saturating_add(1))
            .read_to_end(&mut output)
            .map_err(|e| SitemapError::Invalid(e.to_string()))?;
        inflated = output;
        inflated.as_slice()
    } else {
        body
    };
    if body.len() > max_inflated {
        return Err(SitemapError::TooLarge);
    }
    let invalid = |message: &str| SitemapError::Invalid(message.into());
    let mut reader = Reader::from_reader(body);
    reader.config_mut().expand_empty_elements = true;
    let mut stack = Vec::new();
    let mut root = None;
    let mut loc = String::new();
    let mut urls = Vec::new();
    let mut entries = 0;
    loop {
        let event = reader
            .read_event()
            .map_err(|e| SitemapError::Invalid(e.to_string()))?;
        let in_loc = stack.len() == 3 && stack[2] == b"loc";
        match event {
            Event::DocType(_) => return Err(invalid("DTD and entity declarations are forbidden")),
            Event::Start(tag) => {
                let name = tag.local_name().as_ref().to_vec();
                if stack.is_empty() {
                    if root.is_some() || !matches!(name.as_slice(), b"urlset" | b"sitemapindex") {
                        return Err(invalid("expected one urlset or sitemapindex"));
                    }
                    root = Some(name.clone());
                }
                if stack.len() >= 64 || in_loc {
                    return Err(invalid("invalid sitemap nesting"));
                }
                stack.push(name);
                if stack.len() == 3 && stack[2] == b"loc" {
                    loc.clear();
                }
            }
            Event::Text(text) => {
                let text = text.xml_content().map_err(|e| invalid(&e.to_string()))?;
                if in_loc {
                    loc.push_str(&text);
                } else if stack.is_empty() && !text.trim().is_empty() {
                    return Err(invalid("text outside sitemap root"));
                }
            }
            Event::CData(text) => {
                if in_loc {
                    loc.push_str(&text.decode().map_err(|e| invalid(&e.to_string()))?);
                } else if stack.is_empty() {
                    return Err(invalid("CDATA outside sitemap root"));
                }
            }
            Event::GeneralRef(reference) => {
                let name = reference.decode().map_err(|e| invalid(&e.to_string()))?;
                let escaped = format!("&{name};");
                let value = quick_xml::escape::unescape(&escaped)
                    .map_err(|_| invalid("custom entities are forbidden"))?;
                if in_loc {
                    loc.push_str(&value);
                } else if stack.is_empty() {
                    return Err(invalid("entity outside sitemap root"));
                }
            }
            Event::End(_) => {
                if in_loc
                    && ((stack[0] == b"urlset" && stack[1] == b"url")
                        || (stack[0] == b"sitemapindex" && stack[1] == b"sitemap"))
                {
                    entries += 1;
                    if entries > max_entries {
                        return Err(SitemapError::TooManyEntries);
                    }
                    if let Ok(url) = Url::parse(loc.trim())
                        && matches!(url.scheme(), "http" | "https")
                        && url.has_host()
                    {
                        urls.push(url);
                    }
                }
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(invalid("unclosed sitemap elements"));
    }
    match root.as_deref() {
        Some(b"urlset") => Ok(SitemapEntries::Urls(urls)),
        Some(b"sitemapindex") => Ok(SitemapEntries::Sitemaps(urls)),
        _ => Err(invalid("missing sitemap root")),
    }
}

/// Robots rules are not interpreted; only absolute Sitemap directives are returned.
pub fn robots_sitemaps(robots_txt: &str) -> Vec<Url> {
    robots_txt
        .lines()
        .filter_map(|line| line.split('#').next()?.split_once(':'))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("sitemap"))
        .filter_map(|(_, value)| Url::parse(value.trim()).ok())
        .filter(|url| url.has_host() && matches!(url.scheme(), "http" | "https"))
        .collect()
}
