use super::super::types::short_uuid;
use super::super::{KvPair, KvValue, StoredRow};
use super::DatasetMatcher;

/// Format rows as a column-aligned table for stdout.
pub fn format_table(rows: &[StoredRow], matcher: &DatasetMatcher) -> String {
    if rows.is_empty() {
        return String::from("(no results)");
    }

    let widths = compute_table_widths(rows, matcher);
    let mut out = String::new();

    // Header line.
    append_table_header(&mut out, &widths);
    out.push('\n');

    // Data lines.
    for row in rows {
        append_table_row(&mut out, row, &widths, matcher);
        out.push('\n');
    }

    // Remove trailing newline.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Table formatting internals
// ---------------------------------------------------------------------------

struct TableWidths {
    uuid: usize,
    timestamp: usize,
    commit: usize,
    command: usize,
    mode: usize,
    elapsed: usize,
    input: usize,
    args: usize,
    /// Width of the `env` column. `0` means "elide the column entirely" -
    /// the common case, since captured_env is empty on almost every
    /// historical row and opt-in per brokkr.toml for new runs.
    env: usize,
}

/// Max width for the `args` column - anything longer gets truncated
/// with `…`. Picked to keep a full table row under ~120 columns on
/// typical command shapes.
const ARGS_MAX_WIDTH: usize = 40;

/// Max width for the `env` column (appears only when any row has
/// captured env vars). Narrower than `args` - env vars are typically
/// a handful of `KEY=VALUE` flags.
const ENV_MAX_WIDTH: usize = 30;

/// Format the captured-env map as a short, scannable cell for the
/// results table. Empty string when no vars were captured (the column
/// itself is then elided at header/row time). Keys are shown without
/// the project prefix when all keys share the same leading
/// `UPPERCASE_` token, otherwise the full names are used.
fn format_env_summary(env: &std::collections::BTreeMap<String, String>) -> String {
    if env.is_empty() {
        return String::new();
    }
    let strip = common_uppercase_prefix(env.keys().map(String::as_str));
    let mut parts: Vec<String> = env
        .iter()
        .map(|(k, v)| {
            let short = strip
                .as_deref()
                .and_then(|p| k.strip_prefix(p))
                .unwrap_or(k);
            format!("{short}={v}")
        })
        .collect();
    parts.sort();
    let joined = parts.join(",");
    if joined.chars().count() <= ENV_MAX_WIDTH {
        joined
    } else {
        let mut out: String = joined.chars().take(ENV_MAX_WIDTH - 1).collect();
        out.push('…');
        out
    }
}

/// Return the longest `UPPERCASE_` prefix common to every key (including
/// the trailing underscore), or `None` if the keys don't share one.
fn common_uppercase_prefix<'a>(keys: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut iter = keys.into_iter();
    let first = iter.next()?;
    let end = first.find('_')?;
    if !first[..end].chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let prefix = &first[..=end];
    for k in iter {
        if !k.starts_with(prefix) {
            return None;
        }
    }
    Some(prefix.to_owned())
}

/// Build a compact args summary from the row's `cli_args`, dropping
/// the leading binary path, the subcommand token (already represented
/// by the `command` column, possibly under a preset alias like
/// `write` → `bench-write`), any absolute-path positional arguments
/// (input/output/config files), and the `-o <output>` pair. What's
/// left is the row's distinguishing flag set.
///
/// Truncated to [`ARGS_MAX_WIDTH`] chars with a trailing `…` marker.
/// Returns an empty string for rows without `cli_args` (older rows or
/// internal-only commands).
fn format_args_summary(cli_args: &str) -> String {
    if cli_args.is_empty() {
        return String::new();
    }
    let mut tokens = cli_args.split_whitespace();
    // Drop the binary path.
    tokens.next();
    // Drop the subcommand token unconditionally - preset rows
    // (`write` → `bench-write`, `diff-osc` → `diff`, etc.) have a
    // different spelling than `command`, so matching on `command`
    // would leak the preset name into the args column.
    tokens.next();

    let mut kept: Vec<&str> = Vec::new();
    let mut iter = tokens.peekable();
    while let Some(tok) = iter.next() {
        // `-o <path>` - drop both tokens.
        if tok == "-o" {
            iter.next();
            continue;
        }
        // Absolute paths (inputs, outputs, config files, tmp dirs).
        if tok.starts_with('/') {
            continue;
        }
        kept.push(tok);
    }

    let joined = kept.join(" ");
    if joined.chars().count() <= ARGS_MAX_WIDTH {
        joined
    } else {
        let mut out: String = joined.chars().take(ARGS_MAX_WIDTH - 1).collect();
        out.push('…');
        out
    }
}

