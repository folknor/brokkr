use crate::error::DevError;

/// Current schema version. Increment when adding new migrations.
pub(super) const SCHEMA_VERSION: i64 = 9;

/// Run all pending migrations based on `PRAGMA user_version`.
pub(super) fn run_migrations(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Fresh database — no tables yet, nothing to migrate.
    if !has_table(conn, "runs") {
        return Ok(());
    }

    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    if current < 1 {
        migrate_uuid(conn)?;
    }
    if current < 2 {
        migrate_cli_args_metadata(conn)?;
    }
    if current < 3 {
        migrate_v2_to_v3(conn)?;
    }
    if current < 4 {
        migrate_v3_to_v4(conn)?;
    }
    if current < 5 {
        migrate_v4_to_v5(conn)?;
    }
    if current < 6 {
        migrate_v5_to_v6(conn)?;
    }
    if current < 7 {
        migrate_v6_to_v7(conn)?;
    }
    if current < 8 {
        migrate_v7_to_v8(conn)?;
    }
    if current < 9 {
        migrate_v8_to_v9(conn)?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
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

/// Check whether a column exists on a table.
fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut stmt| {
            let names: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(Result::ok)
                .collect();
            Ok(names.contains(&column.to_owned()))
        })
        .unwrap_or(false)
}

/// Migration v0 -> v1: add uuid column and backfill.
fn migrate_uuid(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_column(conn, "runs", "uuid") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN uuid TEXT")?;
    }

    // Backfill existing rows with generated UUIDs.
    let mut stmt = conn.prepare("SELECT id FROM runs WHERE uuid IS NULL")?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(Result::ok)
        .collect();

    let mut update = conn.prepare("UPDATE runs SET uuid = ?1 WHERE id = ?2")?;
    for id in ids {
        let uuid = super::types::generate_uuid()?;
        update.execute(rusqlite::params![uuid, id])?;
    }

    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_runs_uuid ON runs(uuid)")?;
    Ok(())
}

/// Migration v1 -> v2: add cli_args and metadata columns.
fn migrate_cli_args_metadata(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_column(conn, "runs", "cli_args") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN cli_args TEXT")?;
    }
    if !has_column(conn, "runs", "metadata") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN metadata TEXT")?;
    }
    Ok(())
}

/// Migration v2 -> v3: add peak_rss_mb, project columns, create child tables,
/// migrate extra/metadata JSON into child tables.
fn migrate_v2_to_v3(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Phase 1: DDL additions.
    if !has_column(conn, "runs", "peak_rss_mb") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN peak_rss_mb REAL")?;
    }
    if !has_column(conn, "runs", "project") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN project TEXT NOT NULL DEFAULT 'pbfhogg'")?;
    }

    // Child tables + indexes (all idempotent via IF NOT EXISTS).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS run_distribution (
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            samples INTEGER NOT NULL, min_ms INTEGER NOT NULL,
            p50_ms INTEGER NOT NULL, p95_ms INTEGER NOT NULL, max_ms INTEGER NOT NULL,
            PRIMARY KEY (run_id));

        CREATE TABLE IF NOT EXISTS run_kv (
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            key TEXT NOT NULL, value_int INTEGER, value_real REAL, value_text TEXT,
            PRIMARY KEY (run_id, key));
        CREATE INDEX IF NOT EXISTS idx_run_kv_key ON run_kv(key, run_id);

        CREATE TABLE IF NOT EXISTS hotpath_functions (
            id INTEGER PRIMARY KEY,
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            section TEXT NOT NULL, description TEXT, ordinal INTEGER NOT NULL,
            name TEXT NOT NULL, calls INTEGER, avg TEXT, total TEXT,
            percent_total TEXT, p50 TEXT, p95 TEXT, p99 TEXT);
        CREATE INDEX IF NOT EXISTS idx_hotpath_functions_run_id ON hotpath_functions(run_id);

        CREATE TABLE IF NOT EXISTS hotpath_threads (
            id INTEGER PRIMARY KEY,
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            name TEXT NOT NULL, status TEXT, cpu_percent TEXT, cpu_percent_max TEXT,
            cpu_user TEXT, cpu_sys TEXT, cpu_total TEXT,
            alloc_bytes TEXT, dealloc_bytes TEXT, mem_diff TEXT);
        CREATE INDEX IF NOT EXISTS idx_hotpath_threads_run_id ON hotpath_threads(run_id);

        CREATE INDEX IF NOT EXISTS idx_runs_project ON runs(project);",
    )?;

    // Phase 2: Migrate existing extra/metadata JSON to child tables.
    migrate_json_to_children(conn)?;

    Ok(())
}

