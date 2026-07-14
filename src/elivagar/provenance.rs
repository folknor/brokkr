//! The `elivagar` provenance block, and the comparability gate built on it.
//!
//! Every archive elivagar writes carries a record of the contract it was built
//! under, as a top-level `elivagar` member of the PMTiles metadata JSON. This
//! module reads it and implements the consumer contract from elivagar's
//! `reference/metadata.md`:
//!
//! 1. Compare `input` and `config`. On mismatch, **refuse** the geometry
//!    comparison and report which field differs. Do not emit a diff.
//! 2. Report `build` differences. Never gate on them.
//! 3. Surface `effective` when explaining a diff that survived step 1.
//!
//! The gate exists because a prose rule was not enough.
//! `reference/technical-implementation-spec.md` already forbade reading a
//! regress diff across dataset variants and named the 2026-07-09 false alarm;
//! on 2026-07-14 the same comparison happened again - a locations-blessed
//! baseline against a raw build, reported as 363,620 structural moves and
//! investigated at length as a code regression. Both builds were correct. A
//! rule that asks a human to remember an invariant at exactly the moment they
//! are focused on something else gets broken on a busy afternoon; the same
//! rule enforced against recorded facts does not.

use std::path::Path;

use serde_json::Value;

use crate::error::DevError;

/// The named fields the comparability contract is defined over.
///
/// Deliberately a fixed list rather than a deep-equal over `input` and
/// `config`. `reference/metadata.md` defines the comparison "over named
/// fields, not the whole object", and says adding a member does not require a
/// schema bump because readers ignore unknown members. A deep-equal would
/// refuse two perfectly comparable archives the moment elivagar grew a field.
///
/// `input.name` is absent on purpose: it is a **label**, and gating on it is
/// what 2026-07-14 needed to not do. Two files named for the same region at
/// the same commit can be entirely different contracts. `input.xxh3_128` is
/// the identity, and it subsumes `bytes`, `replication_timestamp` and
/// `features` - all three are functions of the same bytes, so they cannot
/// differ when the hash agrees.
///
/// `effective`, `build` and `execution` are absent because gating them would
/// invert the gate: given identical `input` and `config`, `effective` is a
/// pure function of the code and `build` *is* the code, and a regression gate
/// exists to compare revisions. Gating them would refuse precisely the
/// comparisons the gate is for.
const CONTRACT_FIELDS: &[&str] = &[
    "input.xxh3_128",
    "config.profile",
    "config.min_zoom",
    "config.max_zoom",
    "config.tile.format",
    "config.tile.compression",
    "config.tile.base_compression_level",
    "config.tile.compression_policy",
    "config.seam_reconcile_layers",
    "config.fanout_caps",
    "config.polygon_simplify_factor",
    "config.ocean.mode",
    "config.ocean.runtime_simplification",
    "config.ocean.low_zoom_source",
    "config.ocean.artifact_key",
];

/// Fields reported as context but never gated on.
const DIAGNOSTIC_FIELDS: &[&str] = &[
    "build.elivagar.commit",
    "build.elivagar.dirty",
    "build.pbfhogg_reader.commit",
    "build.pbfhogg_reader.dirty",
    "build.cargo_lock_xxh3_128",
    "build.cargo_features",
    "effective.coordinate_source",
    "effective.way_members",
    "effective.shared_node_pins",
    "execution.resumed_from",
];

/// The schema this reader knows how to interpret.
///
/// Checked against a known constant rather than merely against the other
/// archive's schema. Two blocks at a schema this brokkr has never seen agree
/// with each other trivially - including on fields the bump renamed away,
/// which both would then lack - and would sail through a same-as-each-other
/// check. A bump means the meaning of an existing field changed, so a reader
/// that has not been taught the new meaning cannot gate on it and must say so.
const SCHEMA: u64 = 1;

/// The `unknown` sentinel. Freshness is load-bearing: every value in the block
/// is either established or emitted as `unknown`, never guessed. A contract
/// field that says `unknown` cannot support a gate.
const UNKNOWN: &str = "unknown";

/// One archive's provenance block.
pub struct Provenance {
    block: Value,
}

/// One field on which two archives' contracts disagree.
pub struct Mismatch {
    pub field: String,
    pub current: String,
    pub blessed: String,
}

impl Provenance {
    /// Read the `elivagar` member of an archive's PMTiles metadata.
    ///
    /// `Ok(None)` means the archive carries no block: it predates the schema
    /// or failed to identify its input. Per the freshness rule, a block is
    /// omitted entirely rather than written partially, so absence is
    /// meaningful and never partial.
    pub fn read(path: &Path) -> Result<Option<Self>, DevError> {
        let meta = crate::pmtiles::read_metadata(path)?;
        Ok(meta
            .get("elivagar")
            .map(|block| Self {
                block: block.clone(),
            }))
    }

