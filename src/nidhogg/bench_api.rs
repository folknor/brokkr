//! API query benchmark for nidhogg.
//!
//! Runs spatial queries against the running nidhogg server, collecting timing
//! distributions via curl. Queries are derived from the dataset bbox configured
//! in brokkr.toml.

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run API query benchmarks against the nidhogg server.
///
/// Queries are derived from `bbox` (brokkr.toml format: `lon_min,lat_min,lon_max,lat_max`).
/// For each query (optionally filtered by `only`): warmup, run N timed
/// requests via curl, then report timing distribution plus element counts.
pub fn run(
    harness: &BenchHarness,
    port: u16,
    runs: usize,
    only: Option<&str>,
    input_file: Option<&str>,
    input_mb: Option<f64>,
    bbox: &str,
) -> Result<(), DevError> {
    super::server::check_running(port)?;

    let queries = super::client::build_api_queries(bbox)?;
    let url = super::client::query_url(port);

    let filtered: Vec<&(String, String)> = queries
        .iter()
        .filter(|(name, _)| only.is_none_or(|f| name == f))
        .collect();
    let variant_names: Vec<&str> = filtered.iter().map(|(name, _)| name.as_str()).collect();

    crate::harness::run_variants("query", &variant_names, |name| {
        let (_, body) = filtered.iter().find(|(n, _)| n == name).unwrap();

        // Warmup: one request, discard result.
        run_curl_timed(&url, body)?;

        let config = BenchConfig {
            command: "bench api".into(),
            variant: Some(name.into()),
            input_file: input_file.map(str::to_owned),
            input_mb,
            cargo_features: None,
            cargo_profile: "release".into(),
            runs,
            cli_args: None,
            metadata: vec![
                KvPair::int("meta.port", port as i64),
                KvPair::text("meta.query", name),
            ],
        };

        let url_clone = url.clone();
        let body_owned = body.clone();

        harness.run_distribution(&config, |_i| {
            let ms = run_curl_timed(&url_clone, &body_owned)?;
            Ok(ms)
        })?;

        report_response_stats(&url, body, name)?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a single curl request and return the HTTP round-trip time in milliseconds.
///
/// Uses curl's `--write-out '%{time_total}'` to measure actual HTTP timing,
/// excluding process spawn overhead.
fn run_curl_timed(url: &str, body: &str) -> Result<i64, DevError> {
    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "--compressed",
            "-o",
            "/dev/null",
            "-w",
            "\n%{time_total}",
            "-X",
            "POST",
            url,
            "-H",
            "Content-Type: application/json",
            "-d",
            body,
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: output.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    // curl writes the time_total after a newline in stdout (via -w).
    let stdout = String::from_utf8_lossy(&output.stdout);
    let time_str = stdout.trim();

    // time_total is in seconds with fractional part (e.g., "0.042367").
    let seconds: f64 = time_str.parse().map_err(|_| {
        DevError::Verify(format!("curl time_total not a valid number: '{time_str}'"))
    })?;
    #[allow(clippy::cast_possible_truncation)]
    let ms = (seconds * 1000.0) as i64;

    Ok(ms)
}

/// Make one extra request to report element count and response bytes.
fn report_response_stats(url: &str, body: &str, name: &str) -> Result<(), DevError> {
    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "--compressed",
            "-w",
            "\n%{size_download}",
            "-X",
            "POST",
            url,
            "-H",
            "Content-Type: application/json",
            "-d",
            body,
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

    let download_bytes: u64 = size_str.trim().parse().unwrap_or(0);

    let count = match serde_json::from_str::<serde_json::Value>(json_body) {
        Ok(val) => super::client::element_count(&val),
        Err(_) => 0,
    };

    output::bench_msg(&format!(
        "{name}: {count} elements, {download_bytes} bytes response"
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
