//! Naming what changed between two stored resolved configs: the dotted
//! paths whose values differ, so a comparison can say a `bbox` or an id list
//! moved without the change splitting the pair.

use serde_json::Value;

/// Paths shown before the rest are counted.
const SHOWN: usize = 6;

fn json(text: &str) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Two TOML tables walked in their own value model, so values JSON cannot
/// hold (`inf`, `-inf`, datetimes) still compare as themselves.
fn walk_toml(path: &str, a: &toml::Table, b: &toml::Table, out: &mut Vec<String>) {
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let here = if path.is_empty() {
            segment(key)
        } else {
            format!("{path}.{}", segment(key))
        };
        match (a.get(key), b.get(key)) {
            (Some(toml::Value::Table(x)), Some(toml::Value::Table(y))) => {
                walk_toml(&here, x, y, out);
            }
            // NaN is unequal to itself, so equal spellings count as equal.
            (Some(x), Some(y)) if x == y || x.to_string() == y.to_string() => {}
            (Some(x), Some(y)) => out.push(format!("{here} {x} -> {y}")),
            (Some(_), None) => out.push(format!("-{here}")),
            (None, Some(y)) => out.push(format!("+{here} = {y}")),
            (None, None) => {}
        }
    }
}

/// A key as a path segment: bare when TOML would write it bare, quoted
/// otherwise.
fn segment(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        key.to_owned()
    } else {
        format!("{key:?}")
    }
}

fn walk(
    path: &str,
    a: &serde_json::Map<String, Value>,
    b: &serde_json::Map<String, Value>,
    out: &mut Vec<String>,
) {
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let here = if path.is_empty() {
            segment(key)
        } else {
            format!("{path}.{}", segment(key))
        };
        match (a.get(key), b.get(key)) {
            (Some(Value::Object(x)), Some(Value::Object(y))) => walk(&here, x, y, out),
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => out.push(format!("{here} {x} -> {y}")),
            (Some(_), None) => out.push(format!("-{here}")),
            (None, Some(y)) => out.push(format!("+{here} = {y}")),
            (None, None) => {}
        }
    }
}

/// The line naming how two configs differ, or `None` when their digests
/// agree. Paths are listed in order, at most [`SHOWN`], then counted.
pub(super) fn describe(
    digest_a: &str,
    text_a: &str,
    digest_b: &str,
    text_b: &str,
) -> Option<String> {
    if digest_a == digest_b {
        return None;
    }
    let mut changes = Vec::new();
    if let (Ok(a), Ok(b)) = (
        toml::from_str::<toml::Table>(text_a),
        toml::from_str::<toml::Table>(text_b),
    ) {
        walk_toml("", &a, &b, &mut changes);
    } else if let (Some(a), Some(b)) = (json(text_a), json(text_b)) {
        walk("", &a, &b, &mut changes);
    } else {
        return Some(format!(
            "  config differs: {} -> {} (not both readable as TOML or as JSON, named by digest)",
            &digest_a[..digest_a.len().min(8)],
            &digest_b[..digest_b.len().min(8)]
        ));
    }
    if changes.is_empty() {
        return Some("  config differs in text only, not in value".to_owned());
    }
    let more = changes.len().saturating_sub(SHOWN);
    changes.truncate(SHOWN);
    let tail = if more > 0 {
        format!(", +{more} more")
    } else {
        String::new()
    };
    Some(format!("  config differs: {}{tail}", changes.join(", ")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::describe;

    #[test]
    fn changed_added_and_removed_paths_are_named() {
        let a = "[datasets.denmark]\nbbox = \"1,2,3,4\"\nkeep = 1\nold = true\n";
        let b = "[datasets.denmark]\nbbox = \"1,2,3,5\"\nkeep = 1\nnew = [1]\n";
        assert_eq!(
            describe("a", a, "b", b).unwrap(),
            "  config differs: datasets.denmark.bbox \"1,2,3,4\" -> \"1,2,3,5\", +datasets.denmark.new = [1], -datasets.denmark.old"
        );
        assert_eq!(describe("a", a, "a", b), None);
    }

    #[test]
    fn json_text_reads_and_unreadable_text_is_named_by_digest() {
        assert_eq!(
            describe("a", r#"{"x": {"y": 1}}"#, "b", r#"{"x": {"y": 2}}"#).unwrap(),
            "  config differs: x.y 1 -> 2"
        );
        assert!(
            describe("aaaaaaaaaa", "= nope", "bbbbbbbbbb", "{}")
                .unwrap()
                .contains("aaaaaaaa -> bbbbbbbb")
        );
        assert_eq!(
            describe("a", "x = 1\n", "b", "x   =   1\n").unwrap(),
            "  config differs in text only, not in value"
        );
    }

    #[test]
    fn values_json_cannot_hold_still_compare_as_themselves() {
        assert_eq!(
            describe("a", "x = inf\ny = nan\n", "b", "x = -inf\ny = nan\n").unwrap(),
            "  config differs: x inf -> -inf"
        );
    }

    #[test]
    fn a_long_change_list_is_counted() {
        let a: String = (0..9).map(|i| format!("k{i} = 0\n")).collect();
        let b: String = (0..9).map(|i| format!("k{i} = 1\n")).collect();
        assert!(describe("a", &a, "b", &b).unwrap().ends_with(", +3 more"));
    }
}
