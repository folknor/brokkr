//! Batch query verification for nidhogg.
//!
//! Replaces `test-batch.sh`. Sends a batch query with 4 named filters to
//! `/api/query_batch`, then compares with 4 individual queries. Verifies
//! that all responses contain non-zero elements.

use std::time::Instant;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Batch query payload
// ---------------------------------------------------------------------------

/// Copenhagen bbox shared by all queries.
const BBOX: &str = r#"[55.66,12.55,55.70,12.60]"#;

/// Batch query body with 4 named filters.
fn batch_body() -> String {
    format!(
        r#"{{"bbox":{BBOX},"queries":{{"roads":[{{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}}],"infra":[{{"railway":true}}],"coastline":[{{"natural":["coastline"]}}],"landcover":[{{"landuse":true}}]}}}}"#,
    )
}

/// Individual queries corresponding to the batch filters.
const INDIVIDUAL_QUERIES: &[(&str, &str)] = &[
    (
        "roads",
        r#"{"bbox":[55.66,12.55,55.70,12.60],"query":[{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}]}"#,
    ),
    (
        "infra",
        r#"{"bbox":[55.66,12.55,55.70,12.60],"query":[{"railway":true}]}"#,
    ),
    (
        "coastline",
        r#"{"bbox":[55.66,12.55,55.70,12.60],"query":[{"natural":["coastline"]}]}"#,
    ),
    (
        "landcover",
        r#"{"bbox":[55.66,12.55,55.70,12.60],"query":[{"landuse":true}]}"#,
    ),
];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run batch query verification.
pub fn run(port: u16) -> Result<(), DevError> {
    super::server::check_running(port)?;

    let mut failures = 0u32;

    // --- Batch query ---
    output::verify_msg("=== batch query ===");
    let batch_result = run_batch_query(port)?;

    let total_batch_elements = report_batch_results(&batch_result)?;

    if total_batch_elements == 0 {
        output::error("batch query returned 0 total elements");
        failures += 1;
    }

    // --- Individual queries ---
    output::verify_msg("=== individual queries ===");
    let individual_results = run_individual_queries(port)?;

    for (name, count, elapsed_ms) in &individual_results {
        output::verify_msg(&format!(
            "  {name}: {count} elements ({elapsed_ms} ms)"
        ));
        if *count == 0 {
            output::error(&format!("{name}: returned 0 elements"));
            failures += 1;
        }
    }

    // --- Summary ---
    if failures > 0 {
        return Err(DevError::Config(format!(
            "batch verification failed: {failures} check(s) returned 0 elements"
        )));
    }

    output::verify_msg("batch verification passed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Result of a batch query: per-filter element counts + timing.
struct BatchResult {
    filter_counts: Vec<(String, usize)>,
    elapsed_ms: i64,
}

/// Send the batch query and parse the response.
fn run_batch_query(port: u16) -> Result<BatchResult, DevError> {
    let url = format!("http://localhost:{port}/api/query_batch");
    let body = batch_body();

    let start = Instant::now();

    let result = std::process::Command::new("curl")
        .args([
            "-s",
            "--compressed",
            "-X", "POST",
            &url,
            "-H", "Content-Type: application/json",
            "-d", &body,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| DevError::Subprocess {
            program: "curl".into(),
            code: None,
            stderr: e.to_string(),
        })?;

    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: result.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&result.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)?;

    let mut filter_counts = Vec::new();

    // The batch response should be an object with filter names as keys.
    if let Some(obj) = parsed.as_object() {
        for (name, value) in obj {
            let count = value
                .get("elements")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len);
            filter_counts.push((name.clone(), count));
        }
    }

    filter_counts.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(BatchResult {
        filter_counts,
        elapsed_ms,
    })
}

/// Print batch results and return total element count.
fn report_batch_results(result: &BatchResult) -> Result<usize, DevError> {
    let mut total = 0usize;

    output::verify_msg(&format!("  batch query: {} ms", result.elapsed_ms));

    for (name, count) in &result.filter_counts {
        output::verify_msg(&format!("  {name}: {count} elements"));
        total += count;
    }

    output::verify_msg(&format!("  total: {total} elements"));
    Ok(total)
}

/// Run 4 individual queries and return (name, element_count, elapsed_ms).
fn run_individual_queries(port: u16) -> Result<Vec<(String, usize, i64)>, DevError> {
    let url = format!("http://localhost:{port}/api/query");
    let mut results = Vec::with_capacity(INDIVIDUAL_QUERIES.len());

    for &(name, body) in INDIVIDUAL_QUERIES {
        let start = Instant::now();

        let result = std::process::Command::new("curl")
            .args([
                "-s",
                "--compressed",
                "-X", "POST",
                &url,
                "-H", "Content-Type: application/json",
                "-d", body,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| DevError::Subprocess {
                program: "curl".into(),
                code: None,
                stderr: e.to_string(),
            })?;

        let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(DevError::Subprocess {
                program: "curl".into(),
                code: result.status.code(),
                stderr: stderr.into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&result.stdout);
        let parsed: serde_json::Value = serde_json::from_str(&stdout)?;

        let count = parsed
            .get("elements")
            .and_then(|v| v.as_array())
            .map_or(0, Vec::len);

        results.push((name.to_owned(), count, elapsed_ms));
    }

    Ok(results)
}
