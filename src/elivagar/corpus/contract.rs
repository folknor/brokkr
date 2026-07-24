//! The comparability gate: read an archive's embedded provenance block and
//! decide whether a fresh archive is comparable to the committed baseline.
//!
//! Per the redesign the *schema* stays in elivagar (it authors the block); the
//! *gating policy* - which fields gate comparability and which are diagnostic -
//! is adjudication policy and lives here in brokkr (`elivagar.md`, "What
//! elivagar sheds"). The policy is applied **symmetrically at compare time**:
//! `contract_diff` runs brokkr's current partition over both the stored contract
//! and the fresh block, so a policy change never re-interprets one side only
//! (`brokkr.md`, "The gating policy").

use serde_json::Value;

/// Schema version of the `elivagar` metadata member brokkr understands.
pub const SCHEMA_VERSION: u64 = 1;

/// The archive contract consumed by the gate. Build provenance is kept whole for
/// diagnosis and rotation review, never comparison gating.
#[derive(Debug, Clone)]
pub struct ContractDoc {
    pub input: Value,
    pub config: Value,
    pub build: Value,
}

/// Outcome of reading the provenance block. Everything but `Contract` is an
/// archive-side refusal (exit 2) - the archive cannot be judged on its own terms.
///
/// The non-`Contract` variants and their payloads are surfaced through `Debug`
/// in the refusal message (`{state:?}`); `Unavailable` is constructed by the
/// reader path (metadata unreadable) that lands with the linked crate.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ContractState {
    Contract(ContractDoc),
    Absent,
    Unavailable(String),
    Invalid,
    UnknownSchema(u64),
    Incomplete(&'static str),
}

/// Decode the `elivagar` provenance member out of an archive's metadata JSON.
#[must_use]
pub fn extract_contract(metadata_json: &str) -> ContractState {
    let metadata: Value = match serde_json::from_str(metadata_json) {
        Ok(value) => value,
        Err(_) => return ContractState::Invalid,
    };
    let Some(elivagar) = metadata.get("elivagar") else {
        return ContractState::Absent;
    };
    let Some(object) = elivagar.as_object() else {
        return ContractState::Invalid;
    };
    let Some(schema) = object.get("schema").and_then(Value::as_u64) else {
        return ContractState::Incomplete("schema");
    };
    if schema != SCHEMA_VERSION {
        return ContractState::UnknownSchema(schema);
    }
    let Some(input) = object.get("input") else {
        return ContractState::Incomplete("input");
    };
    let Some(config) = object.get("config") else {
        return ContractState::Incomplete("config");
    };
    let Some(build) = object.get("build") else {
        return ContractState::Incomplete("build");
    };
    ContractState::Contract(ContractDoc {
        input: input.clone(),
        config: config.clone(),
        build: build.clone(),
    })
}

/// The gated paths which differ. `input.name` and the whole `build`/`effective`
/// block are intentionally diagnostic: archives from different revisions are
/// exactly what the gate is meant to compare, so they must never gate.
#[must_use]
pub fn contract_diff(baseline: &ContractDoc, candidate: &ContractDoc) -> Vec<String> {
    let mut out = Vec::new();
    diff_json("config", &baseline.config, &candidate.config, &mut out);
    let mut left = baseline.input.clone();
    let mut right = candidate.input.clone();
    if let Some(value) = left.as_object_mut() {
        value.remove("name");
    }
    if let Some(value) = right.as_object_mut() {
        value.remove("name");
    }
    diff_json("input", &left, &right, &mut out);
    out
}

fn diff_json(path: &str, left: &Value, right: &Value, out: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<_> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                diff_json(
                    &format!("{path}.{key}"),
                    a.get(key).unwrap_or(&Value::Null),
                    b.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        _ if left != right => out.push(path.to_string()),
        _ => {}
    }
}

/// The diagnostic input name, warned on but never gated.
#[must_use]
pub fn input_name(c: &ContractDoc) -> Option<&str> {
    c.input.get("name").and_then(Value::as_str)
}
