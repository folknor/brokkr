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

    // URL-encode the query term.
    let encoded = url_encode(query);
    let url = format!("http://localhost:{port}/api/geocode?q={encoded}");

    output::run_msg(&format!("GET {url}"));

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
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: result.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&result.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)?;

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simple percent-encoding for URL query parameters.
///
/// Encodes spaces, non-ASCII, and reserved characters. This is intentionally
/// minimal -- just enough for place names.
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
