// Shared lock protocol primitives and non-blocking observation of the global
// exclusive lock. `include!`d into both `lockfile` and the standalone guard,
// alongside the literal protocol values from `lock_protocol.rs`, so it carries
// `//` comments, not `//!` (an inner doc comment is rejected there).

use std::os::fd::RawFd;

#[allow(dead_code)]
pub(super) mod protocol {
    include!("lock_protocol.rs");
}

/// A canonical, lowercase 128-bit value shared by the holder and standalone
/// guard. The wrappers below keep each protocol role distinct at call sites.
#[derive(Clone, Eq, Hash, PartialEq)]
struct Hex128(String);

impl Hex128 {
    fn parse(value: &str) -> Option<Self> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        Some(Self(value.to_owned()))
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        Self(hex(bytes))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// The secret capability inherited by a holder's descendants.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct Nonce(Hex128);

impl Nonce {
    pub fn parse(value: &str) -> Option<Self> {
        Hex128::parse(value).map(Self)
    }

    /// Explicitly expose the secret for the child environment that carries it.
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub fn auth_hash(&self) -> AuthHash {
        let bytes = xxhash_rust::xxh3::xxh3_128(self.expose().as_bytes()).to_be_bytes();
        AuthHash(Hex128::from_bytes(&bytes))
    }
}

impl std::fmt::Debug for Nonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Nonce([redacted])")
    }
}

/// The non-secret value published in the lock record for a capability.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AuthHash(Hex128);

impl AuthHash {
    pub fn parse(value: &str) -> Option<Self> {
        Hex128::parse(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for AuthHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AuthHash").field(&self.as_str()).finish()
    }
}

// Minting and the holder-only identities (`AcqId`, `RunToken`) live in
// `lock_tokens.rs`, which only `lockfile` includes beside this file: the guard
// parses and hashes, never mints, and would otherwise carry them as dead code.

fn hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push_str(&format!("{byte:02x}"));
    }
    value
}

/// Try to take a shared flock on `fd`.
///
/// `Ok(true)` means the shared lock was acquired and remains held until `fd`
/// is closed. `Ok(false)` means an exclusive holder conflicts with the probe.
/// Other errors do not establish whether the lock is held.
pub(super) fn try_shared(fd: RawFd) -> std::io::Result<bool> {
    // SAFETY: the caller keeps this open fd alive for the probe and any reads
    // protected by a successfully acquired shared lock.
    if unsafe { libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}
