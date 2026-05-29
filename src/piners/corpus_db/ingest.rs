//! Bulk-insert a parsed harness run into the corpus database.
//!
//! One transaction per run (BEGIN -> insert `run` envelope ->
//! `last_insert_rowid()` -> bulk-insert disposition / dense_na_site /
//! trade_diff / gate_miss children -> COMMIT), mirroring
//! `src/db/write.rs::insert_inner`. `expected`/`gate_ok` come from the
//! registry's pinned expectations; `gate_miss` rows come from the gate
//! violations the harness emitted no disposition line for.

use std::collections::BTreeMap;

use rusqlite::params;

use super::CorpusDb;
use crate::error::DevError;
use crate::piners::gate::GateDiff;
use crate::piners::report::{HarnessReport, ProbeLine, TradeDiffLine};

/// The run-envelope fields that aren't derivable from the parsed report.
pub struct RunRecord<'a> {
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

/// Map an empty harness string to `NULL`, a non-empty one to `Some`.
fn ne(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
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
         (started_at, selector, gated, result, fail_reason, harness_exit_code, \
          probe_count, harness_stderr) \
         VALUES (datetime('now'), ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            run.selector,
            i64::from(run.gated),
            run.result,
            run.fail_reason,
            run.harness_exit_code,
            as_i64(report.probes.len()),
            run.stderr,
        ],
    )?;
    let run_id = conn.last_insert_rowid();

    for p in &report.probes {
        insert_disposition(conn, run_id, p, expected)?;
        for site in &p.dense_na_sites {
            conn.execute(
                "INSERT INTO dense_na_site (run_id, probe, name, call_site, na_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run_id, p.probe, site.name, site.call_site, as_i64(site.na_count)],
            )?;
        }
    }

    for t in &report.trade_diffs {
        insert_trade_diff(conn, run_id, t)?;
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

fn insert_disposition(
    conn: &rusqlite::Connection,
    run_id: i64,
    p: &ProbeLine,
    expected: &BTreeMap<String, Option<String>>,
) -> Result<(), DevError> {
    let disposition = p.disposition();
    // `None` expected (never blessed) is never satisfied - matches gate::evaluate.
    let expected_label = expected.get(&p.probe).cloned().flatten();
    let gate_ok = matches!(&expected_label, Some(e) if *e == disposition);

    let (acc_tier, acc_profile, acc_failing, p90_entry, p90_exit, p90_pnl) = match &p.acceptance {
        Some(a) => {
            let failing = serde_json::to_string(&a.failing).unwrap_or_else(|_| "[]".to_owned());
            let (entry, exit, pnl) = a
                .p90
                .as_ref()
                .map_or((None, None, None), |p| (p.entry, p.exit, p.pnl));
            (ne(&a.tier), ne(&a.profile), failing, entry, exit, pnl)
        }
        None => (None, None, "[]".to_owned(), None, None, None),
    };

    let (sig_domain, sig_leg, sig_dimension, sig_detail, sig_breaches) = match &p.signature {
        Some(s) => (
            ne(&s.domain),
            ne(&s.leg),
            ne(&s.dimension),
            ne(&s.detail),
            Some(as_i64(s.dimension_breaches)),
        ),
        None => (None, None, None, None, None),
    };

    conn.execute(
        "INSERT INTO disposition \
         (run_id, probe, outcome, disposition, expected, gate_ok, matched, ours_only, tv_only, \
          count_tier, acc_tier, acc_profile, acc_failing, p90_entry, p90_exit, p90_pnl, \
          sig_domain, sig_leg, sig_dimension, sig_detail, sig_breaches, error, runtime_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
        params![
            run_id,
            p.probe,
            p.outcome,
            disposition,
            expected_label,
            i64::from(gate_ok),
            as_i64(p.matched),
            as_i64(p.ours_only),
            as_i64(p.tv_only),
            p.count_tier,
            acc_tier,
            acc_profile,
            acc_failing,
            p90_entry,
            p90_exit,
            p90_pnl,
            sig_domain,
            sig_leg,
            sig_dimension,
            sig_detail,
            sig_breaches,
            p.error,
            p.runtime_ms,
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
        let nd = br#"{"probe":"p1","outcome":"parity","matched":218,"ours_only":0,"tv_only":1,"count_tier":"drift","acceptance":{"tier":"actionable_drift","profile":"production","failing":["exit_price"],"p90":{"exit":0.08}},"signature":{"domain":"broker-fidelity","leg":"exit","dimension":"exit_price","dimension_breaches":3},"dense_na_sites":[{"name":"strategy.exit","call_site":"s.pine:12","na_count":7}],"runtime_ms":142.7}
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
            selector: r#"{"keywords":["magnifier"]}"#,
            gated: true,
            result: "fail",
            fail_reason: Some("1 gate deviation(s)"),
            harness_exit_code: Some(0),
            stderr: "",
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
        assert_eq!(disp.tv_only, 1);

        // runtime_ms is stored (store-only for now: reachable via raw SQL, not
        // yet on any canned query row).
        let rt = db
            .raw_sql("SELECT runtime_ms FROM disposition WHERE probe = 'p1'")
            .unwrap();
        assert_eq!(rt.rows[0][0], "142.7");

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
}
