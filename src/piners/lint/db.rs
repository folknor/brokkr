//! `runs.db` - the piners lint corpus run store.
//!
//! Persists every `brokkr lint-corpus` run's per-probe agreement disposition
//! into the per-project SQLite database returned by
//! `piners::db_paths::lint_runs_db_path`. The
//! lint analogue of the trade-parity corpus store (`src/piners/corpus_db`):
//! once a run is in the DB its scratch state can be discarded and its data
//! stays queryable across runs.
//!
//! Deliberately mirrors the `corpus_db` patterns - shared WAL and schema
//! migration policy, single-transaction bulk insert, read-only open for
//! queries, `rusqlite::Error -> DevError` via `?`. Unlike `corpus_db`
//! (a 6-file module) this is a single self-contained file: the lint schema is
//! two small tables and the rendering is two aligned text grids. The store is
//! append-only - FK `REFERENCES` clauses are declarative only, with no cascade.

use std::path::Path;

use rusqlite::params;

use crate::db::store::as_i64;
use crate::error::DevError;
use crate::piners::lint::diff::Signature;
use crate::piners::lint::{PinnableDisposition, ProbeResult};
use crate::piners::run_model::RunVerdict;
use crate::piners::table::grid;

/// Current schema version. Increment when adding a migration in [`run_migrations`].
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS run (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at  TEXT NOT NULL,
    selector    TEXT NOT NULL,
    gated       INTEGER NOT NULL,
    result      TEXT NOT NULL,
    fail_reason TEXT,
    probe_count INTEGER NOT NULL,
    stderr      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_run_started ON run(started_at);

CREATE TABLE IF NOT EXISTS disposition (
    run_id         INTEGER NOT NULL REFERENCES run(id),
    probe          TEXT NOT NULL,
    disposition    TEXT NOT NULL,
    signature      TEXT,
    expected       TEXT,
    gate_ok        INTEGER NOT NULL,
    piners_count   INTEGER NOT NULL,
    lint_count     INTEGER NOT NULL,
    error          TEXT,
    tv_anchored_at TEXT,
    tv_divergent   INTEGER,
    PRIMARY KEY (run_id, probe)
);
CREATE INDEX IF NOT EXISTS idx_disposition_probe ON disposition(probe, run_id);
";

/// Handle to the lint corpus runs database.
pub struct LintDb {
    conn: rusqlite::Connection,
}

/// The run-envelope fields the caller supplies (the per-probe rows ride in
/// separately as `&[ProbeResult]`). `started_at` is passed in - the store
/// never reads a clock, so a caller can record a run at a pinned time.
pub struct RunMeta<'a> {
    /// RFC3339 timestamp the caller minted for this run.
    pub started_at: &'a str,
    /// JSON describing what was selected (resolved ids + raw flags).
    pub selector: &'a str,
    /// Was the per-probe gate enforced?
    pub gated: bool,
    /// The shared result and reason for the run.
    pub verdict: &'a RunVerdict,
    /// Number of probes in the run.
    pub probe_count: usize,
    /// Captured validator stderr (or a spawn-error message).
    pub stderr: &'a str,
}

/// Map an `Option<bool>` to the `0/1/NULL` SQLite tri-state.
fn tri(v: Option<bool>) -> Option<i64> {
    v.map(i64::from)
}

