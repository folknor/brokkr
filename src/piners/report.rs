//! Parse the harness's NDJSON output and aggregate it brokkr-side.
//!
//! The harness emits one JSON object per probe and **no trailing summary
//! line** - brokkr tallies the summary, the divergence breakdown, and the
//! dense-na breakdown from the per-probe stream itself. brokkr is
//! deliberately tolerant of unknown fields: the harness owns the schema and
//! grows it (extra tiers, deltas, provenance) ahead of brokkr learning to
//! render them, the same forward-compat posture as the cargo JSON parser. A
//! stray legacy `summary` line is skipped rather than mis-parsed as a probe.
//! Tolerance covers unknown *fields*, not unusable *records*: a stdout line
//! that is not JSON, or a disposition line that does not deserialize, is kept
//! as an [`InvalidRecord`] for the integrity check to fail the run on.
//!
//! Lines are discriminated by an optional `kind` field. A line with no `kind`
//! (or `kind == "disposition"`) is a per-probe disposition line: the only kind
//! that feeds aggregation and the gate. A `kind == "trade_diff"` line is a
//! per-trade drill-down record; brokkr collects these into the report so the
//! corpus DB can persist them, but they never touch the summary, breakdowns,
//! or exit code. Any other `kind` is tolerated and skipped, so a new harness
//! record kind needs no brokkr change beyond (optionally) modelling it for
//! persistence.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use crate::output;
use crate::piners::registry::{PinnedDisposition, UNEVALUATED};

/// Acceptance detail, present only when `outcome == "parity"`. Models what
/// brokkr reads for the gate and the console; the p90 magnitudes and anything
/// else ride in [`ProbeLine::raw`] and reach the run store from there.
#[derive(Debug, Clone, Deserialize)]
pub struct Acceptance {
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub failing: Vec<String>,
}

/// Divergence signature: a symptom classification, never a cause. Present
/// whenever a dimension breached - which includes a `byte_exact` probe whose
/// only disagreement is a displayed label. brokkr groups the
/// console breakdown by `domain`/`dimension`; `leg`, `detail` and
/// `dimension_breaches` reach the run store through [`ProbeLine::raw`].
/// Forward-compat: serde drops any field not modelled here.
#[derive(Debug, Clone, Deserialize)]
pub struct Signature {
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub dimension: String,
}

/// One dense-`na` call site, present only when non-empty. brokkr aggregates by
/// `name` and sums `na_count` for the console breakdown, and persists
/// `call_site` to the corpus DB so individual sites stay queryable.
#[derive(Debug, Clone, Deserialize)]
pub struct DenseNaSite {
    #[serde(default)]
    pub name: String,
    /// Source location of the call. The harness has historically serialized
    /// this both as a string (`"s.pine:12"`) and, for some builtins, as a bare
    /// integer line number (`12`). brokkr accepts either and normalizes to a
    /// string so a numeric value never sinks the line - see [`de_call_site`].
    #[serde(default, deserialize_with = "de_call_site")]
    pub call_site: String,
    #[serde(default)]
    pub na_count: u64,
}

/// Deserialize a `call_site` from either a JSON string or a JSON integer,
/// normalizing the integer form to its decimal string. The harness emits both
/// shapes; modelling the field as a bare `String` made an integer value fail
/// `from_value`, which (because the whole `ProbeLine` then failed) discarded
/// the probe's gate-critical disposition over a pure diagnostic field. null is
/// tolerated as an empty string for the same reason.
fn de_call_site<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use std::fmt;

    use serde::de::{Unexpected, Visitor};

    struct CallSite;

    impl<'de> Visitor<'de> for CallSite {
        type Value = String;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a string or integer call site")
        }

        fn visit_str<E>(self, v: &str) -> Result<String, E> {
            Ok(v.to_owned())
        }

        fn visit_string<E>(self, v: String) -> Result<String, E> {
            Ok(v)
        }

        fn visit_i64<E>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_u64<E>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_unit<E>(self) -> Result<String, E> {
            Ok(String::new())
        }

        fn visit_none<E>(self) -> Result<String, E> {
            Ok(String::new())
        }

        fn visit_some<D>(self, deserializer: D) -> Result<String, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(self)
        }

        fn visit_f64<E>(self, v: f64) -> Result<String, E>
        where
            E: serde::de::Error,
        {
            // A fractional call site is nonsensical; refuse it rather than
            // silently stringifying a float.
            Err(E::invalid_type(Unexpected::Float(v), &self))
        }
    }

    deserializer.deserialize_any(CallSite)
}

/// Deserialize `dense_na_sites` tolerantly: parse each element on its own and
/// drop a single site that fails rather than sinking the whole disposition.
/// Dense-na sites are pure diagnostics (they feed the console breakdown and the
/// corpus DB, never the gate), so one malformed site must not cost us the
/// probe's gate-critical disposition. The original disposition line remains in
/// `ProbeLine::raw` and is persisted to the run store, so dropping an
/// unparseable typed site does not discard its source data. Paired with
/// [`de_call_site`], which keeps the common numeric-`call_site` site parseable
/// in the first place; this is the backstop for any *other* future field-type
/// drift inside a site.
fn de_dense_na_sites<'de, D>(deserializer: D) -> Result<Vec<DenseNaSite>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| serde_json::from_value::<DenseNaSite>(v).ok())
        .collect())
}

/// Why an `unevaluated` probe's experiment could not be conducted (piners
/// `reference/corpus.md`, Harness output). Any other `reason`, or none, makes
/// the line an invalid record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    InvalidEvidence,
    InsufficientEvidence,
    WitnessContract,
    FeedCoverage,
    NoOverlap,
}

impl RefusalReason {
    pub const ALL: [Self; 5] = [
        Self::InvalidEvidence,
        Self::InsufficientEvidence,
        Self::WitnessContract,
        Self::FeedCoverage,
        Self::NoOverlap,
    ];

    /// The wire spelling.
    pub const fn label(self) -> &'static str {
        match self {
            Self::InvalidEvidence => "invalid_evidence",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::WitnessContract => "witness_contract",
            Self::FeedCoverage => "feed_coverage",
            Self::NoOverlap => "no_overlap",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.label() == label)
    }
}

/// An experiment the harness could not conduct: its reason and the
/// harness's one-line detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: RefusalReason,
    pub error: String,
}

/// A validated disposition record: an evaluated probe with the disposition a
/// pin can hold, or a refused one, which no pin, gate comparison or bless
/// stamp can hold by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Evaluated(PinnedDisposition),
    Unevaluated(Refusal),
}

impl ProbeOutcome {
    /// The gate label as stored and displayed.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Evaluated(d) => d.as_str(),
            Self::Unevaluated(_) => UNEVALUATED,
        }
    }

    /// Whether the harness signals this outcome with exit code 1.
    pub fn is_break(&self) -> bool {
        match self {
            Self::Evaluated(d) => d.is_break(),
            Self::Unevaluated(_) => true,
        }
    }

    /// The disposition of an evaluated probe.
    pub fn evaluated(&self) -> Option<PinnedDisposition> {
        match self {
            Self::Evaluated(d) => Some(*d),
            Self::Unevaluated(_) => None,
        }
    }

    /// The refusal of an unevaluated probe.
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Evaluated(_) => None,
            Self::Unevaluated(r) => Some(r),
        }
    }
}

/// What a line's outcome and tier name, before an `unevaluated` line's own
/// fields are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineLabel {
    Evaluated(PinnedDisposition),
    Unevaluated,
}

impl LineLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Evaluated(d) => d.as_str(),
            Self::Unevaluated => UNEVALUATED,
        }
    }
}

/// The one outcome/tier validator: the label an `outcome` plus its
/// acceptance `tier` (empty counts as absent) stand for, or `None` when the
/// pair is not a valid disposition. `parity` needs a parity tier; any other
/// outcome must be a live non-parity label and carry no tier. So
/// `{"outcome":"accepted"}` (a tier posing as an outcome), `parity` with tier
/// `runtime_fail` and a retired `no_tv_data` are all invalid.
pub fn valid_label(outcome: &str, tier: Option<&str>) -> Option<LineLabel> {
    let tier = tier.filter(|value| !value.is_empty());
    if outcome == "parity" {
        let d = PinnedDisposition::from_label(tier?)?;
        d.is_parity().then_some(LineLabel::Evaluated(d))
    } else if tier.is_some() {
        None
    } else if outcome == UNEVALUATED {
        Some(LineLabel::Unevaluated)
    } else {
        PinnedDisposition::from_label(outcome)
            .filter(|d| !d.is_parity())
            .map(LineLabel::Evaluated)
    }
}

/// The record-level label validator: the label a disposition record stands
/// for, or why it stands for none. The outcome and tier must form a label
/// ([`valid_label`], the primitive), and the harness's own `disposition`
/// field, when the key is present, must be exactly that label as a string. A
/// present null, number, object or empty string is a defect; only a missing
/// key is tolerated (a harness predating the field). An invalid outcome/tier
/// stays invalid even when the field agrees with the derived fallback.
pub fn record_label(
    outcome: &str,
    tier: Option<&str>,
    declared: Option<&serde_json::Value>,
) -> Result<LineLabel, String> {
    let Some(label) = valid_label(outcome, tier) else {
        return Err(format!(
            "outcome '{outcome}' with tier '{}' is not a valid disposition",
            tier.unwrap_or("")
        ));
    };
    match declared {
        None => Ok(label),
        Some(serde_json::Value::String(s)) if s == label.as_str() => Ok(label),
        Some(other) => Err(format!(
            "its disposition field {other} contradicts the derived label \"{}\"",
            label.as_str()
        )),
    }
}

