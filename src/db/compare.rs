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
            cargo_profile: crate::build::CargoProfile::Release,
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

}
