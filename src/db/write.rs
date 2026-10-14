//! Insert operations for the results database.

use super::ResultsDb;
use super::run_store::RunId;
use super::schema::INSERT_SQL;
use super::types::{
    HotpathData, KvClass, KvKey, KvPair, KvValue, Quantity, RunRow, canonical_cargo_features,
};
use super::{generate_uuid, short_uuid};
use crate::error::DevError;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl ResultsDb {
    /// Delete rows for one canonical UUID, using equality rather than the
    /// prefix API. Children are deleted explicitly before the parent: a
    /// database created by an older schema can still carry child tables
    /// without `ON DELETE CASCADE` (`CREATE TABLE IF NOT EXISTS` never
    /// rewrites them), where relying on the cascade would orphan the rows or,
    /// with a plain `REFERENCES`, refuse the delete under `foreign_keys=ON`.
    pub(crate) fn delete_by_uuid(&self, uuid: &RunId) -> Result<usize, DevError> {
        let tx = self.conn.unchecked_transaction()?;
        for table in [
            "run_distribution",
            "run_iterations",
            "run_kv",
            "hotpath_functions",
            "hotpath_threads",
        ] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE run_id IN (SELECT id FROM runs WHERE uuid = ?1)"
                ),
                rusqlite::params![uuid.as_str()],
            )?;
        }
        let removed = tx.execute(
            "DELETE FROM runs WHERE uuid = ?1",
            rusqlite::params![uuid.as_str()],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Insert a benchmark result row. Returns `(full_uuid, short_uuid)`.
    pub fn insert(&self, row: &RunRow) -> Result<(String, String), DevError> {
        let uuid = generate_uuid()?;
        let tx = self.conn.unchecked_transaction()?;
        insert_inner(&tx, row, &uuid)?;
        tx.commit()?;
        let short = short_uuid(&uuid);
        Ok((uuid, short))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn insert_inner(conn: &rusqlite::Connection, row: &RunRow, uuid: &str) -> Result<String, DevError> {
    // Store one stable spelling regardless of whether the feature list came
    // from a host config, a request, or a per-row variant.
    let cargo_features = canonical_cargo_features(row.cargo_features.as_deref());
    let stored_argv = crate::history::redact_secret_argv(&row.invocation.argv);
    let brokkr_args = serde_json::to_string(&stored_argv).map_err(|error| {
        DevError::Database(format!("could not encode brokkr argv array: {error}"))
    })?;
    // An empty key would pair every such row with every other; store none,
    // which pairs by the stored argv like a historical row. Recompute from
    // its redacted form so credentials cannot escape through the opaque key.
    let stored_arm_key =
        (!row.invocation.arm_key.is_empty()).then(|| crate::run_invocation::arm_key(&stored_argv));
    let arm_key = stored_arm_key.as_deref();

    // Envelope row.
    conn.execute(
        INSERT_SQL,
        rusqlite::params![
            row.hostname,
            row.commit.as_str(),
            row.subject,
            row.command,
            row.mode,
            row.input_file,
            row.input_mb,
            row.elapsed.ms(),
            row.elapsed.micros(),
            row.peak_rss_mb,
            cargo_features,
            row.cargo_profile.as_label(),
            row.kernel,
            row.cpu_governor,
            row.avail_memory_mb,
            row.storage_notes,
            uuid,
            row.cli_args,
            row.project,
            row.stop_marker
                .as_ref()
                .and_then(|(_, fired)| fired.as_deref()),
            row.stop_marker
                .as_ref()
                .and_then(|(requested, _)| requested.as_deref()),
            brokkr_args,
            arm_key,
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
        conn.execute(
            "INSERT INTO run_distribution \
             (run_id, samples, min_ms, p50_ms, p95_ms, max_ms, min_us, p50_us, p95_us, max_us) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                run_id,
                dist.samples,
                dist.min.ms(),
                dist.p50.ms(),
                dist.p95.ms(),
                dist.max.ms(),
                dist.min.micros(),
                dist.p50.micros(),
                dist.p95.micros(),
                dist.max.micros(),
            ],
        )?;
    }

    // Each per-iteration wall keeps its own precision until this SQL boundary.
    for (run_idx, wall) in row.iterations.iter().enumerate() {
        conn.execute(
            "INSERT INTO run_iterations (run_id, run_idx, elapsed_ms, elapsed_us) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                run_id,
                i64::try_from(run_idx).unwrap_or(i64::MAX),
                wall.ms(),
                wall.micros()
            ],
        )?;
    }
    Ok(())
}

/// The hotpath child rows, plus the thread summary into `run_kv`.
#[allow(clippy::too_many_lines)]
fn insert_hotpath(
    conn: &rusqlite::Connection,
    run_id: i64,
    hp: &HotpathData,
) -> Result<(), DevError> {
    // The legacy text columns carry each quantity's display spelling, display
    // text included; the typed tables beside them hold numeric quantities only.
    let text = |q: &Option<Quantity>| q.as_ref().map(ToString::to_string);
    for func in &hp.functions {
        let percentile_text = |p: f64| {
            func.percentiles
                .iter()
                .find(|(key, _)| (key.value() - p).abs() < f64::EPSILON)
                .map(|(_, value)| value.to_string())
        };
        conn.execute(
            "INSERT INTO hotpath_functions \
             (run_id, section, description, ordinal, name, calls, avg, total, percent_total, \
              p50, p95, p99) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                run_id,
                func.section,
                func.description,
                func.ordinal,
                func.name,
                func.calls,
                text(&func.avg),
                text(&func.total),
                text(&func.percent_total),
                percentile_text(50.0),
                percentile_text(95.0),
                percentile_text(99.0),
            ],
        )?;
        let function_id = conn.last_insert_rowid();
        for (metric, value) in [
            ("avg", &func.avg),
            ("total", &func.total),
            ("percent_total", &func.percent_total),
        ] {
            if let Some(value) = value {
                insert_typed_hotpath_metric(
                    conn,
                    "hotpath_function_metrics",
                    "function_id",
                    function_id,
                    metric,
                    value,
                )?;
            }
        }
        for (percentile, value) in &func.percentiles {
            let Some((quantity_kind, value_int, value_real)) = value.storage_parts() else {
                continue;
            };
            conn.execute(
                "INSERT INTO hotpath_function_percentiles \
                 (function_id, percentile, quantity_kind, value_int, value_real) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    function_id,
                    percentile.value(),
                    quantity_kind,
                    value_int,
                    value_real,
                ],
            )?;
        }
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
                text(&thread.cpu_percent),
                text(&thread.cpu_percent_max),
                text(&thread.cpu_percent_avg),
                text(&thread.alloc_bytes),
                text(&thread.dealloc_bytes),
                text(&thread.mem_diff),
            ],
        )?;
        let thread_id = conn.last_insert_rowid();
        for (metric, value) in [
            ("cpu_percent", &thread.cpu_percent),
            ("cpu_percent_max", &thread.cpu_percent_max),
            ("cpu_percent_avg", &thread.cpu_percent_avg),
            ("alloc_bytes", &thread.alloc_bytes),
            ("dealloc_bytes", &thread.dealloc_bytes),
            ("mem_diff", &thread.mem_diff),
        ] {
            if let Some(value) = value {
                insert_typed_hotpath_metric(
                    conn,
                    "hotpath_thread_metrics",
                    "thread_id",
                    thread_id,
                    metric,
                    value,
                )?;
            }
        }
    }
    // Thread summary stats into run_kv. These yield to anything already
    // in `row.kv` under the same key: the run's own report is primary.
    for kv in &hp.thread_summary {
        let pair = KvPair {
            key: KvKey::thread_summary(kv.key.name()),
            value: kv.value.clone(),
        };
        insert_kv_row(conn, run_id, &pair, OnConflict::Ignore)?;
    }
    Ok(())
}

