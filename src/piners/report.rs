//! Parse the harness's NDJSON output and aggregate it brokkr-side.
//!
//! The harness emits one JSON object per probe and **no trailing summary
//! line** - brokkr tallies the summary, the root-cause breakdown, and the
//! dense-na breakdown from the per-probe stream itself. brokkr is
//! deliberately tolerant of unknown fields: the harness owns the schema and
//! grows it (extra tiers, deltas, provenance) ahead of brokkr learning to
//! render them, the same forward-compat posture as the cargo JSON parser. A
//! stray legacy `summary` line is skipped rather than mis-parsed as a probe.
//!
//! Lines are discriminated by an optional `kind` field. A line with no `kind`
//! - or `kind == "disposition"` - is a per-probe disposition line: the only
//! kind that feeds aggregation and the gate. Any other `kind` (e.g. the
//! harness's per-trade `trade_diff` drill-down records) is tolerated and
//! skipped, living in `harness.stdout` for inspection but never touching the
//! summary, breakdowns, or exit code. This tolerate-and-skip posture means a
//! new harness record kind needs no brokkr change.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::output;
use crate::piners::registry;

/// Acceptance detail, present only when `outcome == "parity"`.
#[derive(Debug, Clone, Deserialize)]
pub struct Acceptance {
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub failing: Vec<String>,
}

/// Root-cause signature, present on non-exact parity probes. The harness
/// line carries more (`leg`, `detail`, `dimension_breaches`, builtin hints);
/// brokkr models only what it groups by and lets serde drop the rest
/// (forward-compat - these structs deliberately omit `deny_unknown_fields`).
#[derive(Debug, Clone, Deserialize)]
pub struct Signature {
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub dimension: String,
}

/// One dense-`na` call site, present only when non-empty. The harness also
/// carries `call_site`; brokkr aggregates by `name` and sums `na_count`.
#[derive(Debug, Clone, Deserialize)]
pub struct DenseNaSite {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub na_count: u64,
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
    #[serde(default)]
    pub count_tier: Option<String>,
    #[serde(default)]
    pub acceptance: Option<Acceptance>,
    #[serde(default)]
    pub signature: Option<Signature>,
    #[serde(default)]
    pub dense_na_sites: Vec<DenseNaSite>,
    /// Error string carried by a `*_fail` outcome.
    #[serde(default)]
    pub error: Option<String>,
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
}

/// Everything parsed out of one harness run.
#[derive(Debug, Default)]
pub struct HarnessReport {
    pub probes: Vec<ProbeLine>,
}

/// Computed summary tally, replacing the deleted harness-emitted summary.
#[derive(Debug, Default)]
pub struct Summary {
    pub total: u64,
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
    pub no_tv_data: u64,
    pub no_overlap: u64,
    /// Outcomes brokkr does not yet model (forward-compat).
    pub other: u64,
}

/// One root-cause group: non-exact parity probes sharing a
/// `domain/dimension` signature.
#[derive(Debug)]
pub struct RootCauseGroup {
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

/// Parse NDJSON harness stdout. Blank lines are skipped; a legacy `summary`
/// line is skipped (the harness no longer emits one); any line whose `kind`
/// is present and not `"disposition"` (e.g. `trade_diff` drill-down records)
/// is skipped without parsing; a disposition line that fails to parse is
/// surfaced as a warning but does not abort - the run's exit status is the
/// source of truth, and a forward-compat field we cannot model should not
/// sink the report.
pub fn parse(stdout: &[u8]) -> HarnessReport {
    let text = String::from_utf8_lossy(stdout);
    let mut report = HarnessReport::default();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Peek at the line as a generic value to discriminate record kinds
        // before committing to the ProbeLine shape.
        let value = match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => v,
            Err(e) => {
                output::corpus_msg(&format!("warning: unparsable NDJSON line: {e}"));
                continue;
            }
        };

        // Tolerate (and ignore) a stray legacy summary line.
        if value.get("summary").and_then(serde_json::Value::as_bool) == Some(true) {
            continue;
        }

        // Discriminate by `kind`. A line that omits `kind` is a disposition
        // line (the harness's original shape, unchanged); `kind == "disposition"`
        // is the explicit form of the same. Any other `kind` - a `trade_diff`
        // drill-down record today, or a future record type - is skipped: it
        // lives in harness.stdout for inspection but never enters aggregation
        // or the gate.
        match value.get("kind").and_then(serde_json::Value::as_str) {
            None | Some("disposition") => {}
            Some(_) => continue,
        }

        match serde_json::from_value::<ProbeLine>(value) {
            Ok(p) => report.probes.push(p),
            Err(e) => output::corpus_msg(&format!("warning: unparsable probe line: {e}")),
        }
    }

    report
}

/// Tally the per-probe stream into a [`Summary`].
pub fn summarize(probes: &[ProbeLine]) -> Summary {
    let mut s = Summary {
        total: probes.len() as u64,
        ..Summary::default()
    };
    for p in probes {
        match p.outcome.as_str() {
            "parity" => {
                s.parity += 1;
                match p.count_tier.as_deref() {
                    Some("exact") => s.exact += 1,
                    Some("near") => s.near += 1,
                    Some("drift") => s.drift += 1,
                    _ => {}
                }
                if let Some(a) = &p.acceptance {
                    match a.tier.as_str() {
                        "byte_exact" => s.byte_exact += 1,
                        "accepted" => s.accepted += 1,
                        "actionable_drift" => s.actionable_drift += 1,
                        "count_divergent" => s.count_divergent += 1,
                        _ => {}
                    }
                }
            }
            "compile_fail" => s.compile_fail += 1,
            "runtime_fail" => s.runtime_fail += 1,
            "no_tv_data" => s.no_tv_data += 1,
            "no_overlap" => s.no_overlap += 1,
            _ => s.other += 1,
        }
    }
    s
}

