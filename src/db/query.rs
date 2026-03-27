//! Query operations for the results database.

use super::ResultsDb;
use super::schema::SELECT_COLS;
use super::{
    Distribution, HotpathData, HotpathFunction, HotpathThread, KvPair, KvValue, QueryFilter,
    StoredRow,
};
use crate::error::DevError;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl ResultsDb {
    /// Query rows by UUID prefix. Loads all child data (kv, distribution, hotpath).
    pub fn query_by_uuid(&self, prefix: &str) -> Result<Vec<StoredRow>, DevError> {
        let sql =
            format!("SELECT {SELECT_COLS} FROM runs WHERE uuid LIKE ?1||'%' ORDER BY id DESC");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![prefix], map_stored_row)?;
        let mut result = collect_rows(rows)?;
        for row in &mut result {
            load_children(&self.conn, row)?;
        }
        Ok(result)
    }

    /// Query rows with optional filters. Commit matches by prefix (LIKE).
    /// Results ordered by id descending, limited to `filter.limit`.
    pub fn query(&self, filter: &QueryFilter) -> Result<Vec<StoredRow>, DevError> {
        let (sql, params) = build_query_sql(filter);
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
            .iter()
            .map(|p| p as &dyn rusqlite::types::ToSql)
            .collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), map_stored_row)?;
        collect_rows(rows)
    }
}

// ---------------------------------------------------------------------------
// Helpers shared with compare module
// ---------------------------------------------------------------------------

pub(super) fn query_commit_filtered(
    conn: &rusqlite::Connection,
    sql: &str,
    commit: &str,
    params: &[String],
) -> Result<Vec<StoredRow>, DevError> {
    let mut bound = params.to_vec();
    bound[0] = commit.to_owned();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = bound
        .iter()
        .map(|p| p as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), map_stored_row)?;
    collect_rows(rows)
}

pub(super) fn load_children(
    conn: &rusqlite::Connection,
    row: &mut StoredRow,
) -> Result<(), DevError> {
    row.distribution = load_distribution(conn, row.id)?;
    row.kv = load_kv(conn, row.id)?;
    row.hotpath = load_hotpath(conn, row.id, &row.kv)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn collect_rows(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<StoredRow>>,
) -> Result<Vec<StoredRow>, DevError> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn map_stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    Ok(StoredRow {
        id: row.get("id")?,
        timestamp: row.get("timestamp")?,
        hostname: row.get("hostname")?,
        commit: row.get("commit")?,
        subject: row.get("subject")?,
        command: row.get("command")?,
        variant: row.get::<_, Option<String>>("variant")?.unwrap_or_default(),
        input_file: row
            .get::<_, Option<String>>("input_file")?
            .unwrap_or_default(),
        input_mb: row.get("input_mb")?,
        elapsed_ms: row.get("elapsed_ms")?,
        peak_rss_mb: row.get("peak_rss_mb")?,
        cargo_features: row
            .get::<_, Option<String>>("cargo_features")?
            .unwrap_or_default(),
        cargo_profile: row
            .get::<_, Option<String>>("cargo_profile")?
            .unwrap_or_default(),
        kernel: row.get::<_, Option<String>>("kernel")?.unwrap_or_default(),
        cpu_governor: row
            .get::<_, Option<String>>("cpu_governor")?
            .unwrap_or_default(),
        avail_memory_mb: row.get("avail_memory_mb")?,
        storage_notes: row
            .get::<_, Option<String>>("storage_notes")?
            .unwrap_or_default(),
        uuid: row.get::<_, Option<String>>("uuid")?.unwrap_or_default(),
        cli_args: row
            .get::<_, Option<String>>("cli_args")?
            .unwrap_or_default(),
        project: row
            .get::<_, Option<String>>("project")?
            .unwrap_or_else(|| "pbfhogg".to_owned()),
        kv: Vec::new(),
        distribution: None,
        hotpath: None,
    })
}

fn build_query_sql(filter: &QueryFilter) -> (String, Vec<String>) {
    let mut clauses = Vec::new();
    let mut params: Vec<String> = Vec::new();

    if let Some(ref c) = filter.commit {
        params.push(c.clone());
        clauses.push(format!("[commit] LIKE ?{}||'%'", params.len()));
    }
    if let Some(ref cmd) = filter.command {
        params.push(cmd.clone());
        clauses.push(format!("command LIKE '%'||?{}||'%'", params.len()));
    }
    if let Some(ref v) = filter.variant {
        params.push(v.clone());
        clauses.push(format!("variant LIKE '%'||?{}||'%'", params.len()));
    }

    let mut sql = format!("SELECT {SELECT_COLS} FROM runs");
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }

    // Limit param is appended as the next positional parameter.
    params.push(filter.limit.to_string());
    sql.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", params.len()));

    (sql, params)
}

