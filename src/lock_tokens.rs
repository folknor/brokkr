// The minting half of the lock protocol tokens, plus the identities only the
// holder side uses. `include!`d by `lockfile` into the same module as
// `lock_probe.rs` (so it shares `Hex128`'s private constructor), and NOT by the
// standalone `brokkr-rustc-guard`, which only parses a nonce and compares its
// hash - everything here would be dead code there. `//` comments, not `//!`,
// for the same `include!` reason as `lock_probe.rs`.

impl Hex128 {
    fn mint() -> Option<Self> {
        use std::io::Read;

        let mut bytes = [0_u8; 16];
        std::fs::File::open("/dev/urandom")
            .ok()?
            .read_exact(&mut bytes)
            .ok()?;
        Some(Self::from_bytes(&bytes))
    }
}

impl Nonce {
    pub fn mint() -> Option<Self> {
        Hex128::mint().map(Self)
    }
}

/// Public identity for one lock acquisition and its control socket.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AcqId(Hex128);

impl AcqId {
    pub fn mint() -> Option<Self> {
        Hex128::mint().map(Self)
    }

    pub fn parse(value: &str) -> Option<Self> {
        Hex128::parse(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for AcqId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AcqId").field(&self.as_str()).finish()
    }
}

/// Public identity for a registered test-run liveness file.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct RunToken(Hex128);

impl RunToken {
    pub fn mint() -> Option<Self> {
        Hex128::mint().map(Self)
    }

    pub fn parse(value: &str) -> Option<Self> {
        Hex128::parse(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for RunToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RunToken").field(&self.as_str()).finish()
    }
}
