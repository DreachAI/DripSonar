//! Fit markdown: readability, then pruning of low-value blocks, then citation-style links.
//!
//! The pruning score follows Crawl4AI's `PruningContentFilter` as specified, reimplemented here.

use std::collections::HashMap;

use dom_query::{Document, NodeId, NodeRef};
use dom_smoothie::{Config, Readability};
use url::Url;

/// Below this many visible characters a readability article (or a main-content body) is thin.
pub(super) const MIN_TEXT_CHARS: usize = 200;
const PRUNE_THRESHOLD: f64 = 0.48;
const BOILERPLATE_TAGS: &str = "nav, footer, header, aside, script, style, form, iframe, noscript";
/// Leaf elements carry no text of their own; they stand or fall with their container.
const VOID_TAGS: [&str; 4] = ["img", "br", "hr", "wbr"];
/// Private-use markers around image citations, so the converter leaves `![`/`]` unescaped.
const IMAGE_OPEN: char = '\u{E000}';
const IMAGE_CLOSE: char = '\u{E001}';

/// A class or id containing any of these, case-insensitively, marks boilerplate.
const BOILERPLATE_NAMES: [&str; 10] = [
    "nav", "footer", "header", "sidebar", "ads", "comment", "promo", "advert", "social", "share",
];

pub(super) fn text_chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// The readability article's HTML, when it holds at least `MIN_TEXT_CHARS` visible characters.
pub(super) fn readable(body_html: &str) -> Option<String> {
    let config = Config {
        // Pruning reads class and id names after readability.
        keep_classes: true,
        disable_json_ld: true,
        ..Config::default()
    };
    let article = Readability::new(body_html, None, Some(config))
        .ok()?
        .parse()
        .ok()?;
    (text_chars(&article.text_content) >= MIN_TEXT_CHARS).then(|| article.content.to_string())
}

/// Prunes the article (or body) HTML and renders it as markdown with numbered references.
pub(super) fn markdown(html: &str) -> String {
    let doc = Document::from(format!("<body>{html}</body>"));
    let Some(body) = doc.body() else {
        return String::new();
    };
    doc.select(BOILERPLATE_TAGS).remove();
    for node in body.descendants().iter().filter(|n| n.is_comment()) {
        node.remove_from_parent();
    }
    prune(&body);
    let references = cite(&doc);
    let converted = htmd::convert(&body.inner_html()).unwrap_or_default();
    let mut out = super::cap_blank_runs(&converted)
        .replace(IMAGE_OPEN, "![")
        .replace(IMAGE_CLOSE, "]");
    if !references.is_empty() {
        out.push_str("\n\n## References\n\n");
        let lines: Vec<String> = references
            .iter()
            .enumerate()
            .map(|(i, (url, title))| match title.as_str() {
                "" => format!("⟨{}⟩ {url}", i + 1),
                title => format!("⟨{}⟩ {url}: {title}", i + 1),
            })
            .collect();
        out.push_str(&lines.join("\n"));
    }
    out
}

#[derive(Clone, Copy, Default)]
struct Stats {
    text: usize,
    inner: usize,
    outer: usize,
    links: usize,
}

/// Walks top-down, dropping boilerplate-named elements and subtrees scoring under the threshold.
fn prune(body: &NodeRef) {
    let stats = measure(body);
    let mut stack = body.element_children();
    while let Some(node) = stack.pop() {
        if node.class().is_some_and(|v| boilerplate(&v))
            || node.id_attr().is_some_and(|v| boilerplate(&v))
        {
            node.remove_from_parent();
            continue;
        }
        let name = node.node_name().unwrap_or_default();
        if VOID_TAGS.contains(&name.as_ref()) {
            continue;
        }
        let s = stats.get(&node.id).copied().unwrap_or_default();
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

fn boilerplate(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    BOILERPLATE_NAMES.iter().any(|word| name.contains(word))
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

/// Per-element stripped text length, approximate inner and outer markup length, and the text
/// of direct `<a>` children, in one bottom-up pass (linear, no recursion).
fn measure(body: &NodeRef) -> HashMap<NodeId, Stats> {
    let mut stats: HashMap<NodeId, Stats> = HashMap::new();
    for node in body.descendants().iter().rev().filter(|n| n.is_element()) {
        let mut s = Stats::default();
        for child in node.children_it(false) {
            if child.is_text() {
                let text = child.text();
                s.text += text.trim().chars().count();
                s.inner += text.len();
            } else if let Some(c) = stats.get(&child.id).copied() {
                s.text += c.text;
                s.inner += c.outer;
                if child.has_name("a") {
                    s.links += c.text;
                }
            }
        }
        let name = node.node_name().map_or(0, |n| n.len());
        let attrs: usize = node
            .attrs()
            .iter()
            .map(|a| a.name.local.len() + a.value.len() + 4)
            .sum();
        s.outer = s.inner + 2 * name + 5 + attrs;
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