impl LintDb {
    /// Open (or create) the database read-write at `path`, creating the parent
    /// directory, enabling WAL, running migrations, and applying the schema.
    pub fn open(path: &Path) -> Result<Self, DevError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = crate::db::store::open_read_write(path, "piners lint runs", SCHEMA_VERSION)?;
        run_migrations(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Open the database read-only for queries.
    pub fn open_readonly(path: &Path) -> Result<Self, DevError> {
        let conn = crate::db::store::open_read_only_migrating(
            path,
            "piners lint runs",
            SCHEMA_VERSION,
            || {
                drop(Self::open(path)?);
                Ok(())
            },
        )?;
        Ok(Self { conn })
    }

    /// Persist one run and all its per-probe rows in a single transaction.
    /// Returns the new run `id`.
    pub fn record_run(
        &mut self,
        meta: &RunMeta<'_>,
        probes: &[ProbeResult],
    ) -> Result<i64, DevError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO run \
             (started_at, selector, gated, result, fail_reason, probe_count, stderr) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                meta.started_at,
                meta.selector,
                i64::from(meta.gated),
                meta.verdict.result(),
                meta.verdict.reason(),
                as_i64(meta.probe_count),
                meta.stderr,
            ],
        )?;
        let run_id = tx.last_insert_rowid();

        for p in probes {
            tx.execute(
                "INSERT INTO disposition \
                 (run_id, probe, disposition, signature, expected, gate_ok, \
                  piners_count, lint_count, error, tv_anchored_at, tv_divergent) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    run_id,
                    p.probe,
                    p.disposition.as_str(),
                    p.disposition.signature().map(Signature::as_str),
                    p.expected.map(PinnableDisposition::as_str),
                    i64::from(p.gate_ok),
                    as_i64(p.piners_count),
                    as_i64(p.lint_count),
                    p.error,
                    p.tv_anchored_at,
                    tri(p.tv_divergent),
                ],
            )?;
        }

        tx.commit()?;
        Ok(run_id)
    }

    /// The newest run `id`, or `None` if the DB has no runs.
    pub fn latest_run_id(&self) -> Result<Option<i64>, DevError> {
        let id = self.conn.query_row("SELECT MAX(id) FROM run", [], |r| {
            r.get::<_, Option<i64>>(0)
        })?;
        Ok(id)
    }

    /// The most recent `limit` runs, newest first, as an aligned table.
    pub fn recent_runs(&self, limit: usize) -> Result<String, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started_at, result, gated, probe_count, fail_reason, selector \
             FROM run ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([as_i64(limit)], |r| {
            Ok(vec![
                r.get::<_, i64>("id")?.to_string(),
                r.get::<_, String>("started_at")?,
                r.get::<_, String>("result")?,
                if r.get::<_, i64>("gated")? != 0 {
                    "yes".to_owned()
                } else {
                    "no".to_owned()
                },
                r.get::<_, i64>("probe_count")?.to_string(),
                r.get::<_, Option<String>>("fail_reason")?
                    .unwrap_or_default(),
                crate::piners::selector::compact(&r.get::<_, String>("selector")?),
            ])
        })?;
        let cells = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(grid(
            &[
                "run",
                "started_at",
                "result",
                "gated",
                "probes",
                "reason",
                "selector",
            ],
            &cells,
        ))
    }

    /// The per-probe disposition table for a run. When `!full`, only the
    /// gate-deviating rows (`gate_ok = 0`) are shown, with a trailing line
    /// summarizing how many pin-matching rows were hidden.
    pub fn run_dispositions(&self, run_id: i64, full: bool) -> Result<String, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT probe, disposition, signature, expected, gate_ok, \
                    piners_count, lint_count, error, tv_anchored_at, tv_divergent \
             FROM disposition WHERE run_id = ?1 ORDER BY probe",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(DispRow {
                probe: r.get("probe")?,
                disposition: r.get("disposition")?,
                signature: r.get("signature")?,
                expected: r.get("expected")?,
                gate_ok: r.get::<_, i64>("gate_ok")? != 0,
                piners_count: r.get("piners_count")?,
                lint_count: r.get("lint_count")?,
                error: r.get("error")?,
                tv_anchored_at: r.get("tv_anchored_at")?,
                tv_divergent: r.get::<_, Option<i64>>("tv_divergent")?.map(|v| v != 0),
            })
        })?;
        let all = rows.collect::<Result<Vec<_>, _>>()?;

        let (shown, hidden): (Vec<&DispRow>, usize) = if full {
            (all.iter().collect(), 0)
        } else {
            let shown: Vec<&DispRow> = all.iter().filter(|d| !d.gate_ok).collect();
            let hidden = all.len() - shown.len();
            (shown, hidden)
        };

        let cells: Vec<Vec<String>> = shown.iter().map(|d| d.cells()).collect();
        let mut out = grid(
            &[
                "probe",
                "disposition",
                "signature",
                "expected",
                "gate",
                "piners",
                "lint",
                "tv_anchored",
                "tv_div",
                "error",
            ],
            &cells,
        );
        if hidden > 0 {
            out.push_str(&format!(
                "\n{} matching their pin (hidden)",
                crate::output::count(hidden, "probe")
            ));
        }
        Ok(out)
    }
}

