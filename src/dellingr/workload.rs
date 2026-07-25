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
pub(crate) fn resolve(
    dev_config: &DevConfig,
    project_root: &Path,
    name: &str,
) -> Result<Resolved, DevError> {
    let cfg = config(dev_config)?;
    let entry = cfg.workload(name)?;
    let path = project_root.join(&entry.file);

    if !path.exists() {
        return Err(DevError::Config(format!(
            "workload {name:?} not found at {}\n  registered as: {}\n  \
             paths are relative to the directory holding brokkr.toml",
            path.display(),
            entry.file.display(),
        )));
    }

    let origin = format!("[dellingr.workloads.{name}] in brokkr.toml");
    preflight::verify_file_hash(&path, &entry.xxh128, project_root, Some(&origin))?;

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

    /// A fresh scratch dir under the crate's gitignored `target/`
    /// (project rules forbid `/tmp`).
    fn tmpdir(test_name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/dellingr")
            .join(test_name);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a `DevConfig` carrying just the `[dellingr]` block under test.
    fn config_with(toml_src: &str) -> DevConfig {
        let dir = tmpdir("cfg");
        fs::write(dir.join("brokkr.toml"), toml_src).unwrap();
        crate::config::load(&dir).unwrap().1
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
        let cfg = config_with(&format!(
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.read]\nfile = \"examples/fields/read.lua\"\n\
             xxh128 = \"{digest}\"\n"
        ));

        let resolved = resolve(&cfg, &root, "read").unwrap();
        assert_eq!(resolved.name, "read");
        assert_eq!(resolved.path, root.join("examples/fields/read.lua"));
    }

    #[test]
    fn refuses_a_workload_edited_since_registration() {
        let root = tmpdir("drift");
        let digest = src_digest(&root);
        // Registered with SRC's digest, but the file on disk says otherwise -
        // exactly the silent-history-poisoning case the pin exists to catch.
        write_workload(&root, "w.lua", "-- retuned\n");
        let cfg = config_with(&format!(
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.w]\nfile = \"w.lua\"\nxxh128 = \"{digest}\"\n"
        ));

        let err = resolve(&cfg, &root, "w").unwrap_err();
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
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.alpha]\nfile = \"a.lua\"\nxxh128 = \"00\"\n\n\
             [dellingr.workloads.beta]\nfile = \"b.lua\"\nxxh128 = \"11\"\n",
        );

        let err = resolve(&cfg, &root, "gamma").unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("alpha, beta"), "{msg}");
    }

    #[test]
    fn missing_file_reports_the_registration_not_just_the_path() {
        let root = tmpdir("missing");
        let cfg = config_with(
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.w]\nfile = \"nope/w.lua\"\nxxh128 = \"00\"\n",
        );

        let err = resolve(&cfg, &root, "w").unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("nope/w.lua"), "{msg}");
    }

    #[test]
    fn absent_dellingr_block_explains_what_to_write() {
        let cfg = config_with("project = \"dellingr\"\n");
        let err = config(&cfg).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("[dellingr]"), "{msg}");
        assert!(msg.contains("example"), "{msg}");
    }
}
