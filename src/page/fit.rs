//! Fit markdown: readability, then pruning of low-value blocks, then citation-style links.
//!
//! The pruning score follows Crawl4AI's `PruningContentFilter` as specified, reimplemented here.

use std::collections::{HashMap, HashSet};

use dom_query::{Document, NodeId, NodeRef, Selection};
use dom_smoothie::{Config, Readability};
use url::Url;

/// Below this many visible characters a readability article is thin; the body stands in.
const MIN_TEXT_CHARS: usize = 200;
/// Readability scores the tree in several passes, so it stops short of the page DOM limit.
const READABILITY_MAX_ELEMENTS: usize = 100_000;
const PRUNE_THRESHOLD: f64 = 0.48;
/// Pruning that keeps less than 1/`MIN_KEPT_FRACTION` of the text misjudged the page.
const MIN_KEPT_FRACTION: usize = 5;
const BOILERPLATE_TAGS: &str = "nav, footer, header, aside, script, style, form, iframe, noscript";
/// Boilerplate class and id names never remove these or their ancestors.
const CONTENT_MARKERS: &str = "main, article, [role=main]";
/// Leaf elements carry no text of their own; they stand or fall with their container.
const VOID_TAGS: [&str; 4] = ["img", "br", "hr", "wbr"];
/// Private-use markers around image citations, so the converter leaves `![`/`]` unescaped.
const IMAGE_OPEN: char = '\u{E000}';
const IMAGE_CLOSE: char = '\u{E001}';

/// A class or id with any of these as a whole token (split on non-alphanumerics, any case) marks
/// boilerplate.
const BOILERPLATE_NAMES: [&str; 13] = [
    "nav", "footer", "header", "sidebar", "ads", "ad", "comment", "comments", "promo", "advert",
    "social", "share", "sharing",
];

fn text_chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// Renders the cleaned body HTML as fit markdown. Readability runs only when `readable_dom`
/// (the page DOM is within limits); otherwise the body is pruned directly.
pub(super) fn markdown(body_html: &str, readable_dom: bool) -> String {
    let article = readable_dom.then(|| readable(body_html)).flatten();
    let html = article.as_deref().unwrap_or(body_html);
    let mut doc = parse_body(html);
    let before = visible_chars(&doc);
    prune(&doc);
    if visible_chars(&doc) * MIN_KEPT_FRACTION < before {
        doc = parse_body(html);
    }
    render(&doc)
}

/// The readability article's HTML, when it holds at least `MIN_TEXT_CHARS` visible characters.
fn readable(body_html: &str) -> Option<String> {
    let config = Config {
        // Pruning reads class and id names after readability.
        keep_classes: true,
        disable_json_ld: true,
        max_elements_to_parse: READABILITY_MAX_ELEMENTS,
        ..Config::default()
    };
    let article = Readability::new(body_html, None, Some(config))
        .ok()?
        .parse()
        .ok()?;
    (text_chars(&article.text_content) >= MIN_TEXT_CHARS).then(|| article.content.to_string())
}

fn parse_body(html: &str) -> Document {
    let doc = Document::from(format!("<body>{html}</body>"));
    if let Some(body) = doc.body() {
        for node in body.descendants().iter().filter(|n| n.is_comment()) {
            node.remove_from_parent();
        }
    }
    doc
}

fn visible_chars(doc: &Document) -> usize {
    text_chars(&doc.select("body").text())
}

/// Converts the body to markdown with numbered references.
fn render(doc: &Document) -> String {
    let Some(body) = doc.body() else {
        return String::new();
    };
    let references = cite(doc);
    let converted = htmd::convert(&body.inner_html()).unwrap_or_default();
    let mut out = super::cap_blank_runs(&converted)
        .replace(IMAGE_OPEN, "![")
        .replace(IMAGE_CLOSE, "]");
    if !references.is_empty() {
        out.push_str("\n\n## References\n\n");
        let lines: Vec<String> = references
            .iter()
            .enumerate()
            .map(|(i, (url, title))| match escape_reference(title).as_str() {
                "" => format!("⟨{}⟩ {url}", i + 1),
                title => format!("⟨{}⟩ {url}: {title}", i + 1),
            })
            .collect();
        out.push_str(&lines.join("\n"));
    }
    out
}

