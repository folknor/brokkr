//! Per-db `user_version` migrations for the corpus runs database.
//!
//! Mirrors `src/db/migrate.rs`. Version 1 was the initial schema; version 2
//! adds the per-probe `disposition.runtime_ms` column; version 3 adds the
//! `disposition.boundary_ours`/`boundary_tv` window-boundary-artifact discount
//! columns; version 4 adds `run.wall_ms`, brokkr's own measured whole-run
//! harness wall (the pre-run runtime ceiling estimates from these, not from
//! summing the harness's overlapping per-probe `runtime_ms`); version 5
//! rebuilds `disposition` around the stored harness record (`raw_json`, every
//! harness column generated from it - see `schema.rs`). On a fresh database
//! the schema DDL in `schema.rs` creates the current tables (columns
//! included) and stamps the version, so the migration steps below only run for
//! an older db on disk. The `has_table` helper lives here so a future column
//! add stays a localized change, exactly as in `ResultsDb`.

use rusqlite::params;
use serde_json::{Map, Value};

use super::schema::disposition_ddl;
use crate::error::DevError;

/// Current schema version. Increment when adding a migration below.
pub(super) const SCHEMA_VERSION: i64 = 5;

/// Run all pending migrations based on `PRAGMA user_version`. On a fresh
/// database the schema DDL in `schema.rs` creates the current tables and
/// stamps the version, so there is nothing to migrate. A store stamped newer
/// than this binary is refused: `CorpusDb::open` stamps the version it
/// knows, and stamping a newer store down would re-run that newer build's
/// steps the next time it opens the file.
///
/// Every pending step and the version stamp commit as one transaction, so a
/// failed upgrade leaves the store exactly as it was - which matters because
/// `open_readonly` migrates on the way to a read, and a refused read must not
/// have half-upgraded the file. The transaction is `IMMEDIATE`, taking the
/// write lock before reading anything, and the version is re-read under it:
/// `corpus-results` runs outside brokkr's global lock, so two processes can
/// race to migrate, and a deferred transaction that read before the other
/// committed would fail its first write with a stale snapshot. Foreign-key
/// enforcement is off for the duration: the steps move rows that already
/// exist, and must not refuse a store over a disposition whose run row is
/// gone (the bundled SQLite enforces the declarative FK by default). The
/// pragma is a no-op inside a transaction, so it brackets the BEGIN.
pub(super) fn run_migrations(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_table(conn, "run") {
        return Ok(());
    }

    let current = user_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(newer_store(current));
    }
    if current == SCHEMA_VERSION {
        return Ok(());
    }

    let fk: i64 = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", 0)?;
    // Whatever fails - BEGIN, a step, or COMMIT - roll back the transaction
    // this opened if it is still open (the FK pragma is a no-op inside one),
    // then restore enforcement, and report the first error.
    let migrated = match conn.execute_batch("BEGIN IMMEDIATE") {
        // A failed BEGIN opened nothing; any transaction open now is the
        // caller's, not ours to roll back.
        Err(e) => Err(DevError::from(e)),
        Ok(()) => {
            let done = user_version(conn)
                .and_then(|now| match now {
                    // Another process migrated while this one waited.
                    v if v >= SCHEMA_VERSION => Ok(()),
                    v => migrate_from(conn, v),
                })
                .and_then(|()| conn.execute_batch("COMMIT").map_err(DevError::from));
            if done.is_err() && !conn.is_autocommit() {
                conn.execute_batch("ROLLBACK").ok();
            }
            done
        }
    };
    let restored = conn.pragma_update(None, "foreign_keys", fk);
    migrated?;
    restored?;
    Ok(())
}

fn user_version(conn: &rusqlite::Connection) -> Result<i64, DevError> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

fn newer_store(version: i64) -> DevError {
    DevError::Database(format!(
        "corpus runs.db is schema v{version}, written by a newer brokkr than this one \
         (v{SCHEMA_VERSION}); install the newer brokkr, or move runs.db aside to start a \
         fresh store"
    ))
}