/// Migration v3 -> v4: rename pbfhogg variant values after CLI consolidation (22→14).
///
/// Only touches rows where `project = 'pbfhogg'` (or project IS NULL, which defaults
/// to pbfhogg in older schemas).
fn migrate_v3_to_v4(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Variant renames in bench commands / bench blob-filter / hotpath.
    const RENAMES: &[(&str, &str)] = &[
        ("tags-count", "inspect-tags"),
        ("tags-count-way", "inspect-tags-way"),
        ("node-stats", "inspect-nodes"),
        ("removeid", "getid-invert"),
        ("merge-pbf", "cat-dedupe"),
        ("derive-changes", "diff-osc"),
        // blob-filter compound variants
        ("tags-count-way+indexed", "inspect-tags-way+indexed"),
        ("tags-count-way+raw", "inspect-tags-way+raw"),
        ("node-stats+indexed", "inspect-nodes+indexed"),
        ("node-stats+raw", "inspect-nodes+raw"),
        // hotpath variants
        ("merge-zlib", "apply-changes-zlib"),
        ("merge-none", "apply-changes-none"),
        ("merge-zlib/alloc", "apply-changes-zlib/alloc"),
        ("merge-none/alloc", "apply-changes-none/alloc"),
        ("tags-count/alloc", "inspect-tags/alloc"),
    ];

    let mut stmt = conn.prepare(
        "UPDATE runs SET variant = ?1 WHERE variant = ?2 AND (project = 'pbfhogg' OR project IS NULL)"
    )?;

    for &(old, new) in RENAMES {
        stmt.execute(rusqlite::params![new, old])?;
    }

    Ok(())
}

/// Migration v4 -> v5: rename `meta.locations_on_ways` to `meta.locations_on_ways_cli`
/// in run_kv. Existing rows recorded only CLI intent, not runtime detection.
fn migrate_v4_to_v5(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if has_table(conn, "run_kv") {
        conn.execute(
            "UPDATE run_kv SET key = 'meta.locations_on_ways_cli' WHERE key = 'meta.locations_on_ways'",
            [],
        )?;
    }
    Ok(())
}

/// Migration v5 -> v6: rename `meta.tiles_sha256` to `meta.tiles_hash`
/// in run_kv after switching from SHA256 to XXH128.
fn migrate_v5_to_v6(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if has_table(conn, "run_kv") {
        conn.execute(
            "UPDATE run_kv SET key = 'meta.tiles_hash' WHERE key = 'meta.tiles_sha256'",
            [],
        )?;
    }
    Ok(())
}

/// Migration v6 -> v7: replace `cpu_user`, `cpu_sys`, `cpu_total` columns in
/// `hotpath_threads` with `cpu_percent_avg` (hotpath 0.14 schema change).
fn migrate_v6_to_v7(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_table(conn, "hotpath_threads") {
        return Ok(());
    }
    // SQLite doesn't support DROP COLUMN before 3.35.0, and even then it's
    // finicky with constraints.  Safest approach: recreate the table.
    conn.execute_batch(
        "CREATE TABLE hotpath_threads_new (
            id              INTEGER PRIMARY KEY,
            run_id          INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            name            TEXT NOT NULL,
            status          TEXT,
            cpu_percent     TEXT,
            cpu_percent_max TEXT,
            cpu_percent_avg TEXT,
            alloc_bytes     TEXT,
            dealloc_bytes   TEXT,
            mem_diff        TEXT
        );
        INSERT INTO hotpath_threads_new (id, run_id, name, status, cpu_percent, cpu_percent_max,
            alloc_bytes, dealloc_bytes, mem_diff)
            SELECT id, run_id, name, status, cpu_percent, cpu_percent_max,
                   alloc_bytes, dealloc_bytes, mem_diff
            FROM hotpath_threads;
        DROP TABLE hotpath_threads;
        ALTER TABLE hotpath_threads_new RENAME TO hotpath_threads;
        CREATE INDEX idx_hotpath_threads_run_id ON hotpath_threads(run_id);",
    )?;
    Ok(())
}

