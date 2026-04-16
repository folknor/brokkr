//! Comparison queries for the results database.

use super::CompareResult;
use super::ResultsDb;
use super::query::{load_children, query_commit_filtered};
use super::schema::SELECT_COLS;
use crate::error::DevError;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl ResultsDb {
    /// Query two commits for side-by-side comparison. Each commit is matched
    /// by prefix. Optional command/mode/dataset filters narrow the results.
    /// `dataset` is a substring match against the `input_file` column.
    /// Loads kv + hotpath children for each row (needed for output_bytes and diffs).
    pub fn query_compare(
        &self,
        a: &str,
        b: &str,
        command: Option<&str>,
        mode: Option<&str>,
        dataset: Option<&str>,
    ) -> Result<(Vec<super::StoredRow>, Vec<super::StoredRow>), DevError> {
        let mut clauses = vec!["[commit] LIKE ?1||'%'".to_owned()];
        let mut params: Vec<String> = Vec::new();
        // ?1 is the commit, filled per-call below.
        params.push(String::new());
        if let Some(cmd) = command {
            params.push(cmd.to_owned());
            clauses.push(format!("command LIKE '%'||?{}||'%'", params.len()));
        }
        if let Some(v) = mode {
            params.push(v.to_owned());
            clauses.push(format!("mode LIKE '%'||?{}||'%'", params.len()));
        }
        if let Some(d) = dataset {
            params.push(d.to_owned());
            clauses.push(format!("input_file LIKE '%'||?{}||'%'", params.len()));
        }
        let sql = format!(
            "SELECT {SELECT_COLS} FROM runs WHERE {} ORDER BY command, mode, id DESC",
            clauses.join(" AND ")
        );
        let mut rows_a = query_commit_filtered(&self.conn, &sql, a, &params)?;
        let mut rows_b = query_commit_filtered(&self.conn, &sql, b, &params)?;
        for row in rows_a.iter_mut().chain(rows_b.iter_mut()) {
            load_children(&self.conn, row)?;
        }
        Ok((rows_a, rows_b))
    }

    /// Find the two most recent distinct commits and compare them.
    /// Optional command/mode/dataset filters narrow the search.
    pub fn query_compare_last(
        &self,
        command: Option<&str>,
        mode: Option<&str>,
        dataset: Option<&str>,
    ) -> Result<Option<CompareResult>, DevError> {
        // Find two most recent distinct commits matching the filters.
        let mut clauses = Vec::new();
        let mut params: Vec<String> = Vec::new();
        if let Some(cmd) = command {
            params.push(cmd.to_owned());
            clauses.push(format!("command LIKE '%'||?{}||'%'", params.len()));
        }
        if let Some(v) = mode {
            params.push(v.to_owned());
            clauses.push(format!("mode LIKE '%'||?{}||'%'", params.len()));
        }
        if let Some(d) = dataset {
            params.push(d.to_owned());
            clauses.push(format!("input_file LIKE '%'||?{}||'%'", params.len()));
        }
        let mut sql = "SELECT [commit] FROM runs".to_owned();
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" GROUP BY [commit] ORDER BY MAX(id) DESC LIMIT 2");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
            .iter()
            .map(|p| p as &dyn rusqlite::types::ToSql)
            .collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let commits: Vec<String> = stmt
            .query_map(param_refs.as_slice(), |row| row.get(0))?
            .filter_map(Result::ok)
            .collect();

        if commits.len() < 2 {
            return Ok(None);
        }

        let (rows_a, rows_b) =
            self.query_compare(&commits[1], &commits[0], command, mode, dataset)?;
        Ok(Some((
            commits[1].clone(),
            rows_a,
            commits[0].clone(),
            rows_b,
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::db::{QueryFilter, ResultsDb, RunRow};

    fn unique_db(name: &str) -> PathBuf {
        let cwd = std::env::current_dir().expect("cwd");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        cwd.join(".brokkr")
            .join("test-artifacts")
            .join(format!("compare-{name}-{}-{stamp}.db", std::process::id()))
    }

    fn make_row(commit: &str, command: &str, mode: &str) -> RunRow {
        RunRow {
            hostname: String::from("testhost"),
            commit: String::from(commit),
            subject: String::from("test subject"),
            command: String::from(command),
            mode: Some(String::from(mode)),
            input_file: Some(String::from("in.osm.pbf")),
            input_mb: Some(1.0),
            elapsed_ms: 100,
            peak_rss_mb: None,
            cargo_features: None,
            cargo_profile: String::from("release"),
            kernel: None,
            cpu_governor: None,
            avail_memory_mb: None,
            storage_notes: None,
            cli_args: None,
            brokkr_args: None,
            project: String::from("pbfhogg"),
            stop_marker: None,
            kv: vec![],
            distribution: None,
            hotpath: None,
        }
    }

    #[test]
    fn compare_last_returns_none_with_fewer_than_two_commits() {
        let db_path = unique_db("none");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let db = ResultsDb::open(&db_path).expect("open");
        db.insert(&make_row("aaaa1111", "bench read", "sequential"))
            .expect("insert");

        let result = db.query_compare_last(None, None, None).expect("query");
        assert!(result.is_none());

        drop(db);
        drop(std::fs::remove_file(&db_path));
    }

    #[test]
    fn compare_last_respects_command_filter() {
        let db_path = unique_db("filter");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let db = ResultsDb::open(&db_path).expect("open");

        db.insert(&make_row("aaaa1111", "bench read", "sequential"))
            .expect("insert");
        db.insert(&make_row("bbbb2222", "bench read", "sequential"))
            .expect("insert");
        db.insert(&make_row("cccc3333", "bench write", "sync-zlib"))
            .expect("insert");

        let compared = db
            .query_compare_last(Some("bench read"), None, None)
            .expect("compare")
            .expect("two commits");
        assert_eq!(compared.0, "aaaa1111");
        assert_eq!(compared.2, "bbbb2222");
        assert_eq!(compared.1.len(), 1);
        assert_eq!(compared.3.len(), 1);
        assert_eq!(compared.1[0].command, "bench read");
        assert_eq!(compared.3[0].command, "bench read");

        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: Some(String::from("bench write")),
                mode: None,
                dataset: None,
                meta: vec![],
                cli_args: None,
                brokkr_args: None,
                limit: 10,
            })
            .expect("query");
        assert_eq!(rows.len(), 1);

        drop(db);
        drop(std::fs::remove_file(&db_path));
    }
}
