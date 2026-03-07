//! Nidhogg HTTP client helpers.
//!
//! Centralized URL construction, curl GET/POST wrappers, URL encoding,
//! health check, and default query/geocode fixtures.

use std::process::{Command, Stdio};

use crate::error::DevError;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default geocode test queries (Danish cities — used when no queries are specified).
pub const GEOCODE_TEST_QUERIES: &[&str] = &["Kobenhavn", "Aarhus", "Odense"];

/// Convert a brokkr.toml bbox (`lon_min,lat_min,lon_max,lat_max`) to a nidhogg
/// API bbox array string (`[lat_min,lon_min,lat_max,lon_max]`).
pub fn bbox_to_api(bbox: &str) -> Result<String, crate::error::DevError> {
    let parts: Vec<&str> = bbox.split(',').collect();
    if parts.len() != 4 {
        return Err(crate::error::DevError::Config(format!(
            "bbox must have 4 comma-separated values, got {}: {bbox}",
            parts.len()
        )));
    }
    // brokkr.toml: lon_min,lat_min,lon_max,lat_max
    // nidhogg API: [lat_min,lon_min,lat_max,lon_max]
    Ok(format!("[{},{},{},{}]", parts[1], parts[0], parts[3], parts[2]))
}

/// Build a default spatial query JSON from a bbox string (brokkr.toml format).
pub fn default_api_query(bbox: &str) -> Result<String, crate::error::DevError> {
    let api_bbox = bbox_to_api(bbox)?;
    Ok(format!(
        r#"{{"bbox":{api_bbox},"query":[{{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}}]}}"#
    ))
}

/// Shrink a bbox to roughly its center quarter (for small-area queries).
fn shrink_bbox(bbox: &str) -> Result<String, crate::error::DevError> {
    let parts: Vec<f64> = bbox
        .split(',')
        .map(|s| s.trim().parse::<f64>().map_err(|_| {
            crate::error::DevError::Config(format!("invalid bbox component: {s}"))
        }))
        .collect::<Result<Vec<_>, _>>()?;
    if parts.len() != 4 {
        return Err(crate::error::DevError::Config("bbox must have 4 values".into()));
    }
    let (lon_min, lat_min, lon_max, lat_max) = (parts[0], parts[1], parts[2], parts[3]);
    let lon_mid = (lon_min + lon_max) / 2.0;
    let lat_mid = (lat_min + lat_max) / 2.0;
    let lon_q = (lon_max - lon_min) / 4.0;
    let lat_q = (lat_max - lat_min) / 4.0;
    Ok(format!(
        "{},{},{},{}",
        lon_mid - lon_q, lat_mid - lat_q,
        lon_mid + lon_q, lat_mid + lat_q,
    ))
}

