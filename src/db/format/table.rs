use std::path::Path;

use super::super::types::short_uuid;
use super::super::{KvPair, KvValue, StoredRow};

/// Format rows as a column-aligned table for stdout.
pub fn format_table(rows: &[StoredRow]) -> String {
    if rows.is_empty() {
        return String::from("(no results)");
    }

    let widths = compute_table_widths(rows);
    let mut out = String::new();

    // Header line.
    append_table_header(&mut out, &widths);
    out.push('\n');

    // Data lines.
    for row in rows {
        append_table_row(&mut out, row, &widths);
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
}

/// Max width for the `args` column — anything longer gets truncated
/// with `…`. Picked to keep a full table row under ~120 columns on
/// typical command shapes.
const ARGS_MAX_WIDTH: usize = 40;

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
    // Drop the subcommand token unconditionally — preset rows
    // (`write` → `bench-write`, `diff-osc` → `diff`, etc.) have a
    // different spelling than `command`, so matching on `command`
    // would leak the preset name into the args column.
    tokens.next();

    let mut kept: Vec<&str> = Vec::new();
    let mut iter = tokens.peekable();
    while let Some(tok) = iter.next() {
        // `-o <path>` — drop both tokens.
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

fn compute_table_widths(rows: &[StoredRow]) -> TableWidths {
    let mut w = TableWidths {
        uuid: 4,
        timestamp: 9,
        commit: 6,
        command: 7,
        mode: 7,
        elapsed: 7,
        input: "dataset".len(),
        args: "args".len(),
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
        let input_str = format_input_short(&row.input_file);
        if input_str.len() > w.input {
            w.input = input_str.len();
        }
        let args_str = format_args_summary(&row.cli_args);
        if args_str.chars().count() > w.args {
            w.args = args_str.chars().count();
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
}

fn append_table_row(out: &mut String, row: &StoredRow, w: &TableWidths) {
    use std::fmt::Write;
    let uuid_short = short_uuid(&row.uuid);
    let elapsed_str = format_elapsed(row.elapsed_ms);
    let input_str = format_input_short(&row.input_file);
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
}

pub(super) fn format_elapsed(ms: i64) -> String {
    format!("{ms} ms")
}

/// Format an input filename as a short, scannable dataset label for the
/// results table. Extracts the first dash-separated component of the
/// basename (e.g. `europe-20260301-seq4714-with-indexdata.osm` → `europe`,
/// `denmark-raw.osm.pbf` → `denmark`). The full basename (minus extension)
/// is used as a fallback when no dash is present, so non-conforming
/// filenames remain visible.
///
/// Used in the main results table. The file size is not included —
/// it's constant across rows for a given dataset and clutters the
/// table. The detail view surfaces size via its own `input` field.
fn format_input_short(input_file: &str) -> String {
    if input_file.is_empty() {
        return String::new();
    }
    let basename = Path::new(input_file)
        .file_stem()
        .map_or(input_file, |s| s.to_str().unwrap_or(input_file));
    basename
        .split_once('-')
        .map_or(basename, |(head, _)| head)
        .to_owned()
}

/// Format an input filename with size for compare tables and the
/// detail view (e.g. `europe (35262 MB)`).
pub(super) fn format_input(input_file: &str, input_mb: Option<f64>) -> String {
    let short = format_input_short(input_file);
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
    use super::*;

    // -----------------------------------------------------------------------
    // format_input
    // -----------------------------------------------------------------------

    #[test]
    fn format_input_empty_filename() {
        let result = format_input("", None);
        assert_eq!(result, "");
    }

    #[test]
    fn format_input_empty_filename_with_mb() {
        // Even if MB is provided, empty filename returns empty.
        let result = format_input("", Some(42.0));
        assert_eq!(result, "");
    }

    #[test]
    fn format_input_with_extension_no_mb() {
        // No dash in basename: show the stem unchanged.
        let result = format_input("denmark.osm.pbf", None);
        assert_eq!(result, "denmark.osm");
    }

    #[test]
    fn format_input_with_extension_and_mb() {
        let result = format_input("denmark.osm.pbf", Some(123.4));
        assert_eq!(result, "denmark.osm (123 MB)");
    }

    #[test]
    fn format_input_no_extension() {
        let result = format_input("rawfile", None);
        assert_eq!(result, "rawfile");
    }

    #[test]
    fn format_input_no_extension_with_mb() {
        let result = format_input("rawfile", Some(0.5));
        assert_eq!(result, "rawfile (0 MB)");
    }

    #[test]
    fn format_input_path_with_directory() {
        // file_stem should extract from the basename
        let result = format_input("data/inputs/denmark.pbf", None);
        assert_eq!(result, "denmark");
    }

    #[test]
    fn format_input_single_extension() {
        let result = format_input("test.csv", Some(10.0));
        assert_eq!(result, "test (10 MB)");
    }

    #[test]
    fn format_input_dataset_prefix_dated() {
        // Convention filename: <dataset>-<date>-<seq>-<variant>.osm
        let result = format_input("europe-20260301-seq4714-with-indexdata.osm", Some(35262.0));
        assert_eq!(result, "europe (35262 MB)");
    }

    #[test]
    fn format_input_dataset_prefix_raw() {
        let result = format_input("denmark-raw.osm.pbf", None);
        assert_eq!(result, "denmark");
    }

    #[test]
    fn format_input_pmtiles_variant() {
        // PMTiles files like `denmark-elivagar.pmtiles` collapse to the dataset.
        let result = format_input("denmark-elivagar.pmtiles", Some(250.0));
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