fn load_distribution(
    conn: &rusqlite::Connection,
    run_id: i64,
) -> Result<Option<Distribution>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT samples, min_ms, p50_ms, p95_ms, max_ms FROM run_distribution WHERE run_id = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok(Distribution {
            samples: row.get(0)?,
            min_ms: row.get(1)?,
            p50_ms: row.get(2)?,
            p95_ms: row.get(3)?,
            max_ms: row.get(4)?,
        })
    })?;
    match rows.next() {
        Some(Ok(dist)) => Ok(Some(dist)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

fn load_kv(conn: &rusqlite::Connection, run_id: i64) -> Result<Vec<KvPair>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT key, value_int, value_real, value_text FROM run_kv WHERE run_id = ?1 ORDER BY key",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        let key: String = row.get(0)?;
        let vi: Option<i64> = row.get(1)?;
        let vr: Option<f64> = row.get(2)?;
        let vt: Option<String> = row.get(3)?;
        let value = if let Some(v) = vi {
            KvValue::Int(v)
        } else if let Some(v) = vr {
            KvValue::Real(v)
        } else {
            KvValue::Text(vt.unwrap_or_default())
        };
        Ok(KvPair { key, value })
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

fn load_hotpath(
    conn: &rusqlite::Connection,
    run_id: i64,
    kv: &[KvPair],
) -> Result<Option<HotpathData>, DevError> {
    let functions = load_hotpath_functions(conn, run_id)?;
    let threads = load_hotpath_threads(conn, run_id)?;
    if functions.is_empty() && threads.is_empty() {
        return Ok(None);
    }
    // Thread summary stats are stored in run_kv with "threads." prefix.
    let thread_summary: Vec<KvPair> = kv
        .iter()
        .filter(|p| p.key.starts_with("threads."))
        .cloned()
        .collect();
    Ok(Some(HotpathData {
        functions,
        threads,
        thread_summary,
    }))
}

fn load_hotpath_functions(
    conn: &rusqlite::Connection,
    run_id: i64,
) -> Result<Vec<HotpathFunction>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99 \
         FROM hotpath_functions WHERE run_id = ?1 ORDER BY section, ordinal"
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok(HotpathFunction {
            section: row.get(0)?,
            description: row.get(1)?,
            ordinal: row.get(2)?,
            name: row.get(3)?,
            calls: row.get(4)?,
            avg: row.get(5)?,
            total: row.get(6)?,
            percent_total: row.get(7)?,
            p50: row.get(8)?,
            p95: row.get(9)?,
            p99: row.get(10)?,
        })
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

fn load_hotpath_threads(
    conn: &rusqlite::Connection,
    run_id: i64,
) -> Result<Vec<HotpathThread>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT name, status, cpu_percent, cpu_percent_max, cpu_percent_avg, \
         alloc_bytes, dealloc_bytes, mem_diff \
         FROM hotpath_threads WHERE run_id = ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok(HotpathThread {
            name: row.get(0)?,
            status: row.get(1)?,
            cpu_percent: row.get(2)?,
            cpu_percent_max: row.get(3)?,
            cpu_percent_avg: row.get(4)?,
            alloc_bytes: row.get(5)?,
            dealloc_bytes: row.get(6)?,
            mem_diff: row.get(7)?,
        })
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ResultsDb, RunRow};

    fn test_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("brokkr_test_{name}"));
        drop(std::fs::create_dir_all(&dir));
        let db_path = dir.join("test.db");
        drop(std::fs::remove_file(&db_path));
        (dir, db_path)
    }

    fn cleanup(dir: &std::path::Path, db_path: &std::path::Path) {
        drop(std::fs::remove_file(db_path));
        drop(std::fs::remove_file(db_path.with_extension("db-wal")));
        drop(std::fs::remove_file(db_path.with_extension("db-shm")));
        drop(std::fs::remove_dir(dir));
    }

    // -----------------------------------------------------------------------
    // build_query_sql
    // -----------------------------------------------------------------------

    #[test]
    fn build_query_sql_no_filters() {
        let filter = QueryFilter {
            commit: None,
            command: None,
            variant: None,
            limit: 50,
        };
        let (sql, params) = build_query_sql(&filter);

        // No WHERE clause, just ORDER BY + LIMIT
        assert!(!sql.contains("WHERE"), "should have no WHERE clause");
        assert!(sql.contains("ORDER BY id DESC LIMIT ?1"));
        assert_eq!(params.len(), 1);
        assert_eq!(params[0], "50");
    }

    #[test]
    fn build_query_sql_all_filters() {
        let filter = QueryFilter {
            commit: Some(String::from("abc123")),
            command: Some(String::from("read")),
            variant: Some(String::from("mmap")),
            limit: 10,
        };
        let (sql, params) = build_query_sql(&filter);

        assert!(sql.contains("WHERE"));
        assert!(sql.contains("[commit] LIKE ?1||'%'"), "commit should be ?1");
        assert!(
            sql.contains("command LIKE '%'||?2||'%'"),
            "command should be ?2 contains"
        );
        assert!(
            sql.contains("variant LIKE '%'||?3||'%'"),
            "variant should be ?3 contains"
        );
        assert!(sql.contains("LIMIT ?4"), "limit should be ?4");
        assert_eq!(params.len(), 4);
        assert_eq!(params[0], "abc123");
        assert_eq!(params[1], "read");
        assert_eq!(params[2], "mmap");
        assert_eq!(params[3], "10");
    }

    #[test]
    fn build_query_sql_commit_only() {
        let filter = QueryFilter {
            commit: Some(String::from("deadbeef")),
            command: None,
            variant: None,
            limit: 25,
        };
        let (sql, params) = build_query_sql(&filter);

        assert!(sql.contains("[commit] LIKE ?1||'%'"));
        assert!(sql.contains("LIMIT ?2"));
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], "deadbeef");
        assert_eq!(params[1], "25");
    }

    #[test]
    fn build_query_sql_command_and_variant_no_commit() {
        let filter = QueryFilter {
            commit: None,
            command: Some(String::from("write")),
            variant: Some(String::from("direct")),
            limit: 5,
        };
        let (sql, params) = build_query_sql(&filter);

        // Without commit, command becomes ?1, variant ?2, limit ?3
        assert!(sql.contains("command LIKE '%'||?1||'%'"));
        assert!(sql.contains("variant LIKE '%'||?2||'%'"));
        assert!(sql.contains("LIMIT ?3"));
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], "write");
        assert_eq!(params[1], "direct");
        assert_eq!(params[2], "5");
    }

    #[test]
    fn build_query_sql_selects_correct_columns() {
        let filter = QueryFilter {
            commit: None,
            command: None,
            variant: None,
            limit: 1,
        };
        let (sql, _) = build_query_sql(&filter);
        assert!(sql.starts_with(&format!("SELECT {SELECT_COLS} FROM runs")));
    }

    // -----------------------------------------------------------------------
    // Query: command and variant contains match
    // -----------------------------------------------------------------------

    #[test]
    fn query_command_contains_match() {
        let (dir, db_path) = test_db("query_cmd_contains");

        let db = ResultsDb::open(&db_path).expect("open");

        let make_row = |cmd: &str, variant: &str| RunRow {
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test"),
            command: String::from(cmd),
            variant: Some(String::from(variant)),
            input_file: None,
            input_mb: None,
            elapsed_ms: 100,
            peak_rss_mb: None,
            cargo_features: None,
            cargo_profile: String::from("release"),
            kernel: None,
            cpu_governor: None,
            avail_memory_mb: None,
            storage_notes: None,
            cli_args: None,
            project: String::from("test"),
            kv: vec![],
            distribution: None,
            hotpath: None,
        };

        db.insert(&make_row("bench merge", "buffered+zlib"))
            .unwrap();
        db.insert(&make_row("bench merge", "buffered+none"))
            .unwrap();
        db.insert(&make_row("bench read", "mmap")).unwrap();

        // "merge" matches "bench merge" rows only.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: Some(String::from("merge")),
                variant: None,
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 2, "merge should match 2 bench merge rows");

        // "bench" matches all 3 rows.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: Some(String::from("bench")),
                variant: None,
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 3, "bench should match all rows");

        // "read" matches only bench read.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: Some(String::from("read")),
                variant: None,
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, "bench read");

        // Full exact value still works.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: Some(String::from("bench merge")),
                variant: None,
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);

        drop(db);
        cleanup(&dir, &db_path);
    }

    #[test]
    fn query_variant_contains_match() {
        let (dir, db_path) = test_db("query_var_contains");

        let db = ResultsDb::open(&db_path).expect("open");

        let make_row = |variant: &str| RunRow {
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test"),
            command: String::from("bench merge"),
            variant: Some(String::from(variant)),
            input_file: None,
            input_mb: None,
            elapsed_ms: 100,
            peak_rss_mb: None,
            cargo_features: None,
            cargo_profile: String::from("release"),
            kernel: None,
            cpu_governor: None,
            avail_memory_mb: None,
            storage_notes: None,
            cli_args: None,
            project: String::from("test"),
            kv: vec![],
            distribution: None,
            hotpath: None,
        };

        db.insert(&make_row("buffered+zlib")).unwrap();
        db.insert(&make_row("buffered+none")).unwrap();
        db.insert(&make_row("direct+zlib")).unwrap();

        // "zlib" matches buffered+zlib and direct+zlib.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                variant: Some(String::from("zlib")),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 2, "zlib should match 2 rows");

        // "buffered" matches both buffered variants.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                variant: Some(String::from("buffered")),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 2, "buffered should match 2 rows");

        // "none" matches only buffered+none.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                variant: Some(String::from("none")),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].variant, "buffered+none");

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Row hydration: load_children with distribution + kv + hotpath
    // -----------------------------------------------------------------------

    #[test]
    fn load_children_hydrates_all_child_data() {
        use crate::db::{Distribution, HotpathData, HotpathFunction, HotpathThread, KvPair};

        let (dir, db_path) = test_db("hydration");
        let db = ResultsDb::open(&db_path).expect("open");

        let row = RunRow {
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test"),
            command: String::from("hotpath"),
            variant: Some(String::from("default")),
            input_file: None,
            input_mb: None,
            elapsed_ms: 500,
            peak_rss_mb: None,
            cargo_features: None,
            cargo_profile: String::from("release"),
            kernel: None,
            cpu_governor: None,
            avail_memory_mb: None,
            storage_notes: None,
            cli_args: None,
            project: String::from("test"),
            kv: vec![
                KvPair::int("elapsed_ms", 500),
                KvPair::text("threads.rss_bytes", "1024"),
            ],
            distribution: Some(Distribution {
                samples: 5,
                min_ms: 100,
                p50_ms: 110,
                p95_ms: 130,
                max_ms: 150,
            }),
            hotpath: Some(HotpathData {
                functions: vec![HotpathFunction {
                    section: String::from("timing"),
                    description: Some(String::from("main")),
                    ordinal: 0,
                    name: String::from("process"),
                    calls: Some(10),
                    avg: Some(String::from("50ms")),
                    total: Some(String::from("500ms")),
                    percent_total: Some(String::from("100%")),
                    p50: None,
                    p95: None,
                    p99: None,
                }],
                threads: vec![HotpathThread {
                    name: String::from("main"),
                    status: Some(String::from("running")),
                    cpu_percent: Some(String::from("95%")),
                    cpu_percent_max: None,
                    cpu_percent_avg: None,
                    alloc_bytes: None,
                    dealloc_bytes: None,
                    mem_diff: None,
                }],
                thread_summary: vec![KvPair::text("threads.rss_bytes", "1024")],
            }),
        };

        let short = db.insert(&row).expect("insert");

        // query_by_uuid triggers load_children for each row.
        let rows = db.query_by_uuid(&short).expect("query_by_uuid");
        assert_eq!(rows.len(), 1);
        let r = &rows[0];

        // Distribution.
        let dist = r
            .distribution
            .as_ref()
            .expect("distribution should be loaded");
        assert_eq!(dist.samples, 5);
        assert_eq!(dist.min_ms, 100);
        assert_eq!(dist.p50_ms, 110);
        assert_eq!(dist.p95_ms, 130);
        assert_eq!(dist.max_ms, 150);

        // KV pairs.
        assert!(r.kv.len() >= 2, "should have at least 2 kv pairs");
        assert!(
            r.kv.iter().any(|p| p.key == "elapsed_ms"),
            "should have elapsed_ms kv"
        );
        assert!(
            r.kv.iter().any(|p| p.key == "threads.rss_bytes"),
            "should have threads.rss_bytes kv"
        );

        // Hotpath.
        let hp = r.hotpath.as_ref().expect("hotpath should be loaded");
        assert_eq!(hp.functions.len(), 1);
        assert_eq!(hp.functions[0].name, "process");
        assert_eq!(hp.functions[0].section, "timing");
        assert_eq!(hp.threads.len(), 1);
        assert_eq!(hp.threads[0].name, "main");

        // Thread summary should be populated from KV (not a separate load).
        assert_eq!(hp.thread_summary.len(), 1);
        assert_eq!(hp.thread_summary[0].key, "threads.rss_bytes");

        drop(db);
        cleanup(&dir, &db_path);
    }
}
