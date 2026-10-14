//! Shared persisted-selection shape and compact run-table rendering.
//!
//! Corpus and lint runs keep the selection flags plus resolved ids in JSON.
//! All run tables use the intent label from this module so they do not expose
//! the full id list or disagree when users combine selectors.

use serde_json::{Map, Value, json};

/// Serialize the shared selection fields and optional corpus run context.
pub(crate) fn json(
    all: bool,
    keywords: &[String],
    probes: &[String],
    bless: bool,
    ids: &[String],
    context: Option<(bool, &[String])>,
) -> String {
    let mut fields = Map::new();
    fields.insert("all".to_owned(), json!(all));
    fields.insert("keywords".to_owned(), json!(keywords));
    fields.insert("probe".to_owned(), json!(probes));
    fields.insert("bless".to_owned(), json!(bless));
    fields.insert("ids".to_owned(), json!(ids));
    if let Some((debug, harness_args)) = context {
        fields.insert("debug".to_owned(), json!(debug));
        fields.insert("harness_args".to_owned(), json!(harness_args));
    }
    Value::Object(fields).to_string()
}

/// Compact selector JSON to its intent, leaving malformed or unknown shapes
/// visible verbatim. Resolved ids stay in the database for reproducibility.
pub(crate) fn compact(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_owned();
    };
    let mut parts = Vec::new();
    let all = value.get("all").and_then(Value::as_bool) == Some(true);
    if all {
        parts.push("all".to_owned());
    }
    // `--all` takes the whole universe and selection resolution ignores
    // `--probe`/`--keyword` alongside it. Keep the compact label faithful to
    // the resolved intent while retaining the raw flags in the stored JSON.
    if !all {
        match value.get("probe") {
            Some(Value::Array(probes)) => {
                let names: Vec<&str> = probes.iter().filter_map(Value::as_str).collect();
                if !names.is_empty() {
                    parts.push(format!("probe={}", names.join(",")));
                }
            }
            // Older corpus rows stored one probe as a string.
            Some(Value::String(probe)) => parts.push(format!("probe={probe}")),
            _ => {}
        }
        if let Some(keywords) = value.get("keywords").and_then(Value::as_array) {
            let names: Vec<&str> = keywords.iter().filter_map(Value::as_str).collect();
            if !names.is_empty() {
                parts.push(format!("kw={}", names.join(",")));
            }
        }
    }
    if value.get("bless").and_then(Value::as_bool) == Some(true) {
        parts.push("bless".to_owned());
    }
    // Debug is the parity default; only the exception is worth a column.
    if value.get("debug").and_then(Value::as_bool) == Some(false) {
        parts.push("release".to_owned());
    }
    if let Some(args) = value.get("harness_args").and_then(Value::as_array) {
        let flags: Vec<&str> = args.iter().filter_map(Value::as_str).collect();
        if !flags.is_empty() {
            parts.push(format!("-- {}", flags.join(" ")));
        }
    }
    if parts.is_empty() {
        raw.to_owned()
    } else {
        parts.join(" ")
    }
}
