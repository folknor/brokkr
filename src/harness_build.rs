//! Cargo builds for command-owned harness binaries.
//!
//! Shared by every orchestration command that builds a harness from a
//! `[*.harness]` block: ratatoskr's `sync` and `service`, piners'
//! `corpus`, and pbfhogg's `library` (through `pbfhogg/binary.rs`, the one
//! place pbfhogg code may build). Decoupled from `[[check]]`: an orchestration
//! build does not participate in `brokkr check`'s sweep matrix, so everyday
//! checks don't compile orchestration-only feature graphs.
//!
//! The spec is always the current tree's config, also when `project_root` is
//! a `--commit` worktree: package, binary, features and profile name go to
//! cargo as written, and the built tree and its effective cargo
//! configuration give them meaning. A
//! commit that lacks one of those names fails its build with cargo's error;
//! the `--commit` sections of `docs/commands/sync.md` and
//! `docs/commands/corpus.md` state this. Reading the spec at the commit would
//! need the project's definitions versioned with its code, which today's
//! design does not do.
//!
//! The caller resolves the profile once, before building; what it records
//! (run rows, `run.toml`) is the profile this build returns, the one cargo
//! actually built under.

use std::path::{Path, PathBuf};

use crate::build::{self, BuildConfig, RustProfile};
use crate::config::HarnessConfig;
use crate::error::DevError;

/// Result of a successful harness build.
#[derive(Debug)]
pub struct HarnessBuild {
    /// Path to the binary the harness will spawn, under
    /// `<target>/<profile dir>/`.
    pub binary: PathBuf,
    /// Directory containing every binary the package produced. Exported to
    /// a ratatoskr harness as `BROKKR_TEST_BIN_DIR` (the piners corpus
    /// harness gets no brokkr variables) so spawned-binary tests can find
    /// sibling helpers without re-deriving the cargo target dir. The parent
    /// of the executable cargo reported, so a custom profile's directory
    /// needs no mapping of its own.
    pub bin_dir: PathBuf,
    /// Human-readable features summary (e.g. `default`, `feat-a,feat-b`).
    pub features_label: String,
    /// The profile cargo built under.
    pub profile: RustProfile,
    /// The build's identity as a results row records it, provenance included.
    pub identity: crate::measure::BuildIdentity,
}

/// Build the harness `harness_cfg` describes under `profile`.
pub fn build_for_harness(
    project_root: &Path,
    harness_cfg: &HarnessConfig,
    profile: RustProfile,
    on_spawn: Option<&dyn Fn(u32)>,
    on_done: Option<&dyn Fn()>,
    isolation: crate::shutdown::ChildIsolation<'_>,
) -> Result<HarnessBuild, DevError> {
    let features_label = feature_summary(&harness_cfg.features);
    // No announcement of its own: the `[build]` lines cargo_build prints
    // already name the package, features and profile.
    let config = BuildConfig::for_harness_with_profile(harness_cfg, profile);
    let result = build::cargo_build_observed(&config, project_root, on_spawn, isolation);
    if let Some(done) = on_done {
        done();
    }
    let artifact = result?;
    let bin_dir = artifact
        .path
        .parent()
        .ok_or_else(|| {
            DevError::Build(format!(
                "binary path {} has no parent directory",
                artifact.path.display()
            ))
        })?
        .to_path_buf();

    let identity = crate::measure::BuildIdentity::of(&artifact);
    Ok(HarnessBuild {
        binary: artifact.path,
        bin_dir,
        features_label,
        profile: artifact.profile,
        identity,
    })
}

/// Render a features list for the build log line.
fn feature_summary(features: &[String]) -> String {
    if features.is_empty() {
        "default".to_owned()
    } else {
        features.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_summary_default_when_empty() {
        assert_eq!(feature_summary(&[]), "default");
    }

    #[test]
    fn feature_summary_joins_with_commas() {
        let f = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(feature_summary(&f), "a,b");
    }
}
