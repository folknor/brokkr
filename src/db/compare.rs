//! Comparison queries for the results database.

use super::QueryFilter;
use super::ResultsDb;
use super::like::require_prefix;
use super::query::{
    commit_match_expr, load_children, push_row_filter_clauses, query_commit_filtered,
};
use super::schema::SELECT_COLS;
use crate::error::DevError;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl ResultsDb {
    /// Query two commits for side-by-side comparison. Each commit is matched
    /// by prefix. The supplied row filters narrow both sides equally.
    /// Loads kv + hotpath children for each row (needed for output_bytes and diffs).
    /// `filter.limit` is retained for the caller to apply after pairing, so it
    /// cannot cut one arm before its matching row is found.
    ///
    pub fn query_compare(
        &self,
        a: &str,
        b: &str,
        filter: &QueryFilter,
    ) -> Result<(Vec<super::StoredRow>, Vec<super::StoredRow>), DevError> {
        // Both commits are user-supplied prefixes: an empty one would pair
        // every row in the database against the other side.
        require_prefix(a, "commit")?;
        require_prefix(b, "commit")?;
        if filter.commit.is_some() {
            return Err(DevError::Config(
                "--commit cannot be combined with --compare".into(),
            ));
        }
        let mut clauses = vec![commit_match_expr(1, 2)];
        let mut params: Vec<String> = Vec::new();
        // ?1/?2 are the commit bindings, filled per-call below.
        params.push(String::new());
        params.push(String::new());
        push_row_filter_clauses(&mut clauses, &mut params, filter);
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