/// One per-probe disposition row, the rendered subset loaded by
/// [`LintDb::run_dispositions`].
struct DispRow {
    probe: String,
    disposition: String,
    signature: Option<String>,
    expected: Option<String>,
    gate_ok: bool,
    piners_count: i64,
    lint_count: i64,
    error: Option<String>,
    tv_anchored_at: Option<String>,
    tv_divergent: Option<bool>,
}

impl DispRow {
    fn cells(&self) -> Vec<String> {
        let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".to_owned());
        let tv_div = match self.tv_divergent {
            Some(true) => "yes".to_owned(),
            Some(false) => "no".to_owned(),
            None => "-".to_owned(),
        };
        vec![
            self.probe.clone(),
            self.disposition.clone(),
            dash(&self.signature),
            dash(&self.expected),
            if self.gate_ok {
                "ok".to_owned()
            } else {
                "DEVIATES".to_owned()
            },
            self.piners_count.to_string(),
            self.lint_count.to_string(),
            dash(&self.tv_anchored_at),
            tv_div,
            self.error.clone().unwrap_or_default(),
        ]
    }
}

/// Run the initial schema through the shared transaction and version policy.
/// Future schema changes add version-gated steps here.
fn run_migrations(conn: &rusqlite::Connection) -> Result<(), DevError> {
    crate::db::store::migrate(
        conn,
        "piners lint runs",
        SCHEMA_VERSION,
        |conn, _current| {
            // There are no post-v1 steps yet. Keep the version gate because
            // it stamps fresh stores and gives future schema changes one
            // place to migrate existing histories before applying the DDL.
            conn.execute_batch(SCHEMA)?;
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::lint::diff::{LintDisposition, Validator};

    fn probe(name: &str, disposition: &str, gate_ok: bool) -> ProbeResult {
        let disposition = match disposition {
            "agree_clean" => LintDisposition::AgreeClean,
            "agree_flagged" => LintDisposition::AgreeFlagged,
            "divergent" => LintDisposition::Divergent(Signature::PinersOnly),
            "piners_error" => LintDisposition::ToolError(Validator::Piners),
            "lint_error" => LintDisposition::ToolError(Validator::PineLint),
            _ => panic!("unknown test disposition"),
        };
        ProbeResult {
            probe: name.to_owned(),
            disposition,
            expected: disposition.pinnable(),
            gate_ok,
            piners_count: 2,
            lint_count: 2,
            error: None,
            tv_anchored_at: Some("2026-06-01T00:00:00Z".to_owned()),
            tv_divergent: Some(false),
        }
    }

    #[test]
    fn record_run_roundtrips_and_renders() {
        let dir = crate::test_scratch::scratch("piners_lint_db", "record_run_roundtrips");
        let path = dir.join("runs.db");

        let mut db = LintDb::open(&path).unwrap();
        let probes = vec![
            probe("agree_one", "agree_clean", true),
            probe("agree_two", "agree_flagged", true),
            probe("diverge_one", "divergent", false),
        ];
        let meta = RunMeta {
            // Fixed timestamp - the store never reads a clock.
            started_at: "2026-06-22T00:00:00Z",
            selector: r#"{"keywords":["bracket"]}"#,
            gated: true,
            verdict: &RunVerdict::Fail("1 gate deviation(s)".to_owned()),
            probe_count: probes.len(),
            stderr: "validator stderr captured",
        };

        let run_id = db.record_run(&meta, &probes).unwrap();
        assert_eq!(db.latest_run_id().unwrap(), Some(run_id));

        let runs = db.recent_runs(10).unwrap();
        assert!(runs.contains(&run_id.to_string()), "{runs}");
        assert!(runs.contains("2026-06-22T00:00:00Z"), "{runs}");
        assert!(runs.contains("fail"), "{runs}");
        assert!(runs.contains("kw=bracket"), "{runs}");
        let stored_stderr: String = db
            .conn
            .query_row("SELECT stderr FROM run WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored_stderr, "validator stderr captured");

        // Full view shows every probe.
        let full = db.run_dispositions(run_id, true).unwrap();
        assert!(full.contains("agree_one"));
        assert!(full.contains("diverge_one"));

        // Gated view hides the two pin-matching probes and notes the count.
        let gated = db.run_dispositions(run_id, false).unwrap();
        assert!(gated.contains("diverge_one"));
        assert!(!gated.contains("agree_one"));
        assert!(gated.contains("2 probes matching their pin (hidden)"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
