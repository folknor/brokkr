pub mod bench_api;
pub mod bench_ingest;
pub mod geocode;
pub mod hotpath;
pub mod ingest;
pub mod profile;
pub mod query;
pub mod server;
pub mod update;
pub mod verify_batch;
pub mod verify_geocode;
pub mod verify_readonly;

/// Default spatial query: Copenhagen highways.
pub const DEFAULT_API_QUERY: &str = r#"{"bbox":[55.66,12.55,55.70,12.60],"query":[{"highway":["motorway","trunk","primary","secondary","tertiary","residential"]}]}"#;

/// Default geocode test queries (Danish cities).
pub const GEOCODE_TEST_QUERIES: &[&str] = &["Kobenhavn", "Aarhus", "Odense"];

use crate::error::DevError;

/// Send an HTTP GET request via curl and return the response body.
pub fn curl_get(url: &str) -> Result<String, DevError> {
    let output = std::process::Command::new("curl")
        .args(["-s", "--compressed", url])
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

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Send an HTTP POST request with a JSON body via curl and return the response body.
pub fn curl_post(url: &str, body: &str) -> Result<String, DevError> {
    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "--compressed",
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: output.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

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
