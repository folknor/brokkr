use std::path::Path;

use super::types::short_uuid;
use super::{HotpathData, KvPair, KvValue, StoredRow};

// ---------------------------------------------------------------------------
// Public formatting API
// ---------------------------------------------------------------------------

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

/// Format a single result row as a standalone labelled block.
///
/// Used for `brokkr results` (bare, no filters) and `brokkr results <uuid>`,
/// where the one-row compact table is noise. Groups fields into four
/// sections:
///
///   1. Identity: uuid, timestamp, commit+subject, command, mode, elapsed, input
///   2. Host/build context: hostname, cargo, kernel, governor, memory, storage
///   3. Invocations: brokkr_args (single line), cli_args (pretty-printed
///      one flag/positional per line with `\` continuation, copy-pasteable)
///   4. Sidecar hint — only when `has_sidecar` is true. Terse form
///      `--timeline/--markers`.
///
/// Plus trailing sections for distribution stats and kv pairs when present.
pub fn format_single_result(row: &StoredRow, has_sidecar: bool) -> String {
    use std::fmt::Write;

    let ident = identity_fields(row);
    let host = host_fields(row);
    let invo = invocation_fields(row);
    let extras = extras_fields(row);
    let sidecar: Vec<(String, String)> = if has_sidecar {
        vec![("sidecar".into(), "--timeline/--markers".to_owned())]
    } else {
        Vec::new()
    };

    // Label width is computed across every field (including cli_args,
    // which is rendered multi-line but still uses the label column).
    let max_label = [&ident, &host, &invo, &extras, &sidecar]
        .iter()
        .flat_map(|v| v.iter().map(|(l, _)| l.len()))
        .chain(if row.cli_args.is_empty() {
            None
        } else {
            Some("cli_args".len())
        })
        .max()
        .unwrap_or(0);

    let mut out = String::new();

    let render_section = |out: &mut String, sec: &[(String, String)]| {
        if sec.is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        for (l, v) in sec {
            writeln!(out, "{l:<max_label$}  {v}").expect("write to String is infallible");
        }
    };

    render_section(&mut out, &ident);
    render_section(&mut out, &host);

    // Invocations: brokkr_args (single-line) + cli_args (multi-line).
    let has_invo = !invo.is_empty() || !row.cli_args.is_empty();
    if has_invo && !out.is_empty() {
        out.push('\n');
    }
    for (l, v) in &invo {
        writeln!(out, "{l:<max_label$}  {v}").expect("write to String is infallible");
    }
    if !row.cli_args.is_empty() {
        let indent = max_label + 2;
        let rendered = format_cli_args_multiline(&row.cli_args, indent);
        writeln!(out, "{:<max_label$}  {rendered}", "cli_args")
            .expect("write to String is infallible");
    }

    render_section(&mut out, &extras);
    render_section(&mut out, &sidecar);

    // Remove trailing newline.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn identity_fields(row: &StoredRow) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    if !row.uuid.is_empty() {
        fields.push(("uuid".into(), short_uuid(&row.uuid)));
    }
    if !row.timestamp.is_empty() {
        fields.push(("timestamp".into(), row.timestamp.clone()));
    }
    if !row.commit.is_empty() {
        let c = if row.subject.is_empty() {
            row.commit.clone()
        } else {
            format!("{} ({})", row.commit, row.subject)
        };
        fields.push(("commit".into(), c));
    }
    if !row.command.is_empty() {
        fields.push(("command".into(), row.command.clone()));
    }
    if !row.mode.is_empty() {
        fields.push(("mode".into(), row.mode.clone()));
    }
    fields.push(("elapsed".into(), format_elapsed(row.elapsed_ms)));
    if !row.input_file.is_empty() {
        let input = match row.input_mb {
            Some(mb) => format!("{} ({mb:.0} MB)", row.input_file),
            None => row.input_file.clone(),
        };
        fields.push(("input".into(), input));
    }
    fields
}

fn host_fields(row: &StoredRow) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    if !row.hostname.is_empty() {
        fields.push(("hostname".into(), row.hostname.clone()));
    }
    let prof = row.cargo_profile.as_str();
    let s = if row.cargo_features.is_empty() {
        prof.to_owned()
    } else {
        format!("{prof}, features: {}", row.cargo_features)
    };
    fields.push(("cargo".into(), s));
    if !row.kernel.is_empty() {
        fields.push(("kernel".into(), row.kernel.clone()));
    }
    if !row.cpu_governor.is_empty() {
        fields.push(("governor".into(), row.cpu_governor.clone()));
    }
    if let Some(mb) = row.avail_memory_mb {
        fields.push(("memory".into(), format!("{mb} MB")));
    }
    if let Some(mb) = row.peak_rss_mb {
        fields.push(("peak rss".into(), format!("{mb:.1} MB")));
    }
    if !row.storage_notes.is_empty() {
        fields.push(("storage".into(), row.storage_notes.clone()));
    }
    if !row.stop_marker.is_empty() {
        fields.push((
            "stop marker".into(),
            format!("killed at \"{}\"", row.stop_marker),
        ));
    }
    if !row.project.is_empty() && row.project != "pbfhogg" {
        fields.push(("project".into(), row.project.clone()));
    }
    fields
}

