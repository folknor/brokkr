use super::super::types::short_uuid;
use super::super::{KvPair, StoredRow};
use super::table::format_elapsed;

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
    match (row.cargo_profile, row.cargo_features.as_str()) {
        (Some(prof), "") => {
            fields.push(("cargo".into(), prof.as_str().to_owned()));
        }
        (Some(prof), feats) => {
            fields.push(("cargo".into(), format!("{}, features: {feats}", prof.as_str())));
        }
        (None, "") => {}
        (None, feats) => {
            fields.push(("cargo".into(), format!("features: {feats}")));
        }
    }
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
