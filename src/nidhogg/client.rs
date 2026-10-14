//! Nidhogg HTTP client helpers.
//!
//! Centralized URL construction, curl GET/POST wrappers, URL encoding,
//! health check, and default query/geocode fixtures.

use std::process::{Command, Stdio};

use crate::error::DevError;
use crate::resolve::Bbox;

const HIGHWAY_QUERY: &str =
    r#"{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}"#;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default geocode test queries (Danish cities - used when no queries are specified).
pub const GEOCODE_TEST_QUERIES: &[&str] = &["Kobenhavn", "Aarhus", "Odense"];

/// Render a validated bbox in nidhogg's API coordinate order.
pub fn bbox_to_api(bbox: &Bbox) -> String {
    bbox.to_nidhogg_api()
}

/// Build a default spatial query JSON from a validated bbox.
pub fn default_api_query(bbox: &Bbox) -> String {
    let api_bbox = bbox_to_api(bbox);
    format!(r#"{{"bbox":{api_bbox},"query":[{HIGHWAY_QUERY}]}}"#)
}

/// Build the standard set of API benchmark queries from a dataset bbox.
///
/// Returns (name, JSON body) pairs for the resolved bbox.
pub fn build_api_queries(bbox: &Bbox) -> Result<Vec<(String, String)>, DevError> {
    let full = bbox_to_api(bbox);
    let medium_bbox = bbox.shrink_quarter()?;
    let medium = bbox_to_api(&medium_bbox);
    let small_bbox = medium_bbox.shrink_quarter()?;
    let small = bbox_to_api(&small_bbox);

    Ok(vec![
        (
            "highways".into(),
            format!(r#"{{"bbox":{medium},"query":[{HIGHWAY_QUERY}]}}"#),
        ),
        (
            "highways_large".into(),
            format!(r#"{{"bbox":{full},"query":[{HIGHWAY_QUERY}]}}"#),
        ),
        (
            "small_nofilter".into(),
            format!(r#"{{"bbox":{small},"query":[]}}"#),
        ),
        (
            "buildings".into(),
            format!(r#"{{"bbox":{medium},"query":[{{"building":true}}]}}"#),
        ),
    ])
}

/// Build the standard batch verification queries from a dataset bbox.
pub fn build_batch_queries(bbox: &Bbox) -> (String, Vec<(String, String)>) {
    let api_bbox = bbox_to_api(bbox);

    let batch_body = format!(
        r#"{{"bbox":{api_bbox},"queries":{{"roads":[{HIGHWAY_QUERY}],"infra":[{{"railway":true}}],"coastline":[{{"natural":["coastline"]}}],"landcover":[{{"landuse":true}}]}}}}"#
    );

    let individual = vec![
        (
            "roads".into(),
            format!(r#"{{"bbox":{api_bbox},"query":[{HIGHWAY_QUERY}]}}"#),
        ),
        (
            "infra".into(),
            format!(r#"{{"bbox":{api_bbox},"query":[{{"railway":true}}]}}"#),
        ),
        (
            "coastline".into(),
            format!(r#"{{"bbox":{api_bbox},"query":[{{"natural":["coastline"]}}]}}"#),
        ),
        (
            "landcover".into(),
            format!(r#"{{"bbox":{api_bbox},"query":[{{"landuse":true}}]}}"#),
        ),
    ];

    (batch_body, individual)
}

// ---------------------------------------------------------------------------
// URL construction
// ---------------------------------------------------------------------------

/// Build the `/api/query` URL for the given port.
pub fn query_url(port: u16) -> String {
    super::policy::ServerAddress::localhost(port).url("api/query")
}

/// Build the `/api/query_batch` URL for the given port.
pub fn batch_query_url(port: u16) -> String {
    super::policy::ServerAddress::localhost(port).url("api/query_batch")
}

/// Build the `/api/tiles/{z}/{x}/{y}` URL for the given port and tile coordinates.
pub fn tile_url(port: u16, z: u32, x: u32, y: u32) -> String {
    super::policy::ServerAddress::localhost(port).url(&format!("api/tiles/{z}/{x}/{y}"))
}

/// Build the `/api/geocode?q=<encoded>` URL for the given port and search term.
pub fn geocode_url(port: u16, term: &str) -> String {
    let encoded = url_encode(term);
    super::policy::ServerAddress::localhost(port).url(&format!("api/geocode?q={encoded}"))
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Send an HTTP GET request via curl and return the response body.
///
/// Fails on HTTP 4xx/5xx via `--fail-with-body`. Times out after 30s.
pub fn curl_get(url: &str) -> Result<String, DevError> {
    run_curl_body(&[url])
}

/// Send an HTTP POST request with a JSON body via curl and return the response body.
///
/// Fails on HTTP 4xx/5xx via `--fail-with-body`. Times out after 30s.
pub fn curl_post(url: &str, body: &str) -> Result<String, DevError> {
    run_curl_body(&[
        "-X",
        "POST",
        url,
        "-H",
        "Content-Type: application/json",
        "-d",
        body,
    ])
}

/// Longest slice of an error response body carried into the error message.
const ERROR_BODY_PREVIEW: usize = 300;

/// Shared body of [`curl_get`] / [`curl_post`]. `-sS` keeps the progress
/// meter off but curl's own error line on (plain `-s` silences both, which
/// left every failure an empty-stderr exit code). On failure the error
/// carries that line plus a preview of the response body `--fail-with-body`
/// kept, so an HTTP 500's explanation survives.
fn run_curl_body(request_args: &[&str]) -> Result<String, DevError> {
    let max_time = super::policy::curl_seconds(super::policy::CLIENT_REQUEST_TIMEOUT);
    let mut args = vec![
        "-sS",
        "--compressed",
        "--fail-with-body",
        "--max-time",
        &max_time,
    ];
    args.extend_from_slice(request_args);
    let output = Command::new("curl")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| DevError::Spawn {
            program: "curl".into(),
            error,
        })?;

    if !output.status.success() {
        let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let body = String::from_utf8_lossy(&output.stdout);
        let body = body.trim();
        if !body.is_empty() {
            let preview: String = body.chars().take(ERROR_BODY_PREVIEW).collect();
            if !stderr.is_empty() {
                stderr.push('\n');
            }
            stderr.push_str("response body: ");
            stderr.push_str(&preview);
        }
        return Err(DevError::Subprocess {
            program: "curl".into(),
            ending: crate::error::ProcessEnding::from_status(&output.status),
            stderr,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Check if the server is responding on the given port.
///
/// Sends a GET to `/api/health` with a 2s connect timeout and a 5s ceiling
/// on the whole request - a server that accepts and then hangs must read as
/// "not ready", not block `serve`'s readiness poll forever.
/// Returns `true` if the server responds with HTTP 200, `false` if nothing
/// healthy answers (refused, timed out, non-200). Errors only when curl
/// itself cannot be run: "curl is not installed" is not "server not running".
pub fn health_check(port: u16) -> Result<bool, DevError> {
    let url = super::policy::ServerAddress::localhost(port).url("api/health");
    let connect_timeout = super::policy::curl_seconds(super::policy::HEALTH_CONNECT_TIMEOUT);
    let max_time = super::policy::curl_seconds(super::policy::HEALTH_REQUEST_TIMEOUT);

    let output = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--connect-timeout",
            &connect_timeout,
            "--max-time",
            &max_time,
            &url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| DevError::Spawn {
            program: "curl".into(),
            error,
        })?;

    if !output.status.success() {
        return Ok(false);
    }
    let code = String::from_utf8_lossy(&output.stdout);
    Ok(code.trim() == "200")
}

/// One-line description of a failed curl call for PASS/FAIL reports: the
/// exit code plus curl's error line and any response-body preview
/// ([`curl_get`]/[`curl_post`] put both in the error), so "HTTP 500" and
/// "connection refused" read differently.
pub fn describe_curl_error(err: &DevError) -> String {
    match err {
        DevError::Subprocess { ending, stderr, .. } => {
            let detail = stderr
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            let head = match ending {
                crate::error::ProcessEnding::Code(c) => format!("curl exit {c}"),
                crate::error::ProcessEnding::Signal(s) => format!("curl killed by signal {s}"),
            };
            if detail.is_empty() {
                head
            } else {
                format!("{head}: {detail}")
            }
        }
        // `Spawn` renders as "could not run curl: <why>".
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// JSON response helpers
// ---------------------------------------------------------------------------

/// Extract the element count from a JSON response with an "elements" array.
pub fn element_count(parsed: &serde_json::Value) -> usize {
    parsed
        .get("elements")
        .and_then(|v| v.as_array())
        .map_or(0, Vec::len)
}

/// Extract the top geocode result's display name from a JSON response array.
pub fn geocode_top_name(parsed: &serde_json::Value) -> Option<&str> {
    parsed.as_array()?.first()?.get("displayName")?.as_str()
}

// ---------------------------------------------------------------------------
// URL encoding
// ---------------------------------------------------------------------------

/// Percent-encode a string for use in URLs (RFC 3986 unreserved characters).
pub fn url_encode(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len() * 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;

    #[test]
    fn passthrough_alphanumerics() {
        assert_eq!(url_encode("abcXYZ019"), "abcXYZ019");
    }

    #[test]
    fn passthrough_unreserved_symbols() {
        // RFC 3986 unreserved: - _ . ~
        assert_eq!(url_encode("-_.~"), "-_.~");
    }

    #[test]
    fn empty_string() {
        assert_eq!(url_encode(""), "");
    }

    #[test]
    fn space_encoded() {
        assert_eq!(url_encode("hello world"), "hello%20world");
    }

    #[test]
    fn special_chars_ampersand_equals_slash() {
        assert_eq!(url_encode("a&b=c/d"), "a%26b%3Dc%2Fd");
    }

    #[test]
    fn question_mark_and_hash() {
        assert_eq!(url_encode("?q=1#top"), "%3Fq%3D1%23top");
    }

    #[test]
    fn non_ascii_utf8_multibyte() {
        // 'K' with ring = U+00F8 in UTF-8 is 0xC3 0xB8.
        let encoded = url_encode("København");
        assert!(encoded.starts_with("K%C3%B8benhavn"));
    }

    #[test]
    fn full_utf8_multibyte_cjk() {
        // CJK character U+4E16 ('world') = 0xE4 0xB8 0x96 in UTF-8.
        let encoded = url_encode("\u{4E16}");
        assert_eq!(encoded, "%E4%B8%96");
    }

    #[test]
    fn percent_sign_itself_is_encoded() {
        assert_eq!(url_encode("100%"), "100%25");
    }

    #[test]
    fn all_bytes_in_query_string() {
        let input = "key=hello world&other=foo/bar";
        let encoded = url_encode(input);
        // No raw &, =, /, or space should survive.
        assert!(!encoded.contains('&'));
        assert!(!encoded.contains('='));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains(' '));
        // But alphanumerics should pass through.
        assert!(encoded.contains("key"));
        assert!(encoded.contains("hello"));
    }

    #[test]
    fn query_url_uses_the_api_endpoint() {
        // Verify URL helpers produce expected format.
        let url = query_url(3033);
        assert_eq!(url, "http://localhost:3033/api/query");
    }

    #[test]
    fn batch_url_format() {
        let url = batch_query_url(3033);
        assert_eq!(url, "http://localhost:3033/api/query_batch");
    }

    #[test]
    fn geocode_url_encodes_term() {
        let url = geocode_url(3033, "hello world");
        assert_eq!(url, "http://localhost:3033/api/geocode?q=hello%20world");
    }

    #[test]
    fn curl_error_description_keeps_status_and_body() {
        let e = DevError::Subprocess {
            program: "curl".into(),
            ending: crate::error::ProcessEnding::Code(22),
            stderr: "curl: (22) The requested URL returned error: 500\nresponse body: boom".into(),
        };
        assert_eq!(
            describe_curl_error(&e),
            "curl exit 22: curl: (22) The requested URL returned error: 500; response body: boom"
        );
    }

    #[test]
    fn curl_spawn_failure_is_not_described_as_a_request_failure() {
        let e = DevError::Spawn {
            program: "curl".into(),
            error: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        assert!(describe_curl_error(&e).starts_with("could not run curl"));
    }

    #[test]
    fn bbox_to_api_swaps_lon_lat() {
        let bbox = Bbox::parse("12.4,55.6,12.7,55.8").unwrap();
        let result = bbox_to_api(&bbox);
        assert_eq!(result, "[55.6,12.4,55.8,12.7]");
    }

    #[test]
    fn bbox_parser_rejects_bad_count_and_non_finite_values() {
        assert!(Bbox::parse("1,2,3").is_err());
        assert!(Bbox::parse("1,2,3,4,5").is_err());
        assert!(Bbox::parse("1,2,NaN,4").is_err());
    }

    #[test]
    fn shrink_bbox_produces_center_quarter() {
        let bbox = Bbox::parse("10.0,50.0,14.0,54.0").unwrap();
        let result = bbox.shrink_quarter().unwrap();
        let parts = result.coordinates();
        // Center is (12, 52), quarter-width is 1.0 lon, 1.0 lat
        assert!((parts[0] - 11.0).abs() < 1e-10);
        assert!((parts[1] - 51.0).abs() < 1e-10);
        assert!((parts[2] - 13.0).abs() < 1e-10);
        assert!((parts[3] - 53.0).abs() < 1e-10);
    }

    #[test]
    fn build_api_queries_produces_four() {
        let bbox = Bbox::parse("12.4,55.6,12.7,55.8").unwrap();
        let queries = build_api_queries(&bbox).unwrap();
        assert_eq!(queries.len(), 4);
        // All should be valid JSON
        for (_, body) in &queries {
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert!(parsed.get("bbox").is_some());
        }
    }

    #[test]
    fn build_batch_queries_produces_matching_names() {
        let bbox = Bbox::parse("12.4,55.6,12.7,55.8").unwrap();
        let (batch, individual) = build_batch_queries(&bbox);
        // batch body should be valid JSON
        let _: serde_json::Value = serde_json::from_str(&batch).unwrap();
        assert_eq!(individual.len(), 4);
        let names: Vec<&str> = individual.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"roads"));
        assert!(names.contains(&"infra"));
        assert!(names.contains(&"coastline"));
        assert!(names.contains(&"landcover"));
    }
}
