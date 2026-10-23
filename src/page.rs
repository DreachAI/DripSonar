//! Pure HTML transforms: cleanup, main content, markdown, metadata, links.

mod fit;

use std::collections::{BTreeMap, HashSet};

use dom_query::{Document, Selection};
use url::Url;

use crate::extract::css;

pub(crate) const MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;
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

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RenderOptions {
    pub only_main_content: bool,
    pub include_tags: Vec<String>,
    pub exclude_tags: Vec<String>,
    pub remove_base64_images: bool,
    pub markdown: bool,
    pub html: bool,
    pub links: bool,
    /// Firecrawl's `parsePDF: false`: a PDF yields no text. Absent in stored job templates.
    #[serde(default)]
    pub skip_pdf_parsing: bool,
    /// DripSonar's `fitMarkdown` format: readability, pruning, citation-style links.
    #[serde(default)]
    pub fit_markdown: bool,
    /// Deterministic CSS extraction into `json`; never set for jobs.
    #[serde(default)]
    pub selectors: Option<css::Schema>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
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
    pub og_locale_alternate: Option<Vec<String>>,
    pub published_time: Option<String>,
    pub modified_time: Option<String>,
    pub dc_date: Option<String>,
    pub dc_date_created: Option<String>,
    pub dc_terms_created: Option<String>,
    /// Other meta tags by lowercased key: one value as a string, repeated values as an array.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Rendered {
    pub markdown: Option<String>,
    #[serde(default)]
    pub fit_markdown: Option<String>,
    /// CSS extraction result; never persisted by jobs, which refuse the json format.
    #[serde(default)]
    pub json: Option<serde_json::Value>,
    pub html: Option<String>,
    pub links: Option<Vec<String>>,
    pub metadata: PageMetadata,
}

pub fn render(html: &str, base_url: &Url, opts: &RenderOptions) -> Rendered {
    render_document(parse(html), base_url, opts)
}

pub(crate) fn parse(html: &str) -> Document {
    Document::from(truncate(html, MAX_INPUT_BYTES))
}