/// The steps from version `current` to [`SCHEMA_VERSION`], version stamp
/// included; [`run_migrations`] runs them inside its transaction.
fn migrate_from(conn: &rusqlite::Connection, current: i64) -> Result<(), DevError> {
    // v1 -> v2: add the per-probe wall-clock runtime column. IF NOT EXISTS-style
    // guard via `has_column` keeps the migration idempotent if rerun.
    if current < 2 && !has_column(conn, "disposition", "runtime_ms") {
        conn.execute("ALTER TABLE disposition ADD COLUMN runtime_ms REAL", [])?;
    }

    // v2 -> v3: add the window-boundary-artifact discount columns. NOT NULL is
    // safe under ALTER ADD because of the DEFAULT 0 - existing rows (raw counts,
    // no discount recorded) read back as zero, which is exactly "nothing
    // discounted". Idempotency guarded by `has_column` as above.
    if current < 3 {
        if !has_column(conn, "disposition", "boundary_ours") {
            conn.execute(
                "ALTER TABLE disposition ADD COLUMN boundary_ours INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !has_column(conn, "disposition", "boundary_tv") {
            conn.execute(
                "ALTER TABLE disposition ADD COLUMN boundary_tv INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
    }

    // v3 -> v4: add brokkr's measured whole-run harness wall. Nullable (pre-v4
    // runs never recorded it, and a spawn failure has no wall), so no DEFAULT -
    // an absent wall reads back as NULL, which the ceiling estimator treats as
    // "no measured wall for this run". Idempotency guarded by `has_column`.
    if current < 4 && !has_column(conn, "run", "wall_ms") {
        conn.execute("ALTER TABLE run ADD COLUMN wall_ms REAL", [])?;
    }

    // v4 -> v5: rebuild `disposition` around the stored record. A physical
    // column cannot become a generated one in place, so the rows move to a
    // staging table - each with a record rebuilt from exactly the columns it
    // kept - and are checked against the old columns before the swap.
    if current < 5 && has_table(conn, "disposition") && !has_column(conn, "disposition", "raw_json")
    {
        rebuild_disposition(conn)?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// The v4 `disposition` columns the v5 projections must reproduce.
const V4_HARNESS_COLUMNS: [&str; 20] = [
    "outcome",
    "matched",
    "ours_only",
    "tv_only",
    "boundary_ours",
    "boundary_tv",
    "count_tier",
    "acc_tier",
    "acc_profile",
    "acc_failing",
    "p90_entry",
    "p90_exit",
    "p90_pnl",
    "sig_domain",
    "sig_leg",
    "sig_dimension",
    "sig_detail",
    "sig_breaches",
    "error",
    "runtime_ms",
];

/// Move every v4 row into the v5 shape, refusing the swap if any projection
/// disagrees with the column it replaces (compared null-safely).
fn rebuild_disposition(conn: &rusqlite::Connection) -> Result<(), DevError> {
    conn.execute_batch(&disposition_ddl("disposition_v5"))?;
    {
        let mut read = conn.prepare(&format!(
            "SELECT run_id, probe, disposition, expected, gate_ok, {} FROM disposition",
            V4_HARNESS_COLUMNS.join(", ")
        ))?;
        let mut write = conn.prepare(
            "INSERT INTO disposition_v5 \
             (run_id, probe, disposition, expected, gate_ok, raw_source, raw_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'reconstructed', ?6)",
        )?;
        let mut rows = read.query([])?;
        while let Some(row) = rows.next()? {
            let run_id: i64 = row.get("run_id")?;
            let probe: String = row.get("probe")?;
            let raw = reconstruct(row)?;
            write.execute(params![
                run_id,
                probe,
                row.get::<_, String>("disposition")?,
                row.get::<_, Option<String>>("expected")?,
                row.get::<_, i64>("gate_ok")?,
                raw,
            ])?;
        }
    }
    let mismatch = V4_HARNESS_COLUMNS
        .iter()
        .map(|c| format!("o.{c} IS NOT n.{c}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let (old, new, bad): (i64, i64, i64) = conn.query_row(
        &format!(
            "SELECT (SELECT COUNT(*) FROM disposition), (SELECT COUNT(*) FROM disposition_v5), \
                    (SELECT COUNT(*) FROM disposition o JOIN disposition_v5 n \
                     USING (run_id, probe) WHERE {mismatch})"
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if old != new || bad != 0 {
        return Err(DevError::Database(format!(
            "corpus runs.db v5 migration: the rebuilt disposition table does not reproduce \
             the old one ({old} rows before, {new} after, {bad} differing); nothing was \
             changed. The store is run history, not a source of truth: move runs.db aside \
             to start a fresh one"
        )));
    }
    conn.execute_batch(
        "DROP TABLE disposition;
         ALTER TABLE disposition_v5 RENAME TO disposition;
         DROP INDEX IF EXISTS idx_disposition_v5_probe;",
    )?;
    Ok(())
}

/// The harness record a v4 row stands for, rebuilt from exactly its columns
/// in the harness's own shape. Nothing is invented: an `acceptance` object
/// only when a column shows one existed, a `signature` only when
/// `sig_breaches` (written whenever one did) is set, `p90` members only where
/// stored, and no `dense_na_sites` (those live in their own table, without
/// the fields the harness now sends). The counts and the boundary discount
/// are always present, as the v4 columns always held them.
fn reconstruct(row: &rusqlite::Row<'_>) -> Result<String, DevError> {
    let text = |c: &str| row.get::<_, Option<String>>(c);
    let real = |c: &str| row.get::<_, Option<f64>>(c);
    let mut rec = Map::new();
    rec.insert("probe".into(), Value::from(row.get::<_, String>("probe")?));
    rec.insert("outcome".into(), Value::from(row.get::<_, String>("outcome")?));
    for c in ["matched", "ours_only", "tv_only", "boundary_ours", "boundary_tv"] {
        rec.insert(c.into(), Value::from(row.get::<_, i64>(c)?));
    }
    if let Some(v) = text("count_tier")? {
        rec.insert("count_tier".into(), Value::from(v));
    }

    let failing_text: String = row.get("acc_failing")?;
    let failing: Value = serde_json::from_str(&failing_text).map_err(|e| {
        DevError::Database(format!("corpus runs.db v5 migration: acc_failing is not JSON: {e}"))
    })?;
    let (tier, profile) = (text("acc_tier")?, text("acc_profile")?);
    let p90: Vec<(&str, f64)> = [("entry", "p90_entry"), ("exit", "p90_exit"), ("pnl", "p90_pnl")]
        .into_iter()
        .filter_map(|(k, c)| real(c).transpose().map(|v| v.map(|v| (k, v))))
        .collect::<Result<_, _>>()?;
    let has_failing = failing.as_array().is_some_and(|a| !a.is_empty());
    if tier.is_some() || profile.is_some() || has_failing || !p90.is_empty() {
        let mut acc = Map::new();
        if let Some(t) = tier {
            acc.insert("tier".into(), Value::from(t));
        }
        if let Some(p) = profile {
            acc.insert("profile".into(), Value::from(p));
        }
        if has_failing {
            acc.insert("failing".into(), failing);
        }
        if !p90.is_empty() {
            let members: Map<String, Value> =
                p90.into_iter().map(|(k, v)| (k.to_owned(), Value::from(v))).collect();
            acc.insert("p90".into(), Value::Object(members));
        }
        rec.insert("acceptance".into(), Value::Object(acc));
    }

    if let Some(breaches) = row.get::<_, Option<i64>>("sig_breaches")? {
        let mut sig = Map::new();
        for (k, c) in [
            ("domain", "sig_domain"),
            ("leg", "sig_leg"),
            ("dimension", "sig_dimension"),
            ("detail", "sig_detail"),
        ] {
            if let Some(v) = text(c)? {
                sig.insert(k.into(), Value::from(v));
            }
        }
        sig.insert("dimension_breaches".into(), Value::from(breaches));
        rec.insert("signature".into(), Value::Object(sig));
    }
    if let Some(v) = text("error")? {
        rec.insert("error".into(), Value::from(v));
    }
    if let Some(v) = real("runtime_ms")? {
        rec.insert("runtime_ms".into(), Value::from(v));
    }
    Ok(Value::Object(rec).to_string())
}

/// Check whether a column exists on a table (via `PRAGMA table_xinfo`, which
/// unlike `table_info` lists generated columns too).
fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("PRAGMA table_xinfo({table})"))
        .and_then(|mut stmt| {
            let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;
            for name in names {
                if name? == column {
                    found = true;
                }
            }
            Ok(found)
        })
        .unwrap_or(false)
}

/// Check whether a table exists in the database.
fn has_table(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The v1 store as the first brokkr wrote it: the typed disposition table,
    /// before `runtime_ms` (v2) and the boundary columns (v3).
    const V1_SCHEMA: &str = "\
        CREATE TABLE run (run_id INTEGER PRIMARY KEY, started_at TEXT NOT NULL, \
            selector TEXT NOT NULL, gated INTEGER NOT NULL, result TEXT NOT NULL, \
            fail_reason TEXT, harness_exit_code INTEGER, probe_count INTEGER NOT NULL, \
            harness_stderr TEXT);
        CREATE TABLE disposition (run_id INTEGER NOT NULL, probe TEXT NOT NULL, \
            outcome TEXT NOT NULL, disposition TEXT NOT NULL, expected TEXT, \
            gate_ok INTEGER NOT NULL, matched INTEGER NOT NULL, ours_only INTEGER NOT NULL, \
            tv_only INTEGER NOT NULL, count_tier TEXT, acc_tier TEXT, acc_profile TEXT, \
            acc_failing TEXT NOT NULL DEFAULT '[]', p90_entry REAL, p90_exit REAL, \
            p90_pnl REAL, sig_domain TEXT, sig_leg TEXT, sig_dimension TEXT, sig_detail TEXT, \
            sig_breaches INTEGER, error TEXT, PRIMARY KEY (run_id, probe));
        CREATE INDEX idx_disposition_probe ON disposition(probe, run_id);";

    #[test]
    fn v1_to_current_runs_every_step_and_is_idempotent() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        assert!(!has_column(&conn, "disposition", "runtime_ms"));
        assert!(!has_column(&conn, "disposition", "boundary_ours"));

        run_migrations(&conn).unwrap();
        // Every step v1 -> v5 runs when starting from v1.
        assert!(has_column(&conn, "disposition", "raw_json"));
        assert!(has_column(&conn, "disposition", "runtime_ms"));
        assert!(has_column(&conn, "disposition", "boundary_ours"));
        assert!(has_column(&conn, "disposition", "ts_entry_share_pct"));
        assert!(has_column(&conn, "run", "wall_ms"));
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Rerunning is a no-op (the version guard short-circuits; has_column
        // would also prevent a duplicate ALTER).
        run_migrations(&conn).unwrap();
        assert!(has_column(&conn, "disposition", "boundary_ours"));
    }

    #[test]
    fn a_pre_boundary_row_still_reads_as_nothing_discounted() {
        // A v2 row predates the boundary columns; v3 gave it 0/0, and the v5
        // record rebuilt from it carries the same zeros.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute("ALTER TABLE disposition ADD COLUMN runtime_ms REAL", [])
            .unwrap();
        conn.execute(
            "INSERT INTO disposition (run_id, probe, outcome, disposition, gate_ok, matched, \
             ours_only, tv_only) VALUES (1, 'p1', 'parity', 'accepted', 1, 5, 1, 0)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();

        run_migrations(&conn).unwrap();
        let (bo, bt, src): (i64, i64, String) = conn
            .query_row(
                "SELECT boundary_ours, boundary_tv, raw_source FROM disposition \
                 WHERE probe = 'p1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((bo, bt, src.as_str()), (0, 0, "reconstructed"));
    }

    /// A v4 store: the v1 table plus the columns v2-v4 added.
    fn v4_store() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute_batch(
            "ALTER TABLE disposition ADD COLUMN runtime_ms REAL;
             ALTER TABLE disposition ADD COLUMN boundary_ours INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE disposition ADD COLUMN boundary_tv INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE run ADD COLUMN wall_ms REAL;",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
        conn
    }

    fn insert_v4(conn: &rusqlite::Connection, probe: &str, rest: &str) {
        conn.execute_batch(&format!(
            "INSERT INTO disposition (run_id, probe, disposition, expected, gate_ok, outcome, \
             matched, ours_only, tv_only, boundary_ours, boundary_tv, count_tier, acc_tier, \
             acc_profile, acc_failing, p90_entry, p90_exit, p90_pnl, sig_domain, sig_leg, \
             sig_dimension, sig_detail, sig_breaches, error, runtime_ms) \
             VALUES (1, '{probe}', {rest})"
        ))
        .unwrap();
    }

    #[test]
    fn v5_rebuild_reproduces_every_v4_shape() {
        let conn = v4_store();
        // A drifted parity row: full acceptance, partial p90, a signature.
        insert_v4(
            &conn,
            "drift",
            "'actionable_drift', 'actionable_drift', 1, 'parity', 218, 2, 1, 2, 0, 'drift', \
             'actionable_drift', 'production', '[\"exit_price\"]', NULL, 0.08, NULL, \
             'broker-fidelity', 'exit', 'exit_price', NULL, 3, NULL, 142.7",
        );
        // An exact row: a tier, no p90, a zero-breach signature.
        insert_v4(
            &conn,
            "exact",
            "'byte_exact', NULL, 1, 'parity', 10, 0, 0, 0, 0, 'exact', 'byte_exact', \
             'strict', '[]', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, 3.0",
        );
        // A failure: no acceptance, no signature, an error, no runtime.
        insert_v4(
            &conn,
            "broke",
            "'compile_fail', 'compile_fail', 1, 'compile_fail', 0, 0, 0, 0, 0, NULL, NULL, \
             NULL, '[]', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'E0001 boom', NULL",
        );

        run_migrations(&conn).unwrap();

        type Migrated = (String, String, Option<f64>, Option<i64>, String, Option<String>);
        let rows: Vec<Migrated> = conn
            .prepare(
                "SELECT probe, acc_failing, p90_exit, sig_breaches, raw_source, boundary_anchor \
                 FROM disposition ORDER BY probe",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 3);
        let by = |p: &str| rows.iter().find(|r| r.0 == p).unwrap();
        assert_eq!(by("drift").1, "[\"exit_price\"]");
        assert_eq!(by("drift").2, Some(0.08));
        assert_eq!(by("drift").3, Some(3));
        assert_eq!(by("exact").3, Some(0)); // a zero-breach signature survives
        assert_eq!(by("broke").1, "[]");
        assert_eq!(by("broke").3, None);
        for r in &rows {
            assert_eq!(r.4, "reconstructed");
            // A diagnostic the v4 schema never kept reads as not retained.
            assert_eq!(r.5, None);
        }
        // The staging table is gone, and so are both old indexes: the v4 one
        // went with the v4 table, the staging one is dropped by name, and
        // `CorpusDb::open` recreates `idx_disposition_probe` from the DDL.
        assert!(!has_table(&conn, "disposition_v5"));
        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' \
                 AND tbl_name = 'disposition' AND sql IS NOT NULL",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(indexes.is_empty(), "{indexes:?}");
    }

    #[test]
    fn v5_rebuild_refuses_a_row_it_cannot_reproduce_and_changes_nothing() {
        // Hand-edited JSON with spacing the projection normalizes away: the
        // rebuilt `acc_failing` would read differently, so the upgrade is
        // refused. Started from v1, it must not even keep the v2-v4 columns:
        // every step and the version stamp are one transaction.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO disposition (run_id, probe, outcome, disposition, gate_ok, matched, \
             ours_only, tv_only, acc_tier, acc_failing) VALUES (1, 'odd', 'parity', \
             'accepted', 1, 1, 0, 0, 'accepted', '[ \"entry_price\" ]')",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let err = run_migrations(&conn).unwrap_err();
        assert!(format!("{err:?}").contains("does not reproduce the old one"));
        assert!(!has_column(&conn, "disposition", "raw_json"));
        assert!(!has_column(&conn, "disposition", "runtime_ms"));
        assert!(!has_column(&conn, "run", "wall_ms"));
        assert!(!has_table(&conn, "disposition_v5"));
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        // Foreign-key enforcement is restored after the attempt.
        let fk: i64 = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn a_store_from_a_newer_brokkr_is_refused_not_stamped_down() {
        let conn = v4_store();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        let err = run_migrations(&conn).unwrap_err();
        assert!(format!("{err:?}").contains("written by a newer brokkr"));
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION + 1);
    }

    #[test]
    fn a_mistyped_value_projects_as_null_not_as_a_clamped_count() {
        // SQLite orders every integer before any text, so an ungated MIN
        // clamp would turn a string into i64::MAX.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(&super::super::schema::disposition_ddl("disposition"))
            .unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF; \
             INSERT INTO disposition (run_id, probe, disposition, gate_ok, raw_source, raw_json) \
             VALUES (1, 'odd', 'accepted', 1, 'harness', \
             '{\"probe\":\"odd\",\"outcome\":\"parity\",\"clipped_ours\":\"3\",\
             \"runtime_ms\":\"slow\",\"window_sensitive\":\"ta.cum\",\
             \"boundary_rules\":{\"start_anchor_consumed\":\"yes\"}}')",
        )
        .unwrap();
        let row: (Option<i64>, Option<f64>, Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT clipped_ours, runtime_ms, window_sensitive, anchor_consumed \
                 FROM disposition",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, (None, None, None, None));
    }

    #[test]
    fn a_count_past_i64_is_clamped_as_the_typed_schema_stored_it() {
        // The harness sends u64; the old ingest clamped to i64::MAX, and the
        // projection must too, or a typed read of the row fails.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(&super::super::schema::disposition_ddl("disposition"))
            .unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF; \
             INSERT INTO disposition (run_id, probe, disposition, gate_ok, raw_source, raw_json) \
             VALUES (1, 'big', 'accepted', 1, 'harness', \
             '{\"probe\":\"big\",\"outcome\":\"parity\",\"matched\":18446744073709551615,\
             \"signature\":{\"dimension_breaches\":18446744073709551615}}')",
        )
        .unwrap();
        let (matched, breaches): (i64, i64) = conn
            .query_row("SELECT matched, sig_breaches FROM disposition", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((matched, breaches), (i64::MAX, i64::MAX));
    }
}
