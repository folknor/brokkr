//! Query operations for the results database.

use super::ResultsDb;
use super::like::{ESCAPE, contains_pattern, prefix_pattern, require_prefix};
use super::schema::SELECT_COLS;
use super::types::{KvClass, Wall, canonical_cargo_features};
use super::{
    Distribution, HotpathData, HotpathFunction, HotpathThread, KvPair, KvValue, Percentile,
    PreviousRun, Quantity, QueryFilter, RunId, StoredRow,
};
use crate::error::DevError;
use crate::git::ShortCommit;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl ResultsDb {
    /// Query exact run identities with the complete results filter. Returns
    /// the number omitted by `filter.limit` alongside the selected rows.
    pub(crate) fn query_by_run_ids(
        &self,
        ids: &[RunId],
        filter: &QueryFilter,
    ) -> Result<(Vec<StoredRow>, usize), DevError> {
        if ids.is_empty() {
            return Ok((Vec::new(), 0));
        }

        // Keep the placeholder count comfortably below SQLite's variable
        // limit even when a very short prefix names many UUIDs.
        let mut total = 0usize;
        let mut selected = Vec::new();
        for chunk in ids.chunks(250) {
            let mut clauses = vec![format!(
                "uuid IN ({})",
                (1..=chunk.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )];
            let mut params: Vec<String> = chunk.iter().map(|id| id.as_str().to_owned()).collect();
            push_row_filter_clauses(&mut clauses, &mut params, filter);
            let where_clause = clauses.join(" AND ");
            let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
                .iter()
                .map(|p| p as &dyn rusqlite::types::ToSql)
                .collect();
            let chunk_total: i64 = self.conn.query_row(
                &format!("SELECT COUNT(*) FROM runs WHERE {where_clause}"),
                param_refs.as_slice(),
                |row| row.get(0),
            )?;
            total = total.saturating_add(usize::try_from(chunk_total).unwrap_or(usize::MAX));

            let mut selected_params = params.clone();
            selected_params.push(filter.limit.to_string());
            let selected_refs: Vec<&dyn rusqlite::types::ToSql> = selected_params
                .iter()
                .map(|p| p as &dyn rusqlite::types::ToSql)
                .collect();
            let sql = format!(
                "SELECT {SELECT_COLS} FROM runs WHERE {where_clause} ORDER BY id DESC LIMIT ?{}",
                selected_params.len(),
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(selected_refs.as_slice(), map_stored_row)?;
            selected.extend(collect_rows(rows)?);
        }

        selected.sort_by_key(|row| std::cmp::Reverse(row.id));
        selected.truncate(filter.limit);
        for row in &mut selected {
            load_children(&self.conn, row)?;
        }
        let omitted = total.saturating_sub(selected.len());
        Ok((selected, omitted))
    }

    /// Enumerate exact UUID values whose literal prefix matches.
    pub(crate) fn uuids_matching_prefix(&self, prefix: &str) -> Result<Vec<String>, DevError> {
        require_prefix(prefix, "uuid")?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT uuid FROM runs WHERE uuid IS NOT NULL AND uuid LIKE ?1 {ESCAPE} ORDER BY uuid"
        ))?;
        let rows = stmt.query_map(rusqlite::params![prefix_pattern(prefix)], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Enumerate all UUIDs whose commits match the shared two-way prefix rule.
    pub(crate) fn uuids_matching_commit_prefix(
        &self,
        commit_prefix: &str,
    ) -> Result<Vec<String>, DevError> {
        require_prefix(commit_prefix, "commit")?;
        let (pattern, raw) = commit_params(commit_prefix);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT uuid FROM runs WHERE uuid IS NOT NULL AND {} ORDER BY uuid",
            commit_match_expr(1, 2),
        ))?;
        let rows = stmt.query_map(rusqlite::params![pattern, raw], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Whether a results row exists for this exact UUID.
    pub(crate) fn has_uuid(&self, uuid: &str) -> Result<bool, DevError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE uuid = ?1)",
            rusqlite::params![uuid],
            |row| row.get(0),
        )?)
    }

    /// The most recently recorded run in this database, or `None` when it is
    /// empty. Call *before* inserting the current run, or it returns itself.
    ///
    /// Deliberately unfiltered within this project's results database: the
    /// question is what this project recorded most recently, not what last ran
    /// with these flags. Dirty runs and other projects are outside this store.
    /// Ordered by `id` rather than `timestamp` because `timestamp` has
    /// one-second resolution and two runs can share it; `id` is the true
    /// insertion order.
    pub fn previous_run(&self) -> Result<Option<PreviousRun>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid, command, input_file, \
                    CAST(strftime('%s', timestamp) AS INTEGER) \
             FROM runs ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], |row| {
            Ok(PreviousRun {
                uuid: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                command: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                input_file: row.get::<_, Option<String>>(2)?,
                timestamp_epoch: row.get::<_, Option<i64>>(3)?.unwrap_or_default(),
            })
        })?;
        match rows.next() {
            Some(Ok(prev)) => Ok(Some(prev)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
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

/// The `[commit]` match clause for a user-supplied commit bound at `?{pattern}`
/// (its [`commit_params`] pattern) and `?{raw}` (its lowercased text).
///
/// Prefix matching in *both* directions. Stored rows carry whatever width the
/// recording brokkr abbreviated to - `git rev-parse --short` grew with the repo
/// before [`crate::git::SHORT_HASH_LEN`] pinned it - so a one-directional
/// "stored starts with query" match made a full or longer hash from `git log`
/// find nothing on a row recorded at 7. The reverse direction ("query starts
/// with stored") is bounded below by git's minimum abbreviation, 4, so a row
/// with an empty or truncated commit cannot match every query.
pub(super) fn commit_match_expr(pattern: usize, raw: usize) -> String {
    commit_match_expr_on("[commit]", pattern, raw)
}

/// [`commit_match_expr`] over an arbitrary commit column - sidecar.db names
/// it `git_commit`. One definition, so `invalidate --commit` selects the same
/// runs in both databases.
pub(super) fn commit_match_expr_on(column: &str, pattern: usize, raw: usize) -> String {
    format!(
        "({column} LIKE ?{pattern} {ESCAPE} \
          OR (length({column}) >= 4 \
              AND substr(?{raw}, 1, length({column})) = lower({column})))"
    )
}

/// The two bindings [`commit_match_expr`] reads, in its argument order.
pub(super) fn commit_params(commit: &str) -> (String, String) {
    let raw = commit.trim().to_ascii_lowercase();
    (prefix_pattern(&raw), raw)
}

/// Run `sql` with `?1`/`?2` bound to `commit`'s [`commit_params`] (the
/// clause is [`commit_match_expr`]`(1, 2)`) and the rest of `params` as-is.
pub(super) fn query_commit_filtered(
    conn: &rusqlite::Connection,
    sql: &str,
    commit: &str,
    params: &[String],
) -> Result<Vec<StoredRow>, DevError> {
    let mut bound = params.to_vec();
    let (pattern, raw) = commit_params(commit);
    bound[0] = pattern;
    bound[1] = raw;
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
    row.iterations = load_iterations(conn, row.id)?;
    let mut kv = load_kv(conn, row.id)?;
    // Promote `env.*` entries to a first-class `captured_env` field.
    // They're still stored in run_kv (no migration needed), but every
    // downstream consumer - display, pair-key dedup, filters - treats
    // them as a distinct axis instead of anonymous kv.
    let mut captured_env = std::collections::BTreeMap::new();
    kv.retain(|p| match p.class() {
        KvClass::Environment => {
            captured_env.insert(
                KvClass::Environment.suffix(&p.key).to_owned(),
                p.value.to_string(),
            );
            false
        }
        _ => true,
    });
    row.kv = kv;
    row.captured_env = captured_env;
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

/// Warn about an unrecognised `cargo_profile` value, once per distinct value
/// per process. The row mapper runs per row, so a DB with many rows carrying
/// the same typo would otherwise repeat the line for each. A warning, not an
/// error: the row is still read, as `release`.
fn warn_unknown_profile_once(raw: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock, PoisonError};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let first = seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(raw.to_owned());
    if first {
        crate::output::warn(&format!(
            "unknown cargo_profile {raw:?} in results DB - \
             treating as 'release' (likely a migration typo)"
        ));
    }
}

fn map_stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    Ok(StoredRow {
        id: row.get("id")?,
        timestamp: row.get("timestamp")?,
        hostname: row.get("hostname")?,
        commit: read_stored_commit(row)?,
        subject: row.get("subject")?,
        command: row.get("command")?,
        mode: row.get::<_, Option<String>>("mode")?.unwrap_or_default(),
        input_file: row
            .get::<_, Option<String>>("input_file")?
            .unwrap_or_default(),
        input_mb: row.get("input_mb")?,
        elapsed: read_wall(row, "elapsed_ms", "elapsed_us")?,
        peak_rss_mb: row.get("peak_rss_mb")?,
        cargo_features: canonical_cargo_features(
            row.get::<_, Option<String>>("cargo_features")?.as_deref(),
        )
        .unwrap_or_default(),
        cargo_profile: row
            .get::<_, Option<String>>("cargo_profile")?
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| match crate::build::CargoProfile::from_db(s) {
                Ok(p) => p,
                Err(crate::build::UnknownCargoProfile(raw)) => {
                    warn_unknown_profile_once(&raw);
                    crate::build::CargoProfile::Release
                }
            }),
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
        brokkr_args: row
            .get::<_, Option<String>>("brokkr_args")?
            .unwrap_or_default(),
        arm_key: row.get::<_, Option<String>>("arm_key")?,
        // Older or hand-written rows without attribution retain the original
        // results store's project identity.
        project: row
            .get::<_, Option<String>>("project")?
            .unwrap_or_else(|| "pbfhogg".to_owned()),
        stop_marker_requested: row
            .get::<_, Option<String>>("stop_marker_requested")?
            .unwrap_or_default(),
        stop_marker: row
            .get::<_, Option<String>>("stop_marker")?
            .unwrap_or_default(),
        kv: Vec::new(),
        captured_env: std::collections::BTreeMap::new(),
        iterations: Vec::new(),
        distribution: None,
        hotpath: None,
    })
}

