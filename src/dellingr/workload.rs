//! Workload registry resolution: `--lua <name>` -> a verified absolute path.

use std::path::{Path, PathBuf};

use crate::config::{DellingrConfig, DevConfig};
use crate::error::DevError;
use crate::preflight;

/// A workload resolved to an absolute path, with its digest already verified.
#[derive(Debug)]
pub(crate) struct Resolved {
    /// The name `--lua` took, and the label the run is filed under in
    /// `results.db`.
    pub(crate) name: String,
    /// Absolute path to the `.lua` file in the *current* tree.
    pub(crate) path: PathBuf,
}

impl Resolved {
    /// The workload path as a UTF-8 string for the child's argv.
    pub(crate) fn path_str(&self) -> Result<&str, DevError> {
        self.path.to_str().ok_or_else(|| {
            DevError::Config(format!(
                "workload path is not valid UTF-8: {}",
                self.path.display()
            ))
        })
    }
}

/// Fetch the `[dellingr]` block, with an actionable error when it's absent.
pub(crate) fn config(dev_config: &DevConfig) -> Result<&DellingrConfig, DevError> {
    dev_config.dellingr.as_ref().ok_or_else(|| {
        DevError::Config(
            "no [dellingr] section in brokkr.toml - `brokkr dellingr` needs the \
             harness target and at least one workload:\n\n  [dellingr]\n  \
             example = \"hotpath\"\n\n  [dellingr.workloads.same_obj_read]\n  \
             file = \"examples/fields/same_obj_read.lua\"\n  xxh128 = \"...\""
                .into(),
        )
    })
}