/// The parity fields piners defines (`ParityDisposition` in its corpus
/// harness). An `unevaluated` line carries none: the key alone, null
/// included, makes it invalid. Keys outside this list stay tolerated.
const PARITY_KEYS: [&str; 25] = [
    "matched",
    "ours_only",
    "tv_only",
    "boundary_ours",
    "boundary_tv",
    "clipped_ours",
    "clipped_tv",
    "history_prefix_bars",
    "oracle_trimmed",
    "oracle_realtime",
    "oracle_fills",
    "oracle_qty_witness",
    "qty_witnessed",
    "qty_display",
    "tv_identity",
    "boundary_anchor",
    "boundary_rules",
    "count_tier",
    "acceptance",
    "signature",
    "ts_shift",
    "labels",
    "dense_na_sites",
    "signal_extra_entries",
    "window_sensitive",
];

/// An `unevaluated` line's own contract, given its whole value: a known
/// `reason`, a nonblank single-line `error`, and no parity field.
fn refusal_of(value: &serde_json::Value) -> Result<Refusal, String> {
    let present: Vec<&str> = PARITY_KEYS
        .into_iter()
        .filter(|key| value.get(key).is_some())
        .collect();
    if !present.is_empty() {
        return Err(format!(
            "an unevaluated line carries parity fields ({})",
            present.join(", ")
        ));
    }
    let reason = match value.get("reason") {
        Some(serde_json::Value::String(s)) => RefusalReason::from_label(s),
        _ => None,
    }
    .ok_or_else(|| match value.get("reason") {
        None => "an unevaluated line carries no reason".to_owned(),
        Some(other) => format!(
            "an unevaluated line's reason {other} is not one of {}",
            RefusalReason::ALL.map(RefusalReason::label).join(", ")
        ),
    })?;
    match value.get("error") {
        Some(serde_json::Value::String(e)) if !e.trim().is_empty() && !e.contains(['\n', '\r']) => {
            Ok(Refusal {
                reason,
                error: e.clone(),
            })
        }
        None => Err("an unevaluated line carries no error detail".to_owned()),
        Some(_) => Err("an unevaluated line's error is not a nonempty single line".to_owned()),
    }
}

/// One leg of the displayed-label census a parity line carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegCensus {
    pub compared: u64,
    pub mismatched: u64,
    pub ambiguous: u64,
    pub unavailable: u64,
}

/// The displayed-label census over a probe's matched pairs, per leg. The
/// gate ratchets `mismatched` against the pin (`gate.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelCensus {
    pub entry: LegCensus,
    pub exit: LegCensus,
}

/// Read a `labels` value strictly. The census is gate evidence, so nothing
/// in it defaults: both legs, each with all four counts as non-negative
/// integers, and counts that can describe one population - `mismatched` and
/// `ambiguous` are disjoint subsets of `compared`. Unknown keys are
/// tolerated. (`compared + unavailable` is not held to `matched`: the
/// harness leaves discounted start-transient pairs out of the census.)
pub fn parse_label_census(value: &serde_json::Value) -> Result<LabelCensus, String> {
    let leg = |name: &str| -> Result<LegCensus, String> {
        let obj = value
            .get(name)
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| format!("labels.{name} is not an object"))?;
        let count = |key: &str| {
            obj.get(key)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| format!("labels.{name}.{key} is not a non-negative integer"))
        };
        let c = LegCensus {
            compared: count("compared")?,
            mismatched: count("mismatched")?,
            ambiguous: count("ambiguous")?,
            unavailable: count("unavailable")?,
        };
        if c.mismatched > c.compared || c.ambiguous > c.compared - c.mismatched {
            return Err(format!(
                "labels.{name} counts {} mismatched and {} ambiguous of {} compared",
                c.mismatched, c.ambiguous, c.compared
            ));
        }
        Ok(c)
    };
    if !value.is_object() {
        return Err("labels is not an object".to_owned());
    }
    Ok(LabelCensus {
        entry: leg("entry")?,
        exit: leg("exit")?,
    })
}

/// A per-probe disposition line.
#[derive(Debug, Clone, Deserialize)]
pub struct ProbeLine {
    pub probe: String,
    pub outcome: String,
    #[serde(default)]
    pub matched: u64,
    #[serde(default)]
    pub ours_only: u64,
    #[serde(default)]
    pub tv_only: u64,
    /// Count of `ours_only` trades piners classified as window-boundary
    /// artifacts (a data-start phase offset at the shared-window seam, not a
    /// parity bug). Absent on harness output predating the discriminator.
    /// `ours_only`/`tv_only` stay *raw*: the factual unmatched-pairing counts,
    /// reconcilable as `our_trade_count = matched + ours_only` (and disjoint
    /// from the matched-but-divergent `trade_diff` rows). These `boundary_*`
    /// fields explain the gap, and the *effective* divergence piners scored the
    /// label/signature on is the difference (`ours_only - boundary_ours`).
    #[serde(default)]
    pub boundary_ours: u64,
    /// Count of `tv_only` trades piners classified as window-boundary artifacts.
    /// See [`Self::boundary_ours`].
    #[serde(default)]
    pub boundary_tv: u64,
    #[serde(default)]
    pub count_tier: Option<String>,
    #[serde(default)]
    pub acceptance: Option<Acceptance>,
    #[serde(default)]
    pub signature: Option<Signature>,
    #[serde(default, deserialize_with = "de_dense_na_sites")]
    pub dense_na_sites: Vec<DenseNaSite>,
    /// Error string carried by a `*_fail`, `harness_abort` or `unevaluated`
    /// outcome.
    #[serde(default)]
    pub error: Option<String>,
    /// The whole line as parsed, re-serialized compactly (the harness's key
    /// order is kept; its original bytes are not), every field kept,
    /// including the ones this struct does not model. The run store
    /// persists it as the authoritative record and projects its columns from
    /// it, so a diagnostic the harness adds is stored the day it ships rather
    /// than dropped until brokkr learns its name. Set by [`parse`]; it rides
    /// on the line so [`HarnessReport::take_duplicates`] keeps raw and typed
    /// from the same occurrence.
    #[serde(skip)]
    pub raw: String,
    /// Position among the stream's non-blank lines. Set by [`parse`].
    #[serde(skip)]
    pub line: usize,
    /// The harness's own `disposition` field exactly as sent: `None` when
    /// the key is missing (the old-harness tolerance), `Some(Value::Null)`
    /// for a present null. Set by [`parse`]; checked by [`record_label`].
    #[serde(skip)]
    pub declared: Option<serde_json::Value>,
    /// The `labels` census, parsed once by [`parse`]: `None` when the key is
    /// missing (a harness predating it, or a non-parity line) or present but
    /// not a valid census - an explicit null included - which also makes the
    /// whole record invalid, so a valid line's census is whatever it sent.
    #[serde(skip)]
    pub census: Option<LabelCensus>,
    /// The record's validated outcome, or why it has none: computed once, by
    /// [`parse`], from the whole line, and read by every consumer - the
    /// reconciliation, the gate and its label ratchet, bless, the summary and
    /// the render filter. No consumer revalidates the raw fields.
    #[serde(skip, default = "unvalidated")]
    pub verdict: Result<ProbeOutcome, String>,
}

/// The verdict of a line [`parse`] has not validated, which no line it
/// returns carries.
fn unvalidated() -> Result<ProbeOutcome, String> {
    Err("not validated".to_owned())
}

impl ProbeLine {
    /// The probe's disposition label, the unit the gate compares against
    /// `expected`. For a `parity` outcome it is the acceptance tier; for any
    /// other outcome it is the outcome itself. A `parity` line missing its
    /// acceptance (malformed) falls back to `"parity"`, which matches no
    /// pinned label and so trips the gate rather than passing silently.
    pub fn disposition(&self) -> String {
        if self.outcome == "parity" {
            self.acceptance
                .as_ref()
                .map(|a| a.tier.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "parity".to_owned())
        } else {
            self.outcome.clone()
        }
    }

    /// The validated outcome, `None` for an invalid record.
    pub fn outcome(&self) -> Option<&ProbeOutcome> {
        self.verdict.as_ref().ok()
    }

    /// Why the record is invalid, `None` when it is valid.
    pub fn record_defect(&self) -> Option<&str> {
        self.verdict.as_ref().err().map(String::as_str)
    }

    /// The refusal of a valid `unevaluated` line.
    pub fn refusal(&self) -> Option<&Refusal> {
        self.outcome().and_then(ProbeOutcome::refusal)
    }

    /// Validate this line, given the whole value it was read from and its
    /// parsed `labels` census: the outcome, tier and declared label together
    /// ([`record_label`]); a present census whole and consistent; and on an
    /// `unevaluated` line, its reason, error and absence of parity fields
    /// ([`refusal_of`]).
    fn validate(
        &self,
        value: &serde_json::Value,
        census: Result<Option<LabelCensus>, String>,
    ) -> Result<ProbeOutcome, String> {
        let label = record_label(
            &self.outcome,
            self.acceptance.as_ref().map(|a| a.tier.as_str()),
            self.declared.as_ref(),
        )?;
        // A present census is gate evidence: a malformed one makes the whole
        // record invalid, so it taints the probe like any other defect and a
        // clean repeat cannot launder it.
        census?;
        match label {
            LineLabel::Evaluated(d) => Ok(ProbeOutcome::Evaluated(d)),
            LineLabel::Unevaluated => refusal_of(value).map(ProbeOutcome::Unevaluated),
        }
    }

    /// The effective `ours_only` count after discounting window-boundary
    /// artifacts - the real (interior) divergence piners scored the label and
    /// signature on. `saturating_sub` is defensive: by contract
    /// `boundary_ours <= ours_only`, but a malformed line must not underflow.
    pub fn effective_ours(&self) -> u64 {
        self.ours_only.saturating_sub(self.boundary_ours)
    }

    /// The effective `tv_only` count after discounting window-boundary
    /// artifacts. See [`Self::effective_ours`].
    pub fn effective_tv(&self) -> u64 {
        self.tv_only.saturating_sub(self.boundary_tv)
    }

