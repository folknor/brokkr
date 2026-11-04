//! The listing queries a reader asks of the store: which runs a filter
//! selects, in one total order, and each selected run with what the store
//! derived from it. Here rather than in the reader, so a results server
//! answers the same queries from the same SQL.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::StoreError;
use super::document::RunDocument;

/// Which runs a listing selects. Every condition ANDs; an empty filter
/// selects every run of the project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFilter {
    /// The project the runs belong to; required, so one project's history
    /// never leaks into another's.
    pub project: String,
    /// The run uuid starts with this, ASCII case ignored.
    pub uuid_prefix: Option<String>,
    /// The full commit sha starts with this, ASCII case ignored.
    pub commit_prefix: Option<String>,
    /// The bench name contains this literally, ASCII case ignored.
    pub bench: Option<String>,
    /// A captured environment variable with exactly this value.
    pub env: Vec<(String, String)>,
    /// A bound param whose value as listed (its keys joined by `,` in bound
    /// order) equals this exactly.
    pub params: Vec<(String, String)>,
    /// Each term appears literally in the run's invocation: an element of
    /// brokkr's argv, an element of any child's argv, a child's environment
    /// addition or a captured variable as `NAME=VALUE`.
    pub grep: Vec<String>,
    /// No term appears anywhere in the invocation.
    pub grep_v: Vec<String>,
    /// Only runs with a result (outcome `completed`).
    pub with_result: bool,
    /// Only runs of a clean tree.
    pub clean: bool,
}

/// One stored run with what the store derived from it. A listing loads it
/// without children (`children_loaded` false: every iteration's `children`
/// is empty because it was not read, not because none ran).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRun {
    pub doc: RunDocument,
    pub config_digest: String,
    pub pairing_key: String,
    /// The result's iteration; `None` unless the run completed.
    pub best_iteration: Option<usize>,
    pub children_loaded: bool,
}

impl StoredRun {
    /// The pairing key's algorithm tag (`bench-pair-v1`), read from the
    /// stored key itself; `None` for a key that is not a tagged array.
    pub fn pairing_algorithm(&self) -> Option<String> {
        pairing_algorithm(&self.pairing_key)
    }
}

/// A stored pairing key's algorithm tag.
fn pairing_algorithm(key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(key).ok()?;
    value.as_array()?.first()?.as_str().map(str::to_owned)
}

/// One `--grep` term's match: true when the term appears in any invocation
/// source. Binds one parameter, numbered `n`.
fn grep_expr(n: usize) -> String {
    format!(
        "(EXISTS (SELECT 1 FROM json_each(runs.argv) a WHERE instr(a.value, ?{n}) > 0)
          OR EXISTS (SELECT 1 FROM json_each(runs.captured_env) e
                     WHERE instr(e.key || '=' || e.value, ?{n}) > 0)
          OR EXISTS (SELECT 1 FROM children c, json_each(c.argv) a
                     WHERE c.run_id = runs.id AND instr(a.value, ?{n}) > 0)
          OR EXISTS (SELECT 1 FROM children c, json_each(c.env) e
                     WHERE c.run_id = runs.id AND instr(e.key || '=' || e.value, ?{n}) > 0))"
    )
}

/// The uuids `filter` selects, newest first: by start time, then uuid, one
/// total order whatever order the runs were published in.
pub(super) fn select(conn: &Connection, filter: &RunFilter) -> Result<Vec<String>, StoreError> {
    let mut params: Vec<String> = vec![filter.project.clone()];
    let mut clauses: Vec<String> = vec!["project = ?1".to_owned()];
    let bind = |params: &mut Vec<String>, value: String| {
        params.push(value);
        params.len()
    };
    if let Some(prefix) = &filter.uuid_prefix {
        let n = bind(&mut params, prefix.to_ascii_lowercase());
        clauses.push(format!("substr(uuid, 1, length(?{n})) = ?{n}"));
    }
    if let Some(prefix) = &filter.commit_prefix {
        let n = bind(&mut params, prefix.to_ascii_lowercase());
        clauses.push(format!("substr(git_commit, 1, length(?{n})) = ?{n}"));
    }
    if let Some(bench) = &filter.bench {
        let n = bind(&mut params, bench.clone());
        clauses.push(format!("instr(lower(bench), lower(?{n})) > 0"));
    }
    for (name, value) in &filter.env {
        let k = bind(&mut params, name.clone());
        let v = bind(&mut params, value.clone());
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM json_each(runs.captured_env) e
                     WHERE e.key = ?{k} AND e.type = 'text' AND e.value = ?{v})"
        ));
    }
    for (name, value) in &filter.params {
        let k = bind(&mut params, name.clone());
        let v = bind(&mut params, value.clone());
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM run_params p WHERE p.run_id = runs.id AND p.name = ?{k}
                     AND COALESCE((SELECT group_concat(v.key, ',' ORDER BY v.position)
                                   FROM run_param_values v
                                   WHERE v.run_id = p.run_id AND v.name = p.name), '') = ?{v})"
        ));
    }
    for term in &filter.grep {
        let n = bind(&mut params, term.clone());
        clauses.push(grep_expr(n));
    }
    for term in &filter.grep_v {
        let n = bind(&mut params, term.clone());
        clauses.push(format!("NOT {}", grep_expr(n)));
    }
    if filter.with_result {
        clauses.push("outcome = 'completed'".to_owned());
    }
    if filter.clean {
        clauses.push("dirty = 0".to_owned());
    }
    let sql = format!(
        "SELECT uuid FROM runs WHERE {} ORDER BY started_unix_ms DESC, uuid DESC",
        clauses.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let uuids = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(uuids)
}

/// Whether the project has any stored run, whatever its outcome.
pub(super) fn has_runs(conn: &Connection, project: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE project = ?1)",
        [project],
        |row| row.get(0),
    )?)
}

/// Runs of projects other than `project` whose uuid starts with `prefix`,
/// as `(uuid, project)`, newest first.
pub(super) fn elsewhere(
    conn: &Connection,
    project: &str,
    prefix: &str,
) -> Result<Vec<(String, String)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT uuid, project FROM runs
         WHERE project <> ?1 AND substr(uuid, 1, length(?2)) = ?2
         ORDER BY started_unix_ms DESC, uuid DESC",
    )?;
    let rows = stmt
        .query_map([project, &prefix.to_ascii_lowercase()], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}
