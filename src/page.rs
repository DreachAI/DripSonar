//! Pure HTML transforms: cleanup, main content, markdown, metadata, links.

use std::collections::{BTreeMap, HashSet};

use dom_query::{Document, Selection};
use url::Url;

const MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;
const MAX_EXTRA_ENTRIES: usize = 100;
const MAX_EXTRA_CHARS: usize = 1000;
const SMALL_BODY_BYTES: usize = 20 * 1024;

const STRIP_TAGS: &str = "script, style, noscript, template, meta, link";
const NON_MAIN: &str = "header, footer, nav, aside, .header, .top, .navbar, #header, .footer, \
    .bottom, #footer, .sidebar, .side, .aside, #sidebar, .modal, .popup, #modal, .overlay, .ad, \
    .ads, .advert, #ad, .lang-selector, .language, #language-selector, .social, .social-media, \
    .social-links, #social, .menu, .navigation, #nav, .breadcrumbs, #breadcrumbs, .share, #share, \
    .widget, #widget, .cookie, #cookie";

/// An element holding any of these is kept even when it matches `NON_MAIN`: sites wrap their
/// whole page in a `.top` or `.menu` class.
const MAIN_MARKERS: &str = "#main, main, [role=main]";

const CHALLENGE_STATUSES: [u16; 3] = [403, 429, 503];
const CHALLENGE_TITLES: [&str; 6] = [
    "just a moment...",
    "attention required! | cloudflare",
    "access denied",
    "please wait... | cloudflare",
    "ddos-guard",
    "pardon our interruption",
];
const CHALLENGE_BODY_MARKERS: [&str; 2] = ["_cf_chl_opt", "/cdn-cgi/challenge-platform/"];

#[derive(Clone, Debug, Default)]
pub struct RenderOptions {
    pub only_main_content: bool,
    pub include_tags: Vec<String>,
    pub exclude_tags: Vec<String>,
    pub remove_base64_images: bool,
    pub markdown: bool,
    pub html: bool,
    pub links: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageMetadata {
    pub title: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub keywords: Option<String>,
    pub robots: Option<String>,
    pub favicon: Option<String>,
    pub og_title: Option<String>,
    pub og_description: Option<String>,
    pub og_url: Option<String>,
    pub og_image: Option<String>,
    pub og_site_name: Option<String>,
    pub og_locale: Option<String>,
    pub published_time: Option<String>,
    pub modified_time: Option<String>,
    pub dc_date: Option<String>,
    pub dc_date_created: Option<String>,
    pub dc_terms_created: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct Rendered {
    pub markdown: Option<String>,
    pub html: Option<String>,
    pub links: Option<Vec<String>>,
    pub metadata: PageMetadata,
}

pub fn render(html: &str, base_url: &Url, opts: &RenderOptions) -> Rendered {
    let html = truncate(html, MAX_INPUT_BYTES);
    let doc = Document::from(html);
    let base = doc
        .try_select("base[href]")
        .and_then(|b| b.attr("href"))
        .and_then(|h| base_url.join(h.trim()).ok())
        .unwrap_or_else(|| base_url.clone());
    let metadata = metadata(&doc, &base);
    let links = opts.links.then(|| links(&doc, &base));

    doc.select(STRIP_TAGS).remove();
    let body = match doc.try_select("body").map(|b| b.inner_html()) {
        Some(inner) if !opts.include_tags.is_empty() => included(&inner, &opts.include_tags),
        Some(inner) => Document::from(format!("<body>{inner}</body>")),
        None => Document::from("<body></body>"),
    };
    for sel in &opts.exclude_tags {
        remove_matches(&body, sel, false);
    }
    if opts.only_main_content {
        remove_matches(&body, NON_MAIN, true);
    }
    fix_images(&body, opts.remove_base64_images);
    absolutize(&body, &base);

    let cleaned = body.select("body").inner_html().to_string();
    let markdown = opts
        .markdown
        .then(|| collapse_blank_lines(&htmd::convert(&cleaned).unwrap_or_default()));
    Rendered {
        markdown,
        html: opts.html.then_some(cleaned),
        links,
        metadata,
    }
}

pub fn is_challenge(status: u16, headers: &[(String, String)], html: &str) -> bool {
    if headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("cf-mitigated") && v.trim().eq_ignore_ascii_case("challenge")
    }) {
        return true;
    }
    let gated =
        CHALLENGE_STATUSES.contains(&status) || (status == 200 && html.len() < SMALL_BODY_BYTES);
    if !gated {
        return false;
    }
    let title = title_prefix(html).to_lowercase();
    CHALLENGE_TITLES.iter().any(|t| title.contains(t))
        || CHALLENGE_BODY_MARKERS.iter().any(|m| html.contains(m))
}

