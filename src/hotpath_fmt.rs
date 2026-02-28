//! Pretty-print hotpath JSON reports stored in the `extra` column.
//!
//! Reads the JSON structure produced by the `hotpath` crate when
//! `HOTPATH_OUTPUT_FORMAT=json` is set, and formats it as column-aligned
//! ASCII tables.

use std::fmt::Write;

/// Format a hotpath JSON report for display.
///
/// Returns `None` if `extra` doesn't contain hotpath data (no
/// `functions_timing` or `functions_alloc` key).
pub fn format_hotpath_report(extra: &serde_json::Value) -> Option<String> {
    let obj = extra.as_object()?;

    let has_timing = obj.contains_key("functions_timing");
    let has_alloc = obj.contains_key("functions_alloc");

    if !has_timing && !has_alloc {
        return None;
    }

    let mut out = String::new();

    if let Some(timing) = obj.get("functions_timing") {
        format_functions_table(&mut out, timing, "timing");
    }

    if let Some(alloc) = obj.get("functions_alloc") {
        if !out.is_empty() {
            out.push('\n');
        }
        format_functions_table(&mut out, alloc, "alloc");
    }

    if let Some(threads) = obj.get("threads") {
        if !out.is_empty() {
            out.push('\n');
        }
        format_threads_table(&mut out, threads);
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Functions table (timing or alloc)
// ---------------------------------------------------------------------------

fn format_functions_table(out: &mut String, value: &serde_json::Value, label: &str) {
    let Some(obj) = value.as_object() else {
        return;
    };

    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let data = match obj.get("data").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return,
    };

    // Determine which percentile columns exist.
    let percentile_keys = detect_percentile_keys(data);

    // Compute column widths.
    let mut w_name = "Function".len();
    let mut w_calls = "Calls".len();
    let mut w_avg = "Avg".len();
    let mut w_pcts: Vec<usize> = percentile_keys.iter().map(String::len).collect();
    let mut w_total = "Total".len();
    let mut w_pct_total = "% Total".len();

    for entry in data {
        w_name = w_name.max(json_str(entry, "name").len());
        w_calls = w_calls.max(format_calls(entry).len());
        w_avg = w_avg.max(json_str(entry, "avg").len());
        for (i, key) in percentile_keys.iter().enumerate() {
            w_pcts[i] = w_pcts[i].max(json_str(entry, key).len());
        }
        w_total = w_total.max(json_str(entry, "total").len());
        w_pct_total = w_pct_total.max(json_str(entry, "percent_total").len());
    }

    // Header line.
    writeln!(out, "{label} - {description}").expect("write to String");

    // Column headers.
    write!(
        out,
        "{:<w_name$}  {:>w_calls$}  {:>w_avg$}",
        "Function", "Calls", "Avg",
    )
    .expect("write to String");
    for (i, key) in percentile_keys.iter().enumerate() {
        write!(out, "  {:>width$}", key.to_uppercase(), width = w_pcts[i])
            .expect("write to String");
    }
    writeln!(
        out,
        "  {:>w_total$}  {:>w_pct_total$}",
        "Total", "% Total",
    )
    .expect("write to String");

    // Data rows.
    for entry in data {
        write!(
            out,
            "{:<w_name$}  {:>w_calls$}  {:>w_avg$}",
            json_str(entry, "name"),
            format_calls(entry),
            json_str(entry, "avg"),
        )
        .expect("write to String");
        for (i, key) in percentile_keys.iter().enumerate() {
            write!(out, "  {:>width$}", json_str(entry, key), width = w_pcts[i])
                .expect("write to String");
        }
        writeln!(
            out,
            "  {:>w_total$}  {:>w_pct_total$}",
            json_str(entry, "total"),
            json_str(entry, "percent_total"),
        )
        .expect("write to String");
    }
}

/// Detect percentile column keys from data entries.
///
/// The hotpath crate flattens percentiles into the entry object with keys like
/// "p50", "p95", "p99". We scan the first entry to discover which ones exist.
fn detect_percentile_keys(data: &[serde_json::Value]) -> Vec<String> {
    let known_fields = [
        "id",
        "name",
        "calls",
        "avg",
        "total",
        "percent_total",
    ];

    let Some(first) = data.first().and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };

    let mut pct_keys: Vec<String> = first
        .keys()
        .filter(|k| !known_fields.contains(&k.as_str()))
        .filter(|k| k.starts_with('p') && k[1..].chars().all(|c| c.is_ascii_digit()))
        .cloned()
        .collect();

    // Sort numerically by percentile value.
    pct_keys.sort_by_key(|k| k[1..].parse::<u32>().unwrap_or(0));

    pct_keys
}

