// The lock file's line-oriented record, the part every reader shares: the
// first-occurrence field split and the `auth` key. Included by `lockfile`
// (beside `lock_record_codec.rs`, which owns the rest of the format) and by
// the standalone rustc guard, so this file has no crate-relative paths or
// inner doc comments.

use std::collections::HashMap;

/// The published capability hash. Emitted among the identity lines, ahead of
/// everything a holder rewrites, so a torn read cannot blend two holders'.
pub const AUTH: &str = super::lock_probe::protocol::AUTH_KEY;

/// Split a record into `key -> trimmed raw value`, first occurrence winning.
/// `None` when the bytes are not UTF-8.
pub fn fields(bytes: &[u8]) -> Option<HashMap<&str, &str>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut out: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            out.entry(key).or_insert(value.trim());
        }
    }
    Some(out)
}
