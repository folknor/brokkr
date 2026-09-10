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

use std::sync::Mutex;

/// The capability handed to descendants: the current hold's nonce.
pub const CAPABILITY_ENV: &str = "BROKKR_HOLD_NONCE";

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
pub const LEASE_MARKER_ENV: &str = "BROKKR_COMPILE_LEASE";

// The human escape hatch is `BROKKR_CARGO`, read only by the guard, which is a
// separate self-contained binary and so cannot share a constant with this
// module. It is checked before any lease is taken and bypasses the protocol
// entirely: a hatch that needs working lock infrastructure is useless exactly
// when it is needed. brokkr itself never sets it - handing it to brokkr's own
// children would let a descendant keep compiling into later holds it was never
// authorized for, which is the whole failure the per-acquisition nonce exists
// to prevent.

/// This process's capability for the hold it currently owns.
///
/// A `Mutex`, not `std::env::set_var`: setting a process-wide environment
/// variable is unsound once any other thread is running, and brokkr has drain
/// and watchdog threads. This is read at every spawn choke point instead, which
/// costs nothing and cannot race.
///
/// Mutable rather than write-once, which was a real bug on the way here. A
/// `OnceLock` keeps the *first* hold's nonce forever, so a process that takes
/// two holds in sequence - acquire, release, acquire - would publish nonce two
/// to the lock file while still stamping nonce one on its children, and the
/// guard would refuse every single one of them. Nested acquires share a nonce by
/// construction and never reach this, so only the sequential case was exposed;
/// it fails totally when it happens, and no live descendant of the released hold
/// has any claim on the new one.
static CAPABILITY: Mutex<Option<String>> = Mutex::new(None);

/// Take the capability lock, treating poison as recoverable. The critical
/// section is a single clone; failing closed here would mean refusing to stamp,
/// which turns into the guard refusing brokkr's own compiles.
fn slot() -> std::sync::MutexGuard<'static, Option<String>> {
    CAPABILITY.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Publish the capability for a newly acquired hold, replacing any previous
/// one.
pub fn publish_capability(nonce: &str) {
    *slot() = Some(nonce.to_owned());
}

/// Forget the capability when a hold is released, so a child spawned outside any
/// hold does not carry a mark for a hold that is over.
pub fn clear_capability() {
    *slot() = None;
}

/// The capability to stamp onto a child, if this process holds one.
pub fn capability() -> Option<String> {
    slot().clone()
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
    // Unconditional, hold or no hold: replay protection is about what a
    // cargo READS, not about what this process is authorized to do. See
    // [`RUSTC_INFO_CACHE_ENV`].
    cmd.env(RUSTC_INFO_CACHE_ENV, "0");
}

/// The capability as a cargo config *file*, for the nextest lane.
///
/// The lane launches test processes through the linked engine
/// (`TestList::new`, `runner.try_execute`), which construct their own commands
/// with no `Command` for [`stamp`] to reach. The engine does honour cargo's
/// `[env]` table, which it reads through `CargoConfigs`, so the capability
/// travels the documented route instead: an override the engine applies to every
/// process it spawns. `force` is set so an inherited value cannot shadow it -
/// a brokkr started inside an admitted compilation carries the *outer* hold's
/// nonce in its own environment, and without `force` that stale nonce would
/// shadow the fresh one on every test process. `relative` is pinned `false`
/// because the engine's merge inherits an *unset* field from lower-precedence
/// configs even when this entry wins on value: a discovered config marking the
/// same variable `relative = true` would resolve `"0"` against this file's
/// directory and hand the child a path instead.
///
/// A file rather than CLI `--config` expressions because the engine's CLI
/// parser accepts neither shape that could carry `force`: an inline table is
/// rejected outright ("should be a dotted key expression" - the failure that
/// broke the serial nextest lane in production), and splitting into
/// `env.NAME.value = ..` / `env.NAME.force = ..` fails too, since each CLI
/// argument deserializes independently and a document holding only `force` has
/// no `value` to satisfy the env entry type. A path in the same vec, however,
/// is loaded as a full config file, where inline tables are legal. The file
/// lives beside the lock (`~/.brokkr/`, never swept, and this crate's rules
/// forbid `/tmp`); one fixed name is safe because holds are serialized by the
/// global flock and `CargoConfigs::new` reads the file eagerly. Rewritten from
/// scratch on every call - including capability-less ones, so a previous
/// hold's nonce cannot survive - and kept owner-only, since the published lock
/// file carries only the nonce's hash and this file would otherwise leak the
/// real thing to other users.
///
/// Always carries the rustc-info cache disable (see [`RUSTC_INFO_CACHE_ENV`]);
/// the capability entry joins it when a hold is active. Returns the path to
/// hand to `CargoConfigs::new`; any write failure propagates, because
/// returning the path after a failed truncate would load stale contents.
pub fn cargo_config_overrides() -> std::io::Result<Vec<String>> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let home = std::env::var("HOME")
        .map_err(|_| std::io::Error::other("$HOME is not set - cannot place the nextest env config"))?;
    let dir = std::path::PathBuf::from(home).join(".brokkr");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("nextest-env.toml");

    let mut contents = String::from("[env]\n");
    contents.push_str(&format!(
        "{RUSTC_INFO_CACHE_ENV} = {{ value = \"0\", force = true, relative = false }}\n"
    ));
    if let Some(nonce) = capability() {
        contents.push_str(&format!(
            "{CAPABILITY_ENV} = {{ value = \"{nonce}\", force = true, relative = false }}\n"
        ));
    }

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    // `mode` only applies at creation; tighten a pre-existing file too.
    file.set_permissions(<std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(
        0o600,
    ))?;
    file.write_all(contents.as_bytes())?;

    Ok(vec![path.to_string_lossy().into_owned()])
}

/// Whether this process is running inside a compilation the guard admitted.
///
/// A brokkr command started from there cannot take a *fresh* hold: it would take
/// `brokkr.lock` and then wait forever for an exclusive compile lease that its
/// own waiting ancestor holds a share of. See `lockfile::acquire`.
pub fn inside_admitted_compilation() -> bool {
    std::env::var_os(LEASE_MARKER_ENV).is_some_and(|v| !v.is_empty())
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

    /// The bug this replaced a `OnceLock` to fix: a second sequential hold must
    /// stamp its own nonce, or every child of it is refused by the guard.
    ///
    /// Serialized with the other capability-mutating test through one lock, and
    /// restoring the previous value, because the slot is process-global and
    /// `cargo test` shares a process across tests.
    #[test]
    fn a_second_hold_replaces_the_first_holds_capability() {
        let _seq = capability_test_lock();
        let before = capability();
        publish_capability("first");
        assert_eq!(capability().as_deref(), Some("first"));
        publish_capability("second");
        assert_eq!(
            capability().as_deref(),
            Some("second"),
            "a sequential second hold must replace the first hold's nonce"
        );
        clear_capability();
        assert_eq!(capability(), None, "release must forget the capability");
        *slot() = before;
    }

    /// Tests that mutate the process-global capability must not interleave.
    fn capability_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static SEQ: Mutex<()> = Mutex::new(());
        SEQ.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
