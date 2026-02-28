//! API query benchmark for nidhogg.
//!
//! Replaces `bench-api.sh`. Runs 4 hardcoded spatial queries against the
//! running nidhogg server, collecting timing distributions via curl.

use std::time::Instant;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Query definitions
// ---------------------------------------------------------------------------

const QUERIES: &[(&str, &str)] = &[
    (
        "cph_highways",
        r#"{"bbox":[55.66,12.55,55.70,12.60],"query":[{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}]}"#,
    ),
    (
        "cph_large",
        r#"{"bbox":[55.60,12.40,55.75,12.70],"query":[{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}]}"#,
    ),
    (
        "cph_small_nofilter",
        r#"{"bbox":[55.67,12.57,55.68,12.58],"query":[]}"#,
    ),
    (
        "cph_buildings",
        r#"{"bbox":[55.66,12.55,55.70,12.60],"query":[{"building":true}]}"#,
    ),
];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run API query benchmarks against the nidhogg server.
///
/// For each query (optionally filtered by `only`): warmup, run N timed
/// requests via curl, then report timing distribution plus element counts.
pub fn run(
    harness: &BenchHarness,
    port: u16,
    runs: usize,
    only: Option<&str>,
) -> Result<(), DevError> {
    super::server::check_running(port)?;

    let url = format!("http://localhost:{port}/api/query");

    for &(name, body) in QUERIES {
        if let Some(filter) = only
            && name != filter {
                continue;
            }

        output::bench_msg(&format!("=== {name} ==="));

        // Warmup: one request, discard result.
        run_curl_timed(&url, body)?;

        // Timed runs via distribution harness.
        let config = BenchConfig {
            command: "bench api".into(),
            variant: Some(name.into()),
            input_file: None,
            input_mb: None,
            cargo_features: None,
            cargo_profile: "release".into(),
            runs,
        };

        let url_clone = url.clone();
        let body_owned = body.to_owned();

        harness.run_distribution(&config, |_i| {
            let ms = run_curl_timed(&url_clone, &body_owned)?;
            Ok(ms)
        })?;

        // Extra request to get element count and response size.
        report_response_stats(&url, body, name)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a single curl request and return the elapsed time in milliseconds.
fn run_curl_timed(url: &str, body: &str) -> Result<i64, DevError> {
    let start = Instant::now();

    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "--compressed",
            "-o", "/dev/null",
            "-X", "POST",
            url,
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

    let elapsed = start.elapsed();
    let ms = i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: output.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    Ok(ms)
}

/// Make one extra request to report element count and response bytes.
fn report_response_stats(url: &str, body: &str, name: &str) -> Result<(), DevError> {
    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "--compressed",
            "-w", "\n%{size_download}",
            "-X", "POST",
            url,
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

    if !output.status.success() {
        return Ok(()); // Non-fatal: just skip stats reporting.
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // The response body is followed by a newline and the download size.
    // Split on the last newline to separate JSON body from write-out.
    let (json_body, size_str) = split_curl_output(&stdout);

    let download_bytes: u64 = size_str
        .trim()
        .parse()
        .unwrap_or(0);

    let element_count = count_elements(json_body);

    output::bench_msg(&format!(
        "{name}: {element_count} elements, {download_bytes} bytes response"
    ));

    Ok(())
}

/// Split curl output into (response body, size_download write-out).
///
/// The `-w '\n%{size_download}'` flag appends the size after the body.
fn split_curl_output(stdout: &str) -> (&str, &str) {
    match stdout.rfind('\n') {
        Some(pos) => (&stdout[..pos], &stdout[pos + 1..]),
        None => (stdout, "0"),
    }
}

/// Count elements in a JSON response by looking for the "elements" array.
fn count_elements(json_body: &str) -> usize {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(json_body);
    match parsed {
        Ok(val) => {
            val.get("elements")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len)
        }
        Err(_) => 0,
    }
}
