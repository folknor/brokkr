//! Stamp the binary's version string into a compile-time env var that
//! `cli/schema.rs` reads for `--version`.
//!
//! Composes semver (from `CARGO_PKG_VERSION`) with the short git hash and the
//! UTC build time into `BROKKR_LONG_VERSION`, e.g.
//! `0.1.0 (abc123def 2026-07-12 12:34:56 UTC)`, with a `-dirty` suffix on the
//! hash when the tree carried uncommitted changes at build time. This is what
//! makes a stale installed `brokkr` self-evident: `brokkr --version` names the
//! exact commit it was built from instead of the static `0.1.0`.
//!
//! Kept dependency-light: shells `git` and `date` directly rather than pulling
//! crates, and falls back to `unknown` outside a checkout (e.g. a `cargo
//! install` tarball with no `.git`) so a release never fails to build for lack
//! of git metadata.

use std::process::Command;

/// Run `cmd args`, returning trimmed stdout, or `None` on failure / empty
/// output (so a missing `git`/`date` or a non-repo build degrades cleanly).
fn capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

fn main() {
    let hash =
        capture("git", &["rev-parse", "--short=9", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());

    // A non-empty porcelain status means the tree carried uncommitted changes at
    // build time - flag it so a `-dirty` build is never mistaken for one off a
    // clean commit.
    let hash = if capture("git", &["status", "--porcelain"]).is_some() {
        format!("{hash}-dirty")
    } else {
        hash
    };

    let build_time =
        capture("date", &["-u", "+%Y-%m-%d %H:%M:%S UTC"]).unwrap_or_else(|| "unknown".to_owned());

    let semver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_owned());

    println!("cargo:rustc-env=BROKKR_LONG_VERSION={semver} ({hash} {build_time})");

    // Re-run when the checked-out commit or the index moves so the hash and the
    // dirty flag stay honest. brokkr is a root crate, so its `.git` is one level
    // up from this build script. There is no portable way to force a fresh
    // timestamp on an otherwise unchanged rebuild; the build time therefore
    // tracks the last commit/index change, which is the meaningful moment.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
