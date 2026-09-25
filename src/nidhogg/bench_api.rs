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
        let (_, body) = filtered.iter().find(|(n, _)| n == name).expect("variant exists in filtered list");

        // Warmup: one request, discard result.
        run_curl_timed(&url, body)?;

        let config = BenchConfig {
            // Each query variant shares the same brokkr/cli invocation
            // (curl-style HTTP). Different query rows are distinguished
            // by the command column: `api-<query>`.
            command: format!("api-{name}"),
            mode: None,
            input_file: input_file.map(str::to_owned),
            input_mb,
            cargo_features: None,
            cargo_profile: crate::build::CargoProfile::Release,
            runs,
            cli_args: None,
            brokkr_args: None,
            metadata: vec![KvPair::int("meta.port", port as i64)],
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

/// Ceiling on one timed request. A request that exceeds it fails the bench
/// rather than stalling it under the global lock.
const REQUEST_MAX_TIME_SECS: &str = "60";

/// Run a single curl request and return the HTTP round-trip time in
/// milliseconds, rounded to nearest.
///
/// Uses curl's `--write-out '%{time_total}'` to measure actual HTTP timing,
/// excluding process spawn overhead. `--fail` makes an HTTP 4xx/5xx a curl
/// failure: without it a fast 500 was timed and recorded as a successful
/// sample, which is the most flattering number a broken server can produce.
/// (`--fail` rather than `--fail-with-body`: the body goes to `/dev/null`
/// here either way, and curl's `-S` error line names the status.)
fn run_curl_timed(url: &str, body: &str) -> Result<i64, DevError> {
    let output = std::process::Command::new("curl")
        .args([
            "-sS",
            "--compressed",
            "--fail",
            "--max-time",
            REQUEST_MAX_TIME_SECS,
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
        .map_err(|error| DevError::Spawn {
            program: "curl".into(),
            error,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: output.status.code(),
            stderr: stderr.trim().to_owned(),
        });
    }

    // curl writes the time_total after a newline in stdout (via -w).
    let stdout = String::from_utf8_lossy(&output.stdout);
    let time_str = stdout.trim();

    // time_total is in seconds with fractional part (e.g., "0.042367").
    let seconds: f64 = time_str.parse().map_err(|_| {
        DevError::Verify(format!("curl time_total not a valid number: '{time_str}'"))
    })?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(DevError::Verify(format!(
            "curl time_total out of range: '{time_str}'"
        )));
    }
    // Nearest, not truncating - the harness-wide policy (`us_to_ms`): a
    // floored 0.9 ms request would record as 0 ms, reading faster than it
    // was. The distribution columns are integer milliseconds, so a query
    // well under half a millisecond still records 0; sub-ms resolution
    // here would need a microsecond distribution, which the schema lacks.
    #[allow(clippy::cast_possible_truncation)]
    let ms = (seconds * 1000.0).round() as i64;

    Ok(ms)
}

/// Make one extra request to report element count and response bytes.
///
/// Every failure is an error, never a zero: an HTTP error, an unparseable
/// size, a non-JSON body, or a body with no `elements` array used to print
/// "0 elements, 0 bytes response" - indistinguishable from a real empty
/// result, right after a timing run that may have been measuring errors.
fn report_response_stats(url: &str, body: &str, name: &str) -> Result<(), DevError> {
    let output = std::process::Command::new("curl")
        .args([
            "-sS",
            "--compressed",
            "--fail-with-body",
            "--max-time",
            REQUEST_MAX_TIME_SECS,
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
        .map_err(|error| DevError::Spawn {
            program: "curl".into(),
            error,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: output.status.code(),
            stderr: format!("{name} stats request: {}", stderr.trim()),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (download_bytes, count) = parse_response_stats(&stdout)
        .map_err(|why| DevError::Verify(format!("{name} stats request: {why}")))?;

    output::bench_msg(&format!(
        "{name}: {count} elements, {download_bytes} bytes response"
    ));

    Ok(())
}

/// Parse `<json body>\n<size_download>` into (bytes, element count).
fn parse_response_stats(stdout: &str) -> Result<(u64, usize), String> {
    // The `-w '\n%{size_download}'` flag appends the size after the body;
    // split on the last newline. No newline means the write-out is missing.
    let (json_body, size_str) = stdout
        .rfind('\n')
        .map(|pos| (&stdout[..pos], &stdout[pos + 1..]))
        .ok_or_else(|| "curl output carries no size_download write-out".to_owned())?;
    let size_str = size_str.trim();
    let download_bytes: u64 = size_str
        .parse()
        .map_err(|_| format!("size_download not a number: '{size_str}'"))?;
    let val: serde_json::Value = serde_json::from_str(json_body)
        .map_err(|e| format!("response is not JSON: {e}"))?;
    let count = val
        .get("elements")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| "response has no \"elements\" array".to_owned())?;
    Ok((download_bytes, count))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::parse_response_stats;

    #[test]
    fn stats_parse_body_and_size() {
        assert_eq!(
            parse_response_stats("{\"elements\":[1,2,3]}\n42").unwrap(),
            (42, 3)
        );
    }

    #[test]
    fn stats_failures_are_errors_not_zeros() {
        assert!(parse_response_stats("{\"elements\":[]}").is_err());
        assert!(parse_response_stats("{\"elements\":[]}\nabc").is_err());
        assert!(parse_response_stats("<html>500</html>\n12").is_err());
        assert!(parse_response_stats("{\"error\":\"x\"}\n12").is_err());
    }
}
