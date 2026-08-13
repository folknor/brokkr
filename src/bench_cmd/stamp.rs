//! The build environment a baseline was measured under, and the rule that two
//! baselines may only be compared when theirs agree.
//!
//! A criterion baseline is a set of timings with no record of how the code that
//! produced them was built. Compare one measured under `-Ctarget-cpu=native` on
//! a nightly from March against one from a different toolchain or a different
//! machine and criterion will report a confident percentage that means nothing:
//! it has no way to know the two aren't comparable, so it doesn't say. At the
//! effect sizes this command exists to resolve - a percent or two, near
//! criterion's own noise threshold - a toolchain bump is larger than the signal,
//! and nothing about the output would look wrong.
//!
//! So each saved baseline gets a stamp file recording what it was built with,
//! and `--compare` refuses on a mismatch. `--lenient` downgrades the refusal to
//! a warning, because "these differ and I know why" is a legitimate position;
//! what isn't legitimate is not being told.
//!
//! ## What a stamp can and cannot see
//!
//! `rustc -vV` (version + host triple) and the CPU model are read directly.
//! Flags are harder: `RUSTFLAGS` in the environment is visible, but flags set in
//! `~/.cargo/config.toml` - which is where a standing `-Ctarget-cpu=native`
//! usually lives - are not exposed by any stable cargo interface. Rather than
//! report a flag set we know to be incomplete, the stamp records a digest of
//! that file. It cannot say *which* flag changed, only that the file did, which
//! is enough to refuse the comparison. A digest that can't be computed is
//! recorded as absent, and absence never triggers a refusal on its own - a
//! missing reading is not evidence of a difference.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::preflight;

/// `key=value` lines describing one baseline's build environment.
///
/// Ordered so the file is stable across writes, which keeps a stamp diffable
/// when someone wants to see what actually moved.
pub struct Stamp {
    fields: BTreeMap<String, String>,
}

impl Stamp {
    /// Read the current environment.
    ///
    /// Called under the global lock, so the toolchain pin is already moved
    /// aside and `rustc -vV` reports the toolchain that will really build the
    /// bench - not the one the tree asks for and may not have installed.
    pub fn capture(build_root: &Path) -> Self {
        let mut fields = BTreeMap::new();

        if let Some((version, host)) = rustc_version(build_root) {
            fields.insert("rustc".into(), version);
            fields.insert("host".into(), host);
        }
        if let Some(cpu) = cpu_model() {
            fields.insert("cpu".into(), cpu);
        }
        // Both spellings matter: cargo re-exports the encoded form to build
        // scripts, and a caller may have set either.
        for var in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"] {
            if let Ok(val) = std::env::var(var) {
                fields.insert(var.to_lowercase(), val);
            }
        }
        if let Some(digest) = cargo_config_digest() {
            fields.insert("cargo_config".into(), digest);
        }
        // The repo's own `.cargo/config.toml` and those of its ancestors, which
        // is where a project pins `target-cpu`, a linker, or target-specific
        // rustflags. Cargo merges these with the user-level file, so a stamp
        // reading only `$CARGO_HOME` can call two baselines compatible while
        // the code generated for them differs - and a `--commit` run is exactly
        // the case where this file can legitimately change between the two
        // sides being compared.
        for (depth, digest) in repo_config_digests(build_root) {
            fields.insert(format!("repo_cargo_config_d{depth}"), digest);
        }

        Self { fields }
    }

    /// Serialise to the `key=value` form brokkr uses elsewhere for machine
    /// -readable side channels.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (k, v) in &self.fields {
            out.push_str(&format!("{k}={v}\n"));
        }
        out
    }

    /// Parse a stamp file back. Unparseable lines are skipped rather than
    /// fatal: a stamp is advisory metadata, and a corrupt one should not make
    /// an otherwise-usable baseline unreadable.
    pub fn parse(text: &str) -> Self {
        let mut fields = BTreeMap::new();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                fields.insert(k.trim().to_owned(), v.trim().to_owned());
            }
        }
        Self { fields }
    }

    /// Fields present in both stamps whose values disagree.
    ///
    /// A field missing from either side is not a difference. We only ever
    /// compared what both sides recorded, so a stamp written before a field
    /// existed doesn't retroactively invalidate every comparison against it.
    pub fn differences(&self, other: &Self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for (k, mine) in &self.fields {
            if let Some(theirs) = other.fields.get(k)
                && mine != theirs
            {
                out.push((k.clone(), mine.clone(), theirs.clone()));
            }
        }
        out
    }
}

/// `(version string, host triple)` from `rustc -vV`.
fn rustc_version(build_root: &Path) -> Option<(String, String)> {
    let captured = crate::output::run_captured("rustc", &["-vV"], build_root).ok()?;
    if !captured.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&captured.stdout);
    let mut version = None;
    let mut host = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("release: ") {
            version = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("host: ") {
            host = Some(rest.trim().to_owned());
        }
    }
    Some((version?, host?))
}