fn invocation_fields(row: &StoredRow) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    if !row.brokkr_args.is_empty() {
        fields.push(("brokkr_args".into(), row.brokkr_args.clone()));
    }
    // cli_args is rendered specially (multi-line) by the caller — not
    // included here.
    fields
}

fn extras_fields(row: &StoredRow) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(ref dist) = row.distribution {
        fields.push(("samples".into(), dist.samples.to_string()));
        fields.push(("min".into(), format!("{} ms", dist.min_ms)));
        fields.push(("p50".into(), format!("{} ms", dist.p50_ms)));
        fields.push(("p95".into(), format!("{} ms", dist.p95_ms)));
        fields.push(("max".into(), format!("{} ms", dist.max_ms)));
    }
    let mut meta_kv: Vec<&KvPair> = row
        .kv
        .iter()
        .filter(|kv| kv.key.starts_with("meta."))
        .collect();
    meta_kv.sort_by_key(|kv| &kv.key);
    for kv in &meta_kv {
        let label = kv
            .key
            .strip_prefix("meta.")
            .unwrap_or(&kv.key)
            .replace('_', " ");
        fields.push((label, kv.value.to_string()));
    }
    let mut runtime_kv: Vec<&KvPair> = row
        .kv
        .iter()
        .filter(|kv| !kv.key.starts_with("meta.") && !kv.key.starts_with("threads."))
        .collect();
    runtime_kv.sort_by_key(|kv| &kv.key);
    for kv in &runtime_kv {
        let label = kv.key.replace('_', " ");
        fields.push((label, kv.value.to_string()));
    }
    fields
}

/// Pretty-print a cli_args string as multi-line for the single-result
/// view. Pairs `--flag value` on a single line, emits each (flag or
/// positional) on its own line after the first, with `\` continuation
/// so the output copy-pastes into a shell.
fn format_cli_args_multiline(cli_args: &str, indent: usize) -> String {
    let tokens: Vec<&str> = cli_args.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }

    // Group `-f value` pairs onto the same line. Positionals and
    // boolean flags stay alone.
    let mut chunks: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if tok.starts_with('-') && i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
            chunks.push(format!("{tok} {}", tokens[i + 1]));
            i += 2;
        } else {
            chunks.push(tok.to_owned());
            i += 1;
        }
    }

    if chunks.len() == 1 {
        return chunks.into_iter().next().unwrap_or_default();
    }

    let indent_str = " ".repeat(indent);
    let mut out = String::new();
    for (n, chunk) in chunks.iter().enumerate() {
        if n == 0 {
            out.push_str(chunk);
        } else {
            out.push_str(" \\\n");
            out.push_str(&indent_str);
            out.push_str(chunk);
        }
    }
    out
}

