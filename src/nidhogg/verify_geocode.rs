//! Geocode verification for nidhogg.
//!
//! Replaces `test-geocode.sh`. Sends geocode queries for a set of city names
//! and verifies that each returns non-empty results.

use crate::error::DevError;
use crate::output;

/// Default set of geocode test queries (Danish cities).
const DEFAULT_QUERIES: &[&str] = &["Kobenhavn", "Aarhus", "Odense"];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run geocode verification for the given queries (or defaults).
///
/// For each query: GET `/api/geocode?q=<term>`, check non-empty, report
/// PASS/FAIL with result count and top result. Returns error if any failed.
pub fn run(port: u16, queries: &[&str]) -> Result<(), DevError> {
    super::server::check_running(port)?;

    let queries = if queries.is_empty() {
        DEFAULT_QUERIES
    } else {
        queries
    };

    let mut passed = 0u32;
    let mut failed = 0u32;

    for query in queries {
        let result = run_single_geocode(port, query)?;
        match result {
            GeoResult::Success { count, top_name } => {
                output::verify_msg(&format!(
                    "PASS  '{query}': {count} results (top: {top_name})"
                ));
                passed += 1;
            }
            GeoResult::Empty => {
                output::verify_msg(&format!("FAIL  '{query}': 0 results"));
                failed += 1;
            }
            GeoResult::Error(msg) => {
                output::verify_msg(&format!("FAIL  '{query}': {msg}"));
                failed += 1;
            }
        }
    }

    output::verify_msg(&format!(
        "geocode: {passed} passed, {failed} failed"
    ));

    if failed > 0 {
        return Err(DevError::Config(format!(
            "geocode verification failed: {failed} query(ies) returned no results"
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

enum GeoResult {
    Success { count: usize, top_name: String },
    Empty,
    Error(String),
}

/// Run a single geocode query and return the result.
fn run_single_geocode(port: u16, query: &str) -> Result<GeoResult, DevError> {
    let encoded = url_encode(query);
    let url = format!("http://localhost:{port}/api/geocode?q={encoded}");

    let result = std::process::Command::new("curl")
        .args(["-s", "--compressed", &url])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| DevError::Subprocess {
            program: "curl".into(),
            code: None,
            stderr: e.to_string(),
        })?;

    if !result.status.success() {
        return Ok(GeoResult::Error("curl request failed".into()));
    }

    let stdout = String::from_utf8_lossy(&result.stdout);
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);

    match parsed {
        Ok(val) => {
            let arr = val.as_array();
            match arr {
                Some(results) if !results.is_empty() => {
                    let top_name = results
                        .first()
                        .and_then(|v| v.get("displayName"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("(unknown)")
                        .to_owned();
                    Ok(GeoResult::Success {
                        count: results.len(),
                        top_name,
                    })
                }
                Some(_) => Ok(GeoResult::Empty),
                None => Ok(GeoResult::Error(
                    "response is not an array".into(),
                )),
            }
        }
        Err(e) => Ok(GeoResult::Error(format!("JSON parse error: {e}"))),
    }
}

/// Simple percent-encoding for URL query parameters.
fn url_encode(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len() * 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => encoded.push(byte as char),
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}