/// Stored commit text never fails a read. Old databases hold values such as
/// `""`, `unknown` or `dirty`; those are kept and displayed verbatim (see
/// [`ShortCommit::from_stored_lossy`]) so `brokkr results` always shows every
/// row, and a verbatim value only ever pairs with the same verbatim value.
fn read_stored_commit(row: &rusqlite::Row<'_>) -> rusqlite::Result<ShortCommit> {
    let raw: String = row.get("commit")?;
    Ok(ShortCommit::from_stored_lossy(&raw))
}

fn read_wall(
    row: &rusqlite::Row<'_>,
    millis_column: &str,
    micros_column: &str,
) -> rusqlite::Result<Wall> {
    Ok(Wall::from_columns(
        row.get(millis_column)?,
        row.get(micros_column)?,
    ))
}

/// One `--grep` term's match expression: true when the term appears anywhere in
/// the row's invocation. Binds three params, appended to `params`.
///
/// "Invocation" is the three places a user can put something when launching a
/// run: the brokkr argv (`brokkr_args`), the subprocess argv (`cli_args`), and
/// the env assignments that prefix the command line. The env lives in run_kv
/// under `env.<NAME>` rather than in any argv column, so it needs the EXISTS
/// subquery - without it `--grep LAYER_STATS` missed rows that plainly ran with
/// `ELIVAGAR_LAYER_STATS=1`, since an env var is never echoed into argv.
///
/// Each captured var is matched as the `NAME=VALUE` text the user would have
/// typed, with the storage-level `env.` prefix stripped: `--grep LAYER_STATS`
/// and `--grep LAYER_STATS=1` both hit, `--grep env` doesn't hit every row that
/// captured anything.
///
/// `instr`, not `LIKE '%'||?||'%'`: these terms are documented as literal
/// substrings, but LIKE treats `%` and `_` in the *term* as wildcards - so
/// `--grep io_uring` would also match `io-uring`, a different axis. instr is a
/// literal search and needs no escaping. COALESCE because `instr(NULL, x)` is
/// NULL rather than 0, which would drop rows from the negated clause.
fn grep_match_expr(params: &mut Vec<String>, term: &str) -> String {
    params.push(term.to_owned());
    let i = params.len();
    params.push(term.to_owned());
    let j = params.len();
    params.push(term.to_owned());
    let k = params.len();
    format!(
        "(instr(COALESCE(cli_args, ''), ?{i}) > 0 \
          OR instr(COALESCE(brokkr_args, ''), ?{j}) > 0 \
          OR EXISTS (SELECT 1 FROM run_kv WHERE run_kv.run_id = runs.id \
             AND run_kv.key LIKE 'env.%' \
             AND instr(substr(run_kv.key, 5) || '=' \
                       || COALESCE(run_kv.value_text, run_kv.value_int, \
                                   run_kv.value_real, ''), ?{k}) > 0))"
    )
}