    /// Did piners discount any trades on this probe as window-boundary
    /// artifacts? Drives the "no silent drop" summary line.
    pub fn has_boundary_discount(&self) -> bool {
        self.boundary_ours > 0 || self.boundary_tv > 0
    }
}

/// A per-trade `trade_diff` drill-down line (`kind == "trade_diff"`), one per
/// matched pair the harness counts a breach on. brokkr reads only the row key,
/// which it validates; the line itself is kept whole as `raw` and stored as
/// the authoritative record, every field a projection over it (see
/// `corpus_db/schema.rs`), so a field the harness adds is stored the day it
/// ships. Never enters aggregation or the gate.
#[derive(Debug, Clone, Deserialize)]
pub struct TradeDiffLine {
    pub probe: String,
    pub our_index: i64,
    pub tv_index: i64,
    /// The line, re-serialized compactly.
    #[serde(skip)]
    pub raw: String,
}

/// A stdout line that should have been a disposition record and is not one
/// brokkr can use: not JSON at all, or a disposition-kind line that does not
/// deserialize. A protocol violation - the integrity check
/// (`crate::piners::integrity`) fails the run on any - and never a scored
/// probe. `probe` is the id the line named, when it named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRecord {
    pub probe: Option<String>,
    pub reason: String,
    /// The record's position among the stream's non-blank lines.
    pub line: usize,
    /// The line is not JSON at all (a fragment, possibly cut off mid-write),
    /// as opposed to complete JSON with invalid fields.
    pub not_json: bool,
}

/// A diagnostic record brokkr had to drop while parsing the harness stream.
/// Records that are protocol violations are also listed in `invalid`; other
/// dropped diagnostic subrecords do not invalidate their probe. The reason and
/// a short source excerpt are kept with the run for later inspection.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ParseWarning {
    pub line: usize,
    pub kind: String,
    pub probe: Option<String>,
    pub reason: String,
    pub raw: String,
}

/// The harness's versioned contract lines (`docs/projects/piners.md`).
/// Recognised by `kind` alone; the version and fields are validated by
/// [`crate::piners::contract`], never here, so a known kind with a bad
/// version is still seen and judged rather than skipped.
pub const CONTRACT_KINDS: [&str; 7] = [
    "run_start",
    "setup_stage",
    "setup_complete",
    "probe_start",
    "probe_end",
    "run_end",
    "run_error",
];

/// One contract line, kept whole with its stream position.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractLine {
    /// Position among the non-blank lines (ordering).
    pub line: usize,
    /// Physical line number, blank lines counted (`run_start` must be 1).
    pub physical: usize,
    pub kind: String,
    pub value: serde_json::Value,
}

/// Everything parsed out of one harness run.
#[derive(Debug, Default)]
pub struct HarnessReport {
    pub probes: Vec<ProbeLine>,
    /// Every valid `unevaluated` occurrence in emission order, with its
    /// probe id, gathered before repeats collapse: evidence, whether or not
    /// the probe is selected, repeated or tainted by another occurrence.
    pub refusals: Vec<(String, Refusal)>,
    /// Per-trade drill-down records (`kind == "trade_diff"`), collected for
    /// persistence. Empty for an exact run. Never feeds the summary or gate.
    pub trade_diffs: Vec<TradeDiffLine>,
    /// Lines that should have been disposition records and could not be
    /// read as one, in emission order. See [`InvalidRecord`].
    pub invalid: Vec<InvalidRecord>,
    /// Structured diagnostics for records dropped during parsing. Protocol
    /// violations also appear in `invalid`; malformed diagnostic subrecords
    /// such as `trade_diff` rows can be non-fatal.
    pub warnings: Vec<ParseWarning>,
    /// Contract lines in emission order.
    pub contract: Vec<ContractLine>,
    /// Position of the last non-blank line, `None` for an empty stream.
    /// Positions are what lifecycle and terminal-order validation read;
    /// dispositions arrive in completion order, so presentation sorts by id.
    pub last_line: Option<usize>,
    /// The stream did not end with a newline: its final line may be a
    /// fragment cut off mid-write.
    pub unterminated: bool,
}

impl HarnessReport {
    /// Probe ids with at least one invalid occurrence in the stream (a
    /// defective or unparsable record naming them), before or after repeats
    /// collapsed. Such a probe is never scored and never satisfies a pin,
    /// whatever its retained line says.
    pub fn tainted(&self) -> HashSet<&str> {
        self.invalid
            .iter()
            .filter_map(|r| r.probe.as_deref())
            .collect()
    }

    /// Collapse repeated records to the last one emitted, returning a
    /// description of each repeat (sorted, one per affected key).
    ///
    /// The harness contract is one disposition line per probe and one
    /// `trade_diff` line per `(probe, our_index, tv_index)`. A repeat breaks
    /// the run store's primary keys - ingest would fail with an anonymous
    /// `UNIQUE constraint failed` - while the gate and bless, which index
    /// dispositions by probe, would silently keep the last. Collapsing here,
    /// before any consumer sees the report, gives all three the same record;
    /// the caller fails the run on a non-empty return, naming the repeats.
    pub fn take_duplicates(&mut self) -> Vec<String> {
        let mut repeats: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        let mut seen: HashSet<String> = HashSet::new();
        let mut kept: Vec<ProbeLine> = Vec::with_capacity(self.probes.len());
        for p in std::mem::take(&mut self.probes).into_iter().rev() {
            if seen.insert(p.probe.clone()) {
                kept.push(p);
            } else {
                repeats.insert(format!("{} (disposition)", p.probe));
            }
        }
        kept.reverse();
        self.probes = kept;

        let mut seen: HashSet<(String, i64, i64)> = HashSet::new();
        let mut kept: Vec<TradeDiffLine> = Vec::with_capacity(self.trade_diffs.len());
        for t in std::mem::take(&mut self.trade_diffs).into_iter().rev() {
            if seen.insert((t.probe.clone(), t.our_index, t.tv_index)) {
                kept.push(t);
            } else {
                repeats.insert(format!(
                    "{} (trade_diff our_index={} tv_index={})",
                    t.probe, t.our_index, t.tv_index
                ));
            }
        }
        kept.reverse();
        self.trade_diffs = kept;

        repeats.into_iter().collect()
    }
}

/// Computed summary tally, replacing the deleted harness-emitted summary.
#[derive(Debug, Default)]
pub struct Summary {
    pub parity: u64,
    // Count tiers (parity only).
    pub exact: u64,
    pub near: u64,
    pub drift: u64,
    // Acceptance tiers (parity only).
    pub byte_exact: u64,
    pub accepted: u64,
    pub actionable_drift: u64,
    pub count_divergent: u64,
    // Non-parity outcomes.
    pub compile_fail: u64,
    pub runtime_fail: u64,
    /// Probes whose experiment could not be conducted. Always printed; each
    /// one fails the run.
    pub unevaluated: u64,
    /// Probe processes the harness saw die. Always printed, pinned or not.
    pub harness_abort: u64,
    /// Outcomes brokkr does not yet model (forward-compat).
    pub other: u64,
    /// Probes on which piners discounted at least one window-boundary artifact.
    pub boundary_probes: u64,
    /// Total trades (ours + tv) discounted as window-boundary artifacts across
    /// all probes. The "M trades discounted" half of the summary line.
    pub boundary_trades: u64,
}

/// One divergence group: probes whose signature shares a `domain/dimension`.
#[derive(Debug)]
pub struct DivergenceGroup {
    pub key: String,
    pub count: usize,
    pub examples: Vec<String>,
}

/// One dense-na group: call sites sharing a builtin name across probes.
#[derive(Debug)]
pub struct DenseNaGroup {
    pub builtin: String,
    pub sites: usize,
    pub na_total: u64,
    pub probes: usize,
    pub examples: Vec<String>,
}

/// Up to this many example ids are listed per breakdown group.
const MAX_EXAMPLES: usize = 4;

/// A stdout line that is not JSON: a warning, and an invalid record the
/// integrity check fails the run on.
fn record_not_json(
    report: &mut HarnessReport,
    position: usize,
    trimmed: &str,
    error: &serde_json::Error,
) {
    report.warnings.push(ParseWarning {
        line: position,
        kind: "not_json".to_owned(),
        probe: None,
        reason: error.to_string(),
        raw: clip(trimmed),
    });
    report.invalid.push(InvalidRecord {
        probe: None,
        reason: format!("not JSON ({error}): {}", clip(trimmed)),
        line: position,
        not_json: true,
    });
}

/// A disposition line that deserialized: complete it from the whole `value`
/// it was read from, validate it once, and file it. Every occurrence is
/// validated here, before repeats collapse, so a later consistent duplicate
/// cannot hide an earlier defective record; the record stays in `probes`
/// (stored whole) either way, but an invalid one is never scored.
fn record_disposition(
    report: &mut HarnessReport,
    mut p: ProbeLine,
    value: &serde_json::Value,
    position: usize,
) {
    p.raw = value.to_string();
    p.line = position;
    // Missing key and present null are different facts.
    p.declared = value.get("disposition").cloned();
    let census = value.get("labels").map(parse_label_census).transpose();
    p.census = census.clone().ok().flatten();
    p.verdict = p.validate(value, census);
    // Every valid refusal is evidence, kept here before repeats collapse or
    // a taint unscores its probe.
    if let Some(refusal) = p.refusal() {
        report.refusals.push((p.probe.clone(), refusal.clone()));
    }
    if let Some(why) = p.record_defect() {
        report.invalid.push(InvalidRecord {
            probe: Some(p.probe.clone()),
            reason: why.to_owned(),
            line: position,
            not_json: false,
        });
    }
    report.probes.push(p);
}