    /// The schema version, or `None` if the block does not declare one.
    pub fn schema(&self) -> Option<u64> {
        self.block.get("schema")?.as_u64()
    }

    /// Check the block carries positive evidence of what it was built under.
    ///
    /// The gate's whole purpose is to require that evidence, so every contract
    /// field must be *present and meaningful*. Comparing first and validating
    /// never would let absence pass as agreement: two `{}` blocks match on
    /// every field by both lacking it, which is the strongest possible
    /// statement of "we know nothing" being read as "these are comparable".
    ///
    /// Returns every problem found rather than the first, so one run tells you
    /// everything the block is missing.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        if !self.block.is_object() {
            return Err(vec![format!(
                "the elivagar member is {}, not an object",
                json_kind(&self.block)
            )]);
        }

        let mut problems = Vec::new();

        match self.block.get("schema").and_then(Value::as_u64) {
            None => problems.push("schema: absent or not an integer".to_owned()),
            Some(v) if v != SCHEMA => problems.push(format!(
                "schema: {v}, but this brokkr only knows how to read schema {SCHEMA}"
            )),
            Some(_) => {}
        }

        for field in CONTRACT_FIELDS {
            match self.get(field) {
                None => problems.push(format!("{field}: absent")),
                Some(Value::Null) => problems.push(format!("{field}: null")),
                Some(Value::String(s)) if s == UNKNOWN => {
                    problems.push(format!("{field}: {UNKNOWN}"));
                }
                Some(_) => {}
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// Look up a dotted field path within the block.
    fn get(&self, path: &str) -> Option<&Value> {
        path.split('.').try_fold(&self.block, |v, key| v.get(key))
    }

    /// A field's value rendered for a human, or `absent`.
    fn show(&self, path: &str) -> String {
        match self.get(path) {
            None => "absent".to_owned(),
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
        }
    }

    /// The variant label the block carries. A label, never gated - only ever
    /// used to make a refusal legible.
    pub fn input_name(&self) -> Option<&str> {
        self.get("input.name")?.as_str()
    }

    pub fn input_hash(&self) -> Option<&str> {
        self.get("input.xxh3_128")?.as_str()
    }
}

/// Name a JSON value's type, for a diagnostic.
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Compare two blocks' contracts. Empty means comparable.
///
/// Both sides must have passed [`Provenance::validate`] first - otherwise a
/// field absent from both compares equal and reads as agreement. The gate does
/// that; this function assumes it.
pub fn contract_mismatches(current: &Provenance, blessed: &Provenance) -> Vec<Mismatch> {
    let mut out = Vec::new();

    // A schema bump means the meaning of an existing field changed, so
    // same-named fields no longer say the same thing.
    if current.schema() != blessed.schema() {
        out.push(Mismatch {
            field: "schema".to_owned(),
            current: current.show("schema"),
            blessed: blessed.show("schema"),
        });
        return out;
    }

    for field in CONTRACT_FIELDS {
        if current.get(field) != blessed.get(field) {
            out.push(Mismatch {
                field: (*field).to_owned(),
                current: current.show(field),
                blessed: blessed.show(field),
            });
        }
    }
    out
}

/// Render the diagnostic groups for context. Never gated on.
pub fn diagnostics(current: &Provenance, blessed: &Provenance) -> Vec<String> {
    let mut out = Vec::new();
    for field in DIAGNOSTIC_FIELDS {
        let (c, b) = (current.show(field), blessed.show(field));
        if c != b {
            out.push(format!("{field}: {c} (current) vs {b} (blessed)"));
        }
    }
    out
}

/// Explain a refused comparison.
///
/// Names the differing fields, and leads with the input identity when that is
/// what differs - a contract mismatch on `input.xxh3_128` means the two
/// archives were built from different PBFs, which is the 2026-07-14 shape and
/// the one worth naming outright rather than leaving as two hashes.
pub fn refusal_message(
    current: &Provenance,
    blessed: &Provenance,
    mismatches: &[Mismatch],
) -> String {
    let mut msg = String::from(
        "refusing the comparison: the two archives state different contracts, so a \
         geometry diff between them carries no information about the code.\n",
    );

    if mismatches.iter().any(|m| m.field == "input.xxh3_128") {
        msg.push_str(&format!(
            "\nThey were built from different inputs:\n  \
             current: {} ({})\n  blessed: {} ({})\n",
            current.input_name().unwrap_or("unknown"),
            current.input_hash().unwrap_or("unknown"),
            blessed.input_name().unwrap_or("unknown"),
            blessed.input_hash().unwrap_or("unknown"),
        ));
    }

    msg.push_str("\nContract fields that differ:\n");
    for m in mismatches {
        msg.push_str(&format!(
            "  {}: {} (current) vs {} (blessed)\n",
            m.field, m.current, m.blessed
        ));
    }
    msg
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use serde_json::json;

    fn prov(v: Value) -> Provenance {
        Provenance { block: v }
    }

    fn base() -> Value {
        json!({
            "schema": 1,
            "input": {
                "name": "denmark-locations-prepass.osm.pbf",
                "xxh3_128": "58c47f32d3a55b04a56813565efc78ac",
                "bytes": 531_544_047u64,
                "features": { "locations_on_ways": true }
            },
            "config": {
                "profile": "shortbread",
                "min_zoom": 0,
                "max_zoom": 14,
                "tile": {
                    "format": "mvt",
                    "compression": "gzip",
                    "base_compression_level": 6,
                    "compression_policy": "zoom-v1"
                },
                "seam_reconcile_layers": { "boundaries": 8 },
                "fanout_caps": {},
                "polygon_simplify_factor": 1.0,
                "ocean": {
                    "mode": "artifact",
                    "runtime_simplification": true,
                    "low_zoom_source": "simplified",
                    "artifact_key": { "compression_level": 6 }
                }
            },
            "effective": { "coordinate_source": "inline" },
            "build": {
                "elivagar": { "commit": "ec5bd11", "dirty": true },
                "cargo_lock_xxh3_128": "f850fa82"
            },
            "execution": { "resumed_from": Value::Null }
        })
    }

    #[test]
    fn identical_contracts_are_comparable() {
        assert!(prov(base()).validate().is_ok());
        assert!(contract_mismatches(&prov(base()), &prov(base())).is_empty());
    }

    // -- validate: the gate needs positive evidence, not absent disagreement --

    /// Two empty blocks agree on every field by both lacking it. That is the
    /// strongest possible statement of "we know nothing", and it must not read
    /// as "these are comparable".
    #[test]
    fn empty_blocks_are_refused() {
        let problems = prov(json!({})).validate().unwrap_err();
        assert!(problems.iter().any(|p| p.starts_with("schema:")));
        assert!(problems.iter().any(|p| p == "input.xxh3_128: absent"));
        // Every contract field, plus the schema.
        assert_eq!(problems.len(), CONTRACT_FIELDS.len() + 1);

        // The bug this guards: comparing them finds nothing to disagree about.
        assert!(contract_mismatches(&prov(json!({})), &prov(json!({}))).is_empty());
    }

    #[test]
    fn missing_required_fields_on_both_sides_are_refused() {
        let mut thin = base();
        thin["config"]["ocean"].as_object_mut().unwrap().remove("mode");
        thin["config"].as_object_mut().unwrap().remove("profile");

        let problems = prov(thin.clone()).validate().unwrap_err();
        assert!(problems.contains(&"config.ocean.mode: absent".to_owned()));
        assert!(problems.contains(&"config.profile: absent".to_owned()));

        // Both sides missing the same field: no mismatch, hence the gate must
        // rely on validate() rather than on comparison alone.
        assert!(contract_mismatches(&prov(thin.clone()), &prov(thin)).is_empty());
    }

    #[test]
    fn non_object_blocks_are_refused() {
        for v in [json!(42), json!("nope"), json!([1, 2]), Value::Null] {
            let problems = prov(v).validate().unwrap_err();
            assert_eq!(problems.len(), 1);
            assert!(problems[0].contains("not an object"));
        }
    }

    /// A bump means an existing field changed meaning. Two archives at a
    /// schema this reader has never seen agree with each other trivially -
    /// including on fields the bump renamed away, which both would lack - so
    /// checking equality against the *other side* is not enough.
    #[test]
    fn a_future_schema_is_refused_on_both_sides() {
        let mut future = base();
        future["schema"] = json!(2);

        let problems = prov(future.clone()).validate().unwrap_err();
        assert!(problems.iter().any(|p| p.contains("only knows how to read schema 1")));

        // The hole this closes: they match each other perfectly.
        assert!(contract_mismatches(&prov(future.clone()), &prov(future)).is_empty());
    }

    #[test]
    fn a_null_contract_field_is_refused() {
        let mut b = base();
        b["config"]["polygon_simplify_factor"] = Value::Null;
        let problems = prov(b).validate().unwrap_err();
        assert!(problems.contains(&"config.polygon_simplify_factor: null".to_owned()));
    }

    /// Provenance that lies is worse than provenance that is absent. A field
    /// that says `unknown` is not evidence.
    #[test]
    fn an_unknown_contract_field_is_refused() {
        let mut b = base();
        b["input"]["xxh3_128"] = json!("unknown");
        let problems = prov(b).validate().unwrap_err();
        assert!(problems.contains(&"input.xxh3_128: unknown".to_owned()));
    }

    /// Diagnostic fields may legitimately be absent or `unknown` - they are
    /// never gated, so they must not block validation either.
    #[test]
    fn absent_diagnostics_still_validate() {
        let mut b = base();
        b.as_object_mut().unwrap().remove("build");
        b.as_object_mut().unwrap().remove("effective");
        b.as_object_mut().unwrap().remove("execution");
        assert!(prov(b).validate().is_ok());
    }

    /// The 2026-07-14 shape: a locations-blessed baseline against a raw build.
    #[test]
    fn different_input_hash_is_refused() {
        let mut other = base();
        other["input"]["xxh3_128"] = json!("b16340741bcdf2d5ec94ddc5b2ab8d04");
        other["input"]["name"] = json!("denmark-20260220-seq4704.osm.pbf");

        let m = contract_mismatches(&prov(other.clone()), &prov(base()));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].field, "input.xxh3_128");

        let msg = refusal_message(&prov(other), &prov(base()), &m);
        assert!(msg.contains("different inputs"));
        assert!(msg.contains("denmark-20260220-seq4704.osm.pbf"));
    }

    /// A label is not an identity. Same bytes, different filename, still
    /// comparable - gating on the name is what 2026-07-14 needed to not do.
    #[test]
    fn input_name_alone_is_not_gated() {
        let mut other = base();
        other["input"]["name"] = json!("renamed.osm.pbf");
        assert!(contract_mismatches(&prov(other), &prov(base())).is_empty());
    }

    /// Artifact-served tiles are not geometry-identical to extract-computed
    /// ones, so this is a contract mismatch, not a regression.
    #[test]
    fn ocean_mode_is_gated() {
        let mut other = base();
        other["config"]["ocean"]["mode"] = json!("shapefile");
        let m = contract_mismatches(&prov(other), &prov(base()));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].field, "config.ocean.mode");
    }

    #[test]
    fn artifact_key_is_gated() {
        let mut other = base();
        other["config"]["ocean"]["artifact_key"]["compression_level"] = json!(9);
        let m = contract_mismatches(&prov(other), &prov(base()));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].field, "config.ocean.artifact_key");
    }

    /// A regression gate exists to compare revisions. Gating on the code would
    /// refuse precisely the comparison the gate is for.
    #[test]
    fn build_differences_never_gate() {
        let mut other = base();
        other["build"]["elivagar"]["commit"] = json!("deadbee");
        other["build"]["elivagar"]["dirty"] = json!(false);
        other["build"]["cargo_lock_xxh3_128"] = json!("00000000");
        assert!(contract_mismatches(&prov(other.clone()), &prov(base())).is_empty());

        let d = diagnostics(&prov(other), &prov(base()));
        assert!(d.iter().any(|s| s.contains("build.elivagar.commit")));
    }

    /// Given identical input and config, `effective` is a pure function of the
    /// code. It explains a diff; it must not refuse one.
    #[test]
    fn effective_differences_never_gate() {
        let mut other = base();
        other["effective"]["coordinate_source"] = json!("node_store");
        assert!(contract_mismatches(&prov(other.clone()), &prov(base())).is_empty());

        let d = diagnostics(&prov(other), &prov(base()));
        assert!(d.iter().any(|s| s.contains("effective.coordinate_source")));
    }

    /// Readers ignore unknown members: adding one does not require a schema
    /// bump, so it must not refuse a comparison either.
    #[test]
    fn unknown_members_are_ignored() {
        let mut other = base();
        other["config"]["brand_new_knob"] = json!("whatever");
        other["input"]["data_bounds"] = json!([1, 2, 3, 4]);
        assert!(contract_mismatches(&prov(other), &prov(base())).is_empty());
    }

    /// A bump means an existing field changed meaning, so same-named fields no
    /// longer say the same thing. Report the schema, not fifteen field diffs.
    #[test]
    fn schema_mismatch_short_circuits() {
        let mut other = base();
        other["schema"] = json!(2);
        let m = contract_mismatches(&prov(other), &prov(base()));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].field, "schema");
    }

    #[test]
    fn several_config_mismatches_are_all_named() {
        let mut other = base();
        other["config"]["tile"]["format"] = json!("mlt");
        other["config"]["polygon_simplify_factor"] = json!(2.0);
        let m = contract_mismatches(&prov(other), &prov(base()));
        assert_eq!(m.len(), 2);
        let fields: Vec<&str> = m.iter().map(|x| x.field.as_str()).collect();
        assert!(fields.contains(&"config.tile.format"));
        assert!(fields.contains(&"config.polygon_simplify_factor"));
    }
}