/// `column` contains `term` literally (ASCII case-insensitive). Binds one param.
///
/// Shared by [`build_query_sql`] and `query_compare` for `--command`/`--mode`/
/// `--dataset`. `column` is always a fixed identifier from the caller, never
/// user input.
pub(super) fn contains_expr(params: &mut Vec<String>, column: &str, term: &str) -> String {
    params.push(contains_pattern(term));
    format!("{column} LIKE ?{} {ESCAPE}", params.len())
}

/// A run_kv row with key `key` whose value, rendered as text, equals `value`.
/// Binds two params.
///
/// Coalesced across all three value columns, the way `--grep` does: a counter
/// written as `meta.threads=8` is stored in `value_int`, and comparing
/// `value_text` alone made `--meta threads=8` silently match nothing. The CAST
/// is load-bearing - SQLite never considers an INTEGER equal to a TEXT, so the
/// coalesced value must be text before it meets the (text) parameter.
fn kv_equals_expr(params: &mut Vec<String>, key: &str, value: &str) -> String {
    params.push(key.to_owned());
    let key_idx = params.len();
    params.push(value.to_owned());
    let val_idx = params.len();
    format!(
        "EXISTS (SELECT 1 FROM run_kv WHERE run_kv.run_id = runs.id \
         AND run_kv.key = ?{key_idx} \
         AND CAST(COALESCE(run_kv.value_text, run_kv.value_int, run_kv.value_real) AS TEXT) \
             = ?{val_idx})"
    )
}

/// Append the `--grep` / `--grep-v` WHERE clauses.
///
/// Shared by [`build_query_sql`] and `query_compare` so the two can't drift -
/// they previously disagreed about whether `--grep` applied at all.
///
/// - `grep` ANDs: each term must match the row's invocation.
/// - `grep_v` excludes on any hit. Note the De Morgan flip: "this term appears
///   nowhere" is `NOT (cli matches OR brokkr matches OR env matches)`, i.e.
///   every source must miss. Negating the sources separately and ORing them
///   would instead keep rows that merely hide the term in another source.
pub(super) fn push_grep_clauses(
    clauses: &mut Vec<String>,
    params: &mut Vec<String>,
    grep: &[String],
    grep_v: &[String],
) {
    for g in grep {
        let expr = grep_match_expr(params, g);
        clauses.push(expr);
    }
    for g in grep_v {
        let expr = grep_match_expr(params, g);
        clauses.push(format!("NOT {expr}"));
    }
}

fn build_query_sql(filter: &QueryFilter) -> (String, Vec<String>) {
    let mut clauses = Vec::new();
    let mut params: Vec<String> = Vec::new();

    // Commit is a prefix, command/mode/dataset are substrings - all literal
    // (see `like`: `_` used to be a wildcard in every one of them).
    if let Some(ref c) = filter.commit {
        let (pattern, raw) = commit_params(c);
        params.push(pattern);
        params.push(raw);
        clauses.push(commit_match_expr(params.len() - 1, params.len()));
    }
    push_row_filter_clauses(&mut clauses, &mut params, filter);

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

/// Append every user-facing row filter in one place so UUID lookup, recent-row
/// queries, and commit comparisons cannot silently diverge.
pub(super) fn push_row_filter_clauses(
    clauses: &mut Vec<String>,
    params: &mut Vec<String>,
    filters: &QueryFilter,
) {
    if let Some(command) = filters.command.as_deref() {
        let in_command = contains_expr(params, "command", command);
        let in_mode = contains_expr(params, "mode", command);
        clauses.push(format!("({in_command} OR {in_mode})"));
    }
    if let Some(mode) = filters.mode.as_deref() {
        clauses.push(contains_expr(params, "mode", mode));
    }
    if let Some(dataset) = filters.dataset.as_deref() {
        clauses.push(contains_expr(params, "input_file", dataset));
    }
    push_grep_clauses(clauses, params, &filters.grep, &filters.grep_v);
    // Metadata values are text-coalesced across run_kv's three value columns,
    // so numeric entries remain selectable through the same key=value form.
    for (key, value) in &filters.meta {
        clauses.push(kv_equals_expr(
            params,
            &KvClass::Metadata.qualify(key),
            value,
        ));
    }
    // Missing captured environment keys do not match; callers select an
    // explicit baseline value when they need to compare an off arm.
    for (key, value) in &filters.env {
        clauses.push(kv_equals_expr(
            params,
            &KvClass::Environment.qualify(key),
            value,
        ));
    }
}

fn load_distribution(
    conn: &rusqlite::Connection,
    run_id: i64,
) -> Result<Option<Distribution>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT samples, min_ms, p50_ms, p95_ms, max_ms, min_us, p50_us, p95_us, max_us \
         FROM run_distribution WHERE run_id = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![run_id], |row| {
        // All four or none: a partial set is not a distribution anyone
        // measured, so malformed/legacy rows stay wholly millisecond-only.
        let min_ms = row.get(1)?;
        let p50_ms = row.get(2)?;
        let p95_ms = row.get(3)?;
        let max_ms = row.get(4)?;
        let microseconds = (
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<i64>>(8)?,
        );
        let (min, p50, p95, max) = match microseconds {
            (Some(min), Some(p50), Some(p95), Some(max)) => (
                Wall::from_micros(min),
                Wall::from_micros(p50),
                Wall::from_micros(p95),
                Wall::from_micros(max),
            ),
            _ => (
                Wall::from_legacy_millis(min_ms),
                Wall::from_legacy_millis(p50_ms),
                Wall::from_legacy_millis(p95_ms),
                Wall::from_legacy_millis(max_ms),
            ),
        };
        Ok(Distribution {
            samples: row.get(0)?,
            min,
            p50,
            p95,
            max,
        })
    })?;
    match rows.next() {
        Some(Ok(dist)) => Ok(Some(dist)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// Load per-iteration walls ordered by `run_idx`, i.e. back into the order the
/// iterations actually ran. The ordering is the whole point of the table, so
/// the ORDER BY is load-bearing rather than cosmetic.
///
/// Each historical iteration retains its own available precision.
fn load_iterations(conn: &rusqlite::Connection, run_id: i64) -> Result<Vec<Wall>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT elapsed_ms, elapsed_us FROM run_iterations WHERE run_id = ?1 ORDER BY run_idx",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let mut walls = Vec::new();
    for row in rows {
        let (wall_ms, wall_us) = row?;
        walls.push(Wall::from_columns(wall_ms, wall_us));
    }
    Ok(walls)
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
        .filter(|p| p.class() == KvClass::ThreadSummary)
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
    let (mut functions, legacy, ids) = load_hotpath_function_rows(conn, run_id)?;
    load_typed_function_metrics(conn, run_id, &ids, &mut functions)?;
    load_typed_function_percentiles(conn, run_id, &ids, &mut functions)?;
    apply_legacy_function_metrics(&mut functions, legacy);
    Ok(functions)
}

struct LegacyFunctionMetrics {
    avg: Option<String>,
    total: Option<String>,
    percent_total: Option<String>,
    p50: Option<String>,
    p95: Option<String>,
    p99: Option<String>,
}

/// A run's function rows, their legacy text metrics, and each row's index by
/// database id.
type FunctionRows = (
    Vec<HotpathFunction>,
    Vec<LegacyFunctionMetrics>,
    std::collections::HashMap<i64, usize>,
);

fn load_hotpath_function_rows(
    conn: &rusqlite::Connection,
    run_id: i64,
) -> Result<FunctionRows, DevError> {
    let mut functions = Vec::new();
    let mut legacy = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id, section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99 \
         FROM hotpath_functions WHERE run_id = ?1 ORDER BY section, ordinal",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            HotpathFunction {
                section: row.get(1)?,
                description: row.get(2)?,
                ordinal: row.get(3)?,
                name: row.get(4)?,
                calls: row.get(5)?,
                avg: None,
                total: None,
                percent_total: None,
                percentiles: std::collections::BTreeMap::new(),
            },
            LegacyFunctionMetrics {
                avg: row.get(6)?,
                total: row.get(7)?,
                percent_total: row.get(8)?,
                p50: row.get(9)?,
                p95: row.get(10)?,
                p99: row.get(11)?,
            },
        ))
    })?;
    let mut ids = std::collections::HashMap::new();
    for row in rows {
        let (id, function, old_metrics) = row?;
        ids.insert(id, functions.len());
        functions.push(function);
        legacy.push(old_metrics);
    }
    drop(stmt);
    Ok((functions, legacy, ids))
}