/// Build the standard set of API benchmark queries from a dataset bbox.
///
/// Returns (name, JSON body) pairs. The bbox is from brokkr.toml format
/// (`lon_min,lat_min,lon_max,lat_max`).
pub fn build_api_queries(bbox: &str) -> Result<Vec<(String, String)>, crate::error::DevError> {
    let full = bbox_to_api(bbox)?;
    let small_bbox = shrink_bbox(bbox)?;
    let small = bbox_to_api(&small_bbox)?;

    Ok(vec![
        (
            "highways".into(),
            format!(r#"{{"bbox":{full},"query":[{{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}}]}}"#),
        ),
        (
            "highways_large".into(),
            format!(r#"{{"bbox":{full},"query":[{{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}}]}}"#),
        ),
        (
            "small_nofilter".into(),
            format!(r#"{{"bbox":{small},"query":[]}}"#),
        ),
        (
            "buildings".into(),
            format!(r#"{{"bbox":{full},"query":[{{"building":true}}]}}"#),
        ),
    ])
}

/// Build the standard batch verification queries from a dataset bbox.
pub fn build_batch_queries(bbox: &str) -> Result<(String, Vec<(String, String)>), crate::error::DevError> {
    let api_bbox = bbox_to_api(bbox)?;

    let batch_body = format!(
        r#"{{"bbox":{api_bbox},"queries":{{"roads":[{{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}}],"infra":[{{"railway":true}}],"coastline":[{{"natural":["coastline"]}}],"landcover":[{{"landuse":true}}]}}}}"#
    );

    let individual = vec![
        ("roads".into(), format!(r#"{{"bbox":{api_bbox},"query":[{{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}}]}}"#)),
        ("infra".into(), format!(r#"{{"bbox":{api_bbox},"query":[{{"railway":true}}]}}"#)),
        ("coastline".into(), format!(r#"{{"bbox":{api_bbox},"query":[{{"natural":["coastline"]}}]}}"#)),
        ("landcover".into(), format!(r#"{{"bbox":{api_bbox},"query":[{{"landuse":true}}]}}"#)),
    ];

    Ok((batch_body, individual))
}

// ---------------------------------------------------------------------------
// URL construction
// ---------------------------------------------------------------------------

/// Build the `/api/query` URL for the given port.
pub fn query_url(port: u16) -> String {
    format!("http://localhost:{port}/api/query")
}

/// Build the `/api/query_batch` URL for the given port.
pub fn batch_query_url(port: u16) -> String {
    format!("http://localhost:{port}/api/query_batch")
}

/// Build the `/api/tiles/{z}/{x}/{y}` URL for the given port and tile coordinates.
pub fn tile_url(port: u16, z: u32, x: u32, y: u32) -> String {
    format!("http://localhost:{port}/api/tiles/{z}/{x}/{y}")
}

/// Build the `/api/geocode?q=<encoded>` URL for the given port and search term.
pub fn geocode_url(port: u16, term: &str) -> String {
    let encoded = url_encode(term);
    format!("http://localhost:{port}/api/geocode?q={encoded}")
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Send an HTTP GET request via curl and return the response body.
///
/// Fails on HTTP 4xx/5xx via `--fail-with-body`. Times out after 30s.
pub fn curl_get(url: &str) -> Result<String, DevError> {
    let output = Command::new("curl")
        .args(["-s", "--compressed", "--fail-with-body", "--max-time", "30", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Send an HTTP POST request with a JSON body via curl and return the response body.
///
/// Fails on HTTP 4xx/5xx via `--fail-with-body`. Times out after 30s.
pub fn curl_post(url: &str, body: &str) -> Result<String, DevError> {
    let output = Command::new("curl")
        .args([
            "-s",
            "--compressed",
            "--fail-with-body",
            "--max-time", "30",
            "-X", "POST",
            url,
            "-H", "Content-Type: application/json",
            "-d", body,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Check if the server is responding on the given port.
///
/// Sends a minimal POST to `/api/query` with a 2s connect timeout.
/// Returns `true` if the server responds with HTTP 200.
pub fn health_check(port: u16) -> Result<bool, DevError> {
    let url = query_url(port);
    let body = r#"{"bbox":[0,0,0,0],"query":[]}"#;

    let result = Command::new("curl")
        .args([
            "-s",
            "-o", "/dev/null",
            "-w", "%{http_code}",
            "-X", "POST",
            &url,
            "-H", "Content-Type: application/json",
            "-d", body,
            "--connect-timeout", "2",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match result {
        Ok(output) if output.status.success() => {
            let code = String::from_utf8_lossy(&output.stdout);
            Ok(code.trim() == "200")
        }
        _ => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Convert a path to a `&str`, returning a clear error if it's not valid UTF-8.
pub fn path_str(path: &std::path::Path) -> Result<&str, crate::error::DevError> {
    path.to_str().ok_or_else(|| {
        crate::error::DevError::Config(format!(
            "path is not valid UTF-8: {}",
            path.display()
        ))
    })
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
    parsed
        .as_array()?
        .first()?
        .get("displayName")?
        .as_str()
}

// ---------------------------------------------------------------------------
// URL encoding
// ---------------------------------------------------------------------------

/// Percent-encode a string for use in URLs (RFC 3986 unreserved characters).
pub fn url_encode(input: &str) -> String {
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

#[cfg(test)]
mod tests {
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
    fn explicit_level_with_defaults_off_still_works() {
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
    fn bbox_to_api_swaps_lon_lat() {
        // brokkr.toml: lon_min,lat_min,lon_max,lat_max
        // nidhogg API: [lat_min,lon_min,lat_max,lon_max]
        let result = bbox_to_api("12.4,55.6,12.7,55.8").unwrap();
        assert_eq!(result, "[55.6,12.4,55.8,12.7]");
    }

    #[test]
    fn bbox_to_api_rejects_bad_count() {
        assert!(bbox_to_api("1,2,3").is_err());
        assert!(bbox_to_api("1,2,3,4,5").is_err());
    }

    #[test]
    fn shrink_bbox_produces_center_quarter() {
        let result = shrink_bbox("10.0,50.0,14.0,54.0").unwrap();
        let parts: Vec<f64> = result.split(',').map(|s| s.parse().unwrap()).collect();
        assert_eq!(parts.len(), 4);
        // Center is (12, 52), quarter-width is 1.0 lon, 1.0 lat
        assert!((parts[0] - 11.0).abs() < 1e-10);
        assert!((parts[1] - 51.0).abs() < 1e-10);
        assert!((parts[2] - 13.0).abs() < 1e-10);
        assert!((parts[3] - 53.0).abs() < 1e-10);
    }

    #[test]
    fn build_api_queries_produces_four() {
        let queries = build_api_queries("12.4,55.6,12.7,55.8").unwrap();
        assert_eq!(queries.len(), 4);
        // All should be valid JSON
        for (_, body) in &queries {
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert!(parsed.get("bbox").is_some());
        }
    }

    #[test]
    fn build_batch_queries_produces_matching_names() {
        let (batch, individual) = build_batch_queries("12.4,55.6,12.7,55.8").unwrap();
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
