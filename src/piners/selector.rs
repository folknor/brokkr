//! Shared registry selection, persisted-selection shape and run-table rendering.
//!
//! Corpus runs keep the selection flags plus resolved ids in JSON. The run
//! tables use the intent label from this module so they do not expose the
//! full id list or disagree when users combine selectors. The ordered,
//! deduplicating selection algorithm lives here too; the caller passes the
//! policy for pending pins and verify-only errors.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::{Map, Value, json};

use crate::error::DevError;

pub(crate) struct SelectionArgs<'a> {
    pub keywords: &'a [String],
    pub probes: &'a [String],
    pub all: bool,
    pub whole_universe: bool,
}

pub(crate) struct SelectionPolicy<'a> {
    pub label: &'a str,
    pub registry_file: &'a str,
    /// Corpus verification rejects named selectors alongside `--verify-only`.
    pub whole_universe_named_error: Option<&'a str>,
    /// Set for the registry that supports registered-but-unpinned probes.
    pub pending_hint: Option<&'a str>,
}

/// Resolve keyword and direct selections with one ordered, deduplicating
/// implementation. Callers own the pending-probe notice rendering.
pub(crate) fn resolve_selection<P>(
    pins: &BTreeMap<String, P>,
    keywords: &BTreeMap<String, Vec<String>>,
    pending_ids: &BTreeSet<String>,
    args: &SelectionArgs<'_>,
    policy: &SelectionPolicy<'_>,
    mut notify_pending: impl FnMut(&[String]),
) -> Result<Vec<String>, DevError> {
    if args.whole_universe
        && (!args.keywords.is_empty() || !args.probes.is_empty())
        && let Some(message) = policy.whole_universe_named_error
    {
        return Err(DevError::Config(message.to_owned()));
    }

    if args.whole_universe || args.all {
        let pending: Vec<String> = pending_ids.iter().cloned().collect();
        if !pending.is_empty() {
            notify_pending(&pending);
        }
        if pins.is_empty() {
            return Err(DevError::Config(if pending.is_empty() {
                format!(
                    "{}: {} is empty - nothing to select",
                    policy.label, policy.registry_file
                )
            } else {
                format!(
                    "{}: nothing pinned yet - every probe is [pending]: {}",
                    policy.label,
                    pending.join(", ")
                )
            }));
        }
        return Ok(pins.keys().cloned().collect());
    }

    let available = || {
        let names: Vec<&str> = keywords.keys().map(String::as_str).collect();
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    };
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    let mut pending = Vec::new();

    for keyword in args.keywords {
        let ids = keywords.get(keyword).ok_or_else(|| {
            DevError::Config(format!(
                "{}: unknown keyword '{keyword}' (available: {})",
                policy.label,
                available()
            ))
        })?;
        for id in ids {
            if pending_ids.contains(id) {
                if !pending.contains(id) {
                    pending.push(id.clone());
                }
            } else if seen.insert(id.clone()) {
                selected.push(id.clone());
            }
        }
    }

    for probe in args.probes {
        if pending_ids.contains(probe)
            && let Some(suggestion) = policy.pending_hint
        {
            return Err(DevError::Config(format!(
                "{}: probe '{probe}' is [pending], not pinned - pin it with `{suggestion}{probe}`",
                policy.label
            )));
        }
        if !pins.contains_key(probe) {
            return Err(DevError::Config(format!(
                "{}: unknown probe '{probe}' - not pinned in {}",
                policy.label, policy.registry_file
            )));
        }
        if seen.insert(probe.clone()) {
            selected.push(probe.clone());
        }
    }

    if !pending.is_empty() {
        notify_pending(&pending);
    }
    if selected.is_empty() && !pending.is_empty() {
        return Err(DevError::Config(format!(
            "{}: nothing runnable - every selected probe is [pending]: {}",
            policy.label,
            pending.join(", ")
        )));
    }
    if selected.is_empty() {
        return Err(DevError::Config(format!(
            "{}: no probes selected. Pass --keyword <k> (repeatable), \
             --probe <id>, or --all. Available keywords: {}",
            policy.label,
            available()
        )));
    }
    Ok(selected)
}

/// Serialize the shared selection fields and optional corpus run context.
pub(crate) fn json(
    all: bool,
    keywords: &[String],
    probes: &[String],
    bless: bool,
    ids: &[String],
    context: Option<(&crate::build::RustProfile, &[String])>,
) -> String {
    let mut fields = Map::new();
    fields.insert("all".to_owned(), json!(all));
    fields.insert("keywords".to_owned(), json!(keywords));
    fields.insert("probe".to_owned(), json!(probes));
    fields.insert("bless".to_owned(), json!(bless));
    fields.insert("ids".to_owned(), json!(ids));
    if let Some((profile, harness_args)) = context {
        // The profile by name; rows written before custom profiles carry a
        // `debug` bool instead, which `compact` still reads.
        fields.insert("profile".to_owned(), json!(profile.label()));
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
    // Dev is the parity default; only the exception is worth a column.
    match value.get("profile").and_then(Value::as_str) {
        Some("dev") => {}
        Some(profile) => parts.push(profile.to_owned()),
        None => {
            if value.get("debug").and_then(Value::as_bool) == Some(false) {
                parts.push("release".to_owned());
            }
        }
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