fn load_typed_function_metrics(
    conn: &rusqlite::Connection,
    run_id: i64,
    ids: &std::collections::HashMap<i64, usize>,
    functions: &mut [HotpathFunction],
) -> Result<(), DevError> {
    let mut stmt = conn.prepare(
        "SELECT m.function_id, m.metric, m.quantity_kind, m.value_int, m.value_real \
         FROM hotpath_function_metrics AS m \
         JOIN hotpath_functions AS f ON f.id = m.function_id \
         WHERE f.run_id = ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<f64>>(4)?,
        ))
    })?;
    for row in rows {
        let (id, metric, kind, integer, real) = row?;
        let Some(index) = ids.get(&id).copied() else {
            continue;
        };
        let Some(value) = Quantity::from_storage(&kind, integer, real) else {
            continue;
        };
        let function = &mut functions[index];
        match metric.as_str() {
            "avg" => function.avg = Some(value),
            "total" => function.total = Some(value),
            "percent_total" => function.percent_total = Some(value),
            _ => {}
        }
    }
    Ok(())
}

fn load_typed_function_percentiles(
    conn: &rusqlite::Connection,
    run_id: i64,
    ids: &std::collections::HashMap<i64, usize>,
    functions: &mut [HotpathFunction],
) -> Result<(), DevError> {
    let mut stmt = conn.prepare(
        "SELECT p.function_id, p.percentile, p.quantity_kind, p.value_int, p.value_real \
         FROM hotpath_function_percentiles AS p \
         JOIN hotpath_functions AS f ON f.id = p.function_id \
         WHERE f.run_id = ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<f64>>(4)?,
        ))
    })?;
    for row in rows {
        let (id, percentile, kind, integer, real) = row?;
        let Some(index) = ids.get(&id).copied() else {
            continue;
        };
        let (Some(percentile), Some(value)) = (
            Percentile::new(percentile),
            Quantity::from_storage(&kind, integer, real),
        ) else {
            continue;
        };
        functions[index].percentiles.insert(percentile, value);
    }
    Ok(())
}

fn apply_legacy_function_metrics(
    functions: &mut [HotpathFunction],
    legacy: Vec<LegacyFunctionMetrics>,
) {
    // Typed rows take precedence, while each missing field can still be read
    // from the display strings written by earlier schema versions.
    for (function, old) in functions.iter_mut().zip(legacy) {
        function.avg = function
            .avg
            .take()
            .or_else(|| old.avg.as_deref().and_then(Quantity::parse));
        function.total = function
            .total
            .take()
            .or_else(|| old.total.as_deref().and_then(Quantity::parse));
        function.percent_total = function
            .percent_total
            .take()
            .or_else(|| old.percent_total.as_deref().and_then(Quantity::parse));
        for (key, text) in [("p50", old.p50), ("p95", old.p95), ("p99", old.p99)] {
            if let (Some(percentile), Some(value)) = (
                Percentile::parse_key(key),
                text.as_deref().and_then(Quantity::parse),
            ) {
                function.percentiles.entry(percentile).or_insert(value);
            }
        }
    }
}

fn load_hotpath_threads(
    conn: &rusqlite::Connection,
    run_id: i64,
) -> Result<Vec<HotpathThread>, DevError> {
    let (mut threads, legacy, ids) = load_hotpath_thread_rows(conn, run_id)?;
    load_typed_thread_metrics(conn, run_id, &ids, &mut threads)?;
    apply_legacy_thread_metrics(&mut threads, legacy);
    Ok(threads)
}

type LegacyThreadMetrics = [Option<String>; 6];

/// A run's thread rows, their legacy text metrics, and each row's index by
/// database id.
type ThreadRows = (
    Vec<HotpathThread>,
    Vec<LegacyThreadMetrics>,
    std::collections::HashMap<i64, usize>,
);

fn load_hotpath_thread_rows(
    conn: &rusqlite::Connection,
    run_id: i64,
) -> Result<ThreadRows, DevError> {
    let mut threads = Vec::new();
    let mut legacy = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id, name, status, cpu_percent, cpu_percent_max, cpu_percent_avg, \
         alloc_bytes, dealloc_bytes, mem_diff \
         FROM hotpath_threads WHERE run_id = ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            HotpathThread {
                name: row.get(1)?,
                status: row.get(2)?,
                cpu_percent: None,
                cpu_percent_max: None,
                cpu_percent_avg: None,
                alloc_bytes: None,
                dealloc_bytes: None,
                mem_diff: None,
            },
            [
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ],
        ))
    })?;
    let mut ids = std::collections::HashMap::new();
    for row in rows {
        let (id, thread, old_metrics) = row?;
        ids.insert(id, threads.len());
        threads.push(thread);
        legacy.push(old_metrics);
    }
    Ok((threads, legacy, ids))
}

