//! Comparison queries for the results database.

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

}
