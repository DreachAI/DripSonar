//! Keeps one MCP tool result within the configured character budget.

use serde_json::Value;

const MARK: &str = " …[truncated]";
/// Strings are never cut below this length; lists are shortened instead.
const MIN_KEEP: usize = 200;
/// Keys whose string value is page text that may be cut. Identifiers, URLs, codes, and
/// statuses are never cut.
const TEXT_KEYS: [&str; 6] = [
    "content",
    "markdown",
    "fitMarkdown",
    "html",
    "text",
    "description",
];
/// Keys under which every string is extracted or script output that may be cut.
const TEXT_TREES: [&str; 2] = ["json", "javascriptReturns"];

/// Cuts the longest page texts first; while that cannot fit, halves the longest lists. Stops when
/// `value` serializes within `max` characters or nothing more can be cut, so callers check the
/// final size. Returns whether anything was cut.
pub(super) fn bound(value: &mut Value, max: usize) -> bool {
    let mut cut = false;
    loop {
        let size = value.to_string().chars().count();
        let excess = size.saturating_sub(max);
        if excess == 0
            || !(trim_strings(value, excess, false)
                || halve_longest_lists(value)
                || trim_strings(value, excess, true))
        {
            return cut;
        }
        cut = true;
    }
}

/// Cuts every page text longer than the highest common length that frees `excess` characters;
/// unless `force`, does nothing when even the shortest allowed length would not. Lengths count
/// JSON escapes, as the budget does.
fn trim_strings(value: &mut Value, excess: usize, force: bool) -> bool {
    let mark = MARK.chars().count();
    let mut lengths = Vec::new();
    each_text(value, false, &mut |text| {
        lengths.push(text.chars().map(escaped_len).sum::<usize>());
    });
    let freed =
        |cap: usize| -> usize { lengths.iter().map(|l| l.saturating_sub(cap + mark)).sum() };
    if !force && freed(MIN_KEEP) < excess {
        return false;
    }
    let (mut cap, mut high) = (MIN_KEEP, lengths.iter().copied().max().unwrap_or(0));
    while cap < high {
        let mid = cap + (high - cap).div_ceil(2);
        if freed(mid) >= excess {
            cap = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut cut = false;
    each_text(value, false, &mut |text| {
        if text.chars().map(escaped_len).sum::<usize>() > cap + mark {
            let mut kept = 0;
            let head = text.chars().take_while(|&c| {
                kept += escaped_len(c);
                kept <= cap
            });
            *text = head.chain(MARK.chars()).collect();
            cut = true;
        }
    });
    cut
}

fn halve_longest_lists(value: &mut Value) -> bool {
    let mut longest = 0;
    each(value, &mut |node| {
        if let Value::Array(items) = node {
            longest = longest.max(items.len());
        }
    });
    if longest < 2 {
        return false;
    }
    each(value, &mut |node| {
        if let Value::Array(items) = node
            && items.len() == longest
        {
            items.truncate(longest / 2);
        }
    });
    true
}

/// Characters `c` takes inside a serialized JSON string.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if c < ' ' => 6,
        _ => 1,
    }
}

/// Visits the page texts in `value`; `all` marks a subtree whose every string is text.
fn each_text(value: &mut Value, all: bool, visit: &mut impl FnMut(&mut String)) {
    match value {
        Value::String(text) if all => visit(text),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| each_text(item, all, visit)),
        Value::Object(map) => {
            for (key, item) in map {
                match item {
                    Value::String(text) if TEXT_KEYS.contains(&key.as_str()) => visit(text),
                    _ => each_text(item, all || TEXT_TREES.contains(&key.as_str()), visit),
                }
            }
        }
        _ => {}
    }
}

/// Visits `value` and then its descendants.
fn each(value: &mut Value, visit: &mut impl FnMut(&mut Value)) {
    visit(value);
    match value {
        Value::Array(items) => items.iter_mut().for_each(|item| each(item, visit)),
        Value::Object(map) => map.values_mut().for_each(|item| each(item, visit)),
        _ => {}
    }
}
