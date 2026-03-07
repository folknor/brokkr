//! Test query: send a spatial query to the nidhogg API and report results.
//!
//! Replaces `query.sh`. Sends a POST to `/api/query`, parses the JSON
//! response, and prints an element count summary by type.

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Send a spatial query to the nidhogg API and print a summary.
pub fn run(port: u16, query_json: Option<&str>) -> Result<(), DevError> {
    super::server::check_running(port)?;

    let body = query_json.unwrap_or(super::client::DEFAULT_API_QUERY);
    let url = super::client::query_url(port);

    output::run_msg(&format!("POST {url}"));

    let stdout = super::client::curl_post(&url, body)?;
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| DevError::Verify(format!("invalid JSON from query API: {e}")))?;

    let elements = parsed
        .get("elements")
        .and_then(|v| v.as_array());

    match elements {
        Some(arr) => {
            let (ways, relations, other) = count_by_type(arr);
            let total = arr.len();
            output::result_msg(&format!("{total} elements: way={ways}, relation={relations}, other={other}"));
        }
        None => {
            output::result_msg("no elements in response");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Count elements by type field in the JSON array.
fn count_by_type(elements: &[serde_json::Value]) -> (usize, usize, usize) {
    let mut ways = 0usize;
    let mut relations = 0usize;
    let mut other = 0usize;

    for elem in elements {
        match elem.get("type").and_then(|v| v.as_str()) {
            Some("way") => ways += 1,
            Some("relation") => relations += 1,
            _ => other += 1,
        }
    }

    (ways, relations, other)
}
