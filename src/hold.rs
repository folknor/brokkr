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

use crate::lockfile::{AuthHash, Nonce};

/// The capability handed to descendants: the current hold's nonce.
pub(crate) use crate::lockfile::CAPABILITY_ENV;

/// Cargo's switch for its persistent rustc-probe cache
/// (`target/.rustc_info.json`), stamped `0` on every child brokkr starts.
///
/// The cache stores FAILED `rustc -vV`/`--print` probes - stderr included -
/// keyed by a fingerprint that ignores inherited env vars, and replays them
/// without re-running the probe. A foreign cargo refused by the guard during
/// someone's hold therefore poisons the shared cache, and brokkr's own
/// correctly-stamped cargo then replays that refusal verbatim: the nonce is
/// present, the guard never runs, the build fails anyway, and `BROKKR_CARGO=1`
/// cannot cure it (it reaches a guard cargo no longer spawns). Observed live
/// 2026-09-10 against a piners `cargo doc` script check. Disabling the cache
/// for brokkr's children makes every brokkr build re-probe - a few tens of
/// milliseconds per cargo invocation - and immune to historical or concurrent
/// poison. The guard's query admission (`src/bin/rustc_guard.rs::classify`)
/// stops NEW poison at the source; this stops old and racing poison from
/// reaching brokkr.
pub const RUSTC_INFO_CACHE_ENV: &str = "CARGO_CACHE_RUSTC_INFO";

/// Set by the guard on the compiler it admits, so a brokkr command started
/// from inside a compilation - a proc macro that shells out - can refuse to
/// take a fresh hold instead of deadlocking against its own lease. See
/// `lockfile::acquire`.
pub(crate) use crate::lockfile::LEASE_MARKER_ENV;

// The human escape hatch is `BROKKR_CARGO`, whose name is part of the shared
// lock protocol and is read only by the standalone guard. It is checked before
// any lease is taken and bypasses the protocol entirely: a hatch that needs
// working lock infrastructure is useless exactly when it is needed. brokkr
// itself never sets it - handing it to brokkr's own children would let a
// descendant keep compiling into later holds it was never authorized for,
// which is the whole failure the per-acquisition nonce exists to prevent.

/// The capability to stamp onto a child, if this process holds one.
///
/// The nonce lives on the active `LockInner`; looking it up there ties the
/// capability to the hold whose lock-file record publishes its hash. Cloning
/// the nonce here lets a spawn finish constructing its environment without
/// extending the lock's lifetime.
pub fn capability() -> Option<Nonce> {
    crate::lockfile::current_capability()
}

/// Environment entries emitted by the hold policy. Keeping these as values
/// also lets process engines without a `Command` handle render the same
/// entries into their own configuration.
pub(crate) fn child_env() -> Vec<(String, String)> {
    let mut env = Vec::with_capacity(2);
    if let Some(nonce) = capability() {
        env.push((CAPABILITY_ENV.to_owned(), nonce.expose().to_owned()));
    }
    // Unconditional, hold or no hold: replay protection is about what a cargo
    // READS, not about what this process is authorized to do.
    env.push((RUSTC_INFO_CACHE_ENV.to_owned(), "0".to_owned()));
    env
}

/// The published hash of the capability this process INHERITED - the nonce
/// of whatever hold its ancestor owned when it was spawned - or `None` when it
/// inherited none.
///
/// Distinct from [`capability`], which is the hold this process itself owns:
/// a brokkr that takes its own hold stamps that nonce over the inherited one on
/// its children, and the inherited value never becomes an owned one. What the
/// inherited hash is for is recognising, while waiting for the lock, that the
/// current holder is this process's own ancestor (`lockfile::acquire`).
pub fn inherited_capability_hash() -> Option<AuthHash> {
    std::env::var(CAPABILITY_ENV)
        .ok()
        .and_then(|n| Nonce::parse(&n))
        .map(|nonce| nonce.auth_hash())
}

/// Stamp the current capability onto a child command.
///
/// Called at the end of shared child-policy construction, after caller env and
/// the other policy steps. Direct `Command` constructors must use that policy
/// too if their descendants need the hold capability.
pub fn stamp(cmd: &mut std::process::Command) {
    for (name, value) in child_env() {
        cmd.env(name, value);
    }
}

/// Whether this process is running inside a compilation the guard admitted.
///
/// A brokkr command started from there cannot take a *fresh* hold: it would take
/// `brokkr.lock` and then wait forever for an exclusive compile lease that its
/// own waiting ancestor holds a share of. See `lockfile::acquire`.
pub fn inside_admitted_compilation() -> bool {
    std::env::var_os(LEASE_MARKER_ENV).is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_minted_nonce_is_thirty_two_hex_chars() {
        let n = Nonce::mint().expect("/dev/urandom readable on any test host");
        assert_eq!(n.expose().len(), 32);
        assert!(n.expose().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn two_minted_nonces_differ() {
        // Not a randomness test - a guard against a constant sneaking in.
        assert_ne!(Nonce::mint().unwrap(), Nonce::mint().unwrap());
    }

    #[test]
    fn the_hash_is_stable_and_hides_the_nonce() {
        let n = Nonce::parse("0123456789abcdef0123456789abcdef").unwrap();
        let other = Nonce::parse("0123456789abcdef0123456789abcdee").unwrap();
        assert_eq!(n.auth_hash(), n.auth_hash());
        assert_ne!(n.auth_hash().as_str(), n.expose());
        assert_ne!(n.auth_hash(), other.auth_hash());
    }

    #[test]
    fn stamp_disables_the_probe_cache_without_a_capability() {
        let mut cmd = std::process::Command::new("/bin/true");
        stamp(&mut cmd);
        let env: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(env.get(RUSTC_INFO_CACHE_ENV).map(String::as_str), Some("0"));
        if let Some(capability) = capability() {
            assert_eq!(
                env.get(CAPABILITY_ENV).map(String::as_str),
                Some(capability.expose())
            );
        } else {
            assert!(!env.contains_key(CAPABILITY_ENV));
        }
    }
}