fn load_typed_thread_metrics(
    conn: &rusqlite::Connection,
    run_id: i64,
    ids: &std::collections::HashMap<i64, usize>,
    threads: &mut [HotpathThread],
) -> Result<(), DevError> {
    let mut stmt = conn.prepare(
        "SELECT m.thread_id, m.metric, m.quantity_kind, m.value_int, m.value_real \
         FROM hotpath_thread_metrics AS m \
         JOIN hotpath_threads AS t ON t.id = m.thread_id \
         WHERE t.run_id = ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<f64>>(4)?,
        ))
    })?;
    for row in rows {
        let (id, metric, kind, integer, real) = row?;
        let Some(index) = ids.get(&id).copied() else {
            continue;
        };
        let Some(value) = Quantity::from_storage(&kind, integer, real) else {
            continue;
        };
        set_thread_metric(&mut threads[index], &metric, value);
    }
    Ok(())
}

fn apply_legacy_thread_metrics(threads: &mut [HotpathThread], legacy: Vec<LegacyThreadMetrics>) {
    for (thread, old) in threads.iter_mut().zip(legacy) {
        let legacy_values = [
            ("cpu_percent", old[0].as_deref()),
            ("cpu_percent_max", old[1].as_deref()),
            ("cpu_percent_avg", old[2].as_deref()),
            ("alloc_bytes", old[3].as_deref()),
            ("dealloc_bytes", old[4].as_deref()),
            ("mem_diff", old[5].as_deref()),
        ];
        for (metric, text) in legacy_values {
            if let Some(value) = text.and_then(Quantity::parse) {
                set_thread_metric_if_missing(thread, metric, value);
            }
        }
    }
}

fn set_thread_metric(thread: &mut HotpathThread, metric: &str, value: Quantity) {
    match metric {
        "cpu_percent" => thread.cpu_percent = Some(value),
        "cpu_percent_max" => thread.cpu_percent_max = Some(value),
        "cpu_percent_avg" => thread.cpu_percent_avg = Some(value),
        "alloc_bytes" => thread.alloc_bytes = Some(value),
        "dealloc_bytes" => thread.dealloc_bytes = Some(value),
        "mem_diff" => thread.mem_diff = Some(value),
        _ => {}
    }
}