/// Migration v7 -> v8: add sidecar profiler tables.
///
/// Fresh databases already have these tables from the SCHEMA DDL, so this
/// migration only runs on databases created before the sidecar feature.
fn migrate_v7_to_v8(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_table(conn, "sidecar_samples") {
        conn.execute_batch(
            "CREATE TABLE sidecar_samples (
                result_uuid TEXT NOT NULL,
                run_idx     INTEGER NOT NULL DEFAULT 0,
                sample_idx  INTEGER NOT NULL,
                timestamp_us INTEGER NOT NULL,
                rss_kb      INTEGER,
                anon_kb     INTEGER,
                file_kb     INTEGER,
                shmem_kb    INTEGER,
                swap_kb     INTEGER,
                vsize_kb    INTEGER,
                vm_hwm_kb   INTEGER,
                utime       INTEGER,
                stime       INTEGER,
                num_threads INTEGER,
                minflt      INTEGER,
                majflt      INTEGER,
                rchar       INTEGER,
                wchar       INTEGER,
                read_bytes  INTEGER,
                write_bytes INTEGER,
                cancelled_write_bytes INTEGER,
                syscr       INTEGER,
                syscw       INTEGER,
                vol_cs      INTEGER,
                nonvol_cs   INTEGER,
                PRIMARY KEY (result_uuid, run_idx, sample_idx)
            );
            CREATE TABLE sidecar_markers (
                result_uuid  TEXT NOT NULL,
                run_idx      INTEGER NOT NULL DEFAULT 0,
                marker_idx   INTEGER NOT NULL,
                timestamp_us INTEGER NOT NULL,
                name         TEXT NOT NULL,
                PRIMARY KEY (result_uuid, run_idx, marker_idx)
            );
            CREATE TABLE sidecar_summary (
                result_uuid  TEXT NOT NULL,
                run_idx      INTEGER NOT NULL DEFAULT 0,
                vm_hwm_kb    INTEGER,
                sample_count INTEGER,
                marker_count INTEGER,
                wall_time_ms INTEGER,
                PRIMARY KEY (result_uuid, run_idx)
            );",
        )?;
    }
    Ok(())
}

/// Migration v8 -> v9: drop sidecar tables from results.db.
///
/// Sidecar data now lives in a separate `.brokkr/sidecar.db` (gitignored)
/// to keep the tracked results.db small.
fn migrate_v8_to_v9(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if has_table(conn, "sidecar_samples") {
        conn.execute_batch(
            "DROP TABLE IF EXISTS sidecar_samples;
             DROP TABLE IF EXISTS sidecar_markers;
             DROP TABLE IF EXISTS sidecar_summary;",
        )?;
    }
    Ok(())
}

