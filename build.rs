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
//! Kept dependency-light: shells `git` directly rather than pulling crates, and
//! falls back to `unknown` outside a checkout (e.g. a `cargo install` tarball
//! with no `.git`, or no `git` on PATH) so a release never fails to build for
//! lack of git metadata. The build time is formatted here rather than by
//! `date`, and honours `SOURCE_DATE_EPOCH` for reproducible builds.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Run `cmd args`, returning trimmed stdout, or `None` on failure / empty
/// output (so a missing `git` or a non-repo build degrades cleanly).
fn capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

/// Days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The build time as `YYYY-MM-DD HH:MM:SS UTC`: `SOURCE_DATE_EPOCH` when set
/// (the reproducible-builds convention), otherwise the current clock.
fn build_time() -> String {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let secs = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(v) => match v.trim().parse::<i64>() {
            Ok(n) => n,
            Err(_) => return "unknown".to_owned(),
        },
        Err(_) => match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_secs()).unwrap_or(0),
            Err(_) => return "unknown".to_owned(),
        },
    };
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Emit `rerun-if-changed` for `path` only when it exists: cargo treats a
/// missing path as permanently stale and would rerun this script every build.
fn rerun_if_exists(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    let hash =
        capture("git", &["rev-parse", "--short=9", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());

    // A non-empty porcelain status means the tree carried uncommitted changes at
    // build time - flag it. Honest only as of the last time this script ran; the
    // rerun triggers below are what keep that moment current.
    let hash = if capture("git", &["status", "--porcelain"]).is_some() {
        format!("{hash}-dirty")
    } else {
        hash
    };

    let semver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_owned());

    println!("cargo:rustc-env=BROKKR_LONG_VERSION={semver} ({hash} {})", build_time());

    // Rerun triggers. The git files are located by asking git rather than
    // assuming `.git/` is a directory beside this script: in a linked worktree
    // `.git` is a file, HEAD and the index live in the per-worktree git dir,
    // and branch refs live in the common dir.
    //
    // - HEAD changes on checkout/switch, but NOT when a commit advances the
    //   branch it points at - that writes the ref file (or, once packed,
    //   `packed-refs`), so both are watched too.
    // - The index changes on staging and on commit.
    // - An unstaged edit changes neither, so the inputs compiled into the binary
    //   are watched directly: editing (or reverting) one reruns the script and
    //   re-reads the dirty flag. An edit to a file the binary does not contain
    //   (a note, say) can leave the flag stale, but then the binary is
    //   byte-for-byte what the clean commit would build anyway.
    //
    // Declaring any rerun-if-changed replaces cargo's default of rerunning on
    // every package file change, which is why the source inputs are listed.
    if let Some(git_dir) = capture("git", &["rev-parse", "--absolute-git-dir"]) {
        let git_dir = PathBuf::from(git_dir);
        rerun_if_exists(&git_dir.join("HEAD"));
        rerun_if_exists(&git_dir.join("index"));
        // A relative `--git-common-dir` is relative to the cwd (this script
        // runs in the manifest dir), so joining it onto the cwd is exact.
        let common = capture("git", &["rev-parse", "--git-common-dir"])
            .map(PathBuf::from)
            .and_then(|p| {
                if p.is_absolute() {
                    Some(p)
                } else {
                    std::env::current_dir().ok().map(|cwd| cwd.join(p))
                }
            })
            .unwrap_or_else(|| git_dir.clone());
        if let Some(head_ref) = capture("git", &["symbolic-ref", "-q", "HEAD"]) {
            rerun_if_exists(&common.join(head_ref));
        }
        rerun_if_exists(&common.join("packed-refs"));
    }
    for input in ["src", "docs", "scripts", "build.rs", "Cargo.toml", "Cargo.lock"] {
        rerun_if_exists(Path::new(input));
    }
}
