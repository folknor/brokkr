//! The names brokkr owns directly under a Cargo target root.
//!
//! Cargo puts each profile's artifacts in `<target>/<profile dir>/`, and
//! brokkr shares that root with it: isolated sweep targets, worktree targets,
//! fixture-lint targets and scratch all live beside the profile directories.
//! A custom Cargo profile whose name matched one of brokkr's entries would
//! build into it, and `brokkr clean` would treat the profile's artifacts as
//! brokkr scratch. Every writer of a target-root entry takes its name from
//! here, and [`crate::build::ProfileName`] refuses to select a profile that
//! [`reserved_reason`] names, so the two cannot drift apart.
//!
//! Reservations are part of the config contract once a profile name is in
//! use: adding an entry here can turn a `profile = "<name>"` that loads
//! today into a refusal. Add one only with the user's approval.

/// Isolated per-RUSTFLAGS sweep targets: `rustflags-<hash>`.
pub(crate) const RUSTFLAGS_PREFIX: &str = "rustflags-";

/// Isolated feature-unification sweep targets: `unify-<mode>[-<hash>]`.
pub(crate) const UNIFY_PREFIX: &str = "unify-";

/// Per-entry `[[fixture_lint]]` targets: `fixture-<name>`.
pub(crate) const FIXTURE_PREFIX: &str = "fixture-";

/// Per-worktree targets: `brokkr-worktrees/<key>/<short>`.
pub(crate) const WORKTREES: &str = "brokkr-worktrees";

/// The scratch dir exported to tests as `TMPDIR`-style storage.
pub(crate) const TMP: &str = "tmp";

/// Pbfhogg's `verify` output dir.
pub(crate) const VERIFY: &str = "verify";

/// Entries Cargo itself creates at the target root, or profile names Cargo
/// treats specially. A custom profile cannot take these either.
const CARGO_OWNED: &[&str] = &[
    "dev", "release", "test", "bench", "debug", "doc", "build", "package",
];

/// Exact brokkr-owned names.
const EXACT: &[&str] = &[WORKTREES, TMP, VERIFY];

/// Brokkr-owned name prefixes.
const PREFIXES: &[&str] = &[RUSTFLAGS_PREFIX, UNIFY_PREFIX, FIXTURE_PREFIX];

/// Why `name` cannot be a custom profile directory, or `None` when it is free.
pub(crate) fn reserved_reason(name: &str) -> Option<String> {
    if CARGO_OWNED.contains(&name) {
        return Some(format!(
            "`{name}` is a Cargo built-in profile or target-dir entry"
        ));
    }
    if EXACT.contains(&name) {
        return Some(format!(
            "`{name}` is a directory brokkr owns under the Cargo target root"
        ));
    }
    PREFIXES.iter().find(|p| name.starts_with(**p)).map(|p| {
        format!("`{name}` starts with `{p}`, a prefix brokkr owns under the Cargo target root")
    })
}

#[cfg(test)]
mod tests {
    use super::reserved_reason;

    #[test]
    fn brokkr_and_cargo_entries_are_reserved_and_others_are_free() {
        for name in [
            "dev",
            "debug",
            "doc",
            "tmp",
            "verify",
            "brokkr-worktrees",
            "rustflags-x",
            "unify-package",
            "fixture-a",
        ] {
            assert!(reserved_reason(name).is_some(), "{name}");
        }
        for name in ["corpus", "fast-dev", "unify", "fixture", "rustflags"] {
            assert!(reserved_reason(name).is_none(), "{name}");
        }
    }
}