pub(crate) fn render_document(doc: Document, base_url: &Url, opts: &RenderOptions) -> Rendered {
    let base = doc
        .try_select("base[href]")
        .and_then(|b| b.attr("href"))
        .and_then(|h| base_url.join(h.trim()).ok())
        .unwrap_or_else(|| base_url.clone());
    let metadata = metadata(&doc, &base);
    let links = opts.links.then(|| links(&doc, &base));
    let json = opts
        .selectors
        .as_ref()
        .map(|schema| css::extract(&doc, schema));

    doc.select(STRIP_TAGS).remove();
    // Selection used to reparse the body in a fresh wrapper; keep that selector scope.
    doc.select("head")
        .nodes()
        .iter()
        .for_each(|n| n.remove_children());
    for node in doc.select("html, head, body").nodes() {
        node.remove_all_attrs();
    }
    if !opts.include_tags.is_empty() {
        included(&doc, &opts.include_tags);
    }
    for selector in &opts.exclude_tags {
        remove_matches(&doc, selector, false);
    }
    fix_images(&doc, opts.remove_base64_images);
    absolutize(&doc, &base);
    let full = (opts.only_main_content || opts.fit_markdown)
        .then(|| doc.select("body").inner_html().to_string());
    if opts.only_main_content {
        remove_matches(&doc, NON_MAIN, true);
    }
    let mut body = doc;
    // Some sites wrap all their text in a non-main class; an empty page helps nobody.
    if !has_text(&body)
        && let Some(full) = &full
    {
        let fallback = Document::from(format!("<body>{full}</body>"));
        if has_text(&fallback) {
            body = fallback;
        }
    }

    let cleaned = body.select("body").inner_html().to_string();
    // Firecrawl's selectors stay primary; readability stands in when they leave a thin page.
    let thin = opts.markdown
        && opts.only_main_content
        && fit::text_chars(&body.select("body").text()) < fit::MIN_TEXT_CHARS;
    let article = (thin || opts.fit_markdown)
        .then(|| full.as_deref().and_then(fit::readable))
        .flatten();
    let source = article.as_deref().filter(|_| thin).unwrap_or(&cleaned);
    let markdown = opts
        .markdown
        .then(|| cap_blank_runs(&htmd::convert(source).unwrap_or_default()));
    let fit_markdown = opts
        .fit_markdown
        .then(|| fit::markdown(article.as_deref().or(full.as_deref()).unwrap_or_default()));
    Rendered {
        markdown,
        fit_markdown,
        json,
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

fn has_text(body: &Document) -> bool {
    !body.select("body").text().trim().is_empty()
}

/// Keeps only the outermost elements matching any include selector, in document order.
fn included(doc: &Document, selectors: &[String]) {
    let joined = selectors
        .iter()
        .filter(|s| valid(s))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let Some(body) = doc.select("body").nodes().first().copied() else {
        return;
    };
    if joined.is_empty() {
        body.remove_children();
        return;
    }
    let matches = doc.select(&joined);
    if matches.iter().any(|s| s.is("html, body")) {
        return;
    }
    let ids: HashSet<_> = matches.nodes().iter().map(|n| n.id).collect();
    let kept: Vec<_> = matches
        .nodes()
        .iter()
        .copied()
        .filter(|n| {
            !Selection::from(*n).is("head")
                && !n.ancestors(None).iter().any(|a| ids.contains(&a.id))
        })
        .collect();
    for node in &kept {
        node.remove_from_parent();
    }
    body.remove_children();
    for node in kept {
        body.append_child(&node);
    }
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

    let mut named: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in doc.select("meta[content]").nodes() {
        let Some(content) = text(n.attr_or("content", "").to_string()) else {
            continue;
        };
        let mut keys: Vec<String> = ["name", "property", "itemprop"]
            .iter()
            .filter_map(|attr| n.attr(attr).map(|k| k.trim().to_ascii_lowercase()))
            .filter(|k| !k.is_empty())
            .collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            named.entry(key).or_default().push(content.clone());
        }
    }
    let mut take = |key: &str| {
        named
            .remove(key)
            .and_then(|values| values.into_iter().next())
    };
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
    m.og_locale_alternate = named.remove("og:locale:alternate");
    m.title = m
        .title
        .or_else(|| m.og_title.clone())
        .or_else(|| named.get("twitter:title").and_then(|v| v.first().cloned()));
    let clip =
        |v: &String| serde_json::Value::from(v.chars().take(MAX_EXTRA_CHARS).collect::<String>());
    m.extra = named
        .into_iter()
        .take(MAX_EXTRA_ENTRIES)
        .map(|(k, values)| match values.as_slice() {
            [one] => (k, clip(one)),
            many => (k, many.iter().take(MAX_EXTRA_ENTRIES).map(clip).collect()),
        })
        .collect();
    m
}

/// Caps runs of blank lines at two outside fenced code. Everything else stays as converted:
/// trailing spaces are hard breaks, and code keeps its blank lines.
fn cap_blank_runs(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let (mut fence, mut blanks) = (None, 0);
    for line in md.split('\n') {
        if fence.is_none() && line.trim().is_empty() {
            blanks += 1;
            if blanks > 2 {
                continue;
            }
        } else {
            blanks = 0;
        }
        fence = match (fence, fence_marker(line)) {
            (None, Some((marker, _))) => Some(marker),
            (Some((c, n)), Some(((m, len), closes))) if closes && c == m && len >= n => None,
            (open, _) => open,
        };
        out.push_str(line);
        out.push('\n');
    }
    out.trim_matches('\n').to_owned()
}

/// A fence line's character and length, and whether it could close a fence (no info string).
fn fence_marker(line: &str) -> Option<((char, usize), bool)> {
    let line = line.trim_start();
    let c = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = line.chars().take_while(|&x| x == c).count();
    (len >= 3).then(|| ((c, len), line[len..].trim().is_empty()))
}
