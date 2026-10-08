//! Bulk-insert a parsed harness run into the corpus database.
//!
//! One transaction per run (BEGIN -> insert `run` envelope ->
//! `last_insert_rowid()` -> bulk-insert disposition / dense_na_site /
//! trade_diff / gate_miss children -> COMMIT), mirroring
//! `src/db/write.rs::insert_inner`. `expected`/`gate_ok` come from the
//! registry's pinned expectations and the gate's own verdict
//! (`gate::evaluate`), never a second computation of it; `gate_miss` rows
//! come from the gate violations the harness emitted no disposition line for.
//! The caller collapses repeated harness records first
//! (`HarnessReport::take_duplicates`); an insert failure still names the
//! probe it was storing.

use std::collections::BTreeMap;

use rusqlite::params;

use super::CorpusDb;
use crate::error::DevError;
use crate::piners::gate::GateDiff;
use crate::piners::report::{HarnessReport, ProbeLine, TradeDiffLine};

/// The run-envelope fields that aren't derivable from the parsed report.
pub struct RunRecord<'a> {
    /// The id the run was allocated under the lock (its artefact dir is
    /// `run-<id>`), so the dir and the row carry one number. A plain INSERT:
    /// a collision fails the ingest rather than replacing a row. `None` lets
    /// SQLite pick (tests only).
    pub run_id: Option<i64>,
    /// When the run started (`YYYY-MM-DD HH:MM:SS`, UTC), captured before the
    /// build; `None` stamps ingest time (tests only).
    pub started_at: Option<&'a str>,
    /// Full `HEAD` hash of the project checkout at run start, `None` when it
    /// could not be read (not a git repo, git failed).
    pub commit_sha: Option<&'a str>,
    /// Uncommitted changes outside `.brokkr/` at run start (see
    /// `crate::piners::cmd::RunStart`), `None` when unknown.
    pub dirty: Option<bool>,
    /// JSON describing what was selected (resolved ids + raw flags).
    pub selector: &'a str,
    /// Was the per-probe gate enforced (`!--no-gate`)?
    pub gated: bool,
    /// `"pass"` or `"fail"`.
    pub result: &'a str,
    /// One-line failure classification (`None` on pass).
    pub fail_reason: Option<&'a str>,
    /// Harness process exit code (`None` if killed by signal / never spawned).
    pub harness_exit_code: Option<i32>,
    /// Captured harness stderr (or a spawn-error message).
    pub stderr: &'a str,
    /// brokkr's measured whole-run harness wall, in milliseconds. `None` when
    /// the harness never ran (spawn failure). This is the quantity the pre-run
    /// runtime ceiling estimates from - a real wall, not the sum of the
    /// harness's overlapping per-probe `runtime_ms`.
    pub wall_ms: Option<f64>,
}

impl CorpusDb {
    /// Persist one run and all its child rows in a single transaction.
    /// Returns the new `run_id`.
    pub fn record_run(
        &self,
        run: &RunRecord<'_>,
        report: &HarnessReport,
        expected: &BTreeMap<String, Option<String>>,
        gate_diffs: &[GateDiff],
    ) -> Result<i64, DevError> {
        self.conn().execute("BEGIN", [])?;
        let result = record_inner(self.conn(), run, report, expected, gate_diffs);
        match result {
            Ok(run_id) => {
                self.conn().execute("COMMIT", [])?;
                Ok(run_id)
            }
            Err(e) => {
                self.conn().execute("ROLLBACK", []).ok();
                Err(e)
            }
        }
    }
}

/// SQLite stores signed i64; harness counts are `u64`/`usize`. Clamp rather
/// than wrap - these are trade/probe counts that never approach `i64::MAX`.
fn as_i64<T: TryInto<i64>>(v: T) -> i64 {
    v.try_into().unwrap_or(i64::MAX)
}

