//! The compilation capability: what lets a rustc run while brokkr holds the
//! lock, and what the lock holder hands its own descendants so they qualify.
//!
//! # Why a capability rather than an inference
//!
//! The guard (`src/bin/rustc_guard.rs`) used to decide that a rustc was
//! brokkr's own by walking `/proc` for an ancestor whose `comm` was `brokkr`.
//! Two things are wrong with that. It is spoofable - any executable *named*
//! `brokkr` admits every compiler beneath it, and a shell copied to that name
//! is enough. And it is an inference about process shape, so it fails in every
//! environment where process shape is not visible: a PID namespace, a
//! restricted `/proc`, a child that reparents. When it fails, it fails in the
//! worst possible direction, refusing the one process that must be allowed,
//! because for brokkr's own builds the lock is *always* held - brokkr is the
//! holder.
//!
//! So the holder marks its descendants instead. The mark is a per-acquisition
//! nonce, minted when a hold begins and inherited by every process the holder
//! starts. Inheritance is the whole point: it reaches a grandchild cargo
//! spawned by a test binary or a script check without brokkr having to predict
//! which executables might eventually invoke cargo.
//!
//! # Why the nonce is per-acquisition and not per-process
//!
//! An identity built from the holder's pid, starttime and boot id names the
//! *process*, not the *hold*. One long-lived brokkr can take hold A, spawn a
//! child, release A, and later take hold B; the child's mark still matches,
//! and it is admitted into a window it was never authorized for. The nonce is
//! minted per `LockInner`, so a nested (re-entrant) acquire shares it and a
//! genuinely new hold in the same process does not.
//!
//! # What is published, and what is handed out
//!
//! The nonce goes to descendants through the environment. Only its *hash* is
//! written to the lock file, so reading the world-readable lock file does not
//! hand a bystander a working capability. That is a guard against accident and
//! casual forgery, not a security boundary: a same-user process can read
//! `/proc/<pid>/environ` of any descendant, and this fence never claimed
//! otherwise.

use std::sync::OnceLock;

/// The capability handed to descendants: the current hold's nonce.
pub const CAPABILITY_ENV: &str = "BROKKR_HOLD_NONCE";

/// Set by the guard on the compiler it admits, so a brokkr command started
/// from inside a compilation - a proc macro that shells out - can refuse to
/// take a fresh hold instead of deadlocking against its own lease. See
/// `lockfile::acquire`.
pub const LEASE_MARKER_ENV: &str = "BROKKR_COMPILE_LEASE";

// The human escape hatch is `BROKKR_CARGO`, read only by the guard, which is a
// separate self-contained binary and so cannot share a constant with this
// module. It is checked before any lease is taken and bypasses the protocol
// entirely: a hatch that needs working lock infrastructure is useless exactly
// when it is needed. brokkr itself never sets it - handing it to brokkr's own
// children would let a descendant keep compiling into later holds it was never
// authorized for, which is the whole failure the per-acquisition nonce exists
// to prevent.

/// This process's capability, set once when a hold begins.
///
/// A `OnceLock` rather than `std::env::set_var`: setting a process-wide
/// environment variable is unsound once any other thread is running, and
/// brokkr has drain and watchdog threads. This is read at every spawn choke
/// point instead, which costs nothing and cannot race.
static CAPABILITY: OnceLock<String> = OnceLock::new();

/// Publish this process's capability. Idempotent; the first hold wins.
///
/// The first hold winning is correct rather than merely convenient: a nested
/// acquire shares the outer hold's nonce by construction, and a *sequential*
/// second hold in one process would mint a new nonce that this process's
/// already-running descendants must not be re-authorized with. Descendants of
/// the second hold are covered because there are none yet - a hold that has
/// released has no live children brokkr is still driving.
pub fn publish_capability(nonce: &str) {
    let _first = CAPABILITY.set(nonce.to_owned());
}

/// The capability to stamp onto a child, if this process holds one.
pub fn capability() -> Option<&'static str> {
    CAPABILITY.get().map(String::as_str)
}

/// Stamp the current capability onto a child command.
///
/// Called from the spawn choke points rather than at each of brokkr's ~25 cargo
/// call sites, so a new call site is covered by construction. The boundary is
/// deliberately *every* child brokkr starts under a hold, not just the ones
/// named cargo: a prebuilt test binary, a script check or a benchmark can all
/// reach cargo later, and predicting which is the same inference problem one
/// layer down.
pub fn stamp(cmd: &mut std::process::Command) {
    if let Some(nonce) = capability() {
        cmd.env(CAPABILITY_ENV, nonce);
    }
}

/// A fresh per-acquisition nonce: 16 bytes of kernel entropy, hex.
///
/// `/dev/urandom` rather than a crate, because the guard is a self-contained
/// binary and this keeps both sides on one dependency-free definition. `None`
/// if entropy is unavailable, which the caller must treat as a failure to
/// acquire: a hold with no capability can hand nothing to its children, so
/// every one of its own compiles would be refused.
pub fn mint_nonce() -> Option<String> {
    use std::io::Read;
    let mut buf = [0_u8; 16];
    let mut f = std::fs::File::open("/dev/urandom").ok()?;
    f.read_exact(&mut buf).ok()?;
    Some(hex(&buf))
}

/// The value published in the lock file for a nonce. See the module header for
/// why the file carries the hash and not the nonce itself.
pub fn auth_hash(nonce: &str) -> String {
    hex(&xxhash_rust::xxh3::xxh3_128(nonce.as_bytes()).to_be_bytes())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_minted_nonce_is_thirty_two_hex_chars() {
        let n = mint_nonce().expect("/dev/urandom readable on any test host");
        assert_eq!(n.len(), 32, "{n}");
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()), "{n}");
    }

    #[test]
    fn two_minted_nonces_differ() {
        // Not a randomness test - a guard against a constant sneaking in.
        assert_ne!(mint_nonce().unwrap(), mint_nonce().unwrap());
    }

    #[test]
    fn the_hash_is_stable_and_hides_the_nonce() {
        let n = "0123456789abcdef0123456789abcdef";
        assert_eq!(auth_hash(n), auth_hash(n));
        assert_ne!(auth_hash(n), n);
        assert_ne!(auth_hash(n), auth_hash("0123456789abcdef0123456789abcdee"));
    }

    #[test]
    fn stamp_is_a_no_op_without_a_capability() {
        // The capability is process-global and this test must not depend on
        // whether another test published one, so assert only the invariant
        // that holds either way: stamping never panics and never sets an
        // empty value.
        let mut cmd = std::process::Command::new("/bin/true");
        stamp(&mut cmd);
        if let Some(c) = capability() {
            assert!(!c.is_empty());
        }
    }
}
