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
pub fn format_hotpath_report(extra: &serde_json::Value, top: usize) -> Option<String> {
    let obj = extra.as_object()?;

    let has_timing = obj.contains_key("functions_timing");
    let has_alloc = obj.contains_key("functions_alloc");

    if !has_timing && !has_alloc {
        return None;
    }

    let mut out = String::new();

    if let Some(timing) = obj.get("functions_timing") {
        format_functions_table(&mut out, timing, "timing", top);
    }

    if let Some(alloc) = obj.get("functions_alloc") {
        if !out.is_empty() {
            out.push('\n');
        }
        format_functions_table(&mut out, alloc, "alloc", top);
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

fn format_functions_table(out: &mut String, value: &serde_json::Value, label: &str, top: usize) {
    let Some(obj) = value.as_object() else {
        return;
    };

    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let all_data = match obj.get("data").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return,
    };

    let data: &[serde_json::Value] = if top > 0 && top < all_data.len() {
        &all_data[..top]
    } else {
        all_data
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

// ---------------------------------------------------------------------------
// Diff formatting
// ---------------------------------------------------------------------------

/// Format a side-by-side diff of two hotpath JSON reports.
///
/// Returns `None` if neither side contains `functions_timing` or
/// `functions_alloc` data.
pub fn format_hotpath_diff(
    extra_a: &serde_json::Value,
    extra_b: &serde_json::Value,
    top: usize,
) -> Option<String> {
    let obj_a = extra_a.as_object();
    let obj_b = extra_b.as_object();

    let has_timing = obj_a
        .is_some_and(|o| o.contains_key("functions_timing"))
        || obj_b.is_some_and(|o| o.contains_key("functions_timing"));
    let has_alloc = obj_a
        .is_some_and(|o| o.contains_key("functions_alloc"))
        || obj_b.is_some_and(|o| o.contains_key("functions_alloc"));

    if !has_timing && !has_alloc {
        return None;
    }

    let mut out = String::new();

    if has_timing {
        let timing_a = obj_a.and_then(|o| o.get("functions_timing"));
        let timing_b = obj_b.and_then(|o| o.get("functions_timing"));
        let section = format_section_diff(
            "timing",
            timing_a
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str()),
            timing_b
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str()),
            timing_a
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_array())
                .map(Vec::as_slice),
            timing_b
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_array())
                .map(Vec::as_slice),
            top,
        );
        out.push_str(&section);
    }

    if has_alloc {
        let alloc_a = obj_a.and_then(|o| o.get("functions_alloc"));
        let alloc_b = obj_b.and_then(|o| o.get("functions_alloc"));
        if !out.is_empty() {
            out.push('\n');
        }
        let section = format_section_diff(
            "alloc",
            alloc_a
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str()),
            alloc_b
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str()),
            alloc_a
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_array())
                .map(Vec::as_slice),
            alloc_b
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_array())
                .map(Vec::as_slice),
            top,
        );
        out.push_str(&section);
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn format_section_diff(
    label: &str,
    desc_a: Option<&str>,
    desc_b: Option<&str>,
    data_a: Option<&[serde_json::Value]>,
    data_b: Option<&[serde_json::Value]>,
    top: usize,
) -> String {
    use std::collections::HashMap;

    // Build name -> entry maps for each side.
    let mut map_a: HashMap<&str, &serde_json::Value> = HashMap::new();
    if let Some(entries) = data_a {
        for entry in entries {
            let name = json_str(entry, "name");
            if !name.is_empty() {
                map_a.insert(name, entry);
            }
        }
    }

    let mut map_b: HashMap<&str, &serde_json::Value> = HashMap::new();
    if let Some(entries) = data_b {
        for entry in entries {
            let name = json_str(entry, "name");
            if !name.is_empty() {
                map_b.insert(name, entry);
            }
        }
    }

    // Union of function names: A's order first, then new-in-B functions.
    let mut names: Vec<&str> = Vec::new();
    if let Some(entries) = data_a {
        for entry in entries {
            let name = json_str(entry, "name");
            if !name.is_empty() {
                names.push(name);
            }
        }
    }
    if let Some(entries) = data_b {
        for entry in entries {
            let name = json_str(entry, "name");
            if !name.is_empty() && !map_a.contains_key(name) {
                names.push(name);
            }
        }
    }

    if top > 0 && names.len() > top {
        names.truncate(top);
    }

    if names.is_empty() {
        return String::new();
    }

    // Precompute display values for each row.
    let placeholder = "--";
    let mut row_total_a: Vec<&str> = Vec::new();
    let mut row_total_b: Vec<&str> = Vec::new();
    let mut row_change: Vec<String> = Vec::new();

    for &name in &names {
        let ta = map_a.get(name).map(|e| json_str(e, "total"));
        let tb = map_b.get(name).map(|e| json_str(e, "total"));
        row_total_a.push(ta.unwrap_or(placeholder));
        row_total_b.push(tb.unwrap_or(placeholder));
        row_change.push(format_change_str(ta, tb));
    }

    // Compute column widths.
    let mut w_name = "Function".len();
    let mut w_total_a = "Total A".len();
    let mut w_total_b = "Total B".len();
    let mut w_change = "Change".len();

    for (i, &name) in names.iter().enumerate() {
        w_name = w_name.max(name.len());
        w_total_a = w_total_a.max(row_total_a[i].len());
        w_total_b = w_total_b.max(row_total_b[i].len());
        w_change = w_change.max(row_change[i].len());
    }

    let mut out = String::new();

    // Header line — prefer A's description, fall back to B's.
    let description = desc_a.or(desc_b).unwrap_or("");
    writeln!(out, "{label} - {description}").expect("write to String");

    // Column headers.
    writeln!(
        out,
        "{:<w_name$}  {:>w_total_a$}  {:>w_total_b$}  {:>w_change$}",
        "Function", "Total A", "Total B", "Change",
    )
    .expect("write to String");

    // Data rows.
    for (i, &name) in names.iter().enumerate() {
        writeln!(
            out,
            "{:<w_name$}  {:>w_total_a$}  {:>w_total_b$}  {:>w_change$}",
            name, row_total_a[i], row_total_b[i], row_change[i],
        )
        .expect("write to String");
    }

    out
}

