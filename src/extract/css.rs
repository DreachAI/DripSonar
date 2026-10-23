//! Deterministic CSS extraction: a Crawl4AI-like selector schema over the page DOM, no LLM.

use std::collections::HashSet;

use dom_query::{Document, Matcher, NodeRef};
use serde_json::{Map, Value};

use crate::page;

const MAX_ITEMS: usize = 1000;
const MAX_FIELDS: usize = 100;
const MAX_DEPTH: usize = 4;
/// Nodes visited across all selections and values; a careless schema cannot pin a render slot.
const MAX_VISITS: usize = 2_000_000;
/// Output bytes across all keys and values of one extraction.
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Quotes, colon, comma, and a `null` beside each key name.
const KEY_OVERHEAD_BYTES: usize = 8;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Schema {
    base_selector: Option<String>,
    fields: Vec<Field>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Field {
    name: String,
    /// Absent means the element in scope itself.
    selector: Option<String>,
    #[serde(rename = "type")]
    kind: Kind,
    attribute: Option<String>,
    #[serde(default)]
    fields: Vec<Field>,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Text,
    Attribute,
    Html,
    List,
    Nested,
}

impl Schema {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(base) = &self.base_selector {
            check_selector(base, "baseSelector")?;
        }
        check_fields(&self.fields, 1, &mut 0)
    }
}

fn check_selector(selector: &str, name: &str) -> Result<(), String> {
    Matcher::new(selector)
        .map(|_| ())
        .map_err(|_| format!("selectors.{name}: invalid CSS selector"))
}

fn check_fields(fields: &[Field], depth: usize, count: &mut usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("selectors nesting exceeds {MAX_DEPTH}"));
    }
    if fields.is_empty() {
        return Err("selectors fields must not be empty".into());
    }
    let mut names = HashSet::new();
    for field in fields {
        *count += 1;
        if *count > MAX_FIELDS {
            return Err(format!("selectors may define at most {MAX_FIELDS} fields"));
        }
        let name = field.name.as_str();
        if name.is_empty() || !names.insert(name) {
            return Err("selectors field names must be nonempty and unique".into());
        }
        if let Some(selector) = &field.selector {
            check_selector(selector, name)?;
        }
        if matches!(field.kind, Kind::Attribute) != field.attribute.is_some() {
            return Err(format!(
                "selectors.{name}: attribute is required for, and only allowed with, type attribute"
            ));
        }
        match field.kind {
            Kind::List if field.fields.is_empty() => {}
            Kind::List | Kind::Nested => check_fields(&field.fields, depth + 1, count)?,
            _ if !field.fields.is_empty() => {
                return Err(format!(
                    "selectors.{name}: fields are only allowed with list or nested"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// An array of objects, one per `baseSelector` match, or one object for the whole page.
/// `Null` when the DOM exceeds the page limits or the work or output budget runs out, so a
/// partial result never passes as complete.
pub fn extract(doc: &Document, schema: &Schema) -> Value {
    let root = doc.root();
    if !page::dom_within_limits(root) {
        return Value::Null;
    }
    let mut budget = Budget {
        visits: MAX_VISITS,
        bytes: MAX_OUTPUT_BYTES,
        exhausted: false,
    };
    let value = match &schema.base_selector {
        Some(base) => matches(root, base, MAX_ITEMS, &mut budget)
            .into_iter()
            .map(|item| object(item, &schema.fields, &mut budget))
            .collect(),
        None => object(root, &schema.fields, &mut budget),
    };
    if budget.exhausted { Value::Null } else { value }
}

/// Work and output allowance shared by one extraction; once exhausted, every charge fails.
struct Budget {
    visits: usize,
    bytes: usize,
    exhausted: bool,
}

impl Budget {
    fn visit(&mut self) -> bool {
        self.exhausted |= self.visits == 0;
        self.visits = self.visits.saturating_sub(1);
        !self.exhausted
    }

    /// Charged before output is copied, so the budget bounds memory, not just the result.
    fn spend(&mut self, bytes: usize) -> bool {
        self.exhausted |= bytes > self.bytes;
        self.bytes = self.bytes.saturating_sub(bytes);
        !self.exhausted
    }
}

fn object(scope: NodeRef, fields: &[Field], budget: &mut Budget) -> Value {
    let mut out = Map::new();
    for field in fields {
        if !budget.spend(field.name.len() + KEY_OVERHEAD_BYTES) {
            return Value::Null;
        }
        out.insert(field.name.clone(), value(scope, field, budget));
    }
    Value::Object(out)
}

fn value(scope: NodeRef, field: &Field, budget: &mut Budget) -> Value {
    if let Kind::List = field.kind {
        let items = match &field.selector {
            Some(selector) => matches(scope, selector, MAX_ITEMS, budget),
            None => vec![scope],
        };
        return items
            .into_iter()
            .map(|item| match field.fields.as_slice() {
                [] => text(item, budget),
                fields => object(item, fields, budget),
            })
            .collect();
    }
    let target = match &field.selector {
        Some(selector) => matches(scope, selector, 1, budget).pop(),
        None => Some(scope),
    };
    let Some(node) = target else {
        return Value::Null;
    };
    match field.kind {
        Kind::Attribute => field
            .attribute
            .as_deref()
            .and_then(|name| node.attr(name))
            .filter(|v| budget.spend(v.len()))
            .map_or(Value::Null, |v| v.trim().into()),
        Kind::Html => html(node, budget),
        Kind::Nested => object(node, &field.fields, budget),
        Kind::Text | Kind::List => text(node, budget),
    }
}

/// Up to `limit` matches under the scope in document order, spending one visit per node walked.
fn matches<'a>(
    scope: NodeRef<'a>,
    selector: &str,
    limit: usize,
    budget: &mut Budget,
) -> Vec<NodeRef<'a>> {
    let Ok(matcher) = Matcher::new(selector) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for node in scope.descendants_it() {
        if found.len() == limit || !budget.visit() {
            break;
        }
        if node.is_match(&matcher) {
            found.push(node);
        }
    }
    found
}

fn text(node: NodeRef, budget: &mut Budget) -> Value {
    // Raw text bytes bound the whitespace-collapsed copy.
    match size(node, false, budget) {
        Some(bytes) if budget.spend(bytes) => {
            let text = node.text();
            text.split_whitespace().collect::<Vec<_>>().join(" ").into()
        }
        _ => Value::Null,
    }
}

fn html(node: NodeRef, budget: &mut Budget) -> Value {
    let Some(estimate) = size(node, true, budget).filter(|&bytes| budget.spend(bytes)) else {
        return Value::Null;
    };
    let html = node.html();
    // Escaping can outgrow the estimate; charge the rest so the total stays exact.
    if budget.spend(html.len().saturating_sub(estimate)) {
        html.to_string().into()
    } else {
        Value::Null
    }
}

/// The subtree's text bytes, or with `markup` its unescaped serialized size, spending one visit
/// per node; `None` when the visits run out.
fn size(node: NodeRef, markup: bool, budget: &mut Budget) -> Option<usize> {
    let mut bytes = 0;
    for n in std::iter::once(node).chain(node.descendants_it()) {
        if !budget.visit() {
            return None;
        }
        if n.is_text() {
            bytes += n.text().len();
        } else if markup && n.is_element() {
            let name = n.node_name().map_or(0, |name| name.len());
            let attrs: usize = n
                .attrs()
                .iter()
                .map(|a| a.name.local.len() + a.value.len() + 4)
                .sum();
            bytes += 2 * name + 5 + attrs;
        }
    }
    Some(bytes)
}