fn compute_table_widths(rows: &[StoredRow], matcher: &DatasetMatcher) -> TableWidths {
    let has_env = rows.iter().any(|r| !r.captured_env.is_empty());
    let mut w = TableWidths {
        uuid: 4,
        timestamp: 9,
        commit: 6,
        command: 7,
        mode: 7,
        elapsed: 7,
        input: "dataset".len(),
        args: "args".len(),
        env: if has_env { "env".len() } else { 0 },
    };
    for row in rows {
        let uuid_short = short_uuid(&row.uuid);
        if uuid_short.len() > w.uuid {
            w.uuid = uuid_short.len();
        }
        if row.timestamp.len() > w.timestamp {
            w.timestamp = row.timestamp.len();
        }
        if row.commit.len() > w.commit {
            w.commit = row.commit.len();
        }
        if row.command.len() > w.command {
            w.command = row.command.len();
        }
        if row.mode.len() > w.mode {
            w.mode = row.mode.len();
        }
        let elapsed_str = format_elapsed(row.elapsed_ms);
        if elapsed_str.len() > w.elapsed {
            w.elapsed = elapsed_str.len();
        }
        let input_str = matcher.short_name(&row.input_file);
        if input_str.len() > w.input {
            w.input = input_str.len();
        }
        let args_str = format_args_summary(&row.cli_args);
        if args_str.chars().count() > w.args {
            w.args = args_str.chars().count();
        }
        if has_env {
            let env_str = format_env_summary(&row.captured_env);
            if env_str.chars().count() > w.env {
                w.env = env_str.chars().count();
            }
        }
    }
    w
}

fn append_table_header(out: &mut String, w: &TableWidths) {
    use std::fmt::Write;
    write!(
        out,
        "{:<uuid_w$}  {:<ts_w$}  {:<cm_w$}  {:<cmd_w$}  {:<var_w$}  {:>el_w$}  {:<in_w$}  {:<args_w$}",
        "uuid",
        "timestamp",
        "commit",
        "command",
        "mode",
        "elapsed",
        "dataset",
        "args",
        uuid_w = w.uuid,
        ts_w = w.timestamp,
        cm_w = w.commit,
        cmd_w = w.command,
        var_w = w.mode,
        el_w = w.elapsed,
        in_w = w.input,
        args_w = w.args,
    )
    .expect("write to String is infallible");
    if w.env > 0 {
        write!(out, "  {:<env_w$}", "env", env_w = w.env)
            .expect("write to String is infallible");
    }
}

fn append_table_row(
    out: &mut String,
    row: &StoredRow,
    w: &TableWidths,
    matcher: &DatasetMatcher,
) {
    use std::fmt::Write;
    let uuid_short = short_uuid(&row.uuid);
    let elapsed_str = format_elapsed(row.elapsed_ms);
    let input_str = matcher.short_name(&row.input_file);
    let args_str = format_args_summary(&row.cli_args);
    write!(
        out,
        "{:<uuid_w$}  {:<ts_w$}  {:<cm_w$}  {:<cmd_w$}  {:<var_w$}  {:>el_w$}  {:<in_w$}  {:<args_w$}",
        uuid_short,
        row.timestamp,
        row.commit,
        row.command,
        row.mode,
        elapsed_str,
        input_str,
        args_str,
        uuid_w = w.uuid,
        ts_w = w.timestamp,
        cm_w = w.commit,
        cmd_w = w.command,
        var_w = w.mode,
        el_w = w.elapsed,
        in_w = w.input,
        args_w = w.args,
    )
    .expect("write to String is infallible");
    if w.env > 0 {
        let env_str = format_env_summary(&row.captured_env);
        write!(out, "  {env_str:<env_w$}", env_w = w.env)
            .expect("write to String is infallible");
    }
}

pub(super) fn format_elapsed(ms: i64) -> String {
    format!("{ms} ms")
}

/// Format an input filename with size for compare tables
/// (e.g. `europe (35262 MB)`). Uses `matcher` to produce the short
/// dataset label - when configured dataset keys are available this
/// preserves hyphenated names like `greater-london`; otherwise the
/// fallback heuristic emits the first dash-separated component of the
/// basename.
pub(super) fn format_input(
    input_file: &str,
    input_mb: Option<f64>,
    matcher: &DatasetMatcher,
) -> String {
    let short = matcher.short_name(input_file);
    if short.is_empty() {
        return String::new();
    }
    match input_mb {
        Some(mb) => format!("{short} ({mb:.0} MB)"),
        None => short,
    }
}

// ---------------------------------------------------------------------------
// Column helpers shared with compare.rs
// ---------------------------------------------------------------------------

/// Find an integer KV pair by key name.
fn find_kv_int(kv: &[KvPair], key: &str) -> Option<i64> {
    kv.iter()
        .find(|p| p.key == key)
        .and_then(|p| match &p.value {
            KvValue::Int(v) => Some(*v),
            _ => None,
        })
}

/// Find output_bytes in a StoredRow's kv pairs.
pub(super) fn find_output_bytes(kv: &[KvPair]) -> Option<i64> {
    find_kv_int(kv, "output_bytes")
}

/// Compute rewrite ratio percentage from bytes_passthrough and bytes_rewritten KV pairs.
pub(super) fn compute_rewrite_pct(kv: &[KvPair]) -> Option<f64> {
    let pass = find_kv_int(kv, "bytes_passthrough")?;
    let rw = find_kv_int(kv, "bytes_rewritten")?;
    let total = pass + rw;
    if total == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some(rw as f64 / total as f64 * 100.0)
}

