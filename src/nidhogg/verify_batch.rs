//! Batch query verification for nidhogg.
//!
//! Sends a batch query with 4 named filters to `/api/query_batch`, then
//! compares with 4 individual queries. Verifies that all responses contain
//! non-zero elements. Queries are derived from the dataset bbox.

use std::time::Instant;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run batch query verification.
///
/// `bbox` is in brokkr.toml format (`lon_min,lat_min,lon_max,lat_max`).
pub fn run(port: u16, bbox: &str) -> Result<(), DevError> {
    super::server::check_running(port)?;

    let (batch_body, individual_queries) = super::client::build_batch_queries(bbox)?;

    let mut failures = 0u32;

    // --- Batch query ---
    output::verify_msg("=== batch query ===");
    let batch_result = run_batch_query(port, &batch_body)?;

    let total_batch_elements = report_batch_results(&batch_result)?;

    if total_batch_elements == 0 {
        output::error("batch query returned 0 total elements");
        failures += 1;
    }

    // --- Individual queries ---
    output::verify_msg("=== individual queries ===");
    let individual_results = run_individual_queries(port, &individual_queries)?;

    for (name, count, elapsed_ms) in &individual_results {
        output::verify_msg(&format!("  {name}: {count} elements ({elapsed_ms} ms)"));
        if *count == 0 {
            output::error(&format!("{name}: returned 0 elements"));
            failures += 1;
        }
    }

    // --- Batch vs individual comparison ---
    output::verify_msg("=== batch vs individual comparison ===");
    for (ind_name, ind_count, _) in &individual_results {
        let batch_count = batch_result
            .filter_counts
            .iter()
            .find(|(name, _)| name == ind_name)
            .map(|(_, c)| *c);

        match batch_count {
            Some(bc) if bc == *ind_count => {
                output::verify_msg(&format!(
                    "  PASS  {ind_name}: batch={bc}, individual={ind_count}"
                ));
            }
            Some(bc) => {
                output::error(&format!(
                    "{ind_name}: count mismatch — batch={bc}, individual={ind_count}"
                ));
                failures += 1;
            }
            None => {
                output::error(&format!("{ind_name}: missing from batch response"));
                failures += 1;
            }
        }
    }

    // --- Summary ---
    if failures > 0 {
        return Err(DevError::Verify(format!(
            "batch verification failed: {failures} check(s) failed"
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
fn run_batch_query(port: u16, body: &str) -> Result<BatchResult, DevError> {
    let url = super::client::batch_query_url(port);

    let start = Instant::now();
    let stdout = super::client::curl_post(&url, body)?;
    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| DevError::Verify(format!("invalid JSON from batch API: {e}")))?;

    let mut filter_counts = Vec::new();

    // The batch response should be an object with filter names as keys.
    if let Some(obj) = parsed.as_object() {
        for (name, value) in obj {
            let count = super::client::element_count(value);
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

/// Run individual queries and return (name, element_count, elapsed_ms).
fn run_individual_queries(
    port: u16,
    queries: &[(String, String)],
) -> Result<Vec<(String, usize, i64)>, DevError> {
    let url = super::client::query_url(port);
    let mut results = Vec::with_capacity(queries.len());

    for (name, body) in queries {
        let start = Instant::now();
        let stdout = super::client::curl_post(&url, body)?;
        let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

        let parsed: serde_json::Value = serde_json::from_str(&stdout)
            .map_err(|e| DevError::Verify(format!("invalid JSON from batch API: {e}")))?;

        let count = super::client::element_count(&parsed);

        results.push((name.clone(), count, elapsed_ms));
    }

    Ok(results)
}