fn set_thread_metric_if_missing(thread: &mut HotpathThread, metric: &str, value: Quantity) {
    let slot = match metric {
        "cpu_percent" => &mut thread.cpu_percent,
        "cpu_percent_max" => &mut thread.cpu_percent_max,
        "cpu_percent_avg" => &mut thread.cpu_percent_avg,
        "alloc_bytes" => &mut thread.alloc_bytes,
        "dealloc_bytes" => &mut thread.dealloc_bytes,
        "mem_diff" => &mut thread.mem_diff,
        _ => return,
    };
    if slot.is_none() {
        *slot = Some(value);
    }
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
    use super::*;
    use crate::db::{ResultsDb, RunInvocation, RunRow, RunRowBuilder, Wall};

    fn test_invocation(raw: &str) -> RunInvocation {
        RunInvocation {
            argv: raw.split_whitespace().map(str::to_owned).collect(),
            arm_key: raw.to_owned(),
        }
    }

    fn test_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = crate::test_scratch::scratch("db-query", name);
        let db_path = dir.join("test.db");
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
            limit: 50,
            ..Default::default()
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
            mode: Some(String::from("mmap")),
            limit: 10,
            ..Default::default()
        };
        let (sql, params) = build_query_sql(&filter);

        assert!(sql.contains("WHERE"));
        assert!(
            sql.contains("[commit] LIKE ?1 ESCAPE"),
            "commit pattern should be ?1"
        );
        assert!(
            sql.contains("substr(?2, 1, length([commit]))"),
            "raw commit should be ?2"
        );
        assert!(
            sql.contains("command LIKE ?3 ESCAPE"),
            "command should be ?3 contains"
        );
        assert!(
            sql.contains("mode LIKE ?4 ESCAPE"),
            "command should also check variant as ?4"
        );
        assert!(
            sql.contains("mode LIKE ?5 ESCAPE"),
            "variant filter should be ?5 contains"
        );
        assert!(sql.contains("LIMIT ?6"), "limit should be ?6");
        assert_eq!(params.len(), 6);
        assert_eq!(params[0], "abc123%");
        assert_eq!(params[1], "abc123");
        assert_eq!(params[2], "%read%");
        assert_eq!(params[3], "%read%");
        assert_eq!(params[4], "%mmap%");
        assert_eq!(params[5], "10");
    }

    #[test]
    fn build_query_sql_commit_only() {
        let filter = QueryFilter {
            commit: Some(String::from("deadbeef")),
            limit: 25,
            ..Default::default()
        };
        let (sql, params) = build_query_sql(&filter);

        assert!(sql.contains("[commit] LIKE ?1 ESCAPE"));
        assert!(sql.contains("LIMIT ?3"));
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], "deadbeef%");
        assert_eq!(params[1], "deadbeef");
        assert_eq!(params[2], "25");
    }

    /// The commit match runs in both directions: a longer hash finds a row
    /// recorded at a shorter width, and a prefix still finds a longer one -
    /// but a stored commit under git's 4-digit minimum matches nothing by the
    /// reverse direction.
    #[test]
    fn commit_match_is_a_prefix_either_way() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let matches = |stored: &str, query: &str| -> bool {
            let (pattern, raw) = commit_params(query);
            conn.query_row(
                &format!(
                    "SELECT {} FROM (SELECT ?3 AS [commit])",
                    commit_match_expr(1, 2)
                ),
                rusqlite::params![pattern, raw, stored],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
        };
        assert!(
            matches("abc1234", "abc1234def0"),
            "longer query finds shorter row"
        );
        assert!(
            matches("abc1234def", "abc12"),
            "shorter query finds longer row"
        );
        assert!(matches("abc1234", "ABC1234"), "case-insensitive");
        assert!(!matches("abc1234", "abd1234def"));
        assert!(
            !matches("abc", "abcdef1234"),
            "under 4 digits never matches in reverse"
        );
        assert!(!matches("", "abcdef1234"));
    }

    #[test]
    fn build_query_sql_command_and_variant_no_commit() {
        let filter = QueryFilter {
            command: Some(String::from("write")),
            mode: Some(String::from("direct")),
            limit: 5,
            ..Default::default()
        };
        let (sql, params) = build_query_sql(&filter);

        // Without commit, command becomes ?1 (+ variant fallback ?2),
        // variant filter ?3, limit ?4
        assert!(sql.contains("command LIKE ?1 ESCAPE"));
        assert!(sql.contains("mode LIKE ?2 ESCAPE"));
        assert!(sql.contains("mode LIKE ?3 ESCAPE"));
        assert!(sql.contains("LIMIT ?4"));
        assert_eq!(params.len(), 4);
        assert_eq!(params[0], "%write%");
        assert_eq!(params[1], "%write%");
        assert_eq!(params[2], "%direct%");
        assert_eq!(params[3], "5");
    }

    #[test]
    fn build_query_sql_dataset_only() {
        let filter = QueryFilter {
            dataset: Some(String::from("europe")),
            limit: 20,
            ..Default::default()
        };
        let (sql, params) = build_query_sql(&filter);

        assert!(
            sql.contains("input_file LIKE ?1 ESCAPE"),
            "dataset should filter on input_file as ?1"
        );
        assert!(sql.contains("LIMIT ?2"));
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], "%europe%");
        assert_eq!(params[1], "20");
    }

    #[test]
    fn build_query_sql_command_and_dataset() {
        let filter = QueryFilter {
            command: Some(String::from("tags-filter")),
            dataset: Some(String::from("eu")),
            limit: 10,
            ..Default::default()
        };
        let (sql, params) = build_query_sql(&filter);

        // command becomes ?1 + variant fallback ?2, dataset filter ?3, limit ?4
        assert!(sql.contains("command LIKE ?1 ESCAPE"));
        assert!(sql.contains("mode LIKE ?2 ESCAPE"));
        assert!(sql.contains("input_file LIKE ?3 ESCAPE"));
        assert!(sql.contains("LIMIT ?4"));
        assert_eq!(params.len(), 4);
        assert_eq!(params[0], "%tags-filter%");
        assert_eq!(params[1], "%tags-filter%");
        assert_eq!(params[2], "%eu%");
        assert_eq!(params[3], "10");
    }

    #[test]
    fn build_query_sql_selects_correct_columns() {
        let filter = QueryFilter {
            limit: 1,
            ..Default::default()
        };
        let (sql, _) = build_query_sql(&filter);
        assert!(sql.starts_with(&format!("SELECT {SELECT_COLS} FROM runs")));
    }

    #[test]
    fn build_query_sql_single_meta_filter() {
        let filter = QueryFilter {
            meta: vec![("format".into(), "osc".into())],
            limit: 20,
            ..Default::default()
        };
        let (sql, params) = build_query_sql(&filter);

        assert!(sql.contains("EXISTS (SELECT 1 FROM run_kv"));
        assert!(sql.contains("run_kv.run_id = runs.id"));
        assert!(sql.contains("run_kv.key = ?1"));
        assert!(sql.contains("= ?2)"));
        // The user passes "format" but the kv key is stored as "meta.format".
        assert_eq!(params[0], "meta.format");
        assert_eq!(params[1], "osc");
        assert_eq!(params[2], "20"); // limit
    }

    #[test]
    fn build_query_sql_multi_meta_filters_are_anded() {
        let filter = QueryFilter {
            meta: vec![
                ("format".into(), "osc".into()),
                ("from_snapshot".into(), "base".into()),
            ],
            limit: 20,
            ..Default::default()
        };
        let (sql, params) = build_query_sql(&filter);

        // Two EXISTS subqueries joined by AND.
        let exists_count = sql.matches("EXISTS (SELECT 1 FROM run_kv").count();
        assert_eq!(exists_count, 2);
        assert!(sql.contains(" AND "));
        // 4 meta params (two key/value pairs) + 1 limit = 5 total.
        assert_eq!(params.len(), 5);
        assert_eq!(params[0], "meta.format");
        assert_eq!(params[1], "osc");
        assert_eq!(params[2], "meta.from_snapshot");
        assert_eq!(params[3], "base");
    }

    #[test]
    fn meta_filter_excludes_rows_without_key() {
        let (dir, db_path) = test_db("meta_filter_missing");
        let db = ResultsDb::open(&db_path).expect("open");

        let make_row = |variant: &str, kv: Vec<KvPair>| {
            RunRowBuilder::new()
                .update(|row| {
                    row.command = String::from("bench diff-snapshots");
                    row.mode = Some(String::from(variant));
                    row.invocation = test_invocation("");
                    row.kv = kv;
                })
                .build()
        };

        // Row 1: has meta.format = osc
        db.insert(&make_row(
            "diff-snapshots-base-to-20260411",
            vec![KvPair::text("meta.format", "osc")],
        ))
        .unwrap();
        // Row 2: has meta.format = default
        db.insert(&make_row(
            "diff-snapshots-base-to-20260411",
            vec![KvPair::text("meta.format", "default")],
        ))
        .unwrap();
        // Row 3: no meta.format at all
        db.insert(&make_row("diff-snapshots-base-to-20260411", vec![]))
            .unwrap();

        let osc_only = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![("format".into(), "osc".into())],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(osc_only.len(), 1, "only the osc row should match");

        let default_only = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![("format".into(), "default".into())],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(default_only.len(), 1, "only the default row should match");

        // No filter - all 3 rows.
        let all = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(all.len(), 3);

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Query: command and variant contains match
    // -----------------------------------------------------------------------

    #[test]
    fn query_command_contains_match() {
        let (dir, db_path) = test_db("query_cmd_contains");

        let db = ResultsDb::open(&db_path).expect("open");

        let make_row = |cmd: &str, variant: &str| {
            RunRowBuilder::new()
                .update(|row| {
                    row.command = String::from(cmd);
                    row.mode = Some(String::from(variant));
                    row.invocation = test_invocation("");
                })
                .build()
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
                mode: None,
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 2, "merge should match 2 bench merge rows");

        // "bench" matches all 3 rows.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: Some(String::from("bench")),
                mode: None,
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 3, "bench should match all rows");

        // "read" matches only bench read.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: Some(String::from("read")),
                mode: None,
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
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
                mode: None,
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);

        // --command also matches against the variant column, so
        // `--command zstd1` finds rows where variant contains "zstd1"
        // even if the command column doesn't match.
        db.insert(&make_row("bench apply-changes", "direct-io+zstd1"))
            .unwrap();
        db.insert(&make_row("bench apply-changes", "direct-io"))
            .unwrap();
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: Some(String::from("zstd1")),
                mode: None,
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mode, "direct-io+zstd1");

        drop(db);
        cleanup(&dir, &db_path);
    }

    #[test]
    fn query_variant_contains_match() {
        let (dir, db_path) = test_db("query_var_contains");

        let db = ResultsDb::open(&db_path).expect("open");

        let make_row = |variant: &str| {
            RunRowBuilder::new()
                .update(|row| {
                    row.command = String::from("bench merge");
                    row.mode = Some(String::from(variant));
                    row.invocation = test_invocation("");
                })
                .build()
        };

        db.insert(&make_row("buffered+zlib")).unwrap();
        db.insert(&make_row("buffered+none")).unwrap();
        db.insert(&make_row("direct+zlib")).unwrap();

        // "zlib" matches buffered+zlib and direct+zlib.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: Some(String::from("zlib")),
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 2, "zlib should match 2 rows");

        // "buffered" matches both buffered variants.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: Some(String::from("buffered")),
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 2, "buffered should match 2 rows");

        // "none" matches only buffered+none.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: Some(String::from("none")),
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mode, "buffered+none");

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

        let row = RunRowBuilder::new()
            .update(|row| {
                row.command = String::from("hotpath");
                row.mode = Some(String::from("default"));
                row.elapsed = Wall::LegacyMillis(500);
                row.invocation = test_invocation("");
                row.kv = vec![
                    KvPair::int("elapsed_ms", 500),
                    KvPair::text("threads.rss_bytes", "1024"),
                ];
                // Deliberately not ascending: the round-trip must preserve
                // execution order, not helpfully sort it.
                row.iterations = vec![
                    Wall::from_micros(500_400),
                    Wall::from_micros(699_600),
                    Wall::from_micros(600_049),
                ];
                row.distribution = Some(Distribution {
                    samples: 5,
                    min: Wall::from_micros(100_312),
                    p50: Wall::from_micros(110_000),
                    p95: Wall::from_micros(129_501),
                    max: Wall::from_micros(150_499),
                });
                row.hotpath = Some(HotpathData {
                    functions: vec![HotpathFunction {
                        section: String::from("timing"),
                        description: Some(String::from("main")),
                        ordinal: 0,
                        name: String::from("process"),
                        calls: Some(10),
                        avg: Quantity::parse("50 ms"),
                        total: Quantity::parse("500 ms"),
                        percent_total: Quantity::parse("100%"),
                        percentiles: std::collections::BTreeMap::new(),
                    }],
                    threads: vec![HotpathThread {
                        name: String::from("main"),
                        status: Some(String::from("running")),
                        cpu_percent: Quantity::parse("95%"),
                        cpu_percent_max: None,
                        cpu_percent_avg: None,
                        alloc_bytes: None,
                        dealloc_bytes: None,
                        mem_diff: None,
                    }],
                    thread_summary: vec![KvPair::text("threads.rss_bytes", "1024")],
                });
            })
            .build();

        let (full, _) = db.insert(&row).expect("insert");

        // query_by_run_ids triggers load_children for each row.
        let (rows, omitted) = db
            .query_by_run_ids(
                &[RunId::new(full)],
                &QueryFilter {
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("query_by_run_ids");
        assert_eq!(omitted, 0);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];

        // Per-iteration walls, in execution order.
        assert_eq!(
            r.iterations,
            vec![
                Wall::from_micros(500_400),
                Wall::from_micros(699_600),
                Wall::from_micros(600_049),
            ],
            "iterations must round-trip in execution order - sorting them \
             would erase the drift signal they exist to carry"
        );

        // Distribution.
        let dist = r
            .distribution
            .as_ref()
            .expect("distribution should be loaded");
        assert_eq!(dist.samples, 5);
        assert_eq!(dist.min, Wall::from_micros(100_312));
        assert_eq!(dist.p50, Wall::from_micros(110_000));
        assert_eq!(dist.p95, Wall::from_micros(129_501));
        assert_eq!(dist.max, Wall::from_micros(150_499));

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

    #[test]
    fn query_grep_multi_term_and_semantics() {
        let (dir, db_path) = test_db("query_grep_and");
        let db = ResultsDb::open(&db_path).expect("open");

        let make_row = |cli: &str, brokkr: &str| {
            RunRowBuilder::new()
                .update(|row| {
                    row.command = String::from("bench apply-changes");
                    row.mode = Some(String::from("release"));
                    row.cli_args = Some(String::from(cli));
                    row.invocation = test_invocation(brokkr);
                })
                .build()
        };

        // Row 1: zstd + uring, matches both terms.
        db.insert(&make_row(
            "apply-changes --compression zstd:1",
            "brokkr apply-changes --io-uring",
        ))
        .unwrap();
        // Row 2: zstd only, misses `uring`.
        db.insert(&make_row(
            "apply-changes --compression zstd:3",
            "brokkr apply-changes",
        ))
        .unwrap();
        // Row 3: uring only, misses `zstd`.
        db.insert(&make_row(
            "apply-changes --direct-io",
            "brokkr apply-changes --io-uring",
        ))
        .unwrap();

        // Single term still works: `zstd` matches rows 1+2.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: vec!["zstd".into()],
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 2, "single --grep zstd matches rows 1+2");

        // AND: `zstd` + `uring` matches only row 1.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: vec!["zstd".into(), "uring".into()],
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "both --grep terms must match (AND) - only row 1 qualifies"
        );
        assert!(
            rows[0].cli_args.contains("zstd:1"),
            "should be the zstd:1 row"
        );

        // AND across columns: one term hits cli_args, the other hits
        // brokkr_args. Row 1 has zstd only in cli_args and uring only in
        // brokkr_args - AND still resolves.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: vec!["zstd:1".into(), "io-uring".into()],
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);

        // No rows have both "direct-io" AND "zstd" - AND returns empty.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: vec!["direct-io".into(), "zstd".into()],
                grep_v: Vec::new(),
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert!(rows.is_empty(), "no row has both direct-io and zstd");

        drop(db);
        cleanup(&dir, &db_path);
    }

    #[test]
    fn query_grep_v_excludes_and_composes() {
        let (dir, db_path) = test_db("query_grep_v");
        let db = ResultsDb::open(&db_path).expect("open");

        let make_row = |cli: &str, brokkr: &str| {
            RunRowBuilder::new()
                .update(|row| {
                    row.command = String::from("bench apply-changes");
                    row.mode = Some(String::from("release"));
                    row.cli_args = Some(String::from(cli));
                    row.invocation = test_invocation(brokkr);
                })
                .build()
        };

        // Row 1: zstd + uring (the ON arm).
        db.insert(&make_row(
            "apply-changes --compression zstd:1",
            "brokkr apply-changes --io-uring",
        ))
        .unwrap();
        // Row 2: zstd, no uring (the OFF arm - distinguished only by absence).
        db.insert(&make_row(
            "apply-changes --compression zstd:3",
            "brokkr apply-changes",
        ))
        .unwrap();
        // Row 3: uring, no zstd.
        db.insert(&make_row(
            "apply-changes --direct-io",
            "brokkr apply-changes --io-uring",
        ))
        .unwrap();

        // Exclusion reaches into brokkr_args, not just cli_args: rows 1 and 3
        // carry `uring` only there.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: vec!["uring".into()],
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 1, "--grep-v uring must exclude rows 1 and 3");
        assert!(
            rows[0].cli_args.contains("zstd:3"),
            "the surviving row is the one with no uring anywhere"
        );

        // The motivating case: select the OFF arm of an A/B, which --grep
        // alone cannot express because the arm is defined by an absent flag.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: vec!["zstd".into()],
                grep_v: vec!["uring".into()],
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "--grep zstd --grep-v uring isolates the OFF arm"
        );
        assert!(rows[0].cli_args.contains("zstd:3"));

        // Multiple --grep-v terms OR together: any hit excludes.
        let rows = db
            .query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: Vec::new(),
                grep_v: vec!["zstd".into(), "direct-io".into()],
                env: Vec::new(),
                limit: 50,
            })
            .unwrap();
        assert!(
            rows.is_empty(),
            "every row matches at least one --grep-v term, so all are excluded"
        );

        drop(db);
        cleanup(&dir, &db_path);
    }

    #[test]
    fn query_grep_reaches_captured_env() {
        let (dir, db_path) = test_db("query_grep_env");
        let db = ResultsDb::open(&db_path).expect("open");

        // An env-gated A/B: the flag is set in the environment, so it appears
        // in *neither* argv column. This is the case --grep used to miss.
        // Rows are otherwise identical, so `elapsed_ms` stands in as an id -
        // `query` doesn't hydrate children, so `captured_env` is empty here.
        let make_row = |elapsed_ms: i64, env: Option<(&str, &str)>| {
            RunRowBuilder::new()
                .update(|row| {
                    row.command = String::from("bench tilegen");
                    row.mode = Some(String::from("release"));
                    row.elapsed = Wall::LegacyMillis(elapsed_ms);
                    row.cli_args = Some(String::from("tilegen --zoom 14"));
                    row.invocation = test_invocation("brokkr tilegen --bench 3");
                    row.kv = env
                        .map(|(k, v)| vec![KvPair::text(format!("env.{k}"), v)])
                        .unwrap_or_default();
                })
                .build()
        };

        // Row 1: the ON arm. Row 2: explicit OFF baseline. Row 3: no capture.
        db.insert(&make_row(101, Some(("ELIVAGAR_LAYER_STATS", "1"))))
            .unwrap();
        db.insert(&make_row(102, Some(("ELIVAGAR_LAYER_STATS", "0"))))
            .unwrap();
        db.insert(&make_row(103, None)).unwrap();

        let grep = |terms: Vec<String>, not: Vec<String>| {
            db.query(&QueryFilter {
                commit: None,
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: terms,
                grep_v: not,
                env: Vec::new(),
                limit: 50,
            })
            .unwrap()
        };

        // The bare var name finds both arms - neither argv column mentions it.
        let rows = grep(vec!["LAYER_STATS".into()], Vec::new());
        assert_eq!(rows.len(), 2, "--grep must reach env.* keys in run_kv");

        // NAME=VALUE selects one arm, since the var is matched as typed.
        let rows = grep(vec!["ELIVAGAR_LAYER_STATS=1".into()], Vec::new());
        assert_eq!(rows.len(), 1, "--grep NAME=VALUE selects the ON arm");
        assert_eq!(rows[0].elapsed.ms(), 101);

        // grep_v's De Morgan flip: a term absent from every source, including
        // the env, excludes only the rows that actually carry it.
        let rows = grep(Vec::new(), vec!["LAYER_STATS=1".into()]);
        assert_eq!(rows.len(), 2, "--grep-v must exclude only the ON arm");

        // Composes with the argv columns: the term hits brokkr_args, the
        // exclusion hits the env.
        let rows = grep(vec!["tilegen".into()], vec!["LAYER_STATS".into()]);
        assert_eq!(rows.len(), 1, "only the uncaptured row survives");
        assert_eq!(rows[0].elapsed.ms(), 103);

        // The `env.` storage prefix is stripped before matching, so it isn't
        // itself greppable - otherwise every captured row would match `env`.
        let rows = grep(vec!["env.".into()], Vec::new());
        assert!(rows.is_empty(), "the env. key prefix must not be greppable");

        drop(db);
        cleanup(&dir, &db_path);
    }

    fn plain_row(command: &str, kv: Vec<KvPair>) -> RunRow {
        RunRowBuilder::new()
            .update(|row| {
                row.command = String::from(command);
                row.invocation = test_invocation("");
                row.kv = kv;
            })
            .build()
    }

    #[test]
    fn meta_and_env_filters_match_numeric_values() {
        let db_path = crate::test_scratch::scratch("db-query", "meta_filter_numeric").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        db.insert(&plain_row("read", vec![KvPair::int("meta.threads", 8)]))
            .unwrap();
        db.insert(&plain_row("read", vec![KvPair::int("meta.threads", 4)]))
            .unwrap();
        db.insert(&plain_row("read", vec![KvPair::real("env.RATIO", 1.5)]))
            .unwrap();

        let rows = db
            .query(&QueryFilter {
                meta: vec![("threads".into(), "8".into())],
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "an Int meta value must be matchable by --meta"
        );

        let rows = db
            .query(&QueryFilter {
                env: vec![("RATIO".into(), "1.5".into())],
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1, "a Real env value must be matchable by --env");
    }

    #[test]
    fn command_filter_underscore_is_literal() {
        let db_path = crate::test_scratch::scratch("db-query", "underscore_literal").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        db.insert(&plain_row("tags-filter", vec![])).unwrap();
        db.insert(&plain_row("tags_filter", vec![])).unwrap();

        let rows = db
            .query(&QueryFilter {
                command: Some(String::from("tags_filter")),
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1, "_ must not act as a LIKE wildcard");
        assert_eq!(rows[0].command, "tags_filter");
    }

    #[test]
    fn uuid_prefix_enumeration_refuses_empty_prefix() {
        let db_path = crate::test_scratch::scratch("db-query", "uuid_empty").join("t.db");
        let db = ResultsDb::open(&db_path).expect("open");
        db.insert(&plain_row("read", vec![])).unwrap();
        assert!(
            db.uuids_matching_prefix("").is_err(),
            "an empty prefix must not match every row"
        );
    }
}