fn insert_typed_hotpath_metric(
    conn: &rusqlite::Connection,
    table: &str,
    owner_column: &str,
    owner_id: i64,
    metric: &str,
    quantity: &Quantity,
) -> Result<(), DevError> {
    // Display text has no typed form; its legacy text column carries it.
    let Some((quantity_kind, value_int, value_real)) = quantity.storage_parts() else {
        return Ok(());
    };
    conn.execute(
        &format!(
            "INSERT INTO {table} ({owner_column}, metric, quantity_kind, value_int, value_real) \
             VALUES (?1, ?2, ?3, ?4, ?5)"
        ),
        rusqlite::params![owner_id, metric, quantity_kind, value_int, value_real],
    )?;
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
    // Captured environment values are useful result axes, but credentials and
    // URL secrets must not become durable just because a capture pattern
    // selected them. Preserve typed non-secret values; replace a redacted
    // value with text so its original numeric type cannot leak through.
    if kv.class() == KvClass::Environment {
        let original = kv.value.to_string();
        let value = crate::history::redact_env_value(kv.key.name(), &original);
        if value != original {
            let safe = KvPair {
                key: kv.key.clone(),
                value: KvValue::Text(value),
            };
            return insert_kv_value(conn, run_id, &safe, on_conflict);
        }
    }
    insert_kv_value(conn, run_id, kv, on_conflict)
}