/// A disposition line that does not deserialize: a warning, and an invalid
/// record naming the probe when the line carried one.
fn record_unparsable_disposition(
    report: &mut HarnessReport,
    position: usize,
    trimmed: &str,
    probe: Option<String>,
    error: &serde_json::Error,
) {
    report.warnings.push(ParseWarning {
        line: position,
        kind: "disposition_parse".to_owned(),
        probe: probe.clone(),
        reason: error.to_string(),
        raw: clip(trimmed),
    });
    report.invalid.push(InvalidRecord {
        probe,
        reason: format!("unparsable disposition line: {error}"),
        line: position,
        not_json: false,
    });
}

/// Parse NDJSON harness stdout. Blank lines are skipped; a legacy `summary`
/// line is skipped (the harness no longer emits one); a `trade_diff` line is
/// collected, and one that fails to parse is warned about and dropped (it is
/// a diagnostic, never scored); any other non-disposition `kind` is skipped.
/// A line that is not JSON, or a disposition line that does not deserialize,
/// is collected into [`HarnessReport::invalid`] rather than dropped: it is a
/// protocol violation the integrity check fails the run on, and dropping it
/// silently is how a probe used to vanish from a run that could still pass.
/// Unknown *fields* stay tolerated (forward compat; see the module docs).
pub fn parse(stdout: &[u8]) -> HarnessReport {
    let text = String::from_utf8_lossy(stdout);
    let mut report = HarnessReport {
        unterminated: !stdout.is_empty() && !stdout.ends_with(b"\n"),
        ..HarnessReport::default()
    };

    let mut position = 0usize;
    for (index, line) in text.lines().enumerate() {
        // The physical line number, blank lines counted: `run_start` must be
        // physical line 1.
        let physical = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        position += 1;
        report.last_line = Some(position);
        // Peek at the line as a generic value to discriminate record kinds
        // before committing to the ProbeLine shape.
        let value = match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => v,
            Err(e) => {
                record_not_json(&mut report, position, trimmed, &e);
                continue;
            }
        };

        // Contract kinds first: a contract line is judged whatever else it
        // carries, so no tolerance below (the legacy summary skip) can hide one.
        if let Some(kind) = value.get("kind").and_then(serde_json::Value::as_str)
            && CONTRACT_KINDS.contains(&kind)
        {
            let kind = kind.to_owned();
            report.contract.push(ContractLine {
                line: position,
                physical,
                kind,
                value,
            });
            continue;
        }

        // Tolerate (and ignore) a stray legacy summary record, and only that:
        // `summary: true`, no `probe` key, and no `kind` or a kind other than
        // `disposition`. A disposition record (no kind or `kind:
        // "disposition"`, naming a probe) is recognised first whatever flags
        // it carries, so a `summary` flag can never make one vanish.
        if is_legacy_summary(&value) {
            continue;
        }

        // Discriminate by `kind`. A line that omits `kind` is a disposition
        // line (the harness's original shape, unchanged); `kind ==
        // "disposition"` is the explicit form of the same - both feed
        // aggregation and the gate. A `kind == "trade_diff"` line is collected
        // for persistence but stays out of aggregation. Any other `kind` - a
        // future record type - is skipped. A line that fails to parse is
        // surfaced as a warning but never aborts the run.
        // Pull the probe id out of the peeked value before `from_value`
        // consumes it, so a parse failure names the offending probe instead of
        // surfacing an anonymous `invalid type` error with no way to tell which
        // line it came from.
        let probe_id = value
            .get("probe")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let dense_na_warnings = if matches!(
            value.get("kind").and_then(serde_json::Value::as_str),
            None | Some("disposition")
        ) {
            malformed_dense_na_sites(&value, probe_id.as_deref(), position)
        } else {
            Vec::new()
        };
        let at = probe_id
            .as_deref()
            .map_or_else(String::new, |id| format!(" (probe {id})"));
        match value.get("kind").and_then(serde_json::Value::as_str) {
            None | Some("disposition") => {
                match ProbeLine::deserialize(&value) {
                    Ok(p) => record_disposition(&mut report, p, &value, position),
                    Err(e) => {
                        record_unparsable_disposition(&mut report, position, trimmed, probe_id, &e);
                    }
                }
                report.warnings.extend(dense_na_warnings);
            }
            Some("trade_diff") => match TradeDiffLine::deserialize(&value) {
                Ok(t) => report.trade_diffs.push(TradeDiffLine {
                    raw: value.to_string(),
                    ..t
                }),
                Err(e) => {
                    output::warn(&format!("corpus: unparsable trade_diff line{at}: {e}"));
                    report.warnings.push(ParseWarning {
                        line: position,
                        kind: "trade_diff_parse".to_owned(),
                        probe: probe_id,
                        reason: e.to_string(),
                        raw: clip(trimmed),
                    });
                }
            },
            Some(_) => continue,
        }
    }

    report
}

/// Collect malformed dense-na subrecords before `de_dense_na_sites` drops
/// them. The disposition's raw JSON remains intact; this makes each typed
/// omission visible in the persisted run diagnostics too.
fn malformed_dense_na_sites(
    value: &serde_json::Value,
    probe: Option<&str>,
    line: usize,
) -> Vec<ParseWarning> {
    let Some(sites) = value
        .get("dense_na_sites")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    sites
        .iter()
        .enumerate()
        .filter_map(|(index, site)| {
            serde_json::from_value::<DenseNaSite>(site.clone())
                .err()
                .map(|error| {
                    let raw = site.to_string();
                    output::warn(&format!(
                        "corpus: dropped malformed dense_na_sites entry {} on line {line}{}",
                        index + 1,
                        probe.map_or_else(String::new, |id| format!(" (probe {id})"))
                    ));
                    ParseWarning {
                        line,
                        kind: "dense_na_site_parse".to_owned(),
                        probe: probe.map(str::to_owned),
                        reason: error.to_string(),
                        raw: clip(&raw),
                    }
                })
        })
        .collect()
}

/// The legacy trailing summary record the harness once emitted: `summary:
/// true`, no `probe` key, and no `kind` or one other than `disposition`.
fn is_legacy_summary(value: &serde_json::Value) -> bool {
    value.get("summary").and_then(serde_json::Value::as_bool) == Some(true)
        && value.get("probe").is_none()
        && value.get("kind").and_then(serde_json::Value::as_str) != Some("disposition")
}

/// At most this many characters of a non-JSON line are quoted in its
/// invalid-record reason: enough to recognise a stray print, not a megabyte
/// of garbage.
const CLIP_CHARS: usize = 120;

fn clip(line: &str) -> String {
    let mut out: String = line.chars().take(CLIP_CHARS).collect();
    if line.chars().count() > CLIP_CHARS {
        out.push_str("...");
    }
    out
}

/// Tally the per-probe stream into a [`Summary`].
pub fn summarize(probes: &[ProbeLine]) -> Summary {
    let mut s = Summary::default();
    for p in probes {
        // The summary normally receives the reconciliation's scored set.
        // Keep its classification tied to the record validator as well, so
        // malformed raw outcomes cannot enter a disposition total directly.
        match p.outcome() {
            Some(ProbeOutcome::Evaluated(disposition)) => {
                if disposition.is_parity() {
                    s.parity += 1;
                    match p.count_tier.as_deref() {
                        Some("exact") => s.exact += 1,
                        Some("near") => s.near += 1,
                        Some("drift") => s.drift += 1,
                        _ => {}
                    }
                }
                match disposition {
                    PinnedDisposition::ByteExact => s.byte_exact += 1,
                    PinnedDisposition::Accepted => s.accepted += 1,
                    PinnedDisposition::ActionableDrift => s.actionable_drift += 1,
                    PinnedDisposition::CountDivergent => s.count_divergent += 1,
                    PinnedDisposition::CompileFail => s.compile_fail += 1,
                    PinnedDisposition::RuntimeFail => s.runtime_fail += 1,
                    PinnedDisposition::HarnessAbort => s.harness_abort += 1,
                }
            }
            Some(ProbeOutcome::Unevaluated(_)) => s.unevaluated += 1,
            None => s.other += 1,
        }
        // Window-boundary discounting is orthogonal to outcome: tally it for
        // every probe so the "log what was dropped" line covers the full set.
        if p.has_boundary_discount() {
            s.boundary_probes += 1;
            s.boundary_trades += p.boundary_ours + p.boundary_tv;
        }
    }
    s
}

/// Group probes carrying a signature by `domain/dimension`, with counts and
/// a few example ids. Probes without a signature drop out; a `byte_exact`
/// probe with a label signature stays in, as the label disagreement it is.
///
/// This is the divergence breakdown, and it is keyed on the `signature` -
/// which piners computes off the *effective* (boundary-discounted) divergence.
/// A probe whose only divergence is the window-boundary seam therefore arrives
/// with no count-divergence signature and drops out here automatically; the
/// breakdown reports the effective divergent population without brokkr having
/// to re-net the raw `ours_only`/`tv_only` itself. (The discount is still made
/// visible via the summary line and the disposition's `boundary_*` columns.)
pub fn divergence_breakdown(probes: &[ProbeLine]) -> Vec<DivergenceGroup> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in probes {
        if let Some(sig) = &p.signature {
            let dim = if sig.dimension.is_empty() {
                "?"
            } else {
                &sig.dimension
            };
            let domain = if sig.domain.is_empty() {
                "?"
            } else {
                &sig.domain
            };
            groups
                .entry(format!("{domain}/{dim}"))
                .or_default()
                .push(p.probe.clone());
        }
    }
    // Examples by id, not arrival: dispositions arrive in completion order,
    // which varies run to run.
    groups
        .into_iter()
        .map(|(key, mut ids)| {
            ids.sort();
            DivergenceGroup {
                count: ids.len(),
                examples: ids.into_iter().take(MAX_EXAMPLES).collect(),
                key,
            }
        })
        .collect()
}

