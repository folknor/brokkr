//! Insert operations for the results database.

use super::ResultsDb;
use super::like::{ESCAPE, prefix_pattern, require_prefix};
use super::schema::INSERT_SQL;
use super::types::{HotpathData, KvPair, KvValue, RunRow, generate_uuid, short_uuid};
use crate::error::DevError;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl ResultsDb {
    /// Insert a benchmark result row. Returns `(full_uuid, short_uuid)`.
    pub fn insert(&self, row: &RunRow) -> Result<(String, String), DevError> {
        let uuid = generate_uuid()?;
        self.conn.execute("BEGIN", [])?;

        let result = insert_inner(&self.conn, row, &uuid);
        if result.is_err() {
            self.conn.execute("ROLLBACK", []).ok();
            result?;
        }

        self.conn.execute("COMMIT", [])?;
        let short = short_uuid(&uuid);
        Ok((uuid, short))
    }

    /// Delete every run whose uuid starts with `uuid_prefix`, along with all
    /// FK children (`run_distribution`, `run_iterations`, `run_kv`,
    /// `hotpath_functions`, `hotpath_threads`). FK cascade isn't active (no
    /// `PRAGMA foreign_keys`), so children are deleted explicitly in one
    /// transaction.
    ///
    /// Returns the number of `runs` rows removed.
    ///
    /// The prefix matches literally, and an empty one is refused: `LIKE '%'`
    /// is every row, which is what `brokkr invalidate "" -f` used to delete.
    pub fn delete_by_uuid_prefix(&self, uuid_prefix: &str) -> Result<usize, DevError> {
        require_prefix(uuid_prefix, "uuid")?;
        let pattern = prefix_pattern(uuid_prefix);
        let tx = self.conn.unchecked_transaction()?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare(&format!("SELECT id FROM runs WHERE uuid LIKE ?1 {ESCAPE}"))?;
            let rows = stmt.query_map(rusqlite::params![pattern], |row| row.get::<_, i64>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for id in &ids {
            tx.execute("DELETE FROM run_distribution WHERE run_id = ?1", rusqlite::params![id])?;
            tx.execute("DELETE FROM run_iterations WHERE run_id = ?1", rusqlite::params![id])?;
            tx.execute("DELETE FROM run_kv WHERE run_id = ?1", rusqlite::params![id])?;
            tx.execute("DELETE FROM hotpath_functions WHERE run_id = ?1", rusqlite::params![id])?;
            tx.execute("DELETE FROM hotpath_threads WHERE run_id = ?1", rusqlite::params![id])?;
        }
        let removed = tx.execute(
            &format!("DELETE FROM runs WHERE uuid LIKE ?1 {ESCAPE}"),
            rusqlite::params![pattern],
        )?;
        tx.commit()?;
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn insert_inner(conn: &rusqlite::Connection, row: &RunRow, uuid: &str) -> Result<String, DevError> {
    // Envelope row.
    conn.execute(
        INSERT_SQL,
        rusqlite::params![
            row.hostname,
            row.commit,
            row.subject,
            row.command,
            row.mode,
            row.input_file,
            row.input_mb,
            row.elapsed_ms,
            row.elapsed_us,
            row.peak_rss_mb,
            row.cargo_features,
            row.cargo_profile.as_str(),
            row.kernel,
            row.cpu_governor,
            row.avail_memory_mb,
            row.storage_notes,
            uuid,
            row.cli_args,
            row.project,
            row.stop_marker,
            row.brokkr_args,
        ],
    )?;
    let run_id = conn.last_insert_rowid();

    insert_timings(conn, run_id, row)?;

    // Key-value pairs. Last one wins: `build_row` appends its sources in
    // increasing precedence (metadata, env capture, `prev.*`, then the run's
    // own stderr counters), so a later pair with the same key is the one the
    // row must keep.
    for kv in &row.kv {
        insert_kv_row(conn, run_id, kv, OnConflict::Replace)?;
    }

    // Hotpath child rows.
    if let Some(ref hp) = row.hotpath {
        insert_hotpath(conn, run_id, hp)?;
    }

    Ok(short_uuid(uuid))
}

/// The `run_distribution` and `run_iterations` child rows.
fn insert_timings(conn: &rusqlite::Connection, run_id: i64, row: &RunRow) -> Result<(), DevError> {
    // Distribution child row. The microsecond columns stay NULL when the
    // distribution never measured them.
    if let Some(ref dist) = row.distribution {
        let us = dist.us;
        conn.execute(
            "INSERT INTO run_distribution \
             (run_id, samples, min_ms, p50_ms, p95_ms, max_ms, min_us, p50_us, p95_us, max_us) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                run_id,
                dist.samples,
                dist.min_ms,
                dist.p50_ms,
                dist.p95_ms,
                dist.max_ms,
                us.map(|u| u.min),
                us.map(|u| u.p50),
                us.map(|u| u.p95),
                us.map(|u| u.max),
            ],
        )?;
    }

    // Per-iteration walls, in execution order. Microseconds ride along only
    // when there is one per iteration - a shorter list cannot be aligned to
    // the order the table exists to keep.
    let iterations_us =
        (row.iterations_us.len() == row.iterations.len()).then_some(&row.iterations_us);
    for (run_idx, elapsed_ms) in row.iterations.iter().enumerate() {
        let elapsed_us = iterations_us.and_then(|us| us.get(run_idx).copied());
        conn.execute(
            "INSERT INTO run_iterations (run_id, run_idx, elapsed_ms, elapsed_us) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                run_id,
                i64::try_from(run_idx).unwrap_or(i64::MAX),
                elapsed_ms,
                elapsed_us
            ],
        )?;
    }
    Ok(())
}

/// The hotpath child rows, plus the thread summary into `run_kv`.
fn insert_hotpath(conn: &rusqlite::Connection, run_id: i64, hp: &HotpathData) -> Result<(), DevError> {
    for func in &hp.functions {
        conn.execute(
            "INSERT INTO hotpath_functions \
             (run_id, section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                run_id, func.section, func.description, func.ordinal, func.name,
                func.calls, func.avg, func.total, func.percent_total,
                func.p50, func.p95, func.p99,
            ],
        )?;
    }
    for thread in &hp.threads {
        conn.execute(
            "INSERT INTO hotpath_threads \
             (run_id, name, status, cpu_percent, cpu_percent_max, cpu_percent_avg, \
              alloc_bytes, dealloc_bytes, mem_diff) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                run_id,
                thread.name,
                thread.status,
                thread.cpu_percent,
                thread.cpu_percent_max,
                thread.cpu_percent_avg,
                thread.alloc_bytes,
                thread.dealloc_bytes,
                thread.mem_diff,
            ],
        )?;
    }
    // Thread summary stats into run_kv. These yield to anything already
    // in `row.kv` under the same key: the run's own report is primary.
    for kv in &hp.thread_summary {
        insert_kv_row(conn, run_id, kv, OnConflict::Ignore)?;
    }
    Ok(())
}