// ---------------------------------------------------------------------------
// Threads table
// ---------------------------------------------------------------------------

fn format_threads_table(out: &mut String, value: &serde_json::Value) {
    let Some(obj) = value.as_object() else {
        return;
    };

    let data = match obj.get("data").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return,
    };

    // Build header annotation.
    let mut header_parts: Vec<String> = Vec::new();
    if let Some(rss) = obj.get("rss_bytes").and_then(|v| v.as_str()) {
        header_parts.push(format!("RSS: {rss}"));
    }
    if let Some(alloc) = obj.get("total_alloc_bytes").and_then(|v| v.as_str())
        && let Some(dealloc) = obj.get("total_dealloc_bytes").and_then(|v| v.as_str())
    {
        header_parts.push(format!("Alloc: {alloc}"));
        header_parts.push(format!("Dealloc: {dealloc}"));
        if let Some(diff) = obj.get("alloc_dealloc_diff").and_then(|v| v.as_str()) {
            header_parts.push(format!("Diff: {diff}"));
        }
    }

    let has_alloc = data.iter().any(|e| e.get("alloc_bytes").is_some());

    // Compute column widths.
    let mut w_name = "Thread".len();
    let mut w_status = "Status".len();
    let mut w_cpu_pct = "CPU%".len();
    let mut w_cpu_max = "Max%".len();
    let mut w_cpu_user = "CPU User".len();
    let mut w_cpu_sys = "CPU Sys".len();
    let mut w_cpu_total = "CPU Total".len();
    let mut w_alloc = "Alloc".len();
    let mut w_dealloc = "Dealloc".len();
    let mut w_diff = "Diff".len();

    for entry in data {
        w_name = w_name.max(json_str(entry, "name").len());
        w_status = w_status.max(json_str(entry, "status").len());
        w_cpu_pct = w_cpu_pct.max(json_str_opt(entry, "cpu_percent").len());
        w_cpu_max = w_cpu_max.max(json_str_opt(entry, "cpu_percent_max").len());
        w_cpu_user = w_cpu_user.max(json_str(entry, "cpu_user").len());
        w_cpu_sys = w_cpu_sys.max(json_str(entry, "cpu_sys").len());
        w_cpu_total = w_cpu_total.max(json_str(entry, "cpu_total").len());
        if has_alloc {
            w_alloc = w_alloc.max(json_str_opt(entry, "alloc_bytes").len());
            w_dealloc = w_dealloc.max(json_str_opt(entry, "dealloc_bytes").len());
            w_diff = w_diff.max(json_str_opt(entry, "mem_diff").len());
        }
    }

    // Header.
    let annotation = if header_parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", header_parts.join(", "))
    };
    writeln!(out, "threads{annotation}").expect("write to String");

    // Column headers.
    write!(
        out,
        "{:<w_name$}  {:<w_status$}  {:>w_cpu_pct$}  {:>w_cpu_max$}  {:>w_cpu_user$}  {:>w_cpu_sys$}  {:>w_cpu_total$}",
        "Thread", "Status", "CPU%", "Max%", "CPU User", "CPU Sys", "CPU Total",
    )
    .expect("write to String");
    if has_alloc {
        write!(
            out,
            "  {:>w_alloc$}  {:>w_dealloc$}  {:>w_diff$}",
            "Alloc", "Dealloc", "Diff",
        )
        .expect("write to String");
    }
    out.push('\n');

    // Data rows.
    for entry in data {
        write!(
            out,
            "{:<w_name$}  {:<w_status$}  {:>w_cpu_pct$}  {:>w_cpu_max$}  {:>w_cpu_user$}  {:>w_cpu_sys$}  {:>w_cpu_total$}",
            json_str(entry, "name"),
            json_str(entry, "status"),
            json_str_opt(entry, "cpu_percent"),
            json_str_opt(entry, "cpu_percent_max"),
            json_str(entry, "cpu_user"),
            json_str(entry, "cpu_sys"),
            json_str(entry, "cpu_total"),
        )
        .expect("write to String");
        if has_alloc {
            write!(
                out,
                "  {:>w_alloc$}  {:>w_dealloc$}  {:>w_diff$}",
                json_str_opt(entry, "alloc_bytes"),
                json_str_opt(entry, "dealloc_bytes"),
                json_str_opt(entry, "mem_diff"),
            )
            .expect("write to String");
        }
        out.push('\n');
    }
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

/// Get a string field from a JSON object, returning "" if missing.
fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
}

/// Get an optional string field, returning "-" if null/missing.
fn json_str_opt<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-")
}

/// Format the `calls` field (u64 in JSON, display as string).
fn format_calls(entry: &serde_json::Value) -> String {
    entry
        .get("calls")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_default()
}