/// First `model name` in `/proc/cpuinfo`.
fn cpu_model() -> Option<String> {
    let content = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in content.lines() {
        if let Some((key, value)) = line.split_once(':')
            && key.trim() == "model name"
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

/// Digest of the user's cargo config, the usual home of standing rustflags.
fn cargo_config_digest() -> Option<String> {
    let home = match std::env::var_os("CARGO_HOME") {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".cargo"),
    };
    // Cargo accepts both spellings; check the modern one first.
    for name in ["config.toml", "config"] {
        let path = home.join(name);
        if path.exists() {
            return preflight::compute_xxh128(&path).ok();
        }
    }
    None
}

/// Digests of every `.cargo/config{.toml}` from `build_root` upwards, paired
/// with the depth at which each was found (0 = `build_root` itself).
///
/// Keyed by depth rather than discovery order on purpose. Order-based keys
/// shift when one side of a comparison has a config the other lacks, so the
/// shared-fields rule in [`Stamp::differences`] would line up a repo config
/// against a workspace one and report a difference that is really a
/// misalignment. Depth is stable, and it is the same in a `--commit` worktree
/// as in the live tree because the layout below the root is identical.
fn repo_config_digests(build_root: &Path) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (depth, dir) in build_root.ancestors().enumerate() {
        for name in ["config.toml", "config"] {
            let path = dir.join(".cargo").join(name);
            if path.exists() {
                if let Ok(d) = preflight::compute_xxh128(&path) {
                    out.push((depth, d));
                }
                break;
            }
        }
    }
    out
}

/// Reject a baseline label that isn't a single path component.
///
/// The label reaches two joins: this module's stamp path, and criterion's own
/// output paths, which brokkr hands it verbatim. `Path::join` with an absolute
/// argument *replaces* the base rather than extending it, so `--name /tmp/x`
/// writes `/tmp/x.txt` and puts criterion's data somewhere else entirely, while
/// `../` walks out of `.brokkr/`. Neither is a privilege boundary - it is the
/// user's own shell either way - but both quietly relocate a store the rest of
/// the command assumes it owns, and `--baselines` would then list a baseline
/// whose data is not where it says.
///
/// Same rule and same reasoning as `artefacts::validate_test_id`.
pub fn validate_label(label: &str) -> Result<(), DevError> {
    if label.is_empty() {
        return Err(DevError::Config("baseline name is empty".into()));
    }
    if label == "." || label == ".." {
        return Err(DevError::Config(format!(
            "baseline name must not be '.' or '..' (got {label:?})"
        )));
    }
    if label.starts_with('.') {
        return Err(DevError::Config(format!(
            "baseline name must not start with '.' (got {label:?})"
        )));
    }
    if label.contains('/') || label.contains('\\') || label.contains('\0') {
        return Err(DevError::Config(format!(
            "baseline name must be a single path component, no separators (got {label:?})"
        )));
    }
    Ok(())
}

/// Where a baseline's stamp lives. Keyed by baseline name alone, matching
/// criterion's own scoping: a baseline name spans every bench in a run.
pub fn stamp_path(bench_home: &Path, baseline: &str) -> PathBuf {
    bench_home.join("stamps").join(format!("{baseline}.txt"))
}

/// Write a stamp, creating the directory.
pub fn write(bench_home: &Path, baseline: &str, stamp: &Stamp) -> Result<(), DevError> {
    let path = stamp_path(bench_home, baseline);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, stamp.render())?;
    Ok(())
}

/// Read a stamp, if one was recorded.
pub fn read(bench_home: &Path, baseline: &str) -> Option<Stamp> {
    std::fs::read_to_string(stamp_path(bench_home, baseline))
        .ok()
        .map(|t| Stamp::parse(&t))
}

#[cfg(test)]
mod tests {
    use super::Stamp;

    #[test]
    fn roundtrips_through_render_and_parse() {
        let text = "cpu=Ryzen\nhost=x86_64\nrustc=1.90.0\n";
        let stamp = Stamp::parse(text);
        assert_eq!(stamp.render(), text);
    }

    #[test]
    fn differing_shared_fields_are_reported() {
        let a = Stamp::parse("rustc=1.90.0\ncpu=Ryzen\n");
        let b = Stamp::parse("rustc=1.91.0\ncpu=Ryzen\n");
        let diff = a.differences(&b);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].0, "rustc");
        assert_eq!(diff[0].1, "1.90.0");
        assert_eq!(diff[0].2, "1.91.0");
    }

    #[test]
    fn a_field_only_one_side_recorded_is_not_a_difference() {
        // The older stamp predates the field. Refusing on that would make every
        // comparison against a pre-existing baseline fail for no real reason.
        let old = Stamp::parse("rustc=1.90.0\n");
        let new = Stamp::parse("rustc=1.90.0\ncargo_config=abc123\n");
        assert!(old.differences(&new).is_empty());
        assert!(new.differences(&old).is_empty());
    }

    #[test]
    fn label_rejects_separators_and_traversal() {
        // An absolute label is the sharp case: `Path::join` replaces the base
        // rather than extending it, so this would write outside `.brokkr`
        // entirely rather than producing a mangled name inside it.
        for bad in ["", ".", "..", "../escape", "/tmp/absolute", "a/b", "a\\b", ".hidden"] {
            assert!(
                super::validate_label(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn label_accepts_ordinary_names() {
        for good in ["03062cce", "pre-4709", "my_baseline", "v1.2.3"] {
            assert!(
                super::validate_label(good).is_ok(),
                "expected {good:?} to be accepted"
            );
        }
    }

    #[test]
    fn unparseable_lines_are_skipped_not_fatal() {
        let stamp = Stamp::parse("rustc=1.90.0\ngarbage line\n\ncpu=Ryzen\n");
        assert_eq!(stamp.render(), "cpu=Ryzen\nrustc=1.90.0\n");
    }
}