/// What a pair does when its `(run_id, key)` is already in `run_kv`.
#[derive(Clone, Copy)]
enum OnConflict {
    /// Overwrite: the later pair wins.
    Replace,
    /// Skip: the earlier pair wins.
    Ignore,
}

fn insert_kv_row(
    conn: &rusqlite::Connection,
    run_id: i64,
    kv: &KvPair,
    on_conflict: OnConflict,
) -> Result<(), DevError> {
    // REPLACE deletes the old row and inserts the new one whole, so a key
    // that changes type (Int then Text) leaves no stale value column behind.
    let verb = match on_conflict {
        OnConflict::Replace => "REPLACE",
        OnConflict::Ignore => "IGNORE",
    };
    match &kv.value {
        KvValue::Int(v) => conn.execute(
            &format!("INSERT OR {verb} INTO run_kv (run_id, key, value_int) VALUES (?1, ?2, ?3)"),
            rusqlite::params![run_id, kv.key, v],
        )?,
        KvValue::Real(v) => conn.execute(
            &format!("INSERT OR {verb} INTO run_kv (run_id, key, value_real) VALUES (?1, ?2, ?3)"),
            rusqlite::params![run_id, kv.key, v],
        )?,
        KvValue::Text(v) => conn.execute(
            &format!("INSERT OR {verb} INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)"),
            rusqlite::params![run_id, kv.key, v],
        )?,
    };
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use crate::db::{HotpathData, KvPair, QueryFilter, ResultsDb, RunRow};

    #[test]
    fn db_open_and_insert_roundtrip() {
        let dir = std::env::temp_dir().join("brokkr_test_db_roundtrip");
        drop(std::fs::create_dir_all(&dir));
        let db_path = dir.join("test.db");
        // Clean up from previous runs.
        drop(std::fs::remove_file(&db_path));

        let db = ResultsDb::open(&db_path).expect("open db");
        let run = RunRow {
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test subject"),
            command: String::from("read"),
            mode: Some(String::from("bench")),
            input_file: Some(String::from("denmark.osm.pbf")),
            input_mb: Some(42.5),
            elapsed_ms: 1234,
            elapsed_us: None,
            peak_rss_mb: None,
            cargo_features: None,
            cargo_profile: crate::build::CargoProfile::Release,
            kernel: None,
            cpu_governor: None,
            avail_memory_mb: None,
            storage_notes: None,
            cli_args: Some(String::from("--fast")),
            brokkr_args: Some(String::from("brokkr test --fast")),
            project: String::from("test"),
            stop_marker: None,
            kv: vec![],
            iterations: Vec::new(),
            iterations_us: Vec::new(),
            distribution: None,
            hotpath: None,
        };
        let (_, short) = db.insert(&run).expect("insert");
        assert_eq!(short.len(), 8, "short uuid should be 8 chars");

        let rows = db
            .query(&QueryFilter {
                commit: Some(String::from("aabbccdd")),
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 10,
            })
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, "read");
        assert_eq!(rows[0].mode, "bench");
        assert_eq!(rows[0].input_file, "denmark.osm.pbf");
        assert_eq!(rows[0].elapsed_ms, 1234);
        assert_eq!(rows[0].cli_args, "--fast");
        assert_eq!(rows[0].brokkr_args, "brokkr test --fast");

        // Clean up.
        drop(std::fs::remove_file(&db_path));
        drop(std::fs::remove_dir(&dir));
    }

    #[test]
    fn db_migrations_are_idempotent() {
        let dir = std::env::temp_dir().join("brokkr_test_db_migrations");
        drop(std::fs::create_dir_all(&dir));
        let db_path = dir.join("test.db");
        drop(std::fs::remove_file(&db_path));

        // Open twice -- second open should not fail on migrations.
        {
            let _db = ResultsDb::open(&db_path).expect("first open");
        }
        {
            let _db = ResultsDb::open(&db_path).expect("second open");
        }

        drop(std::fs::remove_file(&db_path));
        drop(std::fs::remove_dir(&dir));
    }

    fn kv_row(kv: Vec<KvPair>, hotpath: Option<HotpathData>) -> RunRow {
        RunRow {
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test"),
            command: String::from("read"),
            mode: Some(String::from("bench")),
            input_file: None,
            input_mb: None,
            elapsed_ms: 1,
            elapsed_us: None,
            peak_rss_mb: None,
            cargo_features: None,
            cargo_profile: crate::build::CargoProfile::Release,
            kernel: None,
            cpu_governor: None,
            avail_memory_mb: None,
            storage_notes: None,
            cli_args: None,
            brokkr_args: None,
            project: String::from("test"),
            stop_marker: None,
            kv,
            iterations: Vec::new(),
            iterations_us: Vec::new(),
            distribution: None,
            hotpath,
        }
    }

    fn kv_text(db: &ResultsDb, short: &str, key: &str) -> String {
        let rows = db.query_by_uuid(short).expect("query");
        let pair = rows[0].kv.iter().find(|p| p.key == key).expect("key present");
        pair.value.to_string()
    }

    // `build_row` appends metadata, env, prev.* and then the run's own stderr
    // counters, documenting that the runtime value wins a key collision. That
    // only holds if the later pair is the one stored.
    #[test]
    fn later_kv_pair_wins_a_key_collision() {
        let db_path = crate::test_scratch::scratch("db-write", "kv_last_wins").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        let row = kv_row(
            vec![KvPair::text("meta.cache", "harness"), KvPair::int("meta.cache", 7)],
            None,
        );
        let (_, short) = db.insert(&row).expect("insert");
        assert_eq!(kv_text(&db, &short, "meta.cache"), "7");
    }

    #[test]
    fn thread_summary_yields_to_row_kv() {
        let db_path = crate::test_scratch::scratch("db-write", "kv_thread_summary").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        let hp = HotpathData {
            functions: Vec::new(),
            threads: Vec::new(),
            thread_summary: vec![KvPair::text("threads.rss_bytes", "summary")],
        };
        let row = kv_row(vec![KvPair::text("threads.rss_bytes", "stderr")], Some(hp));
        let (_, short) = db.insert(&row).expect("insert");
        assert_eq!(kv_text(&db, &short, "threads.rss_bytes"), "stderr");
    }

    // A microsecond list that does not cover every iteration cannot be
    // aligned to the execution order, so none of it is stored.
    #[test]
    fn partial_iteration_microseconds_are_not_stored() {
        let db_path = crate::test_scratch::scratch("db-write", "partial_iter_us").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        let mut row = kv_row(Vec::new(), None);
        row.iterations = vec![1, 2];
        row.iterations_us = vec![1_200];
        let (_, short) = db.insert(&row).expect("insert");
        let stored = db.query_by_uuid(&short).expect("query");
        assert_eq!(stored[0].iterations, vec![1, 2]);
        assert!(stored[0].iterations_us.is_empty());
    }

    #[test]
    fn delete_refuses_empty_prefix() {
        let db_path = crate::test_scratch::scratch("db-write", "delete_empty").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        let (_, short) = db.insert(&kv_row(Vec::new(), None)).expect("insert");
        assert!(db.delete_by_uuid_prefix("").is_err());
        assert_eq!(db.query_by_uuid(&short).expect("query").len(), 1, "nothing deleted");
        assert_eq!(db.delete_by_uuid_prefix(&short).expect("delete"), 1);
    }

    // The index used to be created only by the v0->v1 migration, which a fresh
    // database never runs.
    #[test]
    fn fresh_db_has_uuid_index() {
        let db_path = crate::test_scratch::scratch("db-write", "uuid_index").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_runs_uuid'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
