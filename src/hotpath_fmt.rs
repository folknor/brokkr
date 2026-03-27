//! Pretty-print hotpath profiling data from the results database.
//!
//! Reads the structured `HotpathData` produced by parsing the hotpath crate's
//! JSON output, and formats it as column-aligned ASCII tables.

use std::fmt::Write;

use crate::db::{HotpathData, HotpathFunction, HotpathThread, KvPair};

/// Format a hotpath report for display.
///
/// Returns `None` if `data` contains no functions and no threads.
pub fn format_hotpath_report(data: &HotpathData, top: usize) -> Option<String> {
    let timing: Vec<&HotpathFunction> = data
        .functions
        .iter()
        .filter(|f| f.section == "timing")
        .collect();
    let alloc: Vec<&HotpathFunction> = data
        .functions
        .iter()
        .filter(|f| f.section == "alloc")
        .collect();

    if timing.is_empty() && alloc.is_empty() && data.threads.is_empty() {
        return None;
    }

    let mut out = String::new();

    if !timing.is_empty() {
        format_functions_table(&mut out, &timing, "timing", top);
    }

    if !alloc.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        format_functions_table(&mut out, &alloc, "alloc", top);
    }

    if !data.threads.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        format_threads_table(&mut out, &data.threads, &data.thread_summary);
    }

    if out.is_empty() { None } else { Some(out) }
}

// ---------------------------------------------------------------------------
// Functions table (timing or alloc)
// ---------------------------------------------------------------------------