/// Format blob passthrough/rewritten counts from KV pairs.
pub(super) fn format_blob_counts(kv: &[KvPair]) -> Option<String> {
    let pass = find_kv_int(kv, "blobs_passthrough")?;
    let rw = find_kv_int(kv, "blobs_rewritten")?;
    Some(format!("{pass}pt/{rw}rw"))
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
    use super::*;

    fn format_input_with_empty(input_file: &str, input_mb: Option<f64>) -> String {
        format_input(input_file, input_mb, &DatasetMatcher::empty())
    }

    // -----------------------------------------------------------------------
    // format_input
    // -----------------------------------------------------------------------

    #[test]
    fn format_input_empty_filename() {
        let result = format_input_with_empty("", None);
        assert_eq!(result, "");
    }

    #[test]
    fn format_input_empty_filename_with_mb() {
        // Even if MB is provided, empty filename returns empty.
        let result = format_input_with_empty("", Some(42.0));
        assert_eq!(result, "");
    }

    #[test]
    fn format_input_with_extension_no_mb() {
        // No dash in basename: show the stem unchanged.
        let result = format_input_with_empty("denmark.osm.pbf", None);
        assert_eq!(result, "denmark.osm");
    }

    #[test]
    fn format_input_with_extension_and_mb() {
        let result = format_input_with_empty("denmark.osm.pbf", Some(123.4));
        assert_eq!(result, "denmark.osm (123 MB)");
    }

    #[test]
    fn format_input_no_extension() {
        let result = format_input_with_empty("rawfile", None);
        assert_eq!(result, "rawfile");
    }

    #[test]
    fn format_input_no_extension_with_mb() {
        let result = format_input_with_empty("rawfile", Some(0.5));
        assert_eq!(result, "rawfile (0 MB)");
    }

    #[test]
    fn format_input_path_with_directory() {
        // file_stem should extract from the basename
        let result = format_input_with_empty("data/inputs/denmark.pbf", None);
        assert_eq!(result, "denmark");
    }

    #[test]
    fn format_input_single_extension() {
        let result = format_input_with_empty("test.csv", Some(10.0));
        assert_eq!(result, "test (10 MB)");
    }

    #[test]
    fn format_input_dataset_prefix_dated() {
        // Convention filename: <dataset>-<date>-<seq>-<variant>.osm
        let result = format_input_with_empty("europe-20260301-seq4714-with-indexdata.osm", Some(35262.0));
        assert_eq!(result, "europe (35262 MB)");
    }

    #[test]
    fn format_input_dataset_prefix_raw() {
        let result = format_input_with_empty("denmark-raw.osm.pbf", None);
        assert_eq!(result, "denmark");
    }

    #[test]
    fn format_input_pmtiles_variant() {
        // PMTiles files like `denmark-elivagar.pmtiles` collapse to the dataset.
        let result = format_input_with_empty("denmark-elivagar.pmtiles", Some(250.0));
        assert_eq!(result, "denmark (250 MB)");
    }

    // -----------------------------------------------------------------------
    // format_elapsed
    // -----------------------------------------------------------------------

    #[test]
    fn format_elapsed_positive() {
        assert_eq!(format_elapsed(1234), "1234 ms");
    }

    #[test]
    fn format_elapsed_zero() {
        assert_eq!(format_elapsed(0), "0 ms");
    }

    #[test]
    fn format_elapsed_negative() {
        // Shouldn't happen in practice, but verify it doesn't panic.
        assert_eq!(format_elapsed(-5), "-5 ms");
    }

    // -----------------------------------------------------------------------
    // compute_rewrite_pct
    // -----------------------------------------------------------------------

    #[test]
    fn compute_rewrite_pct_both_present() {
        let kv = vec![
            KvPair::int("bytes_passthrough", 920),
            KvPair::int("bytes_rewritten", 80),
        ];
        let pct = compute_rewrite_pct(&kv).unwrap();
        assert!((pct - 8.0).abs() < 0.01);
    }

    #[test]
    fn compute_rewrite_pct_missing_key() {
        let kv = vec![KvPair::int("bytes_passthrough", 920)];
        assert!(compute_rewrite_pct(&kv).is_none());
    }

    #[test]
    fn compute_rewrite_pct_zero_total() {
        let kv = vec![
            KvPair::int("bytes_passthrough", 0),
            KvPair::int("bytes_rewritten", 0),
        ];
        assert!(compute_rewrite_pct(&kv).is_none());
    }

    // -----------------------------------------------------------------------
    // format_blob_counts
    // -----------------------------------------------------------------------

    #[test]
    fn format_blob_counts_both_present() {
        let kv = vec![
            KvPair::int("blobs_passthrough", 1204),
            KvPair::int("blobs_rewritten", 98),
        ];
        assert_eq!(format_blob_counts(&kv).unwrap(), "1204pt/98rw");
    }

    #[test]
    fn format_blob_counts_missing_key() {
        let kv = vec![KvPair::int("blobs_passthrough", 100)];
        assert!(format_blob_counts(&kv).is_none());
    }
}