fn insert_kv_value(
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
            rusqlite::params![run_id, kv.key.as_str(), v],
        )?,
        KvValue::Real(v) => conn.execute(
            &format!("INSERT OR {verb} INTO run_kv (run_id, key, value_real) VALUES (?1, ?2, ?3)"),
            rusqlite::params![run_id, kv.key.as_str(), v],
        )?,
        KvValue::Text(v) => conn.execute(
            &format!("INSERT OR {verb} INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)"),
            rusqlite::params![run_id, kv.key.as_str(), v],
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
    use crate::db::{
        HotpathData, KvPair, QueryFilter, ResultsDb, RunInvocation, RunRow, RunRowBuilder, Wall,
    };

    #[test]
    fn db_open_and_insert_roundtrip() {
        let dir = crate::test_scratch::scratch("db-write", "db_open_insert_roundtrip");
        let db_path = dir.join("test.db");
        // Clean up from previous runs.
        drop(std::fs::remove_file(&db_path));

        let db = ResultsDb::open(&db_path).expect("open db");
        let run = RunRowBuilder::new()
            .update(|run| {
                run.subject = String::from("test subject");
                run.input_file = Some(String::from("denmark.osm.pbf"));
                run.input_mb = Some(42.5);
                run.elapsed = Wall::LegacyMillis(1234);
                run.cli_args = Some(String::from("--fast"));
                run.invocation = RunInvocation {
                    argv: vec![
                        String::from("brokkr"),
                        String::from("test"),
                        String::from("--fast"),
                    ],
                    arm_key: String::from("test/fast"),
                };
            })
            .build();
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
        assert_eq!(rows[0].elapsed, Wall::LegacyMillis(1234));
        assert_eq!(rows[0].cli_args, "--fast");
        assert_eq!(rows[0].brokkr_args, r#"["brokkr","test","--fast"]"#);
        // The writer derives the stored arm key from the stored (redacted)
        // argv, so the caller's opaque key is only a presence flag.
        let expected_arm = crate::run_invocation::arm_key(&[
            String::from("brokkr"),
            String::from("test"),
            String::from("--fast"),
        ]);
        assert_eq!(rows[0].arm_key.as_deref(), Some(expected_arm.as_str()));

        // Clean up.
        drop(std::fs::remove_file(&db_path));
        drop(std::fs::remove_dir(&dir));
    }

    #[test]
    fn db_migrations_are_idempotent() {
        let dir = crate::test_scratch::scratch("db-write", "db_migrations_idempotent");
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
        RunRowBuilder::new()
            .update(|row| {
                row.elapsed = Wall::LegacyMillis(1);
                row.kv = kv;
                row.hotpath = hotpath;
            })
            .build()
    }

    fn rows_for(db: &ResultsDb, uuid: &str) -> Vec<crate::db::StoredRow> {
        let filter = crate::db::QueryFilter {
            limit: 100,
            ..Default::default()
        };
        db.query_by_run_ids(&[crate::db::RunId::new(uuid.to_owned())], &filter)
            .expect("query")
            .0
    }

    fn kv_text(db: &ResultsDb, uuid: &str, key: &str) -> String {
        let rows = rows_for(db, uuid);
        let pair = rows[0]
            .kv
            .iter()
            .find(|p| p.key == key)
            .expect("key present");
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
            vec![
                KvPair::text("meta.cache", "harness"),
                KvPair::int("meta.cache", 7),
            ],
            None,
        );
        let (full, _) = db.insert(&row).expect("insert");
        assert_eq!(kv_text(&db, &full, "meta.cache"), "7");
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
        let (full, _) = db.insert(&row).expect("insert");
        assert_eq!(kv_text(&db, &full, "threads.rss_bytes"), "stderr");
    }

    // Precision is attached to each iteration, so a legacy sample and an
    // exact sample retain their own measured forms in execution order.
    #[test]
    fn mixed_precision_iteration_walls_round_trip() {
        let db_path = crate::test_scratch::scratch("db-write", "mixed_iter_walls").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        let mut row = kv_row(Vec::new(), None);
        row.iterations = vec![Wall::LegacyMillis(1), Wall::from_micros(1_200)];
        let (full, _) = db.insert(&row).expect("insert");
        let stored = rows_for(&db, &full);
        assert_eq!(
            stored[0].iterations,
            vec![Wall::LegacyMillis(1), Wall::from_micros(1_200)]
        );
    }

    #[test]
    fn delete_by_uuid_removes_exactly_one_run() {
        let db_path = crate::test_scratch::scratch("db-write", "delete_exact").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        let (full, short) = db.insert(&kv_row(Vec::new(), None)).expect("insert");
        let (other, _) = db.insert(&kv_row(Vec::new(), None)).expect("insert");
        assert_eq!(
            db.delete_by_uuid(&crate::db::RunId::new(short))
                .expect("delete"),
            0,
            "a prefix is not an identity"
        );
        assert_eq!(rows_for(&db, &full).len(), 1, "nothing deleted");
        assert_eq!(
            db.delete_by_uuid(&crate::db::RunId::new(full.clone()))
                .expect("delete"),
            1
        );
        assert!(rows_for(&db, &full).is_empty());
        assert_eq!(rows_for(&db, &other).len(), 1);
    }

    // A database from an older schema can keep a child table whose `run_id`
    // carries a plain `REFERENCES` without the cascade. Under
    // `foreign_keys=ON` a parent-only delete would be refused there, and a
    // table with no reference at all would keep orphans.
    #[test]
    fn delete_by_uuid_handles_legacy_child_tables_without_cascade() {
        let db_path =
            crate::test_scratch::scratch("db-write", "delete_legacy_children").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        db.conn
            .execute_batch(
                "DROP TABLE run_kv;
                 CREATE TABLE run_kv (
                     run_id INTEGER NOT NULL REFERENCES runs(id),
                     key TEXT NOT NULL, value_int INTEGER, value_real REAL, value_text TEXT,
                     PRIMARY KEY (run_id, key)
                 );
                 DROP TABLE run_iterations;
                 CREATE TABLE run_iterations (
                     run_id INTEGER NOT NULL, run_idx INTEGER NOT NULL,
                     elapsed_ms INTEGER NOT NULL, elapsed_us INTEGER,
                     PRIMARY KEY (run_id, run_idx)
                 );",
            )
            .expect("reshape child tables to a legacy form");
        let mut row = kv_row(vec![KvPair::text("meta.x", "y")], None);
        row.iterations = vec![Wall::LegacyMillis(1), Wall::LegacyMillis(2)];
        let (full, _) = db.insert(&row).expect("insert");
        assert_eq!(
            db.delete_by_uuid(&crate::db::RunId::new(full.clone()))
                .expect("delete"),
            1
        );
        for table in ["run_kv", "run_iterations"] {
            let orphans: i64 = db
                .conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} WHERE run_id NOT IN (SELECT id FROM runs)"
                    ),
                    [],
                    |r| r.get(0),
                )
                .expect("count");
            assert_eq!(orphans, 0, "{table} kept orphans");
        }
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
