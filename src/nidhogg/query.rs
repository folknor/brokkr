//! Test query: send a spatial query to the nidhogg API and report results.
//!
//! Replaces `query.sh`. Sends a POST to `/api/query`, parses the JSON
//! response, and prints an element count summary by type.

use crate::error::DevError;
use crate::output;

/// Default query: Copenhagen highways.
const DEFAULT_QUERY: &str = r#"{"bbox":[55.66,12.55,55.70,12.60],"query":[{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}]}"#;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Send a spatial query to the nidhogg API and print a summary.
pub fn run(port: u16, query_json: Option<&str>) -> Result<(), DevError> {
    super::server::check_running(port)?;

    let body = query_json.unwrap_or(DEFAULT_QUERY);
    let url = format!("http://localhost:{port}/api/query");

    output::run_msg(&format!("POST {url}"));

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