/// Group every probe's dense-na sites by builtin name, tracking how many
/// sites and how many distinct probes hit each builtin, plus example ids.
pub fn dense_na_breakdown(probes: &[ProbeLine]) -> Vec<DenseNaGroup> {
    // builtin -> (site count, total na, ordered distinct probe ids)
    let mut groups: BTreeMap<String, (usize, u64, Vec<String>)> = BTreeMap::new();
    for p in probes {
        for site in &p.dense_na_sites {
            let name = if site.name.is_empty() {
                "?"
            } else {
                &site.name
            };
            let entry = groups.entry(name.to_owned()).or_default();
            entry.0 += 1;
            entry.1 += site.na_count;
            if !entry.2.contains(&p.probe) {
                entry.2.push(p.probe.clone());
            }
        }
    }
    groups
        .into_iter()
        .map(|(builtin, (sites, na_total, mut ids))| {
            ids.sort();
            DenseNaGroup {
                builtin,
                sites,
                na_total,
                probes: ids.len(),
                examples: ids.into_iter().take(MAX_EXAMPLES).collect(),
            }
        })
        .collect()
}

/// What [`render`] prints: the deviating scored lines plus every
/// `harness_abort` line (a crash stays visible even on its pin) and every
/// `unevaluated` one (which no pin can hold anyway), how many
/// scored lines sit quietly on their pin, and the scored lines themselves
/// (the only input to the summary and breakdowns). Sorted by probe id.
struct RenderPlan<'a> {
    shown: Vec<&'a ProbeLine>,
    hidden: usize,
    scored: Vec<ProbeLine>,
}

impl<'a> RenderPlan<'a> {
    fn new(report: &'a HarnessReport, scored: &HashSet<&str>, deviating: &HashSet<&str>) -> Self {
        let mut plan = RenderPlan {
            shown: Vec::new(),
            hidden: 0,
            scored: Vec::new(),
        };
        for p in report
            .probes
            .iter()
            .filter(|p| scored.contains(p.probe.as_str()))
        {
            // Production supplies the reconciliation's valid selected ids,
            // which already excludes malformed records. Recheck the shared
            // validator here so the special crash display cannot key off a
            // raw outcome if that caller contract ever changes.
            if deviating.contains(p.probe.as_str())
                || matches!(
                    p.outcome(),
                    Some(
                        ProbeOutcome::Evaluated(PinnedDisposition::HarnessAbort)
                            | ProbeOutcome::Unevaluated(_)
                    )
                )
            {
                plan.shown.push(p);
            } else {
                plan.hidden += 1;
            }
            plan.scored.push(p.clone());
        }
        plan.shown.sort_by(|a, b| a.probe.cmp(&b.probe));
        plan.scored.sort_by(|a, b| a.probe.cmp(&b.probe));
        plan
    }
}

/// Render the per-probe lines, the computed summary, and the two breakdowns
/// to the `[corpus]` log.
///
/// Only probes that *deviate* from their pinned `expected` are printed
/// individually: `deviating` carries those ids (the gate's violation set, the
/// caller's single source of truth for "not where the pin says it should
/// be"). A probe sitting exactly on its pin is suppressed and folded into one
/// trailing count - its disposition is already pinned and reproduced in the
/// summary, so the surviving lines are the ones worth eyeballing. On an
/// unblessed corpus every probe deviates, so nothing is hidden.
///
/// Everything here is over the **scored** set only - `scored` is the
/// reconciliation's valid selected ids. A report-only extra, an invalid line
/// or a repeat is never a probe "matching its pin" and never feeds the
/// summary or a breakdown; the reconciliation lines name those instead.
/// `reconciliation` is the integrity line the summary leads with.
pub fn render(
    report: &HarnessReport,
    scored: &HashSet<&str>,
    deviating: &HashSet<&str>,
    reconciliation: &str,
) {
    let plan = RenderPlan::new(report, scored, deviating);
    for p in &plan.shown {
        output::corpus_msg(&format_probe(p));
    }
    if plan.hidden > 0 {
        output::corpus_msg(&format!(
            "{} matching their pin (hidden)",
            output::count(plan.hidden, "probe")
        ));
    }
    let scored_lines = plan.scored;

    let summary = summarize(&scored_lines);
    output::corpus_msg(&format_summary(&summary, reconciliation));

    // No silent drop: whenever piners reclassified any unmatched trades as
    // window-boundary artifacts, say so. A probe flipping count_divergent ->
    // accepted on the discount would otherwise read as "we fixed it" when we
    // only discounted the seam.
    if summary.boundary_probes > 0 {
        output::corpus_msg(&format!(
            "boundary artifacts: {}, {} discounted (effective parity scored on the interior)",
            output::count(
                usize::try_from(summary.boundary_probes).unwrap_or(usize::MAX),
                "probe"
            ),
            output::count(
                usize::try_from(summary.boundary_trades).unwrap_or(usize::MAX),
                "trade"
            )
        ));
    }

    let divergence = divergence_breakdown(&scored_lines);
    if !divergence.is_empty() {
        output::corpus_msg("divergence breakdown (probes with a signature, by domain/dimension):");
        for g in &divergence {
            output::corpus_msg(&format!(
                "  {}: {} (e.g. {})",
                g.key,
                g.count,
                g.examples.join(", ")
            ));
        }
    }

    let dense = dense_na_breakdown(&scored_lines);
    if !dense.is_empty() {
        output::corpus_msg("dense-na breakdown (by builtin):");
        for g in &dense {
            output::corpus_msg(&format!(
                "  {}: {}, {} na across {} (e.g. {})",
                g.builtin,
                output::count(g.sites, "site"),
                g.na_total,
                output::count(g.probes, "probe"),
                g.examples.join(", ")
            ));
        }
    }
}

fn format_probe(p: &ProbeLine) -> String {
    if let Some(refusal) = p.refusal() {
        return format!(
            "{}: unevaluated ({}) - {}",
            p.probe,
            refusal.reason.label(),
            refusal.error
        );
    }
    if let Some(err) = &p.error {
        return format!("{}: {} - {err}", p.probe, p.outcome);
    }
    let mut line = format!("{}: {}", p.probe, p.outcome);
    if let Some(a) = &p.acceptance {
        line.push_str(&format!(" [{}", a.tier));
        if !a.profile.is_empty() {
            line.push_str(&format!("/{}", a.profile));
        }
        line.push(']');
        if !a.failing.is_empty() {
            line.push_str(&format!(" failing={}", a.failing.join(",")));
        }
    } else if let Some(ct) = &p.count_tier {
        line.push_str(&format!(" [{ct}]"));
    }
    line.push_str(&format!(
        " matched={} ours_only={} tv_only={}",
        p.matched, p.ours_only, p.tv_only
    ));
    // Make the raw-vs-effective gap explicit on the deviation line: the raw
    // counts above are the factual unmatched pairings (our_trade_count =
    // matched + ours_only), while the discount shows what piners scored the
    // label on.
    if p.has_boundary_discount() {
        line.push_str(&format!(
            " (boundary ours={} tv={}; effective ours={} tv={})",
            p.boundary_ours,
            p.boundary_tv,
            p.effective_ours(),
            p.effective_tv(),
        ));
    }
    line
}