/// Group probes carrying a signature by `domain/dimension`, with counts and
/// a few example ids. Probes without a signature (the exact ones) drop out.
pub fn root_cause_breakdown(probes: &[ProbeLine]) -> Vec<RootCauseGroup> {
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
    groups
        .into_iter()
        .map(|(key, ids)| RootCauseGroup {
            count: ids.len(),
            examples: ids.into_iter().take(MAX_EXAMPLES).collect(),
            key,
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
        .map(|(builtin, (sites, na_total, ids))| DenseNaGroup {
            builtin,
            sites,
            na_total,
            probes: ids.len(),
            examples: ids.into_iter().take(MAX_EXAMPLES).collect(),
        })
        .collect()
}

/// Render the per-probe lines, the computed summary, and the two breakdowns
/// to the `[corpus]` log.
pub fn render(report: &HarnessReport) {
    for p in &report.probes {
        output::corpus_msg(&format_probe(p));
    }

    let summary = summarize(&report.probes);
    output::corpus_msg(&format_summary(&summary));

    let root = root_cause_breakdown(&report.probes);
    if !root.is_empty() {
        output::corpus_msg("root-cause breakdown (non-exact probes by domain/dimension):");
        for g in &root {
            output::corpus_msg(&format!(
                "  {}: {} (e.g. {})",
                g.key,
                g.count,
                g.examples.join(", ")
            ));
        }
    }

    let dense = dense_na_breakdown(&report.probes);
    if !dense.is_empty() {
        output::corpus_msg("dense-na breakdown (by builtin):");
        for g in &dense {
            output::corpus_msg(&format!(
                "  {}: {} site(s), {} na across {} probe(s) (e.g. {})",
                g.builtin,
                g.sites,
                g.na_total,
                g.probes,
                g.examples.join(", ")
            ));
        }
    }
}

fn format_probe(p: &ProbeLine) -> String {
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
    line
}

fn format_summary(s: &Summary) -> String {
    let mut out = format!(
        "summary: {} total, {} parity (exact={} near={} drift={}); \
         tiers byte_exact={} accepted={} actionable_drift={} count_divergent={}; \
         compile_fail={} runtime_fail={} no_tv_data={} no_overlap={}",
        s.total,
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
        s.no_tv_data,
        s.no_overlap,
    );
    if s.other > 0 {
        out.push_str(&format!(" other={}", s.other));
    }
    out
}

/// Assert at compile time that brokkr models every disposition label the
/// gate can pin: the four acceptance tiers plus the four non-parity
/// outcomes. If `registry` grows a label, the summary tally above must too.
const _: () = assert!(registry::DISPOSITION_LABELS.len() == 8);

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
        assert_eq!(r.probes[1].signature.as_ref().unwrap().dimension, "exit_price");
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
{"probe":"e","outcome":"no_tv_data"}
"#,
        );
        let s = summarize(&r.probes);
        assert_eq!((s.total, s.parity), (5, 3));
        assert_eq!((s.exact, s.near, s.drift), (1, 1, 1));
        assert_eq!(s.byte_exact, 1);
        assert_eq!(s.actionable_drift, 1);
        assert_eq!(s.count_divergent, 1);
        assert_eq!(s.compile_fail, 1);
        assert_eq!(s.no_tv_data, 1);
    }

    #[test]
    fn root_cause_groups_by_domain_dimension() {
        let r = parse(
            br#"{"probe":"a","outcome":"parity","signature":{"domain":"broker-fidelity","dimension":"exit_price"}}
{"probe":"b","outcome":"parity","signature":{"domain":"broker-fidelity","dimension":"exit_price"}}
{"probe":"c","outcome":"parity","count_tier":"exact"}
"#,
        );
        let groups = root_cause_breakdown(&r.probes);
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
    fn skips_trade_diff_and_unknown_kind_lines() {
        // A disposition line (no kind), a trade_diff drill-down record, and a
        // hypothetical future record kind. Only the disposition is aggregated;
        // the drill-down records live in stdout but never enter the report.
        let nd = br#"{"probe":"p1","outcome":"parity","count_tier":"drift","acceptance":{"tier":"actionable_drift"}}
{"kind":"trade_diff","probe":"p1","bar":42,"entry_id":"e1","exit_id":"x1","qty":1.0,"side":"long","ours_price":100.5,"tv_price":100.25,"price_delta":0.25}
{"kind":"trade_diff","probe":"p1","bar":58,"entry_id":"e2","exit_id":"x2","qty":2.0,"side":"short","ours_price":99.0,"tv_price":99.0,"price_delta":0.0}
{"kind":"future_record","probe":"p1","whatever":true}
"#;
        let r = parse(nd);
        assert_eq!(r.probes.len(), 1); // only the disposition line
        assert_eq!(r.probes[0].probe, "p1");
        let s = summarize(&r.probes);
        assert_eq!((s.total, s.actionable_drift), (1, 1)); // drill-down lines excluded
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