fn record_inner(
    conn: &rusqlite::Connection,
    run: &RunRecord<'_>,
    report: &HarnessReport,
    expected: &BTreeMap<String, Option<String>>,
    gate_diffs: &[GateDiff],
) -> Result<i64, DevError> {
    conn.execute(
        "INSERT INTO run \
         (run_id, started_at, selector, gated, result, fail_reason, harness_exit_code, \
          probe_count, harness_stderr, wall_ms, commit_sha, dirty) \
         VALUES (?9, COALESCE(?10, datetime('now')), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?11, ?12)",
        params![
            run.selector,
            i64::from(run.gated),
            run.result,
            run.fail_reason,
            run.harness_exit_code,
            as_i64(report.probes.len()),
            run.stderr,
            run.wall_ms,
            run.run_id,
            run.started_at,
            run.commit_sha,
            run.dirty.map(i64::from),
        ],
    )?;
    let run_id = conn.last_insert_rowid();

    // The stored verdict is the gate's own, not a recomputation: a probe is
    // `gate_ok` unless `gate::evaluate` flagged it. The two used to diverge on
    // a line for a probe outside the selection, which the gate ignores but a
    // recomputation against the (selection-only) expected map stored as
    // DEVIATES - so a passing run read as failing in `corpus-results`.
    let deviating: std::collections::HashSet<&str> =
        gate_diffs.iter().map(|d| d.probe.as_str()).collect();

    for p in &report.probes {
        let gate_ok = !deviating.contains(p.probe.as_str());
        insert_disposition(conn, run_id, p, expected, gate_ok).map_err(|e| {
            DevError::Database(format!("storing the disposition of probe '{}': {e}", p.probe))
        })?;
        for site in &p.dense_na_sites {
            conn.execute(
                "INSERT INTO dense_na_site (run_id, probe, name, call_site, na_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run_id, p.probe, site.name, site.call_site, as_i64(site.na_count)],
            )?;
        }
    }

    for t in &report.trade_diffs {
        insert_trade_diff(conn, run_id, t).map_err(|e| {
            DevError::Database(format!(
                "storing trade_diff our_index={} tv_index={} of probe '{}': {e}",
                t.our_index, t.tv_index, t.probe
            ))
        })?;
    }

    // Gate violations the harness emitted NO disposition line for (a selected
    // probe that produced nothing, or a never-blessed + never-emitted probe).
    // The deviation cases that DO have a disposition row are already captured
    // there via gate_ok=0; this table preserves only the no-row case.
    for d in gate_diffs {
        if d.actual.is_none() {
            conn.execute(
                "INSERT OR IGNORE INTO gate_miss (run_id, probe, expected, actual) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![run_id, d.probe, d.expected, d.actual],
            )?;
        }
    }

    Ok(run_id)
}

/// Store one disposition line: the harness record whole, plus brokkr's own
/// annotations. Every harness column is generated from `raw_json` (see
/// `schema.rs`), so nothing here restates a harness field.
fn insert_disposition(
    conn: &rusqlite::Connection,
    run_id: i64,
    p: &ProbeLine,
    expected: &BTreeMap<String, Option<String>>,
    gate_ok: bool,
) -> Result<(), DevError> {
    if p.raw.is_empty() {
        // Only `report::parse` builds lines for ingest, and it always sets the
        // record; an empty one would store a row every projection reads NULL.
        return Err(DevError::Database(format!(
            "internal: probe '{}' reached the run store without its harness record",
            p.probe
        )));
    }
    conn.execute(
        "INSERT INTO disposition \
         (run_id, probe, disposition, expected, gate_ok, raw_source, raw_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'harness', ?6)",
        params![
            run_id,
            p.probe,
            p.disposition(),
            expected.get(&p.probe).cloned().flatten(),
            i64::from(gate_ok),
            p.raw,
        ],
    )?;
    Ok(())
}

