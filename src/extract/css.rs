//! Deterministic CSS extraction: a Crawl4AI-like selector schema over the page DOM, no LLM.

use std::collections::HashSet;

use dom_query::{Document, Matcher, NodeRef, Selection};
use serde_json::{Map, Value};

const MAX_ITEMS: usize = 1000;
const MAX_FIELDS: usize = 100;
const MAX_DEPTH: usize = 4;
/// Nodes visited across all selections and values; a careless schema cannot pin a render slot.
const MAX_VISITS: usize = 2_000_000;

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
/// `Null` when the work limit is reached, so a partial result never passes as complete.
pub fn extract(doc: &Document, schema: &Schema) -> Value {
    let mut budget = MAX_VISITS;
    let root = Selection::from(doc.root());
    let value = match &schema.base_selector {
        Some(base) => all(&root, base, &mut budget)
            .into_iter()
            .map(|item| object(&item, &schema.fields, &mut budget))
            .collect(),
        None => object(&root, &schema.fields, &mut budget),
    };
    if budget == 0 { Value::Null } else { value }
}

fn object(scope: &Selection, fields: &[Field], budget: &mut usize) -> Value {
    let mut out = Map::new();
    for field in fields {
        out.insert(field.name.clone(), value(scope, field, budget));
    }
    Value::Object(out)
}

fn value(scope: &Selection, field: &Field, budget: &mut usize) -> Value {
    if let Kind::List = field.kind {
        let items = match &field.selector {
            Some(selector) => all(scope, selector, budget),
            None => scope.nodes().iter().map(|n| Selection::from(*n)).collect(),
        };
        return items
            .iter()
            .map(|item| match field.fields.as_slice() {
                [] => text(item, budget),
                fields => object(item, fields, budget),
            })
            .collect();
    }
    let target = match &field.selector {
        Some(selector) => first(scope, selector, budget),
        None => scope.nodes().first().copied(),
    };
    let Some(node) = target else {
        return Value::Null;
    };
    let element = Selection::from(node);
    match field.kind {
        Kind::Attribute => field
            .attribute
            .as_deref()
            .and_then(|name| element.attr(name))
            .map_or(Value::Null, |v| v.trim().into()),
        Kind::Html if charge(&node, budget) => element.html().to_string().into(),
        Kind::Html => Value::Null,
        Kind::Nested => object(&element, &field.fields, budget),
        Kind::Text | Kind::List => text(&element, budget),
    }
}

/// Every match under the scope, capped at `MAX_ITEMS`.
fn all<'a>(scope: &Selection<'a>, selector: &str, budget: &mut usize) -> Vec<Selection<'a>> {
    let Ok(matcher) = Matcher::new(selector) else {
        return Vec::new();
    };
    if !scope.nodes().iter().all(|n| charge(n, budget)) {
        return Vec::new();
    }
    let matches = scope.select_matcher(&matcher);
    let nodes = matches.nodes().iter().take(MAX_ITEMS);
    nodes.map(|n| Selection::from(*n)).collect()
}

fn first<'a>(scope: &Selection<'a>, selector: &str, budget: &mut usize) -> Option<NodeRef<'a>> {
    let matcher = Matcher::new(selector).ok()?;
    if !scope.nodes().iter().all(|n| charge(n, budget)) {
        return None;
    }
    scope
        .select_single_matcher(&matcher)
        .nodes()
        .first()
        .copied()
}

fn text(element: &Selection, budget: &mut usize) -> Value {
    if !element.nodes().iter().all(|n| charge(n, budget)) {
        return Value::Null;
    }
    let text = element.text();
    text.split_whitespace().collect::<Vec<_>>().join(" ").into()
}

/// Spends the node's subtree size; counting stops at the remaining budget, so it is bounded too.
fn charge(node: &NodeRef, budget: &mut usize) -> bool {
    let size = 1 + node.descendants_it().take(*budget).count();
    *budget = budget.saturating_sub(size);
    *budget > 0
}
