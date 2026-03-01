//! Comparison queries for the results database.

use crate::error::DevError;
use super::ResultsDb;
use super::schema::SELECT_COLS;
use super::query::{query_commit_filtered, load_children};
use super::CompareResult;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl ResultsDb {
    /// Query two commits for side-by-side comparison. Each commit is matched
    /// by prefix. Optional command/variant filters narrow the results.
    /// Loads kv + hotpath children for each row (needed for output_bytes and diffs).
    pub fn query_compare(
        &self,
        a: &str,
        b: &str,
        command: Option<&str>,
        variant: Option<&str>,
    ) -> Result<(Vec<super::StoredRow>, Vec<super::StoredRow>), DevError> {
        let mut clauses = vec!["[commit] LIKE ?1||'%'".to_owned()];
        let mut params: Vec<String> = Vec::new();
        // ?1 is the commit, filled per-call below.
        params.push(String::new());
        if let Some(cmd) = command {
            params.push(cmd.to_owned());
            clauses.push(format!("command LIKE '%'||?{}||'%'", params.len()));
        }
        if let Some(v) = variant {
            params.push(v.to_owned());
            clauses.push(format!("variant LIKE '%'||?{}||'%'", params.len()));
        }
        let sql = format!(
            "SELECT {SELECT_COLS} FROM runs WHERE {} ORDER BY command, variant, id DESC",
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
    /// Optional command/variant filters narrow the search.
    pub fn query_compare_last(
        &self,
        command: Option<&str>,
        variant: Option<&str>,
    ) -> Result<Option<CompareResult>, DevError> {
        // Find two most recent distinct commits matching the filters.
        let mut clauses = Vec::new();
        let mut params: Vec<String> = Vec::new();
        if let Some(cmd) = command {
            params.push(cmd.to_owned());
            clauses.push(format!("command LIKE '%'||?{}||'%'", params.len()));
        }
        if let Some(v) = variant {
            params.push(v.to_owned());
            clauses.push(format!("variant LIKE '%'||?{}||'%'", params.len()));
        }
        let mut sql = "SELECT DISTINCT [commit] FROM runs".to_owned();
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY id DESC LIMIT 2");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let commits: Vec<String> = stmt
            .query_map(param_refs.as_slice(), |row| row.get(0))?
            .filter_map(Result::ok)
            .collect();

        if commits.len() < 2 {
            return Ok(None);
        }

        let (rows_a, rows_b) = self.query_compare(&commits[1], &commits[0], command, variant)?;
        Ok(Some((commits[1].clone(), rows_a, commits[0].clone(), rows_b)))
    }
}