/// Format the detail fields that aren't shown in the summary table.
///
/// Used for compare-table subheadings and other multi-row contexts
/// where the new `format_single_result` layout would be out of place.
pub fn format_details(row: &StoredRow) -> String {
    let mut out = String::new();
    let mut fields: Vec<(String, String)> = Vec::new();

    if !row.hostname.is_empty() {
        fields.push(("hostname".into(), row.hostname.clone()));
    }
    if !row.input_file.is_empty() {
        let input = match row.input_mb {
            Some(mb) => format!("{} ({mb:.0} MB)", row.input_file),
            None => row.input_file.clone(),
        };
        fields.push(("input".into(), input));
    }
    if !row.subject.is_empty() {
        fields.push(("subject".into(), row.subject.clone()));
    }
    if !row.cargo_features.is_empty() {
        fields.push(("cargo features".into(), row.cargo_features.clone()));
    }
    fields.push(("cargo profile".into(), row.cargo_profile.as_str().to_owned()));
    if !row.kernel.is_empty() {
        fields.push(("kernel".into(), row.kernel.clone()));
    }
    if !row.cpu_governor.is_empty() {
        fields.push(("cpu governor".into(), row.cpu_governor.clone()));
    }
    if let Some(mb) = row.avail_memory_mb {
        fields.push(("avail memory".into(), format!("{mb} MB")));
    }
    if let Some(mb) = row.peak_rss_mb {
        fields.push(("peak rss".into(), format!("{mb:.1} MB")));
    }
    if !row.storage_notes.is_empty() {
        fields.push(("storage".into(), row.storage_notes.clone()));
    }
    if !row.stop_marker.is_empty() {
        fields.push((
            "stop marker".into(),
            format!("killed at \"{}\"", row.stop_marker),
        ));
    }
    if !row.cli_args.is_empty() {
        fields.push(("cli args".into(), row.cli_args.clone()));
    }
    if !row.project.is_empty() && row.project != "pbfhogg" {
        fields.push(("project".into(), row.project.clone()));
    }

    // Distribution stats.
    if let Some(ref dist) = row.distribution {
        fields.push(("samples".into(), dist.samples.to_string()));
        fields.push(("min".into(), format!("{} ms", dist.min_ms)));
        fields.push(("p50".into(), format!("{} ms", dist.p50_ms)));
        fields.push(("p95".into(), format!("{} ms", dist.p95_ms)));
        fields.push(("max".into(), format!("{} ms", dist.max_ms)));
    }

    // Metadata kv pairs (meta. prefix).
    let mut meta_kv: Vec<&KvPair> = row
        .kv
        .iter()
        .filter(|kv| kv.key.starts_with("meta."))
        .collect();
    meta_kv.sort_by_key(|kv| &kv.key);
    for kv in &meta_kv {
        let label = kv
            .key
            .strip_prefix("meta.")
            .unwrap_or(&kv.key)
            .replace('_', " ");
        fields.push((label, kv.value.to_string()));
    }

    // Runtime kv pairs (non-meta, non-threads).
    let mut runtime_kv: Vec<&KvPair> = row
        .kv
        .iter()
        .filter(|kv| !kv.key.starts_with("meta.") && !kv.key.starts_with("threads."))
        .collect();
    runtime_kv.sort_by_key(|kv| &kv.key);
    for kv in &runtime_kv {
        let label = kv.key.replace('_', " ");
        fields.push((label, kv.value.to_string()));
    }

    if fields.is_empty() {
        return out;
    }

    let label_width = fields.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
    for (label, value) in &fields {
        use std::fmt::Write;
        writeln!(out, "  {label:<label_width$}  {value}").expect("write to String is infallible");
    }

    // Remove trailing newline.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Format side-by-side comparison of two commits.
pub fn format_compare(
    commit_a: &str,
    rows_a: &[StoredRow],
    commit_b: &str,
    rows_b: &[StoredRow],
    top: usize,
) -> String {
    let pairs = build_comparison_pairs(rows_a, rows_b);
    if pairs.is_empty() {
        return String::from("(no results)");
    }

    let widths = compute_compare_widths(commit_a, commit_b, &pairs);
    let mut out = String::new();

    append_compare_header(&mut out, commit_a, commit_b, &widths);
    out.push('\n');

    for pair in &pairs {
        append_compare_row(&mut out, pair, &widths);
        out.push('\n');
    }

    // Append hotpath diff tables for pairs that have hotpath data on both sides.
    for pair in &pairs {
        if let (Some(ha), Some(hb)) = (&pair.a_hotpath, &pair.b_hotpath)
            && let Some(diff) = crate::hotpath_fmt::format_hotpath_diff(ha, hb, top)
        {
            let (cmd, var, _) = split_pair_key(&pair.key);
            let label = if var.is_empty() {
                cmd.to_owned()
            } else {
                format!("{cmd} {var}")
            };
            let heading = if pair.input_display.is_empty() {
                format!("\n{label} — {commit_a} vs {commit_b}")
            } else {
                format!(
                    "\n{label} - {} — {commit_a} vs {commit_b}",
                    pair.input_display
                )
            };
            out.push_str(&heading);
            out.push('\n');
            out.push_str(&diff);
        }
    }

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

fn format_elapsed(ms: i64) -> String {
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
fn format_input(input_file: &str, input_mb: Option<f64>) -> String {
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
// Compare formatting internals
// ---------------------------------------------------------------------------

struct CompareWidths {
    command: usize,
    mode: usize,
    input: usize,
    col_a: usize,
    col_b: usize,
    change: usize,
    has_output: bool,
    output_a: usize,
    output_b: usize,
    output_change: usize,
    has_rss: bool,
    rss_a: usize,
    rss_b: usize,
    rss_change: usize,
    has_rewrite: bool,
    rewrite_a: usize,
    rewrite_b: usize,
    has_blobs: bool,
    blobs_a: usize,
    blobs_b: usize,
}

struct ComparisonPair {
    key: String,
    a_ms: Option<i64>,
    b_ms: Option<i64>,
    a_hotpath: Option<HotpathData>,
    b_hotpath: Option<HotpathData>,
    a_output_bytes: Option<i64>,
    b_output_bytes: Option<i64>,
    a_rss_mb: Option<f64>,
    b_rss_mb: Option<f64>,
    a_rewrite_pct: Option<f64>,
    b_rewrite_pct: Option<f64>,
    a_blobs: Option<String>,
    b_blobs: Option<String>,
    /// Pre-formatted input string for display.
    input_display: String,
}

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
fn find_output_bytes(kv: &[KvPair]) -> Option<i64> {
    find_kv_int(kv, "output_bytes")
}

/// Compute rewrite ratio percentage from bytes_passthrough and bytes_rewritten KV pairs.
fn compute_rewrite_pct(kv: &[KvPair]) -> Option<f64> {
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
fn format_blob_counts(kv: &[KvPair]) -> Option<String> {
    let pass = find_kv_int(kv, "blobs_passthrough")?;
    let rw = find_kv_int(kv, "blobs_rewritten")?;
    Some(format!("{pass}pt/{rw}rw"))
}

fn build_comparison_pairs(rows_a: &[StoredRow], rows_b: &[StoredRow]) -> Vec<ComparisonPair> {
    use std::collections::HashMap;

    struct RowData {
        elapsed_ms: i64,
        hotpath: Option<HotpathData>,
        output_bytes: Option<i64>,
        peak_rss_mb: Option<f64>,
        rewrite_pct: Option<f64>,
        blobs: Option<String>,
        input_display: String,
    }

    let mut keys: Vec<String> = Vec::new();
    let mut a_map: HashMap<String, RowData> = HashMap::new();
    let mut b_map: HashMap<String, RowData> = HashMap::new();

    for row in rows_a {
        let key = pair_key(&row.command, &row.mode, &row.input_file);
        if let std::collections::hash_map::Entry::Vacant(e) = a_map.entry(key.clone()) {
            keys.push(key);
            e.insert(RowData {
                elapsed_ms: row.elapsed_ms,
                hotpath: row.hotpath.clone(),
                output_bytes: find_output_bytes(&row.kv),
                peak_rss_mb: row.peak_rss_mb,
                rewrite_pct: compute_rewrite_pct(&row.kv),
                blobs: format_blob_counts(&row.kv),
                input_display: format_input(&row.input_file, row.input_mb),
            });
        }
    }
    for row in rows_b {
        let key = pair_key(&row.command, &row.mode, &row.input_file);
        if let std::collections::hash_map::Entry::Vacant(e) = b_map.entry(key.clone()) {
            if !a_map.contains_key(&key) {
                keys.push(key.clone());
            }
            e.insert(RowData {
                elapsed_ms: row.elapsed_ms,
                hotpath: row.hotpath.clone(),
                output_bytes: find_output_bytes(&row.kv),
                peak_rss_mb: row.peak_rss_mb,
                rewrite_pct: compute_rewrite_pct(&row.kv),
                blobs: format_blob_counts(&row.kv),
                input_display: format_input(&row.input_file, row.input_mb),
            });
        }
    }

    keys.into_iter()
        .map(|k| {
            let a = a_map.remove(&k);
            let b = b_map.remove(&k);
            let input_display = a
                .as_ref()
                .or(b.as_ref())
                .map(|r| r.input_display.clone())
                .unwrap_or_default();
            let a_output_bytes = a.as_ref().and_then(|r| r.output_bytes);
            let b_output_bytes = b.as_ref().and_then(|r| r.output_bytes);
            let a_rss_mb = a.as_ref().and_then(|r| r.peak_rss_mb);
            let b_rss_mb = b.as_ref().and_then(|r| r.peak_rss_mb);
            let a_rewrite_pct = a.as_ref().and_then(|r| r.rewrite_pct);
            let b_rewrite_pct = b.as_ref().and_then(|r| r.rewrite_pct);
            let a_blobs = a.as_ref().and_then(|r| r.blobs.clone());
            let b_blobs = b.as_ref().and_then(|r| r.blobs.clone());
            ComparisonPair {
                key: k,
                a_ms: a.as_ref().map(|r| r.elapsed_ms),
                b_ms: b.as_ref().map(|r| r.elapsed_ms),
                a_hotpath: a.and_then(|r| r.hotpath),
                b_hotpath: b.and_then(|r| r.hotpath),
                a_output_bytes,
                b_output_bytes,
                a_rss_mb,
                b_rss_mb,
                a_rewrite_pct,
                b_rewrite_pct,
                a_blobs,
                b_blobs,
                input_display,
            }
        })
        .collect()
}

fn pair_key(command: &str, variant: &str, input_file: &str) -> String {
    format!("{command}\t{variant}\t{input_file}")
}

fn split_pair_key(key: &str) -> (&str, &str, &str) {
    let mut parts = key.splitn(3, '\t');
    let cmd = parts.next().unwrap_or("");
    let var = parts.next().unwrap_or("");
    let input = parts.next().unwrap_or("");
    (cmd, var, input)
}

fn compute_compare_widths(
    commit_a: &str,
    commit_b: &str,
    pairs: &[ComparisonPair],
) -> CompareWidths {
    let has_output = pairs
        .iter()
        .any(|p| p.a_output_bytes.is_some() || p.b_output_bytes.is_some());
    let has_rss = pairs
        .iter()
        .any(|p| p.a_rss_mb.is_some() || p.b_rss_mb.is_some());
    let has_rewrite = pairs
        .iter()
        .any(|p| p.a_rewrite_pct.is_some() || p.b_rewrite_pct.is_some());
    let has_blobs = pairs
        .iter()
        .any(|p| p.a_blobs.is_some() || p.b_blobs.is_some());
    let mut w = CompareWidths {
        command: 7,
        mode: 7,
        input: "dataset".len(),
        col_a: commit_a.len().max(2),
        col_b: commit_b.len().max(2),
        change: 6,
        has_output,
        output_a: if has_output { "output_a".len() } else { 0 },
        output_b: if has_output { "output_b".len() } else { 0 },
        output_change: if has_output { "out_chg".len() } else { 0 },
        has_rss,
        rss_a: if has_rss { "rss_a".len() } else { 0 },
        rss_b: if has_rss { "rss_b".len() } else { 0 },
        rss_change: if has_rss { "rss_chg".len() } else { 0 },
        has_rewrite,
        rewrite_a: if has_rewrite { "rewrite_a".len() } else { 0 },
        rewrite_b: if has_rewrite { "rewrite_b".len() } else { 0 },
        has_blobs,
        blobs_a: if has_blobs { "blobs_a".len() } else { 0 },
        blobs_b: if has_blobs { "blobs_b".len() } else { 0 },
    };
    for pair in pairs {
        let (cmd, var, _) = split_pair_key(&pair.key);
        w.command = w.command.max(cmd.len());
        w.mode = w.mode.max(var.len());
        w.input = w.input.max(pair.input_display.len());
        w.col_a = w.col_a.max(format_ms_or_dash(pair.a_ms).len());
        w.col_b = w.col_b.max(format_ms_or_dash(pair.b_ms).len());
        w.change = w.change.max(format_change(pair.a_ms, pair.b_ms).len());
        if has_output {
            w.output_a = w
                .output_a
                .max(format_bytes_or_dash(pair.a_output_bytes).len());
            w.output_b = w
                .output_b
                .max(format_bytes_or_dash(pair.b_output_bytes).len());
            w.output_change = w
                .output_change
                .max(format_change_bytes(pair.a_output_bytes, pair.b_output_bytes).len());
        }
        if has_rss {
            w.rss_a = w.rss_a.max(format_rss_or_dash(pair.a_rss_mb).len());
            w.rss_b = w.rss_b.max(format_rss_or_dash(pair.b_rss_mb).len());
            w.rss_change = w
                .rss_change
                .max(format_change_rss(pair.a_rss_mb, pair.b_rss_mb).len());
        }
        if has_rewrite {
            w.rewrite_a = w
                .rewrite_a
                .max(format_pct_or_dash(pair.a_rewrite_pct).len());
            w.rewrite_b = w
                .rewrite_b
                .max(format_pct_or_dash(pair.b_rewrite_pct).len());
        }
        if has_blobs {
            w.blobs_a = w.blobs_a.max(format_opt_str_or_dash(&pair.a_blobs).len());
            w.blobs_b = w.blobs_b.max(format_opt_str_or_dash(&pair.b_blobs).len());
        }
    }
    w
}

fn append_compare_header(out: &mut String, commit_a: &str, commit_b: &str, w: &CompareWidths) {
    use std::fmt::Write;
    write!(
        out,
        "{:<cmd_w$}  {:<var_w$}  {:<in_w$}  {:>a_w$}  {:>b_w$}  {:>ch_w$}",
        "command",
        "mode",
        "dataset",
        commit_a,
        commit_b,
        "change",
        cmd_w = w.command,
        var_w = w.mode,
        in_w = w.input,
        a_w = w.col_a,
        b_w = w.col_b,
        ch_w = w.change,
    )
    .expect("write to String is infallible");
    if w.has_output {
        write!(
            out,
            "  {:>oa_w$}  {:>ob_w$}  {:>oc_w$}",
            "output_a",
            "output_b",
            "out_chg",
            oa_w = w.output_a,
            ob_w = w.output_b,
            oc_w = w.output_change,
        )
        .expect("write to String is infallible");
    }
    if w.has_rss {
        write!(
            out,
            "  {:>ra_w$}  {:>rb_w$}  {:>rc_w$}",
            "rss_a",
            "rss_b",
            "rss_chg",
            ra_w = w.rss_a,
            rb_w = w.rss_b,
            rc_w = w.rss_change,
        )
        .expect("write to String is infallible");
    }
    if w.has_rewrite {
        write!(
            out,
            "  {:>rwa_w$}  {:>rwb_w$}",
            "rewrite_a",
            "rewrite_b",
            rwa_w = w.rewrite_a,
            rwb_w = w.rewrite_b,
        )
        .expect("write to String is infallible");
    }
    if w.has_blobs {
        write!(
            out,
            "  {:>ba_w$}  {:>bb_w$}",
            "blobs_a",
            "blobs_b",
            ba_w = w.blobs_a,
            bb_w = w.blobs_b,
        )
        .expect("write to String is infallible");
    }
}

fn append_compare_row(out: &mut String, pair: &ComparisonPair, w: &CompareWidths) {
    use std::fmt::Write;
    let (cmd, var, _) = split_pair_key(&pair.key);
    let a_str = format_ms_or_dash(pair.a_ms);
    let b_str = format_ms_or_dash(pair.b_ms);
    let ch = format_change(pair.a_ms, pair.b_ms);
    write!(
        out,
        "{:<cmd_w$}  {:<var_w$}  {:<in_w$}  {:>a_w$}  {:>b_w$}  {:>ch_w$}",
        cmd,
        var,
        pair.input_display,
        a_str,
        b_str,
        ch,
        cmd_w = w.command,
        var_w = w.mode,
        in_w = w.input,
        a_w = w.col_a,
        b_w = w.col_b,
        ch_w = w.change,
    )
    .expect("write to String is infallible");
    if w.has_output {
        let oa = format_bytes_or_dash(pair.a_output_bytes);
        let ob = format_bytes_or_dash(pair.b_output_bytes);
        let oc = format_change_bytes(pair.a_output_bytes, pair.b_output_bytes);
        write!(
            out,
            "  {:>oa_w$}  {:>ob_w$}  {:>oc_w$}",
            oa,
            ob,
            oc,
            oa_w = w.output_a,
            ob_w = w.output_b,
            oc_w = w.output_change,
        )
        .expect("write to String is infallible");
    }
    if w.has_rss {
        let ra = format_rss_or_dash(pair.a_rss_mb);
        let rb = format_rss_or_dash(pair.b_rss_mb);
        let rc = format_change_rss(pair.a_rss_mb, pair.b_rss_mb);
        write!(
            out,
            "  {:>ra_w$}  {:>rb_w$}  {:>rc_w$}",
            ra,
            rb,
            rc,
            ra_w = w.rss_a,
            rb_w = w.rss_b,
            rc_w = w.rss_change,
        )
        .expect("write to String is infallible");
    }
    if w.has_rewrite {
        let rwa = format_pct_or_dash(pair.a_rewrite_pct);
        let rwb = format_pct_or_dash(pair.b_rewrite_pct);
        write!(
            out,
            "  {:>rwa_w$}  {:>rwb_w$}",
            rwa,
            rwb,
            rwa_w = w.rewrite_a,
            rwb_w = w.rewrite_b,
        )
        .expect("write to String is infallible");
    }
    if w.has_blobs {
        let ba = format_opt_str_or_dash(&pair.a_blobs);
        let bb = format_opt_str_or_dash(&pair.b_blobs);
        write!(
            out,
            "  {:>ba_w$}  {:>bb_w$}",
            ba,
            bb,
            ba_w = w.blobs_a,
            bb_w = w.blobs_b,
        )
        .expect("write to String is infallible");
    }
}

fn format_ms_or_dash(ms: Option<i64>) -> String {
    match ms {
        Some(v) => format!("{v} ms"),
        None => String::from("--"),
    }
}

fn format_change(a_ms: Option<i64>, b_ms: Option<i64>) -> String {
    match (a_ms, b_ms) {
        (Some(a), Some(b)) if a != 0 => {
            #[allow(clippy::cast_precision_loss)]
            let pct = ((b - a) as f64 / a as f64) * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        _ => String::from("--"),
    }
}

fn format_bytes_or_dash(bytes: Option<i64>) -> String {
    match bytes {
        Some(b) => {
            #[allow(clippy::cast_precision_loss)]
            let mb = b as f64 / (1024.0 * 1024.0);
            format!("{mb:.1} MB")
        }
        None => String::from("--"),
    }
}

fn format_change_bytes(a: Option<i64>, b: Option<i64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if a != 0 => {
            #[allow(clippy::cast_precision_loss)]
            let pct = ((b - a) as f64 / a as f64) * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        _ => String::from("--"),
    }
}

fn format_rss_or_dash(mb: Option<f64>) -> String {
    match mb {
        Some(v) => format!("{v:.1} MB"),
        None => String::from("--"),
    }
}

fn format_change_rss(a: Option<f64>, b: Option<f64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if a > 0.0 => {
            let pct = ((b - a) / a) * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        _ => String::from("--"),
    }
}

fn format_pct_or_dash(pct: Option<f64>) -> String {
    match pct {
        Some(v) => format!("{v:.1}%"),
        None => String::from("--"),
    }
}

fn format_opt_str_or_dash(s: &Option<String>) -> String {
    match s {
        Some(v) => v.clone(),
        None => String::from("--"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::StoredRow;

    // -----------------------------------------------------------------------
    // Helper: build a StoredRow with sensible defaults, overriding key fields
    // -----------------------------------------------------------------------

    fn row(command: &str, variant: &str, input_file: &str, elapsed_ms: i64) -> StoredRow {
        StoredRow {
            id: 0,
            timestamp: String::from("2026-03-01 00:00:00"),
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test commit"),
            command: command.to_owned(),
            mode: variant.to_owned(),
            input_file: input_file.to_owned(),
            input_mb: None,
            elapsed_ms,
            cargo_features: String::new(),
            cargo_profile: crate::build::CargoProfile::Release,
            kernel: String::new(),
            cpu_governor: String::new(),
            avail_memory_mb: None,
            storage_notes: String::new(),
            peak_rss_mb: None,
            uuid: String::from("abcdef1234567890"),
            cli_args: String::new(),
            brokkr_args: String::new(),
            project: String::from("test"),
            stop_marker: String::new(),
            kv: vec![],
            distribution: None,
            hotpath: None,
        }
    }

    // -----------------------------------------------------------------------
    // pair_key / split_pair_key roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn pair_key_roundtrip_normal() {
        let key = pair_key("read", "mmap", "denmark.osm.pbf");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "read");
        assert_eq!(var, "mmap");
        assert_eq!(input, "denmark.osm.pbf");
    }

    #[test]
    fn pair_key_roundtrip_empty_fields() {
        let key = pair_key("read", "", "");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "read");
        assert_eq!(var, "");
        assert_eq!(input, "");
    }

    #[test]
    fn pair_key_roundtrip_all_empty() {
        let key = pair_key("", "", "");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "");
        assert_eq!(var, "");
        assert_eq!(input, "");
    }

    #[test]
    fn pair_key_preserves_tabs_in_values() {
        // If a field contained a tab, splitn(3, '\t') would mangle it.
        // pair_key("a\tb", "c", "d") produces "a\tb\tc\td"
        // splitn(3, '\t') splits into ["a", "b", "c\td"]
        let key = pair_key("a\tb", "c", "d");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "a", "tab in command corrupts first field");
        assert_eq!(var, "b", "original variant is lost");
        assert_eq!(input, "c\td", "variant bleeds into input field");
    }

    #[test]
    fn split_pair_key_no_tabs() {
        let (cmd, var, input) = split_pair_key("notabs");
        assert_eq!(cmd, "notabs");
        assert_eq!(var, "");
        assert_eq!(input, "");
    }

    // -----------------------------------------------------------------------
    // build_comparison_pairs
    // -----------------------------------------------------------------------

    #[test]
    fn comparison_pairs_both_have_same_benchmark() {
        let a = vec![row("read", "mmap", "dk.pbf", 100)];
        let b = vec![row("read", "mmap", "dk.pbf", 90)];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, Some(100));
        assert_eq!(pairs[0].b_ms, Some(90));
    }

    #[test]
    fn comparison_pairs_a_only() {
        let a = vec![row("read", "mmap", "dk.pbf", 100)];
        let b: Vec<StoredRow> = vec![];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, Some(100));
        assert_eq!(pairs[0].b_ms, None);
    }

    #[test]
    fn comparison_pairs_b_only() {
        let a: Vec<StoredRow> = vec![];
        let b = vec![row("write", "", "out.pbf", 200)];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, None);
        assert_eq!(pairs[0].b_ms, Some(200));
    }

    #[test]
    fn comparison_pairs_deduplication_first_entry_wins() {
        // Two rows in A with the same key -- first one should win.
        let a = vec![
            row("read", "mmap", "dk.pbf", 100),
            row("read", "mmap", "dk.pbf", 999),
        ];
        let b = vec![
            row("read", "mmap", "dk.pbf", 50),
            row("read", "mmap", "dk.pbf", 888),
        ];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 1);
        assert_eq!(
            pairs[0].a_ms,
            Some(100),
            "first A entry should win, not 999"
        );
        assert_eq!(pairs[0].b_ms, Some(50), "first B entry should win, not 888");
    }

    #[test]
    fn comparison_pairs_ordering_a_first_then_b_new() {
        // A has benchmarks X and Y (in that order).
        // B has benchmarks Y and Z (in that order).
        // Expected key order: X, Y (from A), then Z (new from B).
        let a = vec![row("x-cmd", "", "", 10), row("y-cmd", "", "", 20)];
        let b = vec![row("y-cmd", "", "", 25), row("z-cmd", "", "", 30)];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 3);
        let key_strings: Vec<String> = pairs
            .iter()
            .map(|p| split_pair_key(&p.key).0.to_owned())
            .collect();
        assert_eq!(key_strings, vec!["x-cmd", "y-cmd", "z-cmd"]);

        // x-cmd: A-only
        assert_eq!(pairs[0].a_ms, Some(10));
        assert_eq!(pairs[0].b_ms, None);
        // y-cmd: both
        assert_eq!(pairs[1].a_ms, Some(20));
        assert_eq!(pairs[1].b_ms, Some(25));
        // z-cmd: B-only
        assert_eq!(pairs[2].a_ms, None);
        assert_eq!(pairs[2].b_ms, Some(30));
    }

    #[test]
    fn comparison_pairs_variant_and_input_matter() {
        // Same command but different variant/input should be separate pairs.
        let a = vec![
            row("read", "mmap", "dk.pbf", 100),
            row("read", "stdio", "dk.pbf", 200),
            row("read", "mmap", "se.pbf", 300),
        ];
        let b = vec![row("read", "mmap", "dk.pbf", 90)];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 3);
        // Only the first pair should have both sides.
        assert!(pairs[0].a_ms.is_some() && pairs[0].b_ms.is_some());
        assert!(pairs[1].a_ms.is_some() && pairs[1].b_ms.is_none());
        assert!(pairs[2].a_ms.is_some() && pairs[2].b_ms.is_none());
    }

    #[test]
    fn comparison_pairs_empty_both_sides() {
        let pairs = build_comparison_pairs(&[], &[]);
        assert!(pairs.is_empty());
    }

    // -----------------------------------------------------------------------
    // format_change
    // -----------------------------------------------------------------------

    #[test]
    fn format_change_improvement() {
        // 100 -> 80 = -20%
        let result = format_change(Some(100), Some(80));
        assert_eq!(result, "-20.0%");
    }

    #[test]
    fn format_change_regression() {
        // 100 -> 130 = +30%
        let result = format_change(Some(100), Some(130));
        assert_eq!(result, "+30.0%");
    }

    #[test]
    fn format_change_same_value() {
        let result = format_change(Some(500), Some(500));
        assert_eq!(result, "+0.0%");
    }

    #[test]
    fn format_change_zero_baseline() {
        // a=0 falls through the guard `a != 0`, returns "--"
        let result = format_change(Some(0), Some(100));
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_missing_a() {
        let result = format_change(None, Some(100));
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_missing_b() {
        let result = format_change(Some(100), None);
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_both_missing() {
        let result = format_change(None, None);
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_large_regression() {
        // 1 -> 1001 = +100000%
        let result = format_change(Some(1), Some(1001));
        assert_eq!(result, "+100000.0%");
    }

    #[test]
    fn format_change_near_zero_result() {
        // 1000 -> 999: -0.1%
        let result = format_change(Some(1000), Some(999));
        assert_eq!(result, "-0.1%");
    }

    #[test]
    fn format_change_both_zero() {
        // a=0 hits the guard, returns "--"
        let result = format_change(Some(0), Some(0));
        assert_eq!(result, "--");
    }

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

    // -----------------------------------------------------------------------
    // compare with merge-specific columns
    // -----------------------------------------------------------------------

    #[test]
    fn comparison_pairs_carry_rewrite_and_blobs() {
        let mut a = row("bench merge", "buffered+zlib:6", "dk.pbf", 4500);
        a.kv = vec![
            KvPair::int("bytes_passthrough", 400_000_000),
            KvPair::int("bytes_rewritten", 40_000_000),
            KvPair::int("blobs_passthrough", 1200),
            KvPair::int("blobs_rewritten", 100),
        ];
        let mut b = row("bench merge", "buffered+zlib:6", "dk.pbf", 4200);
        b.kv = vec![
            KvPair::int("bytes_passthrough", 410_000_000),
            KvPair::int("bytes_rewritten", 35_000_000),
            KvPair::int("blobs_passthrough", 1210),
            KvPair::int("blobs_rewritten", 90),
        ];
        let pairs = build_comparison_pairs(&[a], &[b]);
        assert_eq!(pairs.len(), 1);

        let p = &pairs[0];
        let a_rw = p.a_rewrite_pct.unwrap();
        let b_rw = p.b_rewrite_pct.unwrap();
        // A: 40M / 440M ≈ 9.09%
        assert!((a_rw - 9.09).abs() < 0.1);
        // B: 35M / 445M ≈ 7.87%
        assert!((b_rw - 7.87).abs() < 0.1);

        assert_eq!(p.a_blobs.as_deref(), Some("1200pt/100rw"));
        assert_eq!(p.b_blobs.as_deref(), Some("1210pt/90rw"));
    }

    #[test]
    fn format_compare_shows_rewrite_columns() {
        let mut a = row("bench merge", "buffered+zlib:6", "dk.pbf", 4500);
        a.commit = String::from("abc1234");
        a.kv = vec![
            KvPair::int("bytes_passthrough", 920),
            KvPair::int("bytes_rewritten", 80),
            KvPair::int("blobs_passthrough", 100),
            KvPair::int("blobs_rewritten", 10),
        ];
        let mut b = row("bench merge", "buffered+zlib:6", "dk.pbf", 4200);
        b.commit = String::from("def5678");
        b.kv = vec![
            KvPair::int("bytes_passthrough", 900),
            KvPair::int("bytes_rewritten", 100),
            KvPair::int("blobs_passthrough", 95),
            KvPair::int("blobs_rewritten", 15),
        ];
        let output = format_compare("abc1234", &[a], "def5678", &[b], 10);
        assert!(output.contains("rewrite_a"), "should have rewrite_a header");
        assert!(output.contains("rewrite_b"), "should have rewrite_b header");
        assert!(output.contains("blobs_a"), "should have blobs_a header");
        assert!(output.contains("blobs_b"), "should have blobs_b header");
        assert!(
            output.contains("8.0%"),
            "should show 8.0% rewrite ratio for A"
        );
        assert!(
            output.contains("10.0%"),
            "should show 10.0% rewrite ratio for B"
        );
        assert!(
            output.contains("100pt/10rw"),
            "should show blob counts for A"
        );
        assert!(
            output.contains("95pt/15rw"),
            "should show blob counts for B"
        );
    }

    #[test]
    fn format_compare_hides_rewrite_columns_when_absent() {
        let a = row("read", "mmap", "dk.pbf", 100);
        let b = row("read", "mmap", "dk.pbf", 90);
        let output = format_compare("aaa", &[a], "bbb", &[b], 10);
        assert!(
            !output.contains("rewrite_a"),
            "no rewrite columns for non-merge"
        );
        assert!(!output.contains("blobs_a"), "no blob columns for non-merge");
    }
}
