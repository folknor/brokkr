//! Test geocode: send a geocode query and report results.
//!
//! Replaces `geocode.sh`. Sends a GET to `/api/geocode?q=<term>`, parses
//! the JSON response array, and prints the top result.

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Send a geocode query and print a summary of results.
pub fn run(port: u16, query: &str) -> Result<(), DevError> {
    super::server::check_running(port)?;

    let url = super::client::geocode_url(port, query);

    output::run_msg(&format!("GET {url}"));

    let stdout = super::client::curl_get(&url)?;
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| DevError::Verify(format!("invalid JSON from geocode API: {e}")))?;

    let arr = parsed.as_array();

    match arr {
        Some(results) if !results.is_empty() => {
            output::result_msg(&format!("{} results for '{query}'", results.len()));

            // Print top result details.
            if let Some(top) = results.first() {
                let display_name = top
                    .get("displayName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unknown)");
                let lat = top.get("lat").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                let lon = top.get("lon").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                output::result_msg(&format!("  top: {display_name} ({lat:.4}, {lon:.4})"));
            }
        }
        Some(_) => {
            output::result_msg(&format!("0 results for '{query}'"));
        }
        None => {
            output::result_msg("unexpected response format (not an array)");
        }
    }

    Ok(())
}