/// Collapses whitespace (so no newline survives) and backslash-escapes characters that could
/// forge markup or a reference marker.
fn escape_reference(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        for c in word.chars() {
            if matches!(
                c,
                '\\' | '[' | ']' | '*' | '_' | '`' | '<' | '>' | '⟨' | '⟩'
            ) {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

#[derive(Clone, Copy, Default)]
struct Stats {
    text: usize,
    inner: usize,
    outer: usize,
    links: usize,
    image: bool,
    linked_image: bool,
}

/// Drops boilerplate tags, then walks top-down, dropping boilerplate-named elements and subtrees
/// scoring under the threshold.
fn prune(doc: &Document) {
    doc.select(BOILERPLATE_TAGS).remove();
    let Some(body) = doc.body() else {
        return;
    };
    let stats = measure(&body);
    let protected = content_and_ancestors(&body);
    let mut stack = body.element_children();
    while let Some(node) = stack.pop() {
        let named = node.class().is_some_and(|v| boilerplate(&v))
            || node.id_attr().is_some_and(|v| boilerplate(&v));
        if named && !protected.contains(&node.id) {
            node.remove_from_parent();
            continue;
        }
        let name = node.node_name().unwrap_or_default();
        if VOID_TAGS.contains(&name.as_ref()) {
            continue;
        }
        let s = stats.get(&node.id).copied().unwrap_or_default();
        // A linked image has no text to score; it stands or falls with its first scored container.
        if s.text == 0 && s.linked_image {
            stack.extend(node.element_children());
            continue;
        }
        let density = s.text as f64 / s.inner.max(1) as f64;
        let link_density = s.links as f64 / s.text.max(1) as f64;
        let score = 0.4 * density
            + 0.2 * (1.0 - link_density)
            + 0.2 * tag_weight(&name)
            + 0.1 * (s.text as f64 + 1.0).ln();
        if score < PRUNE_THRESHOLD {
            node.remove_from_parent();
        } else {
            stack.extend(node.element_children());
        }
    }
}

/// Every `CONTENT_MARKERS` element and its ancestors; each node is visited once.
fn content_and_ancestors(body: &NodeRef) -> HashSet<NodeId> {
    let mut ids = HashSet::new();
    for node in Selection::from(*body).select(CONTENT_MARKERS).nodes() {
        for n in std::iter::once(*node).chain(node.ancestors_it(None)) {
            if !ids.insert(n.id) {
                break;
            }
        }
    }
    ids
}

fn boilerplate(name: &str) -> bool {
    name.split(|c: char| !c.is_alphanumeric()).any(|token| {
        BOILERPLATE_NAMES
            .iter()
            .any(|w| token.eq_ignore_ascii_case(w))
    })
}

fn tag_weight(name: &str) -> f64 {
    match name {
        "article" => 1.5,
        "h1" => 1.2,
        "h2" => 1.1,
        "p" | "section" | "h3" => 1.0,
        "h4" => 0.9,
        "h5" => 0.8,
        "h6" => 0.7,
        "span" => 0.3,
        _ => 0.5,
    }
}

/// Characters a value takes once serialized: `&`, `<`, `>` and no-break spaces in text, and `&`,
/// `"` and no-break spaces in attributes, become entities.
fn escaped_chars(value: &str, attribute: bool) -> usize {
    value
        .chars()
        .map(|c| match c {
            '&' => 5,
            '\u{a0}' => 6,
            '<' | '>' if !attribute => 4,
            '"' if attribute => 6,
            _ => 1,
        })
        .sum()
}

/// Per-element stripped text length, inner and outer serialized length, the text of direct `<a>`
/// children, and image presence, in characters, in one bottom-up pass (linear, no recursion).
fn measure(body: &NodeRef) -> HashMap<NodeId, Stats> {
    let mut stats: HashMap<NodeId, Stats> = HashMap::new();
    for node in body.descendants().iter().rev().filter(|n| n.is_element()) {
        let mut s = Stats::default();
        for child in node.children_it(false) {
            if child.is_text() {
                let text = child.text();
                s.text += text.trim().chars().count();
                s.inner += escaped_chars(&text, false);
            } else if let Some(c) = stats.get(&child.id).copied() {
                s.text += c.text;
                s.inner += c.outer;
                s.image |= c.image;
                s.linked_image |= c.linked_image;
                if child.has_name("a") {
                    s.links += c.text;
                }
            }
        }
        let name = node.node_name().unwrap_or_default();
        s.image |= name.as_ref() == "img";
        s.linked_image |= s.image && name.as_ref() == "a";
        let name_chars = name.chars().count();
        let attrs: usize = node
            .attrs()
            .iter()
            .map(|a| a.name.local.chars().count() + escaped_chars(&a.value, true) + 4)
            .sum();
        // `<name attrs>`, plus `</name>` unless void.
        s.outer = s.inner + name_chars + 2 + attrs;
        if !VOID_TAGS.contains(&name.as_ref()) {
            s.outer += name_chars + 3;
        }
        stats.insert(node.id, s);
    }
    stats
}

/// Replaces each http(s) link with `text⟨N⟩` and each image with `![alt⟨N⟩]`, numbering unique
/// URLs in first-seen order; returns `(url, title)` per number. Other links keep only their text.
fn cite(doc: &Document) -> Vec<(String, String)> {
    let mut refs: Vec<(String, String)> = Vec::new();
    let mut numbers: HashMap<String, usize> = HashMap::new();
    for node in doc.select("a[href], img").nodes() {
        let image = node.has_name("img");
        let label = if image {
            node.attr_or("alt", "").trim().to_string()
        } else {
            node.text().split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let url = node
            .attr(if image { "src" } else { "href" })
            .and_then(|u| Url::parse(u.trim()).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .map(String::from);
        let number = url.map(|url| {
            *numbers.entry(url.clone()).or_insert_with(|| {
                let title = node.attr("title").map(|t| t.trim().to_string());
                refs.push((
                    url,
                    title.filter(|t| !t.is_empty()).unwrap_or(label.clone()),
                ));
                refs.len()
            })
        });
        let marker = number.map(|n| format!("⟨{n}⟩")).unwrap_or_default();
        if image {
            let text = if marker.is_empty() {
                label
            } else {
                format!("{IMAGE_OPEN}{label}{marker}{IMAGE_CLOSE}")
            };
            node.replace_with(&doc.tree.new_text(text));
        } else {
            node.append_child(&doc.tree.new_text(marker));
            // Unwrapping a child lifts the anchor's children into its place.
            if let Some(child) = node.first_child() {
                child.unwrap_node();
            }
        }
    }
    refs
}