/// The summary line. It leads with the reconciliation (`N selected, M scored,
/// K missing`, see [`crate::piners::integrity::Reconciliation::summary`])
/// rather than a bare line count, so an empty report reads as what it is - a
/// selection the harness did not report - and never as "0 total". The tallies
/// after it count every disposition line the report carries.
fn format_summary(s: &Summary, reconciliation: &str) -> String {
    let mut out = format!(
        "summary: {reconciliation}; {} parity (exact={} near={} drift={}); \
         tiers byte_exact={} accepted={} actionable_drift={} count_divergent={}; \
         compile_fail={} runtime_fail={} unevaluated={} harness_abort={}",
        s.parity,
        s.exact,
        s.near,
        s.drift,
        s.byte_exact,
        s.accepted,
        s.actionable_drift,
        s.count_divergent,
        s.compile_fail,
        s.runtime_fail,
        s.unevaluated,
        s.harness_abort,
    );
    if s.other > 0 {
        out.push_str(&format!(" other={}", s.other));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn parses_enriched_probe_lines_and_skips_legacy_summary() {
        let nd = br#"{"probe":"p1","outcome":"parity","matched":42,"ours_only":0,"tv_only":0,"count_tier":"exact","acceptance":{"tier":"byte_exact","profile":"production","failing":[]}}
{"probe":"p2","outcome":"parity","matched":218,"count_tier":"drift","acceptance":{"tier":"actionable_drift","profile":"production","failing":["exit_price"]},"signature":{"domain":"broker-fidelity","leg":"exit","dimension":"exit_price","dimension_breaches":3},"dense_na_sites":[{"name":"strategy.exit","call_site":"s.pine:12","na_count":7}]}
{"probe":"p3","outcome":"compile_fail","error":"unexpected token"}
{"summary":true,"total":3,"parity":2}
"#;
        let r = parse(nd);
        assert_eq!(r.probes.len(), 3); // legacy summary line skipped
        assert_eq!(
            r.probes[1].signature.as_ref().unwrap().dimension,
            "exit_price"
        );
        assert_eq!(r.probes[1].dense_na_sites[0].name, "strategy.exit");
        assert_eq!(r.probes[2].error.as_deref(), Some("unexpected token"));
    }

    #[test]
    fn disposition_maps_parity_to_tier_and_else_to_outcome() {
        let r = parse(
            br#"{"probe":"p1","outcome":"parity","acceptance":{"tier":"accepted"}}
{"probe":"p2","outcome":"compile_fail","error":"x"}
{"probe":"p3","outcome":"parity"}
"#,
        );
        assert_eq!(r.probes[0].disposition(), "accepted");
        assert_eq!(r.probes[1].disposition(), "compile_fail");
        assert_eq!(r.probes[2].disposition(), "parity"); // malformed -> trips gate
    }

    #[test]
    fn summarize_tallies_tiers_and_outcomes() {
        let r = parse(
            br#"{"probe":"a","outcome":"parity","count_tier":"exact","acceptance":{"tier":"byte_exact"}}
{"probe":"b","outcome":"parity","count_tier":"drift","acceptance":{"tier":"actionable_drift"}}
{"probe":"c","outcome":"parity","count_tier":"near","acceptance":{"tier":"count_divergent"}}
{"probe":"d","outcome":"compile_fail","error":"x"}
{"probe":"e","outcome":"unevaluated","reason":"no_overlap","error":"disjoint"}
{"probe":"f","outcome":"no_tv_data"}
"#,
        );
        let s = summarize(&r.probes);
        assert_eq!(s.parity, 3);
        assert_eq!((s.exact, s.near, s.drift), (1, 1, 1));
        assert_eq!(s.byte_exact, 1);
        assert_eq!(s.actionable_drift, 1);
        assert_eq!(s.count_divergent, 1);
        assert_eq!(s.compile_fail, 1);
        assert_eq!(s.unevaluated, 1);
        // A retired outcome on a new line is no disposition at all.
        assert_eq!(s.other, 1);
        assert!(format_summary(&s, "x").contains("unevaluated=1 harness_abort=0"));
    }

    #[test]
    fn an_unevaluated_line_needs_a_known_reason_and_a_one_line_error() {
        let line = |fields: &str| {
            parse(format!("{{\"probe\":\"p\",\"outcome\":\"unevaluated\"{fields}}}").as_bytes())
        };
        let ok = line(
            r#","disposition":"unevaluated","reason":"feed_coverage","error":"past the feed""#,
        );
        assert!(ok.invalid.is_empty(), "{:?}", ok.invalid);
        let p = &ok.probes[0];
        let want = Refusal {
            reason: RefusalReason::FeedCoverage,
            error: "past the feed".to_owned(),
        };
        assert_eq!(p.outcome(), Some(&ProbeOutcome::Unevaluated(want.clone())));
        assert_eq!(p.refusal(), Some(&want));
        assert!(format_probe(p).starts_with("p: unevaluated (feed_coverage) - past the feed"));
        // runtime_ms and keys piners does not define stay tolerated.
        let extra = line(r#","reason":"no_overlap","error":"x","runtime_ms":4,"future":1"#);
        assert!(extra.invalid.is_empty(), "{:?}", extra.invalid);
        for bad in [
            r#","error":"x""#,
            r#","reason":"no_tv_data","error":"x""#,
            r#","reason":7,"error":"x""#,
            r#","reason":"no_overlap""#,
            r#","reason":"no_overlap","error":"  ""#,
            r#","reason":"no_overlap","error":"two\nlines""#,
            r#","reason":"no_overlap","error":"x","acceptance":{"tier":"accepted"}"#,
            r#","reason":"no_overlap","error":"x","acceptance":{"tier":""}"#,
            r#","reason":"no_overlap","error":"x","matched":1"#,
            r#","reason":"no_overlap","error":"x","boundary_ours":3"#,
            r#","reason":"no_overlap","error":"x","signature":null"#,
        ] {
            let r = line(bad);
            assert_eq!(r.invalid.len(), 1, "{bad}");
            assert_eq!(r.probes[0].outcome(), None, "{bad}");
            assert_eq!(r.probes[0].refusal(), None, "{bad}");
        }
        // Every parity key piners defines, null included, makes it invalid.
        // (A null on a field the line deserializes as a number fails before
        // the refusal check; invalid either way.)
        for key in PARITY_KEYS {
            let r = line(&format!(
                r#","reason":"no_overlap","error":"x","{key}":null"#
            ));
            assert_eq!(r.invalid.len(), 1, "{key}");
            assert!(r.probes.iter().all(|p| p.outcome().is_none()), "{key}");
            assert!(r.refusals.is_empty(), "{key}");
        }
        // A `reason` on any other line is ignored, whatever its type.
        let other = parse(br#"{"probe":"p","outcome":"compile_fail","error":"x","reason":3}"#);
        assert!(other.invalid.is_empty());
        assert_eq!(other.probes[0].refusal(), None);
    }

    #[test]
    fn retired_outcomes_are_invalid_on_a_new_line() {
        for outcome in ["no_tv_data", "no_overlap"] {
            let r = parse(format!("{{\"probe\":\"p\",\"outcome\":\"{outcome}\"}}").as_bytes());
            assert_eq!(r.invalid.len(), 1, "{outcome}");
            assert_eq!(r.probes[0].outcome(), None, "{outcome}");
            assert!(crate::piners::registry::RETIRED_LABELS.contains(&outcome));
        }
    }

    #[test]
    fn an_unevaluated_line_is_always_shown() {
        let r = parse(br#"{"probe":"u","outcome":"unevaluated","reason":"invalid_evidence","error":"quoted cell"}"#);
        let scored: HashSet<&str> = ["u"].into_iter().collect();
        let plan = RenderPlan::new(&r, &scored, &HashSet::new());
        assert_eq!(plan.shown.len(), 1);
        assert_eq!(plan.hidden, 0);
    }

    #[test]
    fn divergence_groups_by_domain_dimension() {
        let r = parse(
            br#"{"probe":"a","outcome":"parity","signature":{"domain":"broker-fidelity","dimension":"exit_price"}}
{"probe":"b","outcome":"parity","signature":{"domain":"broker-fidelity","dimension":"exit_price"}}
{"probe":"c","outcome":"parity","count_tier":"exact"}
"#,
        );
        let groups = divergence_breakdown(&r.probes);
        assert_eq!(groups.len(), 1); // exact probe c has no signature
        assert_eq!(groups[0].key, "broker-fidelity/exit_price");
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].examples, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn dense_na_groups_by_builtin_with_site_and_probe_counts() {
        let r = parse(
            br#"{"probe":"a","outcome":"parity","dense_na_sites":[{"name":"ta.ema","na_count":3},{"name":"ta.ema","na_count":1}]}
{"probe":"b","outcome":"parity","dense_na_sites":[{"name":"ta.ema","na_count":5}]}
"#,
        );
        let groups = dense_na_breakdown(&r.probes);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].builtin, "ta.ema");
        assert_eq!(groups[0].sites, 3); // two in a, one in b
        assert_eq!(groups[0].probes, 2);
    }

    #[test]
    fn tolerates_unknown_fields_and_blank_lines() {
        let nd = br#"
{"probe":"p1","outcome":"parity","matched":1,"future_field":{"x":1},"tv_only":0}

"#;
        let r = parse(nd);
        assert_eq!(r.probes.len(), 1);
        assert_eq!(r.probes[0].probe, "p1");
    }

    #[test]
    fn collects_trade_diff_and_skips_unknown_kind_lines() {
        // A disposition line (no kind), two trade_diff drill-down records in
        // the authoritative 26-field shape, and a hypothetical future record
        // kind. The disposition feeds aggregation; the trade_diffs are
        // collected for persistence but stay out of the summary; the unknown
        // kind is skipped. The second trade_diff exercises the open/partial
        // shape - the deltas and every `tv_*` field omitted.
        let nd = br#"{"probe":"p1","outcome":"parity","count_tier":"drift","acceptance":{"tier":"actionable_drift"}}
{"kind":"trade_diff","probe":"p1","our_index":1,"tv_index":1,"entry_ts_delta":0,"exit_ts_delta":0,"entry_price_delta":2.2737367544323206e-13,"exit_price_delta":0.08000000000015461,"our_entry_ts":1745295300,"our_exit_ts":1745295300,"our_entry_bar":125,"our_exit_bar":125,"our_entry_price":1582.6000000000001,"our_exit_price":1582.14,"our_qty":1.0,"our_pnl":-0.4600000000000364,"our_side":"Long","our_entry_id":"L","our_exit_id":"X","tv_entry_ts":1745295300,"tv_exit_ts":1745295300,"tv_entry_price":1582.6,"tv_exit_price":1582.06,"tv_entry_qty":1.0,"tv_pnl":-0.54,"tv_entry_signal":"entry long","tv_exit_signal":"mid-bar stop"}
{"kind":"trade_diff","probe":"p1","our_index":2,"tv_index":2,"our_entry_ts":1745295600,"our_exit_ts":1745295900,"our_entry_price":99.0,"our_exit_price":100.0,"our_qty":2.0,"our_pnl":2.0}
{"kind":"future_record","probe":"p1","whatever":true}
"#;
        let r = parse(nd);
        assert_eq!(r.probes.len(), 1); // only the disposition line
        assert_eq!(r.probes[0].probe, "p1");
        assert_eq!(r.trade_diffs.len(), 2); // both drill-down records collected
        // The whole line is kept for the store, fields brokkr never names
        // included.
        let first: serde_json::Value = serde_json::from_str(&r.trade_diffs[0].raw).unwrap();
        assert_eq!(first["tv_exit_signal"], "mid-bar stop");
        assert_eq!(r.trade_diffs[1].our_index, 2);
        let second: serde_json::Value = serde_json::from_str(&r.trade_diffs[1].raw).unwrap();
        assert!(second.get("tv_pnl").is_none()); // an omitted leg stays omitted
        let s = summarize(&r.probes);
        assert_eq!(s.actionable_drift, 1); // drill-down lines excluded
    }

    #[test]
    fn parses_boundary_fields_and_computes_effective() {
        // A boundary-only probe: raw ours_only=2, all discounted -> effective 0.
        // piners scores it accepted off the effective counts; brokkr keeps the
        // raw counts and carries the discount alongside.
        let r = parse(
            br#"{"probe":"p1","outcome":"parity","matched":208,"ours_only":2,"tv_only":1,"boundary_ours":2,"boundary_tv":1,"acceptance":{"tier":"accepted"}}
{"probe":"p2","outcome":"parity","matched":50,"ours_only":3,"tv_only":0,"acceptance":{"tier":"actionable_drift"}}
"#,
        );
        assert_eq!((r.probes[0].ours_only, r.probes[0].boundary_ours), (2, 2));
        assert_eq!((r.probes[0].tv_only, r.probes[0].boundary_tv), (1, 1));
        assert_eq!(
            (r.probes[0].effective_ours(), r.probes[0].effective_tv()),
            (0, 0)
        );
        assert!(r.probes[0].has_boundary_discount());
        // Absent fields default to 0 -> no discount, effective == raw.
        assert!(!r.probes[1].has_boundary_discount());
        assert_eq!(r.probes[1].effective_ours(), 3);
        // Defensive: a malformed boundary > raw saturates at 0, never underflows.
        let bad = parse(br#"{"probe":"x","outcome":"parity","ours_only":1,"boundary_ours":5}"#);
        assert_eq!(bad.probes[0].effective_ours(), 0);
    }

    #[test]
    fn summarize_counts_boundary_artifacts() {
        let r = parse(
            br#"{"probe":"a","outcome":"parity","ours_only":2,"boundary_ours":2,"acceptance":{"tier":"accepted"}}
{"probe":"b","outcome":"parity","ours_only":1,"tv_only":3,"boundary_ours":1,"boundary_tv":2,"acceptance":{"tier":"actionable_drift"}}
{"probe":"c","outcome":"parity","ours_only":4,"acceptance":{"tier":"count_divergent"}}
"#,
        );
        let s = summarize(&r.probes);
        // a (2) and b (1+2=3) carry discounts; c does not.
        assert_eq!(s.boundary_probes, 2);
        assert_eq!(s.boundary_trades, 5);
    }

    #[test]
    fn boundary_only_probe_drops_from_breakdown_but_side_flip_stays() {
        // The #2c contract fixture. Two count-divergence triggers exist on the
        // piners side: (1) effective ours_only/tv_only > 0 -> sig dimension
        // ours_only/tv_only; (2) a timestamp-matched opposing-direction pair
        // (the side guard) -> sig dimension `side`. Boundary discounting nets
        // only trigger (1). So:
        //  - `boundary-only`: raw ours_only=2 fully discounted -> piners emits
        //    `accepted` with NO count-divergence signature. It must NOT appear
        //    in the divergence breakdown.
        //  - `side-flip`: a genuine long/short flip stays `count_divergent` with
        //    sig_dimension=`side` even at effective 0/0. It MUST stay.
        let r = parse(
            br#"{"probe":"boundary-only","outcome":"parity","matched":208,"ours_only":2,"tv_only":0,"boundary_ours":2,"acceptance":{"tier":"accepted"}}
{"probe":"side-flip","outcome":"parity","matched":40,"ours_only":0,"tv_only":0,"acceptance":{"tier":"count_divergent"},"signature":{"domain":"broker-fidelity","dimension":"side"}}
"#,
        );
        let groups = divergence_breakdown(&r.probes);
        // Only the side-flip's signature survives; the boundary-only probe (no
        // signature) is absent.
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].key, "broker-fidelity/side");
        assert_eq!(groups[0].examples, vec!["side-flip".to_owned()]);
        // And the boundary-only probe is tallied as a discount, not a divergence.
        let s = summarize(&r.probes);
        assert_eq!((s.accepted, s.count_divergent), (1, 1));
        assert_eq!((s.boundary_probes, s.boundary_trades), (1, 2));
    }

    #[test]
    fn accepts_numeric_call_site_and_keeps_disposition() {
        // The regression: the harness emitted a numeric `call_site` (a bare line
        // number) inside an optional diagnostic. Modelling it as a bare String
        // made `from_value` fail, which discarded the whole probe - so the gate
        // later reported "harness emitted no disposition". Now the integer is
        // accepted and normalized to a string, and the disposition survives.
        let r = parse(
            br#"{"probe":"matrix-eigen-rank-deficient-cov-01","outcome":"parity","disposition":"accepted","acceptance":{"tier":"accepted"},"dense_na_sites":[{"name":"matrix.eigenvalues","call_site":13,"na_count":9968,"call_count":9968,"stub_count":0}]}
"#,
        );
        assert_eq!(r.probes.len(), 1);
        assert_eq!(r.probes[0].disposition(), "accepted");
        assert_eq!(r.probes[0].dense_na_sites[0].name, "matrix.eigenvalues");
        assert_eq!(r.probes[0].dense_na_sites[0].call_site, "13"); // int -> string
        assert_eq!(r.probes[0].dense_na_sites[0].na_count, 9968);
    }

    #[test]
    fn malformed_dense_na_site_does_not_sink_disposition() {
        // A pure-diagnostic site with the wrong type on a non-call_site field
        // (na_count as a string) is dropped on its own, but the gate-critical
        // disposition and the sibling well-formed (numeric-call_site) site both
        // survive. One bad diagnostic must never cost us the probe.
        let r = parse(
            br#"{"probe":"p1","outcome":"parity","acceptance":{"tier":"accepted"},"dense_na_sites":[{"name":"a","call_site":8,"na_count":5},{"name":"b","na_count":"oops"}]}
"#,
        );
        assert_eq!(r.probes.len(), 1);
        assert_eq!(r.probes[0].disposition(), "accepted");
        assert_eq!(r.probes[0].dense_na_sites.len(), 1); // bad site dropped
        assert_eq!(r.probes[0].dense_na_sites[0].name, "a");
        assert_eq!(r.probes[0].dense_na_sites[0].call_site, "8");
    }

    #[test]
    fn a_present_label_census_must_be_whole_and_consistent() {
        let leg = r#"{"compared":3,"mismatched":1,"ambiguous":2,"unavailable":0}"#;
        let line = |labels: &str| {
            format!(
                "{{\"probe\":\"p\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"accepted\"}}{labels}}}"
            )
        };
        // Absent: valid, no census.
        let r = parse(line("").as_bytes());
        assert!(r.invalid.is_empty());
        assert_eq!(r.probes[0].census, None);
        // Whole and consistent: valid.
        let r = parse(line(&format!(",\"labels\":{{\"entry\":{leg},\"exit\":{leg}}}")).as_bytes());
        assert!(r.invalid.is_empty());
        assert_eq!(r.probes[0].census.unwrap().exit.ambiguous, 2);
        // Each defect makes the whole record invalid.
        for bad in [
            ",\"labels\":null".to_owned(),
            format!(",\"labels\":{{\"entry\":{leg}}}"),
            format!(
                ",\"labels\":{{\"entry\":{leg},\"exit\":{{\"compared\":3,\"mismatched\":-1,\
                 \"ambiguous\":0,\"unavailable\":0}}}}"
            ),
            format!(
                ",\"labels\":{{\"entry\":{leg},\"exit\":{{\"compared\":3,\"mismatched\":2,\
                 \"ambiguous\":2,\"unavailable\":0}}}}"
            ),
            format!(
                ",\"labels\":{{\"entry\":{leg},\"exit\":{{\"compared\":3,\"mismatched\":1.5,\
                 \"ambiguous\":0,\"unavailable\":0}}}}"
            ),
        ] {
            let r = parse(line(&bad).as_bytes());
            assert_eq!(r.invalid.len(), 1, "{bad}");
            assert!(r.probes[0].outcome().is_none(), "{bad}");
        }
    }

    #[test]
    fn take_duplicates_keeps_the_last_line_and_names_each_repeat() {
        let mut r = parse(
            br#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}
{"probe":"b","outcome":"parity","acceptance":{"tier":"accepted"}}
{"probe":"a","outcome":"compile_fail","error":"x"}
{"kind":"trade_diff","probe":"b","our_index":1,"tv_index":1,"our_entry_ts":1,"our_exit_ts":2,"our_entry_price":1.0,"our_exit_price":2.0,"our_qty":1.0,"our_pnl":1.0}
{"kind":"trade_diff","probe":"b","our_index":1,"tv_index":1,"our_entry_ts":1,"our_exit_ts":2,"our_entry_price":1.0,"our_exit_price":2.0,"our_qty":1.0,"our_pnl":9.0}
"#,
        );
        let dups = r.take_duplicates();
        assert_eq!(dups.len(), 2);
        assert!(dups[0].starts_with("a (disposition)"));
        assert!(dups[1].contains("b (trade_diff our_index=1 tv_index=1)"));
        // One record per key survives, the last emitted, in emission order.
        assert_eq!(r.probes.len(), 2);
        assert_eq!(r.probes[0].probe, "b");
        assert_eq!(r.probes[1].disposition(), "compile_fail");
        assert_eq!(r.trade_diffs.len(), 1);
        assert!(r.trade_diffs[0].raw.contains("\"our_pnl\":9.0"));
        // A clean report has nothing to report.
        assert!(r.take_duplicates().is_empty());
    }

    #[test]
    fn unusable_disposition_records_are_collected_not_dropped() {
        let r = parse(
            b"thread 'main' panicked at src/x.rs:1\n\
{\"probe\":\"a\",\"outcome\":7}\n\
{\"outcome\":\"parity\"}\n\
{\"probe\":\"b\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"accepted\"}}\n",
        );
        assert_eq!(r.probes.len(), 1);
        assert_eq!(r.invalid.len(), 3);
        assert_eq!(r.invalid[0].probe, None);
        assert!(
            r.invalid[0].reason.starts_with("not JSON"),
            "{:?}",
            r.invalid[0]
        );
        assert!(r.invalid[0].reason.contains("panicked"));
        assert_eq!(r.invalid[1].probe.as_deref(), Some("a"));
        // A disposition line with no probe id cannot be attributed.
        assert_eq!(r.invalid[2].probe, None);
    }

    #[test]
    fn the_validator_checks_outcome_and_tier_together() {
        assert_eq!(
            valid_label("parity", Some("accepted")),
            Some(LineLabel::Evaluated(PinnedDisposition::Accepted))
        );
        assert_eq!(
            valid_label("compile_fail", None),
            Some(LineLabel::Evaluated(PinnedDisposition::CompileFail))
        );
        assert_eq!(
            valid_label("unevaluated", Some("")),
            Some(LineLabel::Unevaluated)
        );
        assert_eq!(valid_label("unevaluated", Some("accepted")), None);
        // A retired outcome is no live label.
        assert_eq!(valid_label("no_overlap", None), None);
        // A tier posing as an outcome.
        assert_eq!(valid_label("accepted", None), None);
        // An outcome posing as a tier.
        assert_eq!(valid_label("parity", Some("runtime_fail")), None);
        assert_eq!(valid_label("parity", None), None);
        // A non-parity outcome carrying a tier.
        assert_eq!(valid_label("runtime_fail", Some("accepted")), None);
        assert_eq!(valid_label("weird", None), None);
    }

    #[test]
    fn harness_abort_is_a_valid_label_and_vocabulary_is_single_sourced() {
        assert_eq!(
            valid_label("harness_abort", None),
            Some(LineLabel::Evaluated(PinnedDisposition::HarnessAbort))
        );
        assert_eq!(valid_label("harness_abort", Some("accepted")), None);
        let labels: Vec<&str> = crate::piners::registry::ALL_PINNED
            .iter()
            .map(|disposition| disposition.as_str())
            .collect();
        assert_eq!(
            labels,
            vec![
                "byte_exact",
                "accepted",
                "actionable_drift",
                "count_divergent",
                "compile_fail",
                "runtime_fail",
                "harness_abort",
            ]
        );
        let reasons: Vec<&str> = RefusalReason::ALL.map(RefusalReason::label).to_vec();
        assert_eq!(
            reasons,
            vec![
                "invalid_evidence",
                "insufficient_evidence",
                "witness_contract",
                "feed_coverage",
                "no_overlap",
            ]
        );
    }

    #[test]
    fn contract_lines_are_recognised_by_kind_with_positions() {
        let r = parse(
            b"{\"kind\":\"setup_stage\",\"version\":1,\"stage\":\"manifest\"}\n\
{\"kind\":\"probe_start\",\"probe\":\"a\"}\n\
{\"probe\":\"a\",\"outcome\":\"harness_abort\",\"error\":\"signal 11\"}\n\
{\"kind\":\"future\",\"x\":1}\n\
{\"kind\":\"run_end\",\"version\":\"1\",\"exit\":1}",
        );
        let kinds: Vec<(&str, usize)> = r
            .contract
            .iter()
            .map(|c| (c.kind.as_str(), c.line))
            .collect();
        // A missing or malformed version is still recognised; the unknown kind is not.
        assert_eq!(
            kinds,
            vec![("setup_stage", 1), ("probe_start", 2), ("run_end", 5)]
        );
        assert_eq!(r.probes[0].line, 3);
        assert_eq!(r.last_line, Some(5));
        assert!(r.unterminated);
        assert!(r.invalid.is_empty());
    }

    #[test]
    fn only_a_real_legacy_summary_is_skipped() {
        let r = parse(
            b"{\"kind\":\"disposition\",\"probe\":\"a\",\"outcome\":\"compile_fail\",\"error\":\"x\",\"disposition\":\"accepted\",\"summary\":true}\n\
{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"error\":\"x\",\"summary\":true}\n\
{\"summary\":true,\"total\":2}\n\
{\"kind\":\"disposition\",\"summary\":true}\n",
        );
        // a (flagged, contradictory) and b (flagged, no kind) are dispositions.
        let ids: Vec<&str> = r.probes.iter().map(|p| p.probe.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        // a's contradiction is listed; the kind:disposition line with no probe
        // is not a summary either - it is an unparsable disposition.
        assert_eq!(r.invalid.len(), 2, "{:?}", r.invalid);
        assert_eq!(r.invalid[0].probe.as_deref(), Some("a"));
        assert!(
            r.invalid[1]
                .reason
                .starts_with("unparsable disposition line")
        );
    }

    #[test]
    fn run_start_is_recognised_with_its_physical_line() {
        let r = parse(b"\n{\"kind\":\"run_start\",\"version\":1}\n");
        assert_eq!(r.contract.len(), 1);
        assert_eq!(
            (
                r.contract[0].kind.as_str(),
                r.contract[0].line,
                r.contract[0].physical
            ),
            ("run_start", 1, 2)
        );
    }

    #[test]
    fn a_missing_disposition_field_is_told_apart_from_a_null_one() {
        let r = parse(
            b"{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"error\":\"x\"}\n{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"error\":\"x\",\"disposition\":null}\n",
        );
        assert_eq!(r.probes[0].declared, None);
        assert_eq!(
            r.probes[0].outcome(),
            Some(&ProbeOutcome::Evaluated(PinnedDisposition::CompileFail))
        );
        assert_eq!(r.probes[1].declared, Some(serde_json::Value::Null));
        assert_eq!(r.probes[1].outcome(), None);
        // The defective record is kept whole (stored) and listed invalid.
        assert_eq!(r.probes.len(), 2);
        assert!(r.probes[1].raw.contains("\"disposition\":null"));
        assert_eq!(r.invalid.len(), 1);
        assert_eq!(r.invalid[0].probe.as_deref(), Some("b"));
    }

    #[test]
    fn a_contract_line_flagged_as_a_legacy_summary_is_still_a_contract_line() {
        let r = parse(b"{\"kind\":\"run_error\",\"version\":2,\"summary\":true}\n{\"summary\":true,\"total\":1}\n");
        assert_eq!(r.contract.len(), 1);
        assert_eq!(r.contract[0].kind, "run_error");
        assert!(r.probes.is_empty() && r.invalid.is_empty());
    }

    #[test]
    fn a_pinned_crash_stays_visible_and_output_is_sorted_by_id() {
        let r = parse(
            br#"{"probe":"z","outcome":"harness_abort","error":"killed by signal 9"}
{"probe":"m","outcome":"parity","acceptance":{"tier":"accepted"},"signature":{"domain":"d","dimension":"x"}}
{"probe":"b","outcome":"parity","acceptance":{"tier":"accepted"},"signature":{"domain":"d","dimension":"x"}}
"#,
        );
        let scored: HashSet<&str> = ["z", "m", "b"].into_iter().collect();
        // Nothing deviates: z is pinned to its crash, yet still shown.
        let plan = RenderPlan::new(&r, &scored, &HashSet::new());
        let shown: Vec<&str> = plan.shown.iter().map(|p| p.probe.as_str()).collect();
        assert_eq!(shown, vec!["z"]);
        assert_eq!(plan.hidden, 2);
        let order: Vec<&str> = plan.scored.iter().map(|p| p.probe.as_str()).collect();
        assert_eq!(order, vec!["b", "m", "z"]);
        let s = summarize(&plan.scored);
        assert_eq!(s.harness_abort, 1);
        assert!(format_summary(&s, "x").contains("harness_abort=1"));
        // Breakdown examples sort by id whatever the arrival order.
        assert_eq!(
            divergence_breakdown(&r.probes)[0].examples,
            vec!["b".to_owned(), "m".to_owned()]
        );
    }

    #[test]
    fn malformed_harness_abort_does_not_enter_the_summary_total() {
        let r =
            parse(br#"{"probe":"a","outcome":"harness_abort","acceptance":{"tier":"accepted"}}"#);
        let summary = summarize(&r.probes);
        assert_eq!(summary.harness_abort, 0);
        assert_eq!(summary.other, 1);
    }

    #[test]
    fn malformed_harness_abort_is_not_specially_rendered() {
        let r =
            parse(br#"{"probe":"a","outcome":"harness_abort","acceptance":{"tier":"accepted"}}"#);
        let scored: HashSet<&str> = ["a"].into_iter().collect();
        let plan = RenderPlan::new(&r, &scored, &HashSet::new());
        assert!(plan.shown.is_empty());
        assert_eq!(plan.hidden, 1);
    }

    #[test]
    fn render_plan_covers_only_the_scored_set() {
        let r = parse(
            br#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"},"signature":{"domain":"d","dimension":"x"}}
{"probe":"b","outcome":"parity","acceptance":{"tier":"accepted"}}
{"probe":"extra","outcome":"parity","acceptance":{"tier":"accepted"},"signature":{"domain":"d","dimension":"y"}}
{"probe":"bad","outcome":"accepted"}
"#,
        );
        let scored: HashSet<&str> = ["a", "b"].into_iter().collect();
        let deviating: HashSet<&str> = ["a", "bad"].into_iter().collect();
        let plan = RenderPlan::new(&r, &scored, &deviating);
        assert_eq!(plan.shown.len(), 1);
        assert_eq!(plan.shown[0].probe, "a");
        // `extra` and `bad` are neither hidden matches nor tallied.
        assert_eq!(plan.hidden, 1);
        assert_eq!(plan.scored.len(), 2);
        let root = divergence_breakdown(&plan.scored);
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].key, "d/x");
    }

    #[test]
    fn summary_leads_with_the_reconciliation_not_a_bare_total() {
        let line = format_summary(&Summary::default(), "850 selected, 0 scored, 850 missing");
        assert!(
            line.starts_with("summary: 850 selected, 0 scored, 850 missing; 0 parity"),
            "{line}"
        );
        assert!(!line.contains("total"), "{line}");
    }

    #[test]
    fn accepts_explicit_disposition_kind() {
        // The harness may also tag disposition lines uniformly with
        // kind:"disposition"; brokkr treats that identically to no kind.
        let r = parse(
            br#"{"kind":"disposition","probe":"p1","outcome":"parity","acceptance":{"tier":"accepted"}}
"#,
        );
        assert_eq!(r.probes.len(), 1);
        assert_eq!(r.probes[0].disposition(), "accepted");
    }
}
