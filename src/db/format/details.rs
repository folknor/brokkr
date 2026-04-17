use super::super::{KvPair, StoredRow};

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
    if let Some(prof) = row.cargo_profile {
        fields.push(("cargo profile".into(), prof.as_str().to_owned()));
    }
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
    for (k, v) in &row.captured_env {
        fields.push((format!("env {k}"), v.clone()));
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