/// Resolve `--lua <name>` against the registry and verify its digest.
///
/// **The path is always resolved against `project_root` - the tree holding
/// `brokkr.toml` - never against a `--commit` worktree.** This is deliberate
/// and is the one place the two roots must not be interchanged. A baseline run
/// exists to vary the VM while holding the workload fixed; the old commit's
/// copy of the same path may differ, and silently benchmarking *that* would
/// attribute a workload change to the VM. The harness still comes from the
/// worktree; only the workload is pinned to the registration.
///
/// The digest check therefore lands on the file actually loaded, which is the
/// only file anyone resolved.
///
/// `instrumented` selects which registered file the run gets: `--hotpath` /
/// `--alloc` resolve the `hotpath_file` / `hotpath_xxh128` pair, everything
/// else resolves `file` / `xxh128`. The pair is *required* for instrumented
/// runs - see [`crate::config::DellingrWorkload`] for why a seconds-scale
/// workload under hotpath instrumentation is a memory cliff, not a slow run.
pub(crate) fn resolve(
    dev_config: &DevConfig,
    project_root: &Path,
    name: &str,
    instrumented: bool,
) -> Result<Resolved, DevError> {
    let cfg = config(dev_config)?;
    let entry = cfg.workload(name)?;

    // A half-registered pair is rejected by `parse_dellingr`, so it cannot
    // reach here; the arm stays as defence rather than a silent fallback to
    // the wrong scale.
    let (file, xxh128, origin_key) = match (&entry.hotpath_file, &entry.hotpath_xxh128) {
        (Some(_), None) | (None, Some(_)) => {
            return Err(DevError::Config(format!(
                "[dellingr.workloads.{name}] registers only half of the hotpath \
                 pair - `hotpath_file` and `hotpath_xxh128` must come together"
            )));
        }
        (Some(file), Some(hash)) if instrumented => (file, hash, "hotpath_file"),
        (None, None) if instrumented => {
            return Err(DevError::Config(format!(
                "workload {name:?} has no hotpath variant - instrumented modes \
                 (--hotpath / --alloc) refuse the seconds-scale `file` because \
                 hotpath's per-call event queue is unbounded and a seconds-scale \
                 run backlogs tens of GB of RAM.\n  add an instrumentation-scale \
                 variant (tens-of-ms per _bench call) to the existing \
                 [dellingr.workloads.{name}] table:\n  \
                 hotpath_file = \"...\"\n  hotpath_xxh128 = \"...\""
            )));
        }
        _ => (&entry.file, &entry.xxh128, "file"),
    };

    let path = project_root.join(file);

    if !path.exists() {
        return Err(DevError::Config(format!(
            "workload {name:?} not found at {}\n  registered as: {}\n  \
             paths are relative to the directory holding brokkr.toml",
            path.display(),
            file.display(),
        )));
    }

    // Names the pin, not just the table: a workload has two of them, and the
    // fix is to re-register the one that actually drifted.
    let origin = format!("[dellingr.workloads.{name}].{origin_key} in brokkr.toml");
    preflight::verify_file_hash(&path, xxh128, project_root, Some(&origin))?;

    Ok(Resolved {
        name: name.to_owned(),
        path,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::fs;

    /// A fresh scratch dir for one test. `test_name` must be unique within
    /// this module; the allocator deletes the path before returning it and
    /// asserts uniqueness rather than trusting it (see `crate::test_scratch`,
    /// which exists because this module's `config_with` broke that rule).
    fn tmpdir(test_name: &str) -> PathBuf {
        crate::test_scratch::scratch("dellingr", test_name)
    }

    /// Build a `DevConfig` carrying just the `[dellingr]` block under test,
    /// with `brokkr.toml` written into the caller's OWN scratch dir.
    ///
    /// Takes the dir rather than allocating one: a helper that allocated its
    /// own would have to name it, and any fixed name is shared by every caller
    /// (see `tmpdir`). Writing into the caller's root also matches how the
    /// config is really found - `project_root` and the directory holding
    /// `brokkr.toml` are the same place in every non-test path.
    fn config_with(dir: &Path, toml_src: &str) -> DevConfig {
        fs::write(dir.join("brokkr.toml"), toml_src).unwrap();
        crate::config::load(dir).unwrap().1
    }

    const SRC: &str = "-- workload\n";

    /// xxh128 of `SRC`, computed the same way `verify_file_hash` does, so the
    /// test pins behaviour rather than a hard-coded digest of its own.
    fn src_digest(root: &Path) -> String {
        let f = root.join("digest_probe.lua");
        fs::write(&f, SRC).unwrap();
        preflight::compute_xxh128(&f).unwrap()
    }

    fn write_workload(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn resolves_and_verifies_a_registered_workload() {
        let root = tmpdir("ok");
        let digest = src_digest(&root);
        write_workload(&root, "examples/fields/read.lua", SRC);
        let cfg = config_with(&root, &format!(
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.read]\nfile = \"examples/fields/read.lua\"\n\
             xxh128 = \"{digest}\"\n"
        ));

        let resolved = resolve(&cfg, &root, "read", false).unwrap();
        assert_eq!(resolved.name, "read");
        assert_eq!(resolved.path, root.join("examples/fields/read.lua"));
    }

    #[test]
    fn instrumented_mode_resolves_the_hotpath_variant() {
        let root = tmpdir("hp_ok");
        let digest = src_digest(&root);
        write_workload(&root, "bench/read.lua", SRC);
        write_workload(&root, "examples/fields/read.lua", SRC);
        let cfg = config_with(&root, &format!(
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.read]\nfile = \"bench/read.lua\"\n\
             xxh128 = \"{digest}\"\n\
             hotpath_file = \"examples/fields/read.lua\"\n\
             hotpath_xxh128 = \"{digest}\"\n"
        ));

        let bench = resolve(&cfg, &root, "read", false).unwrap();
        assert_eq!(bench.path, root.join("bench/read.lua"));
        let instrumented = resolve(&cfg, &root, "read", true).unwrap();
        assert_eq!(instrumented.path, root.join("examples/fields/read.lua"));
    }

    #[test]
    fn instrumented_mode_refuses_a_workload_without_a_hotpath_variant() {
        let root = tmpdir("hp_missing");
        let digest = src_digest(&root);
        write_workload(&root, "bench/read.lua", SRC);
        let cfg = config_with(&root, &format!(
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.read]\nfile = \"bench/read.lua\"\n\
             xxh128 = \"{digest}\"\n"
        ));

        assert!(resolve(&cfg, &root, "read", false).is_ok());
        let err = resolve(&cfg, &root, "read", true).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        // The refusal must say what to register and why the bench file is
        // not an acceptable fallback.
        assert!(msg.contains("hotpath_file"), "{msg}");
        assert!(msg.contains("unbounded"), "{msg}");
    }

    /// A half-registered pair never reaches `resolve` - `parse_dellingr`
    /// rejects it, so a workload nobody runs today is still reported. The
    /// parse-time behaviour is pinned in `config_parts/tests.rs`.
    #[test]
    fn half_registered_hotpath_pair_is_rejected_before_resolution() {
        let dir = tmpdir("hp_half");
        fs::write(
            dir.join("brokkr.toml"),
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.read]\nfile = \"bench/read.lua\"\n\
             xxh128 = \"00\"\nhotpath_file = \"examples/fields/read.lua\"\n",
        )
        .unwrap();

        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("hotpath_file without hotpath_xxh128"), "{msg}");
    }

    #[test]
    fn refuses_a_hotpath_variant_edited_since_registration() {
        let root = tmpdir("hp_drift");
        let digest = src_digest(&root);
        write_workload(&root, "bench/read.lua", SRC);
        write_workload(&root, "examples/fields/read.lua", "-- retuned\n");
        let cfg = config_with(&root, &format!(
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.read]\nfile = \"bench/read.lua\"\n\
             xxh128 = \"{digest}\"\n\
             hotpath_file = \"examples/fields/read.lua\"\n\
             hotpath_xxh128 = \"{digest}\"\n"
        ));

        // The bench side is untouched and still resolves.
        assert!(resolve(&cfg, &root, "read", false).is_ok());
        let err = resolve(&cfg, &root, "read", true).unwrap_err();
        let DevError::Preflight(msgs) = err else {
            panic!("expected a preflight refusal, got {err:?}");
        };
        let joined = msgs.join("\n");
        assert!(joined.contains("hash mismatch"), "{joined}");
        // The refusal must name the field, so the re-registration lands on
        // the hotpath pin and not the bench one.
        assert!(joined.contains("hotpath_file"), "{joined}");
        assert!(joined.contains("[dellingr.workloads.read]"), "{joined}");
    }

    #[test]
    fn refuses_a_workload_edited_since_registration() {
        let root = tmpdir("drift");
        let digest = src_digest(&root);
        // Registered with SRC's digest, but the file on disk says otherwise -
        // exactly the silent-history-poisoning case the pin exists to catch.
        write_workload(&root, "w.lua", "-- retuned\n");
        let cfg = config_with(&root, &format!(
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.w]\nfile = \"w.lua\"\nxxh128 = \"{digest}\"\n"
        ));

        let err = resolve(&cfg, &root, "w", false).unwrap_err();
        let DevError::Preflight(msgs) = err else {
            panic!("expected a preflight refusal, got {err:?}");
        };
        let joined = msgs.join("\n");
        assert!(joined.contains("hash mismatch"), "{joined}");
        // The refusal must name the registration, not just the path, so the
        // fix (re-register deliberately) is obvious.
        assert!(joined.contains("[dellingr.workloads.w]"), "{joined}");
    }

    #[test]
    fn unknown_workload_lists_the_registered_names() {
        let root = tmpdir("unknown");
        let cfg = config_with(
            &root,
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.alpha]\nfile = \"a.lua\"\nxxh128 = \"00\"\n\n\
             [dellingr.workloads.beta]\nfile = \"b.lua\"\nxxh128 = \"11\"\n",
        );

        let err = resolve(&cfg, &root, "gamma", false).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("alpha, beta"), "{msg}");
    }

    #[test]
    fn missing_file_reports_the_registration_not_just_the_path() {
        let root = tmpdir("missing");
        let cfg = config_with(
            &root,
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.w]\nfile = \"nope/w.lua\"\nxxh128 = \"00\"\n",
        );

        let err = resolve(&cfg, &root, "w", false).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("nope/w.lua"), "{msg}");
    }

    #[test]
    fn absent_dellingr_block_explains_what_to_write() {
        let root = tmpdir("no_block");
        let cfg = config_with(&root, "project = \"dellingr\"\n");
        let err = config(&cfg).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("[dellingr]"), "{msg}");
        assert!(msg.contains("example"), "{msg}");
    }
}
