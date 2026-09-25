//! Comparison queries for the results database.

use super::ResultsDb;
use super::like::{ESCAPE, prefix_pattern, require_prefix};
use super::query::{contains_expr, load_children, push_grep_clauses, query_commit_filtered};
use super::schema::SELECT_COLS;
use crate::error::DevError;

/// Filters narrowing a `--compare` query, on both sides equally.
///
/// A struct rather than positional arguments because the list outgrew a
/// readable signature once `grep`/`grep_v` joined it.
#[derive(Default)]
pub struct CompareFilter<'a> {
    pub command: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub dataset: Option<&'a str>,
    /// Invocation substrings that must be present (AND). "Invocation" spans
    /// `cli_args`, `brokkr_args`, and the captured env - see `push_grep_clauses`.
    pub grep: &'a [String],
    /// Invocation substrings that exclude a row (any hit).
    pub grep_v: &'a [String],
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl ResultsDb {
    /// Query two commits for side-by-side comparison. Each commit is matched
    /// by prefix. Optional command/mode/dataset filters narrow the results.
    /// `dataset` is a substring match against the `input_file` column.
    /// Loads kv + hotpath children for each row (needed for output_bytes and diffs).
    ///
    /// `grep`/`grep_v` apply here exactly as they do to `brokkr results`. They
    /// used to be silently dropped on this path, which mattered most for the
    /// case they exist to serve: `--compare` is the natural A/B tool, so
    /// `--grep-v` narrowing a comparison to one arm has to actually narrow it.
    pub fn query_compare(
        &self,
        a: &str,
        b: &str,
        filter: &CompareFilter<'_>,
    ) -> Result<(Vec<super::StoredRow>, Vec<super::StoredRow>), DevError> {
        // Both commits are user-supplied prefixes: an empty one would pair
        // every row in the database against the other side.
        require_prefix(a, "commit")?;
        require_prefix(b, "commit")?;
        let mut clauses = vec![format!("[commit] LIKE ?1 {ESCAPE}")];
        let mut params: Vec<String> = Vec::new();
        // ?1 is the commit pattern, filled per-call below.
        params.push(String::new());
        if let Some(cmd) = filter.command {
            clauses.push(contains_expr(&mut params, "command", cmd));
        }
        if let Some(v) = filter.mode {
            clauses.push(contains_expr(&mut params, "mode", v));
        }
        if let Some(d) = filter.dataset {
            clauses.push(contains_expr(&mut params, "input_file", d));
        }
        push_grep_clauses(&mut clauses, &mut params, filter.grep, filter.grep_v);
        let sql = format!(
            "SELECT {SELECT_COLS} FROM runs WHERE {} ORDER BY command, mode, id DESC",
            clauses.join(" AND ")
        );
        let mut rows_a = query_commit_filtered(&self.conn, &sql, &prefix_pattern(a), &params)?;
        let mut rows_b = query_commit_filtered(&self.conn, &sql, &prefix_pattern(b), &params)?;
        for row in rows_a.iter_mut().chain(rows_b.iter_mut()) {
            load_children(&self.conn, row)?;
        }
        Ok((rows_a, rows_b))
    }

}