/// Parse existing extra/metadata JSON and insert into child tables.
fn migrate_json_to_children(conn: &rusqlite::Connection) -> Result<(), DevError> {
    let mut stmt = conn.prepare(
        "SELECT id, extra, metadata FROM runs WHERE extra IS NOT NULL OR metadata IS NOT NULL",
    )?;
    let rows: Vec<(i64, Option<String>, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .filter_map(Result::ok)
        .collect();

    for (run_id, extra_json, metadata_json) in &rows {
        // Migrate extra JSON.
        if let Some(json_str) = extra_json
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(obj) = val.as_object()
        {
            migrate_extra_object(conn, *run_id, obj)?;
        }
        // Migrate metadata JSON -> run_kv with meta. prefix.
        if let Some(json_str) = metadata_json
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(obj) = val.as_object()
        {
            for (key, value) in obj {
                let prefixed = format!("meta.{key}");
                insert_kv_from_json(conn, *run_id, &prefixed, value)?;
            }
        }
    }

    Ok(())
}

/// Migrate a single extra JSON object into the appropriate child tables.
fn migrate_extra_object(
    conn: &rusqlite::Connection,
    run_id: i64,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), DevError> {
    // Case 1: Distribution stats.
    let is_distribution = obj.contains_key("min_ms")
        && obj.contains_key("p50_ms")
        && obj.contains_key("p95_ms")
        && obj.contains_key("max_ms")
        && obj.contains_key("samples");

    if is_distribution {
        let get_i64 = |k: &str| obj.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
        conn.execute(
            "INSERT OR IGNORE INTO run_distribution (run_id, samples, min_ms, p50_ms, p95_ms, max_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![run_id, get_i64("samples"), get_i64("min_ms"), get_i64("p50_ms"), get_i64("p95_ms"), get_i64("max_ms")],
        )?;
        // Remaining keys (beyond the 5 distribution keys) go to run_kv.
        let dist_keys = ["min_ms", "p50_ms", "p95_ms", "max_ms", "samples"];
        for (key, value) in obj {
            if !dist_keys.contains(&key.as_str()) {
                insert_kv_from_json(conn, run_id, key, value)?;
            }
        }
        return Ok(());
    }

    // Case 2: Hotpath data.
    let is_hotpath = obj.contains_key("functions_timing") || obj.contains_key("functions_alloc");

    if is_hotpath {
        migrate_hotpath_section(conn, run_id, obj, "functions_timing", "timing")?;
        migrate_hotpath_section(conn, run_id, obj, "functions_alloc", "alloc")?;

        if let Some(threads_val) = obj.get("threads")
            && let Some(threads_obj) = threads_val.as_object()
        {
            // Thread summary stats -> run_kv with threads. prefix.
            for key in &[
                "rss_bytes",
                "total_alloc_bytes",
                "total_dealloc_bytes",
                "alloc_dealloc_diff",
            ] {
                if let Some(v) = threads_obj.get(*key) {
                    let prefixed = format!("threads.{key}");
                    insert_kv_from_json(conn, run_id, &prefixed, v)?;
                }
            }
            // Thread data rows.
            if let Some(data) = threads_obj.get("data").and_then(|v| v.as_array()) {
                for entry in data {
                    let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
                    conn.execute(
                        "INSERT INTO hotpath_threads \
                         (run_id, name, status, cpu_percent, cpu_percent_max, cpu_user, cpu_sys, cpu_total, \
                          alloc_bytes, dealloc_bytes, mem_diff) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        rusqlite::params![
                            run_id,
                            s("name").unwrap_or_default(),
                            s("status"), s("cpu_percent"), s("cpu_percent_max"),
                            s("cpu_user"), s("cpu_sys"), s("cpu_total"),
                            s("alloc_bytes"), s("dealloc_bytes"), s("mem_diff"),
                        ],
                    )?;
                }
            }
        }
        return Ok(());
    }

    // Case 3: Plain kv pairs.
    for (key, value) in obj {
        insert_kv_from_json(conn, run_id, key, value)?;
    }

    Ok(())
}

/// Migrate a hotpath functions section (timing or alloc) from JSON to the child table.
fn migrate_hotpath_section(
    conn: &rusqlite::Connection,
    run_id: i64,
    obj: &serde_json::Map<String, serde_json::Value>,
    json_key: &str,
    section_name: &str,
) -> Result<(), DevError> {
    let Some(section_val) = obj.get(json_key) else {
        return Ok(());
    };
    let Some(section_obj) = section_val.as_object() else {
        return Ok(());
    };
    let description = section_obj.get("description").and_then(|v| v.as_str());
    let Some(data) = section_obj.get("data").and_then(|v| v.as_array()) else {
        return Ok(());
    };

    for (ordinal, entry) in data.iter().enumerate() {
        let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
        let calls = entry.get("calls").and_then(serde_json::Value::as_i64);
        #[allow(clippy::cast_possible_wrap)]
        let ord = ordinal as i64;
        conn.execute(
            "INSERT INTO hotpath_functions \
             (run_id, section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                run_id, section_name, description,
                ord,
                s("name").unwrap_or_default(),
                calls, s("avg"), s("total"), s("percent_total"),
                s("p50"), s("p95"), s("p99"),
            ],
        )?;
    }

    Ok(())
}

