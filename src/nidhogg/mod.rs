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
}