/// Parse a formatted metric string to a raw `f64` value.
///
/// Handles duration units (ns/µs/ms/s → result in ms), byte units
/// (B/KB/MB/GB → result in bytes), and bare percentages.
fn parse_metric(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Bare percentage: "42.5%"
    if let Some(num_str) = s.strip_suffix('%') {
        return num_str.trim().parse::<f64>().ok();
    }

    // Split on last space to get (number, unit).
    let split_pos = s.rfind(' ')?;
    let num_str = s[..split_pos].trim();
    let unit = s[split_pos + 1..].trim();

    let number: f64 = num_str.parse().ok()?;

    let multiplier = match unit {
        // Duration → ms
        "ns" => 1e-6,
        "µs" => 1e-3,
        "ms" => 1.0,
        "s" => 1e3,
        // Bytes → bytes
        "B" => 1.0,
        "KB" => 1024.0,
        "MB" => 1_048_576.0,
        "GB" => 1_073_741_824.0,
        _ => return None,
    };

    Some(number * multiplier)
}

/// Format a change string comparing two metric values.
///
/// Returns a percentage like "+1.0%" or "-3.2%", or a status string
/// for missing/unparseable values.
fn format_change_str(a: Option<&str>, b: Option<&str>) -> String {
    match (a, b) {
        (Some(sa), Some(sb)) => {
            let va = parse_metric(sa);
            let vb = parse_metric(sb);
            match (va, vb) {
                (Some(fa), Some(fb)) if fa.abs() > f64::EPSILON => {
                    let pct = (fb - fa) / fa * 100.0;
                    format!("{pct:+.1}%")
                }
                _ => "--".to_string(),
            }
        }
        (Some(_), None) => "(gone)".to_string(),
        (None, Some(_)) => "(new)".to_string(),
        (None, None) => "--".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // parse_metric
    // -----------------------------------------------------------------------

    #[test]
    fn parse_metric_duration_ns() {
        let v = parse_metric("500 ns").unwrap();
        assert!((v - 0.0005).abs() < 1e-10, "500 ns should be 0.0005 ms, got {v}");
    }

    #[test]
    fn parse_metric_duration_us() {
        let v = parse_metric("1.5 µs").unwrap();
        assert!((v - 0.0015).abs() < 1e-10, "1.5 µs should be 0.0015 ms, got {v}");
    }

    #[test]
    fn parse_metric_duration_ms() {
        let v = parse_metric("42.3 ms").unwrap();
        assert!((v - 42.3).abs() < 1e-10, "42.3 ms should be 42.3 ms, got {v}");
    }

    #[test]
    fn parse_metric_duration_s() {
        let v = parse_metric("2.5 s").unwrap();
        assert!((v - 2500.0).abs() < 1e-10, "2.5 s should be 2500 ms, got {v}");
    }

    #[test]
    fn parse_metric_bytes_b() {
        let v = parse_metric("100 B").unwrap();
        assert!((v - 100.0).abs() < 1e-10, "100 B should be 100 bytes, got {v}");
    }

    #[test]
    fn parse_metric_bytes_kb() {
        let v = parse_metric("2 KB").unwrap();
        assert!((v - 2048.0).abs() < 1e-10, "2 KB should be 2048 bytes, got {v}");
    }

    #[test]
    fn parse_metric_bytes_mb() {
        let v = parse_metric("1 MB").unwrap();
        assert!((v - 1_048_576.0).abs() < 1e-10, "1 MB should be 1048576 bytes, got {v}");
    }

    #[test]
    fn parse_metric_bytes_gb() {
        let v = parse_metric("1.5 GB").unwrap();
        let expected = 1.5 * 1_073_741_824.0;
        assert!((v - expected).abs() < 1.0, "1.5 GB should be {expected} bytes, got {v}");
    }

    #[test]
    fn parse_metric_percentage() {
        let v = parse_metric("99.2%").unwrap();
        assert!((v - 99.2).abs() < 1e-10, "99.2% should be 99.2, got {v}");
    }

    #[test]
    fn parse_metric_percentage_with_space_before_pct() {
        // "42.5 %" has a space before %, so strip_suffix('%') gives "42.5 "
        // which trims to "42.5" and parses fine.
        let v = parse_metric("42.5 %").unwrap();
        assert!((v - 42.5).abs() < 1e-10, "42.5 % should parse as 42.5, got {v}");
    }

    #[test]
    fn parse_metric_empty_string() {
        assert!(parse_metric("").is_none(), "empty string should return None");
    }

    #[test]
    fn parse_metric_whitespace_only() {
        assert!(parse_metric("   ").is_none(), "whitespace-only should return None");
    }

    #[test]
    fn parse_metric_no_space_between_number_and_unit() {
        // "42ms" has no space, rfind(' ') returns None, and it doesn't end with '%'
        assert!(parse_metric("42ms").is_none(), "no-space metric should return None");
    }

    #[test]
    fn parse_metric_unknown_unit() {
        assert!(parse_metric("10 furlongs").is_none(), "unknown unit should return None");
    }

    #[test]
    fn parse_metric_leading_trailing_whitespace() {
        let v = parse_metric("  42.3 ms  ").unwrap();
        assert!((v - 42.3).abs() < 1e-10, "trimmed '42.3 ms' should parse, got {v}");
    }

    #[test]
    fn parse_metric_negative_number() {
        // Negative durations are unusual but the parser should handle them.
        let v = parse_metric("-5 ms").unwrap();
        assert!((v - (-5.0)).abs() < 1e-10, "-5 ms should be -5.0, got {v}");
    }

    #[test]
    fn parse_metric_zero() {
        let v = parse_metric("0 ms").unwrap();
        assert!((v - 0.0).abs() < 1e-10, "0 ms should be 0.0, got {v}");
    }

    #[test]
    fn parse_metric_bare_number_no_unit_no_pct() {
        // "42" — no space, no %, so rfind(' ') returns None → None.
        assert!(parse_metric("42").is_none(), "bare number without unit should return None");
    }

    // -----------------------------------------------------------------------
    // format_change_str
    // -----------------------------------------------------------------------

    #[test]
    fn format_change_str_normal_increase() {
        let result = format_change_str(Some("100 ms"), Some("110 ms"));
        assert_eq!(result, "+10.0%", "110ms vs 100ms should be +10.0%");
    }

    #[test]
    fn format_change_str_normal_decrease() {
        let result = format_change_str(Some("200 ms"), Some("180 ms"));
        assert_eq!(result, "-10.0%", "180ms vs 200ms should be -10.0%");
    }

    #[test]
    fn format_change_str_no_change() {
        let result = format_change_str(Some("50 ms"), Some("50 ms"));
        assert_eq!(result, "+0.0%", "same value should be +0.0%");
    }

    #[test]
    fn format_change_str_cross_unit() {
        // 1 s = 1000 ms. Going from 500 ms to 1 s is a +100% change.
        let result = format_change_str(Some("500 ms"), Some("1 s"));
        assert_eq!(result, "+100.0%", "500ms → 1s should be +100.0%");
    }

    #[test]
    fn format_change_str_gone() {
        let result = format_change_str(Some("100 ms"), None);
        assert_eq!(result, "(gone)", "present → absent should be (gone)");
    }

    #[test]
    fn format_change_str_new() {
        let result = format_change_str(None, Some("100 ms"));
        assert_eq!(result, "(new)", "absent → present should be (new)");
    }

    #[test]
    fn format_change_str_both_none() {
        let result = format_change_str(None, None);
        assert_eq!(result, "--", "both absent should be --");
    }

    #[test]
    fn format_change_str_unparseable_a() {
        let result = format_change_str(Some("garbage"), Some("100 ms"));
        assert_eq!(result, "--", "unparseable A should yield --");
    }

    #[test]
    fn format_change_str_unparseable_b() {
        let result = format_change_str(Some("100 ms"), Some("garbage"));
        assert_eq!(result, "--", "unparseable B should yield --");
    }

    #[test]
    fn format_change_str_both_unparseable() {
        let result = format_change_str(Some("foo"), Some("bar"));
        assert_eq!(result, "--", "both unparseable should yield --");
    }

    #[test]
    fn format_change_str_near_zero_baseline() {
        // Baseline is essentially zero (below f64::EPSILON).
        // The guard `fa.abs() > f64::EPSILON` should prevent division by near-zero.
        let result = format_change_str(Some("0 ms"), Some("100 ms"));
        assert_eq!(result, "--", "zero baseline should yield -- to avoid division by zero");
    }

    #[test]
    fn format_change_str_byte_units() {
        // 1 KB = 1024 B, 2 KB = 2048 B → +100%
        let result = format_change_str(Some("1 KB"), Some("2 KB"));
        assert_eq!(result, "+100.0%", "1KB → 2KB should be +100.0%");
    }

    // -----------------------------------------------------------------------
    // detect_percentile_keys
    // -----------------------------------------------------------------------

    #[test]
    fn detect_percentile_keys_standard() {
        let data = vec![json!({
            "name": "foo",
            "calls": 10,
            "avg": "1 ms",
            "total": "10 ms",
            "percent_total": "50%",
            "p50": "0.9 ms",
            "p95": "1.5 ms",
            "p99": "2.0 ms"
        })];
        let keys = detect_percentile_keys(&data);
        assert_eq!(keys, vec!["p50", "p95", "p99"], "should find p50, p95, p99 in numeric order");
    }

    #[test]
    fn detect_percentile_keys_numeric_sort_not_lexicographic() {
        // p5 < p10 < p99 numerically, but lexicographically p10 < p5 < p99.
        let data = vec![json!({
            "name": "bar",
            "calls": 1,
            "avg": "1 ms",
            "total": "1 ms",
            "percent_total": "100%",
            "p99": "5 ms",
            "p5": "0.1 ms",
            "p10": "0.2 ms"
        })];
        let keys = detect_percentile_keys(&data);
        assert_eq!(keys, vec!["p5", "p10", "p99"], "should sort numerically: p5, p10, p99");
    }

    #[test]
    fn detect_percentile_keys_no_percentiles() {
        let data = vec![json!({
            "name": "baz",
            "calls": 1,
            "avg": "1 ms",
            "total": "1 ms",
            "percent_total": "100%"
        })];
        let keys = detect_percentile_keys(&data);
        assert!(keys.is_empty(), "no percentile keys should return empty vec");
    }

    #[test]
    fn detect_percentile_keys_empty_data() {
        let data: Vec<serde_json::Value> = vec![];
        let keys = detect_percentile_keys(&data);
        assert!(keys.is_empty(), "empty data should return empty vec");
    }

    #[test]
    fn detect_percentile_keys_non_numeric_p_prefix_excluded() {
        // "parent" starts with 'p' but is not pNN. "pid" starts with 'p' but
        // p[1..] = "id" which is not all digits. These should be excluded.
        let data = vec![json!({
            "name": "fn",
            "calls": 1,
            "avg": "1 ms",
            "total": "1 ms",
            "percent_total": "100%",
            "parent": "main",
            "pid": "1234",
            "p50": "0.5 ms"
        })];
        let keys = detect_percentile_keys(&data);
        assert_eq!(keys, vec!["p50"], "'parent' and 'pid' should not appear, only p50");
    }

    #[test]
    fn detect_percentile_keys_known_fields_excluded() {
        // "id" is in the known_fields list and should be excluded even though it
        // doesn't start with 'p'. Just verifying known_fields filtering works.
        let data = vec![json!({
            "id": "abc",
            "name": "fn",
            "calls": 1,
            "avg": "1 ms",
            "total": "1 ms",
            "percent_total": "100%",
            "p75": "2 ms"
        })];
        let keys = detect_percentile_keys(&data);
        assert_eq!(keys, vec!["p75"], "known fields like 'id' should be excluded");
    }

    #[test]
    fn detect_percentile_keys_first_entry_only() {
        // Only the first entry is scanned. p99 in the second entry should not appear.
        let data = vec![
            json!({"name": "a", "calls": 1, "avg": "1 ms", "total": "1 ms", "percent_total": "50%", "p50": "1 ms"}),
            json!({"name": "b", "calls": 1, "avg": "2 ms", "total": "2 ms", "percent_total": "50%", "p50": "2 ms", "p99": "3 ms"}),
        ];
        let keys = detect_percentile_keys(&data);
        assert_eq!(keys, vec!["p50"], "only first entry's keys should be detected");
    }

    // -----------------------------------------------------------------------
    // format_hotpath_report — --top truncation
    // -----------------------------------------------------------------------

    fn make_timing_report(n: usize) -> serde_json::Value {
        let entries: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                json!({
                    "name": format!("fn_{i}"),
                    "calls": i + 1,
                    "avg": format!("{} ms", i + 1),
                    "total": format!("{} ms", (i + 1) * 10),
                    "percent_total": format!("{}%", 100 / n)
                })
            })
            .collect();
        json!({
            "functions_timing": {
                "description": "wall clock",
                "data": entries
            }
        })
    }

    #[test]
    fn format_hotpath_report_top_zero_shows_all() {
        let report = make_timing_report(5);
        let output = format_hotpath_report(&report, 0).unwrap();
        // Count data rows (all lines minus the header line and column header line).
        let lines: Vec<&str> = output.lines().collect();
        // Line 0: "timing - wall clock"
        // Line 1: column headers
        // Lines 2..6: data rows
        assert_eq!(lines.len(), 7, "top=0 should show all 5 data rows + 2 header lines");
    }

    #[test]
    fn format_hotpath_report_top_limits_rows() {
        let report = make_timing_report(10);
        let output = format_hotpath_report(&report, 3).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        // 2 header lines + 3 data rows = 5 lines total
        assert_eq!(lines.len(), 5, "top=3 should show 3 data rows + 2 header lines");
        assert!(lines[2].contains("fn_0"), "first data row should be fn_0");
        assert!(lines[4].contains("fn_2"), "last data row should be fn_2");
        assert!(!output.contains("fn_3"), "fn_3 should be truncated");
    }

    #[test]
    fn format_hotpath_report_top_larger_than_data() {
        let report = make_timing_report(3);
        let output = format_hotpath_report(&report, 100).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 5, "top=100 with 3 entries should show all 3 + 2 headers");
    }

    #[test]
    fn format_hotpath_report_returns_none_for_no_sections() {
        let extra = json!({"unrelated": "data"});
        assert!(format_hotpath_report(&extra, 0).is_none(), "no timing/alloc → None");
    }

    #[test]
    fn format_hotpath_report_returns_none_for_non_object() {
        let extra = json!("a string");
        assert!(format_hotpath_report(&extra, 0).is_none(), "non-object → None");
    }

    #[test]
    fn format_hotpath_report_returns_none_for_empty_data() {
        let extra = json!({
            "functions_timing": {
                "description": "wall clock",
                "data": []
            }
        });
        assert!(format_hotpath_report(&extra, 0).is_none(), "empty data array → None");
    }

    // -----------------------------------------------------------------------
    // format_section_diff
    // -----------------------------------------------------------------------

    #[test]
    fn format_section_diff_matched_functions() {
        let data_a = vec![
            json!({"name": "alpha", "total": "100 ms"}),
            json!({"name": "beta", "total": "200 ms"}),
        ];
        let data_b = vec![
            json!({"name": "alpha", "total": "110 ms"}),
            json!({"name": "beta", "total": "180 ms"}),
        ];
        let result = format_section_diff(
            "timing",
            Some("wall clock"),
            Some("wall clock"),
            Some(&data_a),
            Some(&data_b),
            0,
        );
        assert!(result.contains("alpha"), "output should contain matched fn 'alpha'");
        assert!(result.contains("beta"), "output should contain matched fn 'beta'");
        assert!(result.contains("+10.0%"), "alpha 100→110 should show +10.0%");
        assert!(result.contains("-10.0%"), "beta 200→180 should show -10.0%");
    }

    #[test]
    fn format_section_diff_unmatched_functions() {
        let data_a = vec![json!({"name": "only_in_a", "total": "50 ms"})];
        let data_b = vec![json!({"name": "only_in_b", "total": "75 ms"})];
        let result = format_section_diff(
            "timing",
            Some("desc"),
            Some("desc"),
            Some(&data_a),
            Some(&data_b),
            0,
        );
        assert!(result.contains("only_in_a"), "should contain function only in A");
        assert!(result.contains("only_in_b"), "should contain function only in B");
        // only_in_a is present in A but absent in B → (gone)
        assert!(result.contains("(gone)"), "function only in A should show (gone)");
        // only_in_b is absent in A but present in B → (new)
        assert!(result.contains("(new)"), "function only in B should show (new)");
    }

    #[test]
    fn format_section_diff_ordering_a_first_then_new_in_b() {
        let data_a = vec![
            json!({"name": "second", "total": "20 ms"}),
            json!({"name": "first", "total": "10 ms"}),
        ];
        let data_b = vec![
            json!({"name": "newcomer", "total": "30 ms"}),
            json!({"name": "first", "total": "11 ms"}),
        ];
        let result = format_section_diff("t", None, None, Some(&data_a), Some(&data_b), 0);
        let lines: Vec<&str> = result.lines().collect();
        // Lines: header, col headers, then data rows in order: second, first, newcomer
        let data_lines: Vec<&&str> = lines.iter().skip(2).collect();
        assert!(
            data_lines[0].starts_with("second"),
            "A's ordering should come first: got {}",
            data_lines[0]
        );
        assert!(
            data_lines[1].starts_with("first"),
            "A's ordering should come first: got {}",
            data_lines[1]
        );
        assert!(
            data_lines[2].starts_with("newcomer"),
            "new-in-B should come after A's entries: got {}",
            data_lines[2]
        );
    }

    #[test]
    fn format_section_diff_top_truncation() {
        let data_a = vec![
            json!({"name": "a", "total": "1 ms"}),
            json!({"name": "b", "total": "2 ms"}),
            json!({"name": "c", "total": "3 ms"}),
            json!({"name": "d", "total": "4 ms"}),
        ];
        let data_b = vec![
            json!({"name": "a", "total": "1.1 ms"}),
            json!({"name": "b", "total": "2.2 ms"}),
            json!({"name": "c", "total": "3.3 ms"}),
            json!({"name": "d", "total": "4.4 ms"}),
            json!({"name": "e", "total": "5.5 ms"}),
        ];
        let result = format_section_diff("t", Some("d"), None, Some(&data_a), Some(&data_b), 2);
        let lines: Vec<&str> = result.lines().collect();
        // 1 header + 1 col header + 2 data rows = 4
        assert_eq!(lines.len(), 4, "top=2 should yield 4 lines total, got {}", lines.len());
        assert!(!result.contains("\"c\"") && !result.contains("  c  "), "c should be truncated");
        assert!(!result.contains("  d  "), "d should be truncated");
        assert!(!result.contains("  e  "), "e should be truncated");
    }

    #[test]
    fn format_section_diff_both_sides_none() {
        let result = format_section_diff("t", None, None, None, None, 0);
        assert!(result.is_empty(), "both sides None should yield empty string");
    }

    #[test]
    fn format_section_diff_one_side_none() {
        let data_b = vec![json!({"name": "fn1", "total": "10 ms"})];
        let result = format_section_diff("t", None, Some("d"), None, Some(&data_b), 0);
        assert!(result.contains("fn1"), "function from B side should appear");
        assert!(result.contains("(new)"), "function absent from A should show (new)");
        assert!(result.contains("--"), "A's total should be placeholder --");
    }

    #[test]
    fn format_section_diff_description_fallback() {
        // desc_a is preferred; when missing, desc_b is used.
        let data = vec![json!({"name": "f", "total": "1 ms"})];
        let result =
            format_section_diff("timing", None, Some("fallback desc"), Some(&data), Some(&data), 0);
        assert!(
            result.contains("fallback desc"),
            "should fall back to B's description"
        );
    }

    #[test]
    fn format_section_diff_empty_named_entries_skipped() {
        // Entries with empty name should be excluded from the union.
        let data_a = vec![
            json!({"name": "", "total": "1 ms"}),
            json!({"name": "real", "total": "2 ms"}),
        ];
        let result = format_section_diff("t", None, None, Some(&data_a), Some(&data_a), 0);
        let data_lines: Vec<&str> = result.lines().skip(2).collect();
        assert_eq!(data_lines.len(), 1, "empty-name entry should be excluded, leaving 1 row");
        assert!(data_lines[0].starts_with("real"), "only 'real' should appear");
    }
}
