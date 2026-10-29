//! What `--compare` says about a library row that did not pair
//! (`brokkr man library`, Measuring a scenario): the other side's latest row
//! of the same command, mode, input, captured environment and build is
//! examined, and the first place its library identity differs is named,
//! down to the plan or context field when the documents are stored. This
//! is a diagnosis beside the pairing, never part of it, and never a claim
//! that the named difference is the only one.

use std::collections::BTreeMap;

use super::super::StoredRow;
use super::super::types::KvClass;

/// The `meta.library.identity` key, without its namespace.
pub(crate) const IDENTITY_KEY: &str = "library.identity";

/// A row's library identity text, when it is a library row.
pub(crate) fn identity(row: &StoredRow) -> Option<String> {
    row.kv
        .iter()
        .find(|p| p.class() == KvClass::Metadata && p.key.name() == IDENTITY_KEY)
        .map(|p| p.value.to_string())
}

/// Every plan and context digest a library row's identity names.
pub(crate) fn digests(row: &StoredRow) -> Vec<String> {
    let Some(text) = identity(row) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut out: Vec<String> = value
        .get("plan")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .into_iter()
        .collect();
    if let Some(contexts) = value.get("contexts").and_then(serde_json::Value::as_object) {
        out.extend(
            contexts
                .values()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned),
        );
    }
    out
}

/// The note for `row`, one-sided on `side`, against `candidates` (the
/// other side's rows of the same command, mode, input, environment and
/// build, latest first). `None` for a row that is not a library row or has
/// no candidate.
pub(crate) fn unpaired_note(
    row: &StoredRow,
    side: &str,
    other: &str,
    candidates: &[&StoredRow],
    documents: &BTreeMap<String, String>,
) -> Option<String> {
    let mine = identity(row)?;
    let candidate = candidates.first()?;
    let short = super::super::types::short_uuid(&candidate.uuid);
    let Some(theirs) = identity(candidate) else {
        return Some(format!(
            "  unpaired on {side}: {other}'s latest {} row {short} records no library identity",
            row.command
        ));
    };
    if mine == theirs {
        return Some(format!(
            "  unpaired on {side}: {other}'s latest {} row {short} has the same library \
             identity; the rows differ in their invocation",
            row.command
        ));
    }
    let (a, b) = if side == "A" {
        (&mine, &theirs)
    } else {
        (&theirs, &mine)
    };
    Some(format!(
        "  unpaired on {side}: {other}'s latest {} row {short} has another library identity; \
         first difference: {}",
        row.command,
        first_difference(a, b, documents)
    ))
}

/// The first difference between two identity texts (A's, then B's), the
/// plan or a context resolved to its first differing field when both
/// documents are stored.
fn first_difference(a: &str, b: &str, documents: &BTreeMap<String, String>) -> String {
    let parse = |t: &str| serde_json::from_str::<serde_json::Value>(t).ok();
    let (Some(x), Some(y)) = (parse(a), parse(b)) else {
        return "(an identity is not JSON)".to_owned();
    };
    let digest = |v: &serde_json::Value, path: &[&str]| -> Option<String> {
        path.iter()
            .try_fold(v, |v, k| v.get(*k))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let field = |da: Option<String>, db: Option<String>| -> Option<String> {
        let (da, db) = (da?, db?);
        let (ta, tb) = (documents.get(&da)?, documents.get(&db)?);
        crate::pbfhogg::library::canonical_difference(ta, tb)
    };
    let (pa, pb) = (digest(&x, &["plan"]), digest(&y, &["plan"]));
    if pa != pb {
        return match field(pa.clone(), pb.clone()) {
            Some(at) => format!("plan field {at}"),
            None => format!(
                "the plan ({} vs {})",
                pa.unwrap_or_default(),
                pb.unwrap_or_default()
            ),
        };
    }
    let calls = |v: &serde_json::Value| -> Vec<String> {
        v.get("contexts")
            .and_then(serde_json::Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    let mut all = calls(&x);
    all.extend(calls(&y));
    all.sort();
    all.dedup();
    for call in all {
        let (ca, cb) = (
            digest(&x, &["contexts", &call]),
            digest(&y, &["contexts", &call]),
        );
        if ca != cb {
            return match field(ca.clone(), cb.clone()) {
                Some(at) => format!("context of call {call}, field {at}"),
                None => format!(
                    "context of call {call} ({} vs {})",
                    ca.unwrap_or_else(|| "none".to_owned()),
                    cb.unwrap_or_else(|| "none".to_owned())
                ),
            };
        }
    }
    let ca = serde_json::to_string(&x).unwrap_or_default();
    let cb = serde_json::to_string(&y).unwrap_or_default();
    crate::pbfhogg::library::canonical_difference(&ca, &cb)
        .unwrap_or_else(|| "(none found)".to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_plan_difference_names_its_field_when_the_documents_are_stored() {
        let a = r#"{"contexts":{"a":"context-v1:c"},"datasets":{},"features":"","generated":{},"plan":"plan-v1:a","v":1}"#;
        let b = a.replace("plan-v1:a", "plan-v1:b");
        let docs = BTreeMap::from([
            (
                "plan-v1:a".to_owned(),
                r#"{"calls":{"a":{"n":1}}}"#.to_owned(),
            ),
            (
                "plan-v1:b".to_owned(),
                r#"{"calls":{"a":{"n":2}}}"#.to_owned(),
            ),
        ]);
        assert_eq!(
            first_difference(a, &b, &docs),
            "plan field calls.a.n (1 vs 2)"
        );
        assert_eq!(
            first_difference(a, &b, &BTreeMap::new()),
            "the plan (plan-v1:a vs plan-v1:b)"
        );
        let c = a.replace("\"datasets\":{}", "\"datasets\":{\"input\":\"xxh128:1\"}");
        assert_eq!(
            first_difference(a, &c, &docs),
            "datasets.input (absent vs \"xxh128:1\")"
        );
        let d = a.replace("context-v1:c", "context-v1:d");
        assert_eq!(
            first_difference(a, &d, &docs),
            "context of call a (context-v1:c vs context-v1:d)"
        );
    }
}
