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
//! rustc's version and host triple, and the CPU model, are read directly.
//! Flags are harder: `RUSTFLAGS` in the environment is visible, but flags set in
//! `~/.cargo/config.toml` - which is where a standing `-Ctarget-cpu=native`
//! usually lives - are not exposed by any stable cargo interface. Rather than
//! report a flag set we know to be incomplete, the stamp records a digest of
//! that file. It cannot say *which* flag changed, only that the file did, which
//! is enough to refuse the comparison.
//!
//! "Not there" and "could not read" are kept apart. An unset `RUSTFLAGS`, a
//! missing user cargo config or a build root with no `.cargo` config is a
//! reading, recorded as `(unset)` or `(absent)` and compared like any value,
//! so unset against set refuses. A digest that can't be computed is no
//! reading: the field is left out, and a field one side lacks never refuses on
//! its own - a missing reading is not evidence of a difference, and a stamp
//! written before a field existed must stay comparable.

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

        if let Ok(info) = crate::cargo::rustc::load(build_root) {
            fields.insert("rustc".into(), info.release.clone());
            fields.insert("host".into(), info.host.clone());
            if let Some(commit_hash) = &info.commit_hash {
                fields.insert("rustc_commit".into(), commit_hash.clone());
            }
        }
        if let Some(cpu) = crate::host::cpu::collect().model {
            fields.insert("cpu".into(), cpu);
        }
        // Both spellings matter: cargo re-exports the encoded form to build
        // scripts, and a caller may have set either. Unset is recorded as
        // such, so unset against set is a difference rather than a field one
        // side lacks.
        for var in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"] {
            let val = std::env::var_os(var)
                .map_or_else(|| UNSET.to_owned(), |v| v.to_string_lossy().into_owned());
            fields.insert(var.to_lowercase(), val);
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
        // The build root's own config is the same directory on both sides of
        // any comparison, so its absence is a reading like any other: a config
        // added between two commits is a difference. Deeper ancestors are not
        // - a `--commit` worktree's parents are not the live tree's - so they
        // stay one-sided when absent.
        if !fields.contains_key("repo_cargo_config_d0")
            && crate::user_dirs::cargo_config_file(&build_root.join(".cargo")).is_none()
        {
            fields.insert("repo_cargo_config_d0".into(), ABSENT.into());
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

/// The value of an environment variable that was not set.
const UNSET: &str = "(unset)";

/// The value of a config file that does not exist.
const ABSENT: &str = "(absent)";

/// Digest of the user's cargo config, the usual home of standing rustflags.
///
/// Digests the file cargo actually reads, which is the extensionless legacy
/// `config` when both spellings exist (`user_dirs::cargo_config_file`). No
/// file is [`ABSENT`], a value like any other; a file that exists but cannot
/// be digested, or no cargo home at all, is no reading.
fn cargo_config_digest() -> Option<String> {
    let home = crate::user_dirs::cargo_home()?;
    let Some(path) = crate::user_dirs::cargo_config_file(&home) else {
        return Some(ABSENT.into());
    };
    preflight::compute_file_digest(&path)
        .map(|d| d.to_string())
        .ok()
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
        if let Some(path) = crate::user_dirs::cargo_config_file(&dir.join(".cargo"))
            && let Ok(d) = preflight::compute_file_digest(&path).map(|d| d.to_string())
        {
            out.push((depth, d));
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
        return Err(DevError::Refused("baseline name is empty".into()));
    }
    if label == "." || label == ".." {
        return Err(DevError::Refused(format!(
            "baseline name must not be '.' or '..' (got {label:?})"
        )));
    }
    if label.starts_with('.') {
        return Err(DevError::Refused(format!(
            "baseline name must not start with '.' (got {label:?})"
        )));
    }
    if label.contains('/') || label.contains('\\') || label.contains('\0') {
        return Err(DevError::Refused(format!(
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

/// Read a stamp. `Ok(None)` when none was recorded; any other failure to read
/// it is an error, so a caller gating on the stamp cannot mistake "could not
/// read" for "nothing to compare".
pub fn read(bench_home: &Path, baseline: &str) -> Result<Option<Stamp>, DevError> {
    let path = stamp_path(bench_home, baseline);
    match std::fs::read_to_string(&path) {
        Ok(t) => Ok(Some(Stamp::parse(&t))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DevError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot read baseline stamp {}: {e}", path.display()),
        ))),
    }
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

    /// Unset is a value, not a missing field: RUSTFLAGS going from unset to
    /// set, or a repo config appearing, refuses. The old one-sided rule let
    /// both through.
    #[test]
    fn unset_against_set_is_a_difference() {
        let unset = Stamp::parse("rustflags=(unset)\nrepo_cargo_config_d0=(absent)\n");
        let set = Stamp::parse("rustflags=-Ctarget-cpu=native\nrepo_cargo_config_d0=abc\n");
        let keys: Vec<String> = unset.differences(&set).into_iter().map(|d| d.0).collect();
        assert_eq!(keys, ["repo_cargo_config_d0", "rustflags"]);
    }

    /// A capture taken now records every unset flag variable and an absent
    /// build-root config explicitly, so two new stamps always share them.
    #[test]
    fn a_capture_records_absence_explicitly() {
        let root = crate::test_scratch::scratch("bench_stamp", "absence");
        let stamp = Stamp::capture(&root);
        for key in [
            "rustflags",
            "cargo_encoded_rustflags",
            "repo_cargo_config_d0",
        ] {
            assert!(
                stamp.fields.contains_key(key),
                "{key} missing: {}",
                stamp.render()
            );
        }
        assert_eq!(stamp.fields["repo_cargo_config_d0"], super::ABSENT);
    }

    #[test]
    fn label_rejects_separators_and_traversal() {
        // An absolute label is the sharp case: `Path::join` replaces the base
        // rather than extending it, so this would write outside `.brokkr`
        // entirely rather than producing a mangled name inside it.
        for bad in [
            "",
            ".",
            "..",
            "../escape",
            "/tmp/absolute",
            "a/b",
            "a\\b",
            ".hidden",
        ] {
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