fn insert_trade_diff(
    conn: &rusqlite::Connection,
    run_id: i64,
    t: &TradeDiffLine,
) -> Result<(), DevError> {
    conn.execute(
        "INSERT INTO trade_diff \
         (run_id, probe, our_index, tv_index, our_entry_ts, our_exit_ts, our_entry_price, \
          our_exit_price, our_qty, our_pnl, entry_ts_delta, exit_ts_delta, entry_price_delta, \
          exit_price_delta, our_entry_bar, our_exit_bar, our_side, our_entry_id, our_exit_id, \
          tv_entry_ts, tv_exit_ts, tv_entry_price, tv_exit_price, tv_entry_qty, tv_pnl, \
          tv_entry_signal, tv_exit_signal) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
                 ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)",
        params![
            run_id,
            t.probe,
            t.our_index,
            t.tv_index,
            t.our_entry_ts,
            t.our_exit_ts,
            t.our_entry_price,
            t.our_exit_price,
            t.our_qty,
            t.our_pnl,
            t.entry_ts_delta,
            t.exit_ts_delta,
            t.entry_price_delta,
            t.exit_price_delta,
            t.our_entry_bar,
            t.our_exit_bar,
            t.our_side,
            t.our_entry_id,
            t.our_exit_id,
            t.tv_entry_ts,
            t.tv_exit_ts,
            t.tv_entry_price,
            t.tv_exit_price,
            t.tv_entry_qty,
            t.tv_pnl,
            t.tv_entry_signal,
            t.tv_exit_signal,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
    use super::*;
    use crate::piners::gate::GateDiff;
    use crate::piners::report::parse;

    fn expected_map(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(p, e)| ((*p).to_owned(), e.map(str::to_owned)))
            .collect()
    }

    #[test]
    fn record_and_query_roundtrip() {
        let nd = br#"{"probe":"p1","outcome":"parity","matched":218,"ours_only":2,"tv_only":1,"boundary_ours":2,"boundary_tv":0,"count_tier":"drift","acceptance":{"tier":"actionable_drift","profile":"production","failing":["exit_price"],"p90":{"exit":0.08}},"signature":{"domain":"broker-fidelity","leg":"exit","dimension":"exit_price","dimension_breaches":3},"dense_na_sites":[{"name":"strategy.exit","call_site":"s.pine:12","na_count":7}],"runtime_ms":142.7}
{"kind":"trade_diff","probe":"p1","our_index":1,"tv_index":1,"exit_price_delta":0.08,"our_entry_ts":1745295300,"our_exit_ts":1745295300,"our_entry_price":1582.6,"our_exit_price":1582.14,"our_qty":1.0,"our_pnl":-0.46,"our_side":"Long","tv_pnl":-0.54}
{"kind":"trade_diff","probe":"p1","our_index":2,"tv_index":2,"our_entry_ts":1,"our_exit_ts":2,"our_entry_price":9.0,"our_exit_price":10.0,"our_qty":1.0,"our_pnl":1.0}
"#;
        let report = parse(nd);
        let expected = expected_map(&[("p1", Some("actionable_drift")), ("p2", Some("accepted"))]);
        // p2 was selected but emitted no line -> a gate miss.
        let gate_diffs = vec![GateDiff {
            probe: "p2".to_owned(),
            expected: Some("accepted".to_owned()),
            actual: None,
        }];

        let db = CorpusDb::open_in_memory().unwrap();
        let run = RunRecord {
            run_id: None,
            started_at: None,
            commit_sha: None,
            dirty: None,
            selector: r#"{"keywords":["magnifier"]}"#,
            gated: true,
            result: "fail",
            fail_reason: Some("1 gate deviation(s)"),
            harness_exit_code: Some(0),
            stderr: "",
            wall_ms: Some(1234.0),
        };
        let run_id = db.record_run(&run, &report, &expected, &gate_diffs).unwrap();
        assert_eq!(run_id, 1);

        // Run envelope.
        let runs = db.recent_runs(10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].probe_count, 1);
        assert_eq!(runs[0].result, "fail");

        // Disposition: gate_ok true (actionable_drift == expected), p90 stored.
        let disp = db.disposition_for_probe(run_id, "p1").unwrap().unwrap();
        assert_eq!(disp.disposition, "actionable_drift");
        assert!(disp.gate_ok);
        assert_eq!(disp.p90_exit, Some(0.08));
        // Raw counts persist verbatim; the boundary discount rides alongside.
        assert_eq!((disp.ours_only, disp.tv_only), (2, 1));
        assert_eq!((disp.boundary_ours, disp.boundary_tv), (2, 0));

        // runtime_ms is projected from the stored record.
        let rt = db
            .raw_sql("SELECT runtime_ms, raw_source FROM disposition WHERE probe = 'p1'")
            .unwrap();
        assert_eq!(rt.rows[0], vec!["142.7".to_owned(), "harness".to_owned()]);

        // Both trade_diff rows persisted; the second has NULL tv_pnl.
        let diffs = db.trade_diffs_for_probe(run_id, "p1").unwrap();
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].tv_pnl, Some(-0.54));
        assert_eq!(diffs[1].tv_pnl, None);

        // Gate miss for the unemitted probe.
        let misses = db.gate_misses_for_run(run_id).unwrap();
        assert_eq!(misses.len(), 1);
        assert_eq!(misses[0].probe, "p2");

        // Trend over runs.
        let trend = db.trend_for_probe("p1", 5).unwrap();
        assert_eq!(trend.len(), 1);
        assert_eq!(trend[0].disposition, "actionable_drift");
    }

    #[test]
    fn a_field_brokkr_never_modelled_is_stored_and_projected() {
        // The harness adds diagnostics brokkr has no struct field for; the
        // record keeps them, and the named ones read back as columns.
        let report = parse(
            br#"{"probe":"p1","outcome":"parity","matched":4,"acceptance":{"tier":"accepted"},"boundary_anchor":"carry_tv","boundary_rules":{"start_ours":0,"tail_ours":0,"start_tv":2,"end_tv":0,"start_anchor_consumed":true},"ts_shift":{"entry_considered":4,"entry_shifted":2,"entry_share_pct":50.0,"exit_considered":0,"exit_shifted":0},"clipped_tv":3,"future_field":{"x":1}}
"#,
        );
        let db = CorpusDb::open_in_memory().unwrap();
        let run = RunRecord {
            run_id: None,
            started_at: None,
            commit_sha: None,
            dirty: None,
            selector: "{}",
            gated: true,
            result: "pass",
            fail_reason: None,
            harness_exit_code: Some(0),
            stderr: "",
            wall_ms: None,
        };
        db.record_run(&run, &report, &expected_map(&[("p1", Some("accepted"))]), &[])
            .unwrap();
        let t = db
            .raw_sql(
                "SELECT boundary_anchor, anchor_consumed, rule_start_tv, ts_entry_share_pct, \
                        ts_exit_considered, ts_exit_share_pct, clipped_tv, \
                        json_extract(raw_json, '$.future_field.x') FROM disposition",
            )
            .unwrap();
        // An exit census with nothing comparable has no share: NULL, not 0.
        assert_eq!(t.rows[0], ["carry_tv", "1", "2", "50", "0", "", "3", "1"]);
    }

    #[test]
    fn stored_gate_verdict_is_the_gates_own() {
        // `stray` was not selected: the gate ignores its line, so the store
        // must not mark it DEVIATES. `p1` deviates per the gate.
        let report = parse(
            br#"{"probe":"p1","outcome":"parity","acceptance":{"tier":"accepted"}}
{"probe":"stray","outcome":"parity","acceptance":{"tier":"accepted"}}
"#,
        );
        let expected = expected_map(&[("p1", Some("byte_exact"))]);
        let gate_diffs = vec![GateDiff {
            probe: "p1".to_owned(),
            expected: Some("byte_exact".to_owned()),
            actual: Some("accepted".to_owned()),
        }];
        let db = CorpusDb::open_in_memory().unwrap();
        let run = RunRecord {
            run_id: None,
            started_at: None,
            commit_sha: None,
            dirty: None,
            selector: "{}",
            gated: true,
            result: "fail",
            fail_reason: Some("1 gate deviation(s)"),
            harness_exit_code: Some(0),
            stderr: "",
            wall_ms: None,
        };
        let run_id = db.record_run(&run, &report, &expected, &gate_diffs).unwrap();
        assert!(!db.disposition_for_probe(run_id, "p1").unwrap().unwrap().gate_ok);
        let stray = db.disposition_for_probe(run_id, "stray").unwrap().unwrap();
        assert!(stray.gate_ok);
        assert_eq!(stray.expected, None);
    }
}