/// Insert a single JSON value into run_kv, auto-detecting type.
fn insert_kv_from_json(
    conn: &rusqlite::Connection,
    run_id: i64,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), DevError> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_i64() {
                conn.execute(
                    "INSERT OR IGNORE INTO run_kv (run_id, key, value_int) VALUES (?1, ?2, ?3)",
                    rusqlite::params![run_id, key, v],
                )?;
            } else if let Some(v) = n.as_f64() {
                conn.execute(
                    "INSERT OR IGNORE INTO run_kv (run_id, key, value_real) VALUES (?1, ?2, ?3)",
                    rusqlite::params![run_id, key, v],
                )?;
            }
        }
        serde_json::Value::String(s) => {
            conn.execute(
                "INSERT OR IGNORE INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
                rusqlite::params![run_id, key, s],
            )?;
        }
        serde_json::Value::Bool(b) => {
            conn.execute(
                "INSERT OR IGNORE INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
                rusqlite::params![run_id, key, b.to_string()],
            )?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{QueryFilter, ResultsDb};

    // -----------------------------------------------------------------------
    // Old schema definitions for migration tests
    // -----------------------------------------------------------------------

    /// v0 schema: no uuid, cli_args, metadata, peak_rss_mb, or project columns.
    const V0_SCHEMA: &str = "\
        CREATE TABLE runs (
            id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL, hostname TEXT NOT NULL,
            [commit] TEXT NOT NULL, subject TEXT NOT NULL, command TEXT NOT NULL,
            variant TEXT, input_file TEXT, input_mb REAL, elapsed_ms INTEGER NOT NULL,
            cargo_features TEXT, cargo_profile TEXT DEFAULT 'release',
            kernel TEXT, cpu_governor TEXT, avail_memory_mb INTEGER,
            storage_notes TEXT, extra TEXT
        )";

    /// v2 schema: adds uuid, cli_args, metadata over v0.
    const V2_SCHEMA: &str = "\
        CREATE TABLE runs (
            id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL, hostname TEXT NOT NULL,
            [commit] TEXT NOT NULL, subject TEXT NOT NULL, command TEXT NOT NULL,
            variant TEXT, input_file TEXT, input_mb REAL, elapsed_ms INTEGER NOT NULL,
            cargo_features TEXT, cargo_profile TEXT DEFAULT 'release',
            kernel TEXT, cpu_governor TEXT, avail_memory_mb INTEGER,
            storage_notes TEXT, extra TEXT, uuid TEXT, cli_args TEXT, metadata TEXT
        )";

    const V0_INSERT: &str = "\
        INSERT INTO runs (timestamp, hostname, [commit], subject, command, variant,
            input_file, input_mb, elapsed_ms, extra)
        VALUES ('2026-01-01 00:00:00', 'testhost', 'aabb', 'old commit', 'bench read',
            'mmap', 'denmark.osm.pbf', 42.5, 1234, ?1)";

    const V2_INSERT: &str = "\
        INSERT INTO runs (timestamp, hostname, [commit], subject, command, variant,
            input_file, input_mb, elapsed_ms, extra, uuid, cli_args, metadata)
        VALUES ('2026-01-01 00:00:00', 'testhost', 'aabb', 'old commit', 'bench read',
            'mmap', 'denmark.osm.pbf', 42.5, 1234, ?1, 'existing_uuid', '--fast', ?2)";

    /// Create a temp directory and db path with a unique name per test.
    fn test_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("brokkr_test_{name}"));
        drop(std::fs::create_dir_all(&dir));
        let db_path = dir.join("test.db");
        drop(std::fs::remove_file(&db_path));
        (dir, db_path)
    }

    fn cleanup(dir: &std::path::Path, db_path: &std::path::Path) {
        drop(std::fs::remove_file(db_path));
        // WAL/SHM files.
        drop(std::fs::remove_file(db_path.with_extension("db-wal")));
        drop(std::fs::remove_file(db_path.with_extension("db-shm")));
        drop(std::fs::remove_dir(dir));
    }

    // -----------------------------------------------------------------------
    // Migration: v0 -> v3
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v0_to_v3() {
        let (dir, db_path) = test_db("migrate_v0");

        // Create v0 database with one row.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V0_SCHEMA).unwrap();
            conn.execute(V0_INSERT, rusqlite::params![rusqlite::types::Null])
                .unwrap();
        }

        // Open via ResultsDb — triggers all migrations.
        let db = ResultsDb::open(&db_path).expect("open should migrate v0 to v3");

        // Row is preserved and queryable.
        let rows = db
            .query(&QueryFilter {
                commit: Some(String::from("aabb")),
                command: None,
                variant: None,
                dataset: None,
                limit: 10,
            })
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, "bench read");
        assert_eq!(rows[0].elapsed_ms, 1234);

        // UUID was backfilled.
        assert!(!rows[0].uuid.is_empty(), "uuid should be backfilled");

        // project defaults to pbfhogg.
        assert_eq!(rows[0].project, "pbfhogg");

        // Schema version is current.
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Child tables exist (can query without error).
        db.conn
            .execute_batch("SELECT COUNT(*) FROM run_distribution")
            .unwrap();
        db.conn
            .execute_batch("SELECT COUNT(*) FROM run_kv")
            .unwrap();
        db.conn
            .execute_batch("SELECT COUNT(*) FROM hotpath_functions")
            .unwrap();
        db.conn
            .execute_batch("SELECT COUNT(*) FROM hotpath_threads")
            .unwrap();

        // New columns exist.
        assert!(has_column(&db.conn, "runs", "peak_rss_mb"));
        assert!(has_column(&db.conn, "runs", "project"));
        assert!(has_column(&db.conn, "runs", "cli_args"));
        assert!(has_column(&db.conn, "runs", "metadata"));

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v2 -> v3 with distribution JSON
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v2_distribution_json() {
        let (dir, db_path) = test_db("migrate_v2_dist");

        let extra = r#"{"samples":10,"min_ms":100,"p50_ms":150,"p95_ms":200,"max_ms":250,"output_bytes":999}"#;

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(V2_INSERT, rusqlite::params![extra, rusqlite::types::Null])
                .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate v2 to v3");

        // project column added with default.
        let rows = db
            .query(&QueryFilter {
                commit: Some(String::from("aabb")),
                command: None,
                variant: None,
                dataset: None,
                limit: 10,
            })
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project, "pbfhogg");

        // Distribution migrated to child table.
        let dist: (i64, i64, i64, i64, i64) = db.conn.query_row(
            "SELECT samples, min_ms, p50_ms, p95_ms, max_ms FROM run_distribution WHERE run_id = 1",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        ).expect("distribution row should exist");
        assert_eq!(dist, (10, 100, 150, 200, 250));

        // Extra kv (output_bytes) migrated to run_kv.
        let val: i64 = db
            .conn
            .query_row(
                "SELECT value_int FROM run_kv WHERE run_id = 1 AND key = 'output_bytes'",
                [],
                |r| r.get(0),
            )
            .expect("output_bytes kv should exist");
        assert_eq!(val, 999);

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v2 -> v3 with hotpath JSON
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v2_hotpath_json() {
        let (dir, db_path) = test_db("migrate_v2_hotpath");

        let extra = r#"{
            "functions_timing": {
                "description": "wall-clock timing",
                "data": [
                    {"name": "parse_header", "calls": 100, "avg": "1.2ms", "total": "120ms", "percent_total": "60%"},
                    {"name": "parse_body", "calls": 50, "avg": "2.0ms", "total": "100ms", "percent_total": "40%"}
                ]
            },
            "threads": {
                "rss_bytes": "1048576",
                "data": [
                    {"name": "main", "status": "running", "cpu_percent": "95%"}
                ]
            }
        }"#;

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(V2_INSERT, rusqlite::params![extra, rusqlite::types::Null])
                .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate hotpath");

        // Hotpath functions migrated.
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM hotpath_functions WHERE run_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "should have 2 hotpath function rows");

        // Check first function.
        let (name, calls, section): (String, i64, String) = db.conn.query_row(
            "SELECT name, calls, section FROM hotpath_functions WHERE run_id = 1 AND ordinal = 0",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(name, "parse_header");
        assert_eq!(calls, 100);
        assert_eq!(section, "timing");

        // Thread data migrated.
        let thread_name: String = db
            .conn
            .query_row(
                "SELECT name FROM hotpath_threads WHERE run_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(thread_name, "main");

        // Thread summary kv migrated.
        let rss: String = db
            .conn
            .query_row(
                "SELECT value_text FROM run_kv WHERE run_id = 1 AND key = 'threads.rss_bytes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rss, "1048576");

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v2 -> v3 with metadata JSON
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v2_metadata_json() {
        let (dir, db_path) = test_db("migrate_v2_meta");

        let metadata = r#"{"compression":"zlib","io_mode":"buffered"}"#;

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(
                V2_INSERT,
                rusqlite::params![rusqlite::types::Null, metadata],
            )
            .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate metadata");

        // Metadata migrated to run_kv with meta. prefix.
        let compression: String = db
            .conn
            .query_row(
                "SELECT value_text FROM run_kv WHERE run_id = 1 AND key = 'meta.compression'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(compression, "zlib");

        let io_mode: String = db
            .conn
            .query_row(
                "SELECT value_text FROM run_kv WHERE run_id = 1 AND key = 'meta.io_mode'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(io_mode, "buffered");

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v3 -> v4 variant renames
    // -----------------------------------------------------------------------

    /// v3 schema (full current schema minus v4 changes).
    const V3_SCHEMA: &str = "\
        CREATE TABLE runs (
            id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL, hostname TEXT NOT NULL,
            [commit] TEXT NOT NULL, subject TEXT NOT NULL, command TEXT NOT NULL,
            variant TEXT, input_file TEXT, input_mb REAL, elapsed_ms INTEGER NOT NULL,
            peak_rss_mb REAL, cargo_features TEXT, cargo_profile TEXT DEFAULT 'release',
            kernel TEXT, cpu_governor TEXT, avail_memory_mb INTEGER,
            storage_notes TEXT, extra TEXT, uuid TEXT, cli_args TEXT, metadata TEXT,
            project TEXT NOT NULL DEFAULT 'pbfhogg'
        )";

    const V3_INSERT: &str = "\
        INSERT INTO runs (timestamp, hostname, [commit], subject, command, variant,
            input_file, input_mb, elapsed_ms, uuid, project)
        VALUES ('2026-01-01 00:00:00', 'testhost', 'aabb', 'test', ?1, ?2,
            'denmark.osm.pbf', 42.5, 1234, 'uuid1', ?3)";

    #[test]
    fn migrate_v3_to_v4_renames_variants() {
        let (dir, db_path) = test_db("migrate_v3_v4");

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V3_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();

            // pbfhogg rows that should be renamed.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "tags-count", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "node-stats", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "removeid", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "derive-changes", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "merge-pbf", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench blob-filter", "node-stats+raw", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "merge-zlib", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "tags-count/alloc", "pbfhogg"],
            )
            .unwrap();

            // pbfhogg row that should NOT be renamed (already correct).
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "inspect", "pbfhogg"],
            )
            .unwrap();

            // elivagar row with same old variant name — should NOT be touched.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench self", "tags-count", "elivagar"],
            )
            .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate v3 to v4");

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Helper to query variant by row id.
        let variant_of = |id: i64| -> String {
            db.conn
                .query_row("SELECT variant FROM runs WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };

        assert_eq!(variant_of(1), "inspect-tags"); // tags-count -> inspect-tags
        assert_eq!(variant_of(2), "inspect-nodes"); // node-stats -> inspect-nodes
        assert_eq!(variant_of(3), "getid-invert"); // removeid -> getid-invert
        assert_eq!(variant_of(4), "diff-osc"); // derive-changes -> diff-osc
        assert_eq!(variant_of(5), "cat-dedupe"); // merge-pbf -> cat-dedupe
        assert_eq!(variant_of(6), "inspect-nodes+raw"); // node-stats+raw -> inspect-nodes+raw
        assert_eq!(variant_of(7), "apply-changes-zlib"); // merge-zlib -> apply-changes-zlib
        assert_eq!(variant_of(8), "inspect-tags/alloc"); // tags-count/alloc -> inspect-tags/alloc
        assert_eq!(variant_of(9), "inspect"); // unchanged
        assert_eq!(variant_of(10), "tags-count"); // elivagar row: NOT renamed

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Fresh database gets correct schema version
    // -----------------------------------------------------------------------

    #[test]
    fn fresh_db_has_correct_schema_version() {
        let (dir, db_path) = test_db("fresh_version");

        let db = ResultsDb::open(&db_path).expect("open fresh db");

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        drop(db);
        cleanup(&dir, &db_path);
    }
}
