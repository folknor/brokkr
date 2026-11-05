//! What the identity route says about the process answering it, and what
//! `deploy` compares it with.
//!
//! The build is an xxh3-128 digest of the executable the process runs,
//! streamed from an opened `/proc/self/exe`, so it names the running file
//! object even after its path was replaced or unlinked. It is an
//! accidental-mismatch check of the executable's bytes, not authentication,
//! and says nothing of libraries or configuration. A probe is a caller's
//! nonce the route echoes, so the caller knows the answer was made for its
//! own request rather than replayed.

use std::io::Read;

use xxhash_rust::xxh3::Xxh3;

/// The `server` field of the identity answer.
pub(crate) const SERVER: &str = "brokkr-results-server";

/// The longest probe the route echoes.
const PROBE_MAX: usize = 64;

/// The digest of this process's own executable, 32 lowercase hex.
pub(crate) fn executable_digest() -> Result<String, String> {
    let mut file = std::fs::File::open("/proc/self/exe")
        .map_err(|e| format!("cannot open /proc/self/exe: {e}"))?;
    let mut hasher = Xxh3::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read /proc/self/exe: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:032x}", hasher.digest128()))
}

/// The probe a query names, if any: `probe=<1 to 64 lowercase hex>`, at
/// most once. Other parameters are ignored.
pub(crate) fn probe_of(query: Option<&str>) -> Result<Option<String>, String> {
    let mut found = None;
    for pair in query.unwrap_or_default().split('&') {
        let Some(value) = pair.strip_prefix("probe=") else {
            continue;
        };
        if found.is_some() {
            return Err("the query names probe more than once".to_owned());
        }
        if value.is_empty()
            || value.len() > PROBE_MAX
            || !value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(format!(
                "a probe is 1 to {PROBE_MAX} lowercase hex characters"
            ));
        }
        found = Some(value.to_owned());
    }
    Ok(found)
}

/// A fresh random probe, 32 lowercase hex, from the kernel's generator.
/// Never blocks: a generator not yet initialised is an error, not a wait.
pub(crate) fn fresh_probe() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    // SAFETY: the buffer is valid for writes of its whole length.
    let read =
        unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), libc::GRND_NONBLOCK) };
    if usize::try_from(read).ok() != Some(bytes.len()) {
        return Err(format!(
            "cannot draw a random probe: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_is_short_lowercase_hex_named_once() {
        assert_eq!(probe_of(None).unwrap(), None);
        assert_eq!(probe_of(Some("other=1")).unwrap(), None);
        assert_eq!(
            probe_of(Some("x=1&probe=ab12")).unwrap().as_deref(),
            Some("ab12")
        );
        assert!(probe_of(Some("probe=")).is_err());
        assert!(probe_of(Some("probe=AB")).is_err());
        assert!(probe_of(Some(&format!("probe={}", "a".repeat(65)))).is_err());
        assert!(probe_of(Some("probe=ab&probe=ab")).is_err());
        let fresh = fresh_probe().unwrap();
        assert_eq!(
            probe_of(Some(&format!("probe={fresh}"))).unwrap(),
            Some(fresh)
        );
    }

    #[test]
    fn the_digest_names_the_running_executable() {
        let digest = executable_digest().unwrap();
        assert_eq!(digest.len(), 32);
        assert_eq!(digest, executable_digest().unwrap());
    }
}