fn format_functions_table(
    out: &mut String,
    functions: &[&HotpathFunction],
    label: &str,
    top: usize,
) {
    if functions.is_empty() {
        return;
    }

    let description = functions
        .first()
        .and_then(|f| f.description.as_deref())
        .unwrap_or("");

    let data: &[&HotpathFunction] = if top > 0 && top < functions.len() {
        &functions[..top]
    } else {
        functions
    };

    // Determine which percentile columns exist.
    let percentile_keys = detect_percentile_keys(functions);

    // Compute column widths.
    let mut w_name = "Function".len();
    let mut w_calls = "Calls".len();
    let mut w_avg = "Avg".len();
    let mut w_pcts: Vec<usize> = percentile_keys.iter().map(String::len).collect();
    let mut w_total = "Total".len();
    let mut w_pct_total = "% Total".len();

    for f in data {
        w_name = w_name.max(f.name.len());
        w_calls = w_calls.max(f.calls.map(|c| c.to_string()).unwrap_or_default().len());
        w_avg = w_avg.max(f.avg.as_deref().unwrap_or("").len());
        for (i, key) in percentile_keys.iter().enumerate() {
            let val = percentile_value(f, key);
            w_pcts[i] = w_pcts[i].max(val.len());
        }
        w_total = w_total.max(f.total.as_deref().unwrap_or("").len());
        w_pct_total = w_pct_total.max(f.percent_total.as_deref().unwrap_or("").len());
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
    writeln!(out, "  {:>w_total$}  {:>w_pct_total$}", "Total", "% Total",)
        .expect("write to String");

    // Data rows.
    for f in data {
        write!(
            out,
            "{:<w_name$}  {:>w_calls$}  {:>w_avg$}",
            f.name,
            f.calls.map(|c| c.to_string()).unwrap_or_default(),
            f.avg.as_deref().unwrap_or(""),
        )
        .expect("write to String");
        for (i, key) in percentile_keys.iter().enumerate() {
            write!(
                out,
                "  {:>width$}",
                percentile_value(f, key),
                width = w_pcts[i]
            )
            .expect("write to String");
        }
        writeln!(
            out,
            "  {:>w_total$}  {:>w_pct_total$}",
            f.total.as_deref().unwrap_or(""),
            f.percent_total.as_deref().unwrap_or(""),
        )
        .expect("write to String");
    }
}

/// Detect which percentile columns are present across the function entries.
///
/// Checks for `p50`, `p95`, `p99` fields being `Some` on any entry, and
/// returns the keys in numeric order.
fn detect_percentile_keys(functions: &[&HotpathFunction]) -> Vec<String> {
    let mut keys = Vec::new();

    let has_p50 = functions.iter().any(|f| f.p50.is_some());
    let has_p95 = functions.iter().any(|f| f.p95.is_some());
    let has_p99 = functions.iter().any(|f| f.p99.is_some());

    if has_p50 {
        keys.push("p50".to_string());
    }
    if has_p95 {
        keys.push("p95".to_string());
    }
    if has_p99 {
        keys.push("p99".to_string());
    }

    keys
}

/// Get the percentile value from a function entry by key name.
fn percentile_value<'a>(f: &'a HotpathFunction, key: &str) -> &'a str {
    match key {
        "p50" => f.p50.as_deref().unwrap_or(""),
        "p95" => f.p95.as_deref().unwrap_or(""),
        "p99" => f.p99.as_deref().unwrap_or(""),
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Threads table
// ---------------------------------------------------------------------------

fn format_threads_table(out: &mut String, threads: &[HotpathThread], summary: &[KvPair]) {
    if threads.is_empty() {
        return;
    }

    // Build header annotation from summary KvPairs.
    let mut header_parts: Vec<String> = Vec::new();
    if let Some(rss) = summary.iter().find(|kv| kv.key == "threads.rss_bytes") {
        header_parts.push(format!("RSS: {}", kv_value_str(rss)));
    }
    let alloc_kv = summary
        .iter()
        .find(|kv| kv.key == "threads.total_alloc_bytes");
    let dealloc_kv = summary
        .iter()
        .find(|kv| kv.key == "threads.total_dealloc_bytes");
    if let (Some(alloc), Some(dealloc)) = (alloc_kv, dealloc_kv) {
        header_parts.push(format!("Alloc: {}", kv_value_str(alloc)));
        header_parts.push(format!("Dealloc: {}", kv_value_str(dealloc)));
        if let Some(diff) = summary
            .iter()
            .find(|kv| kv.key == "threads.alloc_dealloc_diff")
        {
            header_parts.push(format!("Diff: {}", kv_value_str(diff)));
        }
    }

    let has_alloc = threads.iter().any(|t| t.alloc_bytes.is_some());

    // Compute column widths.
    let mut w_name = "Thread".len();
    let mut w_status = "Status".len();
    let mut w_cpu_pct = "CPU%".len();
    let mut w_cpu_max = "Max%".len();
    let mut w_cpu_avg = "Avg%".len();
    let mut w_alloc = "Alloc".len();
    let mut w_dealloc = "Dealloc".len();
    let mut w_diff = "Diff".len();

    for t in threads {
        w_name = w_name.max(t.name.len());
        w_status = w_status.max(t.status.as_deref().unwrap_or("").len());
        w_cpu_pct = w_cpu_pct.max(opt_or_dash(t.cpu_percent.as_deref()).len());
        w_cpu_max = w_cpu_max.max(opt_or_dash(t.cpu_percent_max.as_deref()).len());
        w_cpu_avg = w_cpu_avg.max(opt_or_dash(t.cpu_percent_avg.as_deref()).len());
        if has_alloc {
            w_alloc = w_alloc.max(opt_or_dash(t.alloc_bytes.as_deref()).len());
            w_dealloc = w_dealloc.max(opt_or_dash(t.dealloc_bytes.as_deref()).len());
            w_diff = w_diff.max(opt_or_dash(t.mem_diff.as_deref()).len());
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
        "{:<w_name$}  {:<w_status$}  {:>w_cpu_pct$}  {:>w_cpu_max$}  {:>w_cpu_avg$}",
        "Thread", "Status", "CPU%", "Max%", "Avg%",
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
    for t in threads {
        write!(
            out,
            "{:<w_name$}  {:<w_status$}  {:>w_cpu_pct$}  {:>w_cpu_max$}  {:>w_cpu_avg$}",
            t.name,
            t.status.as_deref().unwrap_or(""),
            opt_or_dash(t.cpu_percent.as_deref()),
            opt_or_dash(t.cpu_percent_max.as_deref()),
            opt_or_dash(t.cpu_percent_avg.as_deref()),
        )
        .expect("write to String");
        if has_alloc {
            write!(
                out,
                "  {:>w_alloc$}  {:>w_dealloc$}  {:>w_diff$}",
                opt_or_dash(t.alloc_bytes.as_deref()),
                opt_or_dash(t.dealloc_bytes.as_deref()),
                opt_or_dash(t.mem_diff.as_deref()),
            )
            .expect("write to String");
        }
        out.push('\n');
    }
}

/// Return the string value of a `KvPair` for display.
fn kv_value_str(kv: &KvPair) -> String {
    kv.value.to_string()
}

/// Return the value if `Some`, otherwise "-".
fn opt_or_dash(val: Option<&str>) -> &str {
    val.unwrap_or("-")
}

// ---------------------------------------------------------------------------
// Diff formatting
// ---------------------------------------------------------------------------

/// Format a side-by-side diff of two hotpath reports.
///
/// Returns `None` if neither side contains timing or alloc function data.
pub fn format_hotpath_diff(
    data_a: &HotpathData,
    data_b: &HotpathData,
    top: usize,
) -> Option<String> {
    let timing_a: Vec<&HotpathFunction> = data_a
        .functions
        .iter()
        .filter(|f| f.section == "timing")
        .collect();
    let timing_b: Vec<&HotpathFunction> = data_b
        .functions
        .iter()
        .filter(|f| f.section == "timing")
        .collect();
    let alloc_a: Vec<&HotpathFunction> = data_a
        .functions
        .iter()
        .filter(|f| f.section == "alloc")
        .collect();
    let alloc_b: Vec<&HotpathFunction> = data_b
        .functions
        .iter()
        .filter(|f| f.section == "alloc")
        .collect();

    let has_timing = !timing_a.is_empty() || !timing_b.is_empty();
    let has_alloc = !alloc_a.is_empty() || !alloc_b.is_empty();

    if !has_timing && !has_alloc {
        return None;
    }

    let mut out = String::new();

    if has_timing {
        let section = format_section_diff("timing", &timing_a, &timing_b, top);
        out.push_str(&section);
    }

    if has_alloc {
        if !out.is_empty() {
            out.push('\n');
        }
        let section = format_section_diff("alloc", &alloc_a, &alloc_b, top);
        out.push_str(&section);
    }

    if out.is_empty() { None } else { Some(out) }
}

fn format_section_diff(
    label: &str,
    data_a: &[&HotpathFunction],
    data_b: &[&HotpathFunction],
    top: usize,
) -> String {
    use std::collections::HashMap;

    // Build name -> entry maps for each side.
    let mut map_a: HashMap<&str, &HotpathFunction> = HashMap::new();
    for f in data_a {
        if !f.name.is_empty() {
            map_a.insert(&f.name, f);
        }
    }

    let mut map_b: HashMap<&str, &HotpathFunction> = HashMap::new();
    for f in data_b {
        if !f.name.is_empty() {
            map_b.insert(&f.name, f);
        }
    }

    // Union of function names: A's order first, then new-in-B functions.
    let mut names: Vec<&str> = Vec::new();
    for f in data_a {
        if !f.name.is_empty() {
            names.push(&f.name);
        }
    }
    for f in data_b {
        if !f.name.is_empty() && !map_a.contains_key(f.name.as_str()) {
            names.push(&f.name);
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
        let ta = map_a.get(name).and_then(|f| f.total.as_deref());
        let tb = map_b.get(name).and_then(|f| f.total.as_deref());
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
    let desc_a = data_a.first().and_then(|f| f.description.as_deref());
    let desc_b = data_b.first().and_then(|f| f.description.as_deref());
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
/// Handles duration units (ns/us/ms/s -> result in ms), byte units
/// (B/KB/MB/GB -> result in bytes), and bare percentages.
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
        // Duration -> ms
        "ns" => 1e-6,
        "\u{b5}s" => 1e-3,
        "ms" => 1.0,
        "s" => 1e3,
        // Bytes -> bytes
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
    use crate::db::{HotpathData, HotpathFunction};

    // -----------------------------------------------------------------------
    // parse_metric
    // -----------------------------------------------------------------------

    #[test]
    fn parse_metric_duration_ns() {
        let v = parse_metric("500 ns").unwrap();
        assert!(
            (v - 0.0005).abs() < 1e-10,
            "500 ns should be 0.0005 ms, got {v}"
        );
    }

    #[test]
    fn parse_metric_duration_us() {
        let v = parse_metric("1.5 \u{b5}s").unwrap();
        assert!(
            (v - 0.0015).abs() < 1e-10,
            "1.5 \u{b5}s should be 0.0015 ms, got {v}"
        );
    }

    #[test]
    fn parse_metric_duration_ms() {
        let v = parse_metric("42.3 ms").unwrap();
        assert!(
            (v - 42.3).abs() < 1e-10,
            "42.3 ms should be 42.3 ms, got {v}"
        );
    }

    #[test]
    fn parse_metric_duration_s() {
        let v = parse_metric("2.5 s").unwrap();
        assert!(
            (v - 2500.0).abs() < 1e-10,
            "2.5 s should be 2500 ms, got {v}"
        );
    }

    #[test]
    fn parse_metric_bytes_b() {
        let v = parse_metric("100 B").unwrap();
        assert!(
            (v - 100.0).abs() < 1e-10,
            "100 B should be 100 bytes, got {v}"
        );
    }

    #[test]
    fn parse_metric_bytes_kb() {
        let v = parse_metric("2 KB").unwrap();
        assert!(
            (v - 2048.0).abs() < 1e-10,
            "2 KB should be 2048 bytes, got {v}"
        );
    }

    #[test]
    fn parse_metric_bytes_mb() {
        let v = parse_metric("1 MB").unwrap();
        assert!(
            (v - 1_048_576.0).abs() < 1e-10,
            "1 MB should be 1048576 bytes, got {v}"
        );
    }

    #[test]
    fn parse_metric_bytes_gb() {
        let v = parse_metric("1.5 GB").unwrap();
        let expected = 1.5 * 1_073_741_824.0;
        assert!(
            (v - expected).abs() < 1.0,
            "1.5 GB should be {expected} bytes, got {v}"
        );
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
        assert!(
            (v - 42.5).abs() < 1e-10,
            "42.5 % should parse as 42.5, got {v}"
        );
    }

    #[test]
    fn parse_metric_empty_string() {
        assert!(
            parse_metric("").is_none(),
            "empty string should return None"
        );
    }

    #[test]
    fn parse_metric_whitespace_only() {
        assert!(
            parse_metric("   ").is_none(),
            "whitespace-only should return None"
        );
    }

    #[test]
    fn parse_metric_no_space_between_number_and_unit() {
        // "42ms" has no space, rfind(' ') returns None, and it doesn't end with '%'
        assert!(
            parse_metric("42ms").is_none(),
            "no-space metric should return None"
        );
    }

    #[test]
    fn parse_metric_unknown_unit() {
        assert!(
            parse_metric("10 furlongs").is_none(),
            "unknown unit should return None"
        );
    }

    #[test]
    fn parse_metric_leading_trailing_whitespace() {
        let v = parse_metric("  42.3 ms  ").unwrap();
        assert!(
            (v - 42.3).abs() < 1e-10,
            "trimmed '42.3 ms' should parse, got {v}"
        );
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
        // "42" -- no space, no %, so rfind(' ') returns None -> None.
        assert!(
            parse_metric("42").is_none(),
            "bare number without unit should return None"
        );
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
        assert_eq!(result, "+100.0%", "500ms -> 1s should be +100.0%");
    }

    #[test]
    fn format_change_str_gone() {
        let result = format_change_str(Some("100 ms"), None);
        assert_eq!(result, "(gone)", "present -> absent should be (gone)");
    }

    #[test]
    fn format_change_str_new() {
        let result = format_change_str(None, Some("100 ms"));
        assert_eq!(result, "(new)", "absent -> present should be (new)");
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
        assert_eq!(
            result, "--",
            "zero baseline should yield -- to avoid division by zero"
        );
    }

    #[test]
    fn format_change_str_byte_units() {
        // 1 KB = 1024 B, 2 KB = 2048 B -> +100%
        let result = format_change_str(Some("1 KB"), Some("2 KB"));
        assert_eq!(result, "+100.0%", "1KB -> 2KB should be +100.0%");
    }

    // -----------------------------------------------------------------------
    // detect_percentile_keys
    // -----------------------------------------------------------------------

    fn make_fn(
        name: &str,
        p50: Option<&str>,
        p95: Option<&str>,
        p99: Option<&str>,
    ) -> HotpathFunction {
        HotpathFunction {
            section: "timing".to_string(),
            description: None,
            ordinal: 0,
            name: name.to_string(),
            calls: Some(1),
            avg: Some("1 ms".to_string()),
            total: Some("1 ms".to_string()),
            percent_total: Some("100%".to_string()),
            p50: p50.map(String::from),
            p95: p95.map(String::from),
            p99: p99.map(String::from),
        }
    }

    #[test]
    fn detect_percentile_keys_standard() {
        let funcs = vec![make_fn(
            "foo",
            Some("0.9 ms"),
            Some("1.5 ms"),
            Some("2.0 ms"),
        )];
        let refs: Vec<&HotpathFunction> = funcs.iter().collect();
        let keys = detect_percentile_keys(&refs);
        assert_eq!(
            keys,
            vec!["p50", "p95", "p99"],
            "should find p50, p95, p99 in numeric order"
        );
    }

    #[test]
    fn detect_percentile_keys_only_p50() {
        let funcs = vec![make_fn("bar", Some("0.5 ms"), None, None)];
        let refs: Vec<&HotpathFunction> = funcs.iter().collect();
        let keys = detect_percentile_keys(&refs);
        assert_eq!(keys, vec!["p50"], "should find only p50");
    }

    #[test]
    fn detect_percentile_keys_no_percentiles() {
        let funcs = vec![make_fn("baz", None, None, None)];
        let refs: Vec<&HotpathFunction> = funcs.iter().collect();
        let keys = detect_percentile_keys(&refs);
        assert!(
            keys.is_empty(),
            "no percentile keys should return empty vec"
        );
    }

    #[test]
    fn detect_percentile_keys_empty_data() {
        let refs: Vec<&HotpathFunction> = Vec::new();
        let keys = detect_percentile_keys(&refs);
        assert!(keys.is_empty(), "empty data should return empty vec");
    }

    #[test]
    fn detect_percentile_keys_mixed_across_entries() {
        // p50 on first entry, p99 on second — both should be detected.
        let funcs = vec![
            make_fn("a", Some("1 ms"), None, None),
            make_fn("b", None, None, Some("3 ms")),
        ];
        let refs: Vec<&HotpathFunction> = funcs.iter().collect();
        let keys = detect_percentile_keys(&refs);
        assert_eq!(
            keys,
            vec!["p50", "p99"],
            "should detect p50 from first and p99 from second"
        );
    }

    #[test]
    fn detect_percentile_keys_p95_and_p99_only() {
        let funcs = vec![make_fn("fn", None, Some("2 ms"), Some("5 ms"))];
        let refs: Vec<&HotpathFunction> = funcs.iter().collect();
        let keys = detect_percentile_keys(&refs);
        assert_eq!(
            keys,
            vec!["p95", "p99"],
            "should find p95 and p99 without p50"
        );
    }

    // -----------------------------------------------------------------------
    // format_hotpath_report -- --top truncation
    // -----------------------------------------------------------------------

    fn make_timing_data(n: usize) -> HotpathData {
        let functions: Vec<HotpathFunction> = (0..n)
            .map(|i| HotpathFunction {
                section: "timing".to_string(),
                description: Some("wall clock".to_string()),
                ordinal: i as i64,
                name: format!("fn_{i}"),
                calls: Some((i + 1) as i64),
                avg: Some(format!("{} ms", i + 1)),
                total: Some(format!("{} ms", (i + 1) * 10)),
                percent_total: Some(format!("{}%", 100 / n)),
                p50: None,
                p95: None,
                p99: None,
            })
            .collect();
        HotpathData {
            functions,
            threads: Vec::new(),
            thread_summary: Vec::new(),
        }
    }

    #[test]
    fn format_hotpath_report_top_zero_shows_all() {
        let data = make_timing_data(5);
        let output = format_hotpath_report(&data, 0).unwrap();
        // Count data rows (all lines minus the header line and column header line).
        let lines: Vec<&str> = output.lines().collect();
        // Line 0: "timing - wall clock"
        // Line 1: column headers
        // Lines 2..6: data rows
        assert_eq!(
            lines.len(),
            7,
            "top=0 should show all 5 data rows + 2 header lines"
        );
    }

    #[test]
    fn format_hotpath_report_top_limits_rows() {
        let data = make_timing_data(10);
        let output = format_hotpath_report(&data, 3).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        // 2 header lines + 3 data rows = 5 lines total
        assert_eq!(
            lines.len(),
            5,
            "top=3 should show 3 data rows + 2 header lines"
        );
        assert!(lines[2].contains("fn_0"), "first data row should be fn_0");
        assert!(lines[4].contains("fn_2"), "last data row should be fn_2");
        assert!(!output.contains("fn_3"), "fn_3 should be truncated");
    }

    #[test]
    fn format_hotpath_report_top_larger_than_data() {
        let data = make_timing_data(3);
        let output = format_hotpath_report(&data, 100).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(
            lines.len(),
            5,
            "top=100 with 3 entries should show all 3 + 2 headers"
        );
    }

    #[test]
    fn format_hotpath_report_returns_none_for_empty_data() {
        let data = HotpathData {
            functions: Vec::new(),
            threads: Vec::new(),
            thread_summary: Vec::new(),
        };
        assert!(
            format_hotpath_report(&data, 0).is_none(),
            "empty data -> None"
        );
    }

    // -----------------------------------------------------------------------
    // format_section_diff
    // -----------------------------------------------------------------------

    fn make_diff_fn(name: &str, total: &str) -> HotpathFunction {
        HotpathFunction {
            section: "timing".to_string(),
            description: Some("wall clock".to_string()),
            ordinal: 0,
            name: name.to_string(),
            calls: None,
            avg: None,
            total: Some(total.to_string()),
            percent_total: None,
            p50: None,
            p95: None,
            p99: None,
        }
    }

    #[test]
    fn format_section_diff_matched_functions() {
        let data_a = vec![
            make_diff_fn("alpha", "100 ms"),
            make_diff_fn("beta", "200 ms"),
        ];
        let data_b = vec![
            make_diff_fn("alpha", "110 ms"),
            make_diff_fn("beta", "180 ms"),
        ];
        let refs_a: Vec<&HotpathFunction> = data_a.iter().collect();
        let refs_b: Vec<&HotpathFunction> = data_b.iter().collect();
        let result = format_section_diff("timing", &refs_a, &refs_b, 0);
        assert!(
            result.contains("alpha"),
            "output should contain matched fn 'alpha'"
        );
        assert!(
            result.contains("beta"),
            "output should contain matched fn 'beta'"
        );
        assert!(
            result.contains("+10.0%"),
            "alpha 100->110 should show +10.0%"
        );
        assert!(
            result.contains("-10.0%"),
            "beta 200->180 should show -10.0%"
        );
    }

    #[test]
    fn format_section_diff_unmatched_functions() {
        let data_a = vec![make_diff_fn("only_in_a", "50 ms")];
        let data_b = vec![make_diff_fn("only_in_b", "75 ms")];
        let refs_a: Vec<&HotpathFunction> = data_a.iter().collect();
        let refs_b: Vec<&HotpathFunction> = data_b.iter().collect();
        let result = format_section_diff("timing", &refs_a, &refs_b, 0);
        assert!(
            result.contains("only_in_a"),
            "should contain function only in A"
        );
        assert!(
            result.contains("only_in_b"),
            "should contain function only in B"
        );
        // only_in_a is present in A but absent in B -> (gone)
        assert!(
            result.contains("(gone)"),
            "function only in A should show (gone)"
        );
        // only_in_b is absent in A but present in B -> (new)
        assert!(
            result.contains("(new)"),
            "function only in B should show (new)"
        );
    }

    #[test]
    fn format_section_diff_ordering_a_first_then_new_in_b() {
        let data_a = vec![
            make_diff_fn("second", "20 ms"),
            make_diff_fn("first", "10 ms"),
        ];
        let data_b = vec![
            make_diff_fn("newcomer", "30 ms"),
            make_diff_fn("first", "11 ms"),
        ];
        let refs_a: Vec<&HotpathFunction> = data_a.iter().collect();
        let refs_b: Vec<&HotpathFunction> = data_b.iter().collect();
        let result = format_section_diff("t", &refs_a, &refs_b, 0);
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
            make_diff_fn("a", "1 ms"),
            make_diff_fn("b", "2 ms"),
            make_diff_fn("c", "3 ms"),
            make_diff_fn("d", "4 ms"),
        ];
        let data_b = vec![
            make_diff_fn("a", "1.1 ms"),
            make_diff_fn("b", "2.2 ms"),
            make_diff_fn("c", "3.3 ms"),
            make_diff_fn("d", "4.4 ms"),
            make_diff_fn("e", "5.5 ms"),
        ];
        let refs_a: Vec<&HotpathFunction> = data_a.iter().collect();
        let refs_b: Vec<&HotpathFunction> = data_b.iter().collect();
        let result = format_section_diff("t", &refs_a, &refs_b, 2);
        let lines: Vec<&str> = result.lines().collect();
        // 1 header + 1 col header + 2 data rows = 4
        assert_eq!(
            lines.len(),
            4,
            "top=2 should yield 4 lines total, got {}",
            lines.len()
        );
        assert!(
            !result.contains("\"c\"") && !result.contains("  c  "),
            "c should be truncated"
        );
        assert!(!result.contains("  d  "), "d should be truncated");
        assert!(!result.contains("  e  "), "e should be truncated");
    }

    #[test]
    fn format_section_diff_both_sides_empty() {
        let refs_a: Vec<&HotpathFunction> = Vec::new();
        let refs_b: Vec<&HotpathFunction> = Vec::new();
        let result = format_section_diff("t", &refs_a, &refs_b, 0);
        assert!(
            result.is_empty(),
            "both sides empty should yield empty string"
        );
    }

    #[test]
    fn format_section_diff_one_side_empty() {
        let data_b = vec![make_diff_fn("fn1", "10 ms")];
        let refs_a: Vec<&HotpathFunction> = Vec::new();
        let refs_b: Vec<&HotpathFunction> = data_b.iter().collect();
        let result = format_section_diff("t", &refs_a, &refs_b, 0);
        assert!(result.contains("fn1"), "function from B side should appear");
        assert!(
            result.contains("(new)"),
            "function absent from A should show (new)"
        );
        assert!(result.contains("--"), "A's total should be placeholder --");
    }

    #[test]
    fn format_section_diff_description_fallback() {
        // desc_a is preferred; when missing, desc_b is used.
        let mut fn_a = make_diff_fn("f", "1 ms");
        fn_a.description = None;
        let mut fn_b = make_diff_fn("f", "1 ms");
        fn_b.description = Some("fallback desc".to_string());
        let data_a = vec![fn_a];
        let data_b = vec![fn_b];
        let refs_a: Vec<&HotpathFunction> = data_a.iter().collect();
        let refs_b: Vec<&HotpathFunction> = data_b.iter().collect();
        let result = format_section_diff("timing", &refs_a, &refs_b, 0);
        assert!(
            result.contains("fallback desc"),
            "should fall back to B's description"
        );
    }

    #[test]
    fn format_section_diff_empty_named_entries_skipped() {
        // Entries with empty name should be excluded from the union.
        let data_a = vec![make_diff_fn("", "1 ms"), make_diff_fn("real", "2 ms")];
        let refs_a: Vec<&HotpathFunction> = data_a.iter().collect();
        let result = format_section_diff("t", &refs_a, &refs_a, 0);
        let data_lines: Vec<&str> = result.lines().skip(2).collect();
        assert_eq!(
            data_lines.len(),
            1,
            "empty-name entry should be excluded, leaving 1 row"
        );
        assert!(
            data_lines[0].starts_with("real"),
            "only 'real' should appear"
        );
    }
}
