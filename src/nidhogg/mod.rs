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