/// Text of the first `<title>`, found by scanning only the first 64 KiB.
fn title_prefix(html: &str) -> String {
    let head = truncate(html, 64 * 1024);
    let lower = head.to_ascii_lowercase();
    let Some(open) = lower.find("<title") else {
        return String::new();
    };
    let Some(start) = lower[open..].find('>').map(|i| open + i + 1) else {
        return String::new();
    };
    let end = lower[start..]
        .find("</title")
        .map_or(lower.len(), |i| start + i);
    head[start..end].trim().to_string()
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Keeps only the outermost elements matching any include selector, in document order.
fn included(inner: &str, selectors: &[String]) -> Document {
    let src = Document::from(format!("<body>{inner}</body>"));
    let out = Document::from("<body></body>");
    let joined = selectors
        .iter()
        .filter(|s| valid(s))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if joined.is_empty() {
        return out;
    }
    let matches = src.select(&joined);
    let ids: HashSet<_> = matches.nodes().iter().map(|n| n.id).collect();
    let mut html = String::new();
    for n in matches.nodes() {
        if !n.ancestors(None).iter().any(|a| ids.contains(&a.id)) {
            html.push_str(&n.html());
        }
    }
    out.select("body").set_html(html);
    out
}

fn valid(selector: &str) -> bool {
    dom_query::Matcher::new(selector).is_ok()
}

fn remove_matches(doc: &Document, selector: &str, keep_main: bool) {
    if !valid(selector) {
        return;
    }
    for n in doc.select(selector).nodes() {
        let sel = Selection::from(*n);
        if keep_main && (sel.is(MAIN_MARKERS) || sel.select(MAIN_MARKERS).exists()) {
            continue;
        }
        n.remove_from_parent();
    }
}

fn fix_images(doc: &Document, remove_base64: bool) {
    for n in doc.select("img").nodes() {
        let img = Selection::from(*n);
        if remove_base64 && img.attr_or("src", "").trim_start().starts_with("data:") {
            match img.attr("alt").filter(|a| !a.trim().is_empty()) {
                Some(alt) => n.replace_with_html(alt.trim().to_string()),
                None => n.remove_from_parent(),
            }
        }
    }
}

fn absolutize(doc: &Document, base: &Url) {
    for (sel, attr) in [("a[href]", "href"), ("img[src]", "src")] {
        for n in doc.select(sel).nodes() {
            let Some(v) = n.attr(attr).filter(|v| !v.trim_start().starts_with('#')) else {
                continue;
            };
            if let Ok(u) = base.join(v.trim()) {
                n.set_attr(attr, u.as_str());
            }
        }
    }
    for n in doc.select("img[srcset]").nodes() {
        let best = n.attr("srcset").and_then(|s| largest_candidate(&s));
        if let Some(u) = best.and_then(|c| base.join(&c).ok()) {
            n.set_attr("src", u.as_str());
        }
    }
}

/// Picks the srcset candidate with the largest `w`/`x` descriptor.
fn largest_candidate(srcset: &str) -> Option<String> {
    srcset
        .split(',')
        .filter_map(|c| {
            let mut parts = c.split_whitespace();
            let url = parts.next()?;
            let weight = parts
                .next()
                .and_then(|d| d.trim_end_matches(['w', 'x']).parse::<f64>().ok())
                .unwrap_or(1.0);
            Some((weight, url))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, u)| u.to_string())
}

fn links(doc: &Document, base: &Url) -> Vec<String> {
    let mut seen = HashSet::new();
    doc.select("a[href]")
        .nodes()
        .iter()
        .filter_map(|n| {
            let href = n.attr("href")?;
            let href = href.trim();
            base.join(href).ok().filter(|_| !href.starts_with('#'))
        })
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .map(|u| u.to_string())
        .filter(|u| seen.insert(u.clone()))
        .collect()
}

fn metadata(doc: &Document, base: &Url) -> PageMetadata {
    let mut m = PageMetadata::default();
    let text = |s: String| Some(s.trim().to_string()).filter(|t| !t.is_empty());
    m.title = text(doc.select("title").first().text().to_string());
    m.language = doc
        .select("html")
        .attr("lang")
        .and_then(|l| text(l.to_string()));
    m.favicon = doc
        .select("link[rel][href]")
        .nodes()
        .iter()
        .filter(|n| n.attr_or("rel", "").to_ascii_lowercase().contains("icon"))
        .filter_map(|n| base.join(n.attr("href")?.trim()).ok())
        .find(|u| matches!(u.scheme(), "http" | "https"))
        .map(|u| u.to_string());

    let mut named = BTreeMap::new();
    for n in doc.select("meta[content]").nodes() {
        let Some(content) = text(n.attr_or("content", "").to_string()) else {
            continue;
        };
        for attr in ["name", "property", "itemprop"] {
            if let Some(key) = n
                .attr(attr)
                .map(|k| k.trim().to_ascii_lowercase())
                .filter(|k| !k.is_empty())
            {
                named.entry(key).or_insert_with(|| content.clone());
            }
        }
    }
    let mut take = |key: &str| named.remove(key);
    m.description = take("description");
    m.keywords = take("keywords");
    m.robots = take("robots");
    m.og_title = take("og:title");
    m.og_description = take("og:description");
    m.og_url = take("og:url");
    m.og_image = take("og:image");
    m.og_site_name = take("og:site_name");
    m.og_locale = take("og:locale");
    m.published_time = take("article:published_time");
    m.modified_time = take("article:modified_time");
    m.dc_date = take("dc.date");
    m.dc_date_created = take("dc.date.created");
    m.dc_terms_created = take("dcterms.created");
    m.extra = named
        .into_iter()
        .take(MAX_EXTRA_ENTRIES)
        .map(|(k, v)| (k, v.chars().take(MAX_EXTRA_CHARS).collect()))
        .collect();
    m
}

/// Keeps at most one empty line in a row and trims the ends.
fn collapse_blank_lines(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut blank = false;
    for line in md.lines().map(str::trim_end) {
        if line.is_empty() {
            blank = true;
            continue;
        }
        if blank && !out.is_empty() {
            out.push('\n');
        }
        blank = false;
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}
