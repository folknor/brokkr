//! Sweep-aware builds for the service-test harness.
//!
//! `service-test` consults `[ratatoskr.harness]` in `brokkr.toml`,
//! finds the matching `[[check]]` entry, builds every `build_packages`
//! member with the sweep's feature flags, and returns the path to the
//! primary binary. Same feature contract `brokkr check` enforces, so
//! the harness can never run against a feature combination the rest of
//! the toolchain has not validated.
//!
//! The actual cargo invocation goes through [`crate::build::cargo_build`],
//! which already knows how to pass `--message-format=json` and pick
//! the produced executable out of cargo's stdout. Everything here is
//! just orchestration: pick the right sweep, iterate `build_packages`,
//! capture the binary that matches `[ratatoskr.harness].binary`.

use std::path::{Path, PathBuf};

use crate::build::{self, BuildConfig};
use crate::config::{CheckEntry, HarnessConfig};
use crate::error::DevError;
use crate::output;

/// Result of a successful harness build.
#[derive(Debug)]
pub struct HarnessBuild {
    /// Path to the binary the harness will spawn (per
    /// `[ratatoskr.harness].binary`). Always under `<target>/<profile>/`.
    pub binary: PathBuf,
    /// Directory containing every binary the sweep produced. The
    /// harness exports this as `BROKKR_TEST_BIN_DIR` so spawned-binary
    /// tests can find sibling helpers (`parent_death_helper`, etc.)
    /// without re-deriving the cargo target dir.
    pub bin_dir: PathBuf,
    /// `[[check]].name` of the sweep used. Surfaced in error messages.
    pub sweep_label: String,
}

/// Build the harness sweep. `debug = true` produces a dev-profile
/// build at `<target>/debug/`; the default is release for parity with
/// `brokkr test`'s default and to match what users will run in
/// production.
///
/// Returns the binary path, the bin dir for sibling helpers, and the
/// sweep label. Build failures surface as [`DevError::Build`] with
/// cargo's filtered diagnostic output already printed by
/// [`crate::build::cargo_build`].
pub fn build_for_harness(
    project_root: &Path,
    check_entries: &[CheckEntry],
    harness_cfg: &HarnessConfig,
    debug: bool,
    on_spawn: Option<&dyn Fn(u32)>,
    on_done: Option<&dyn Fn()>,
) -> Result<HarnessBuild, DevError> {
    // The cross-check in src/config.rs already verified that the named
    // sweep exists and that `binary` is in its `build_packages`. The
    // lookup here is defensive in case a code path ever reaches us
    // bypassing that validation.
    let entry = check_entries
        .iter()
        .find(|e| e.name == harness_cfg.sweep)
        .ok_or_else(|| {
            DevError::Config(format!(
                "[ratatoskr.harness].sweep '{}' has no matching `[[check]]` entry",
                harness_cfg.sweep
            ))
        })?;

    let profile_name = if debug { "dev" } else { "release" };

    output::ratatoskr_msg(&format!(
        "building sweep '{}' (features: {}, profile: {profile_name})",
        entry.name,
        feature_summary(entry),
    ));

    let mut binary_path: Option<PathBuf> = None;
    for pkg in &entry.build_packages {
        let cfg = BuildConfig {
            package: Some(pkg.clone()),
            bin: None,
            example: None,
            features: entry.features.clone(),
            default_features: !entry.no_default_features,
            profile: profile_name,
        };
        // Fire on_done after EACH cargo invocation - both success and
        // failure - so the orchestrator's `child_pid` slot doesn't keep
        // pointing at a now-reaped cargo PID between package builds (or
        // after a build error). Without this, a multi-package sweep
        // leaves a stale PID for `brokkr kill --hard` to potentially
        // SIGKILL after the kernel recycles it.
        let result = build::cargo_build_observed(&cfg, project_root, on_spawn);
        if let Some(cb) = on_done {
            cb();
        }
        let path = result?;
        if pkg == &harness_cfg.binary {
            binary_path = Some(path);
        }
    }

    let binary = binary_path.ok_or_else(|| {
        DevError::Build(format!(
            "[ratatoskr.harness].binary '{}' was not produced by sweep '{}' \
             (build_packages: {:?})",
            harness_cfg.binary, entry.name, entry.build_packages
        ))
    })?;
    let bin_dir = binary
        .parent()
        .ok_or_else(|| {
            DevError::Build(format!(
                "binary path {} has no parent directory",
                binary.display()
            ))
        })?
        .to_path_buf();

    Ok(HarnessBuild {
        binary,
        bin_dir,
        sweep_label: entry.name.clone(),
    })
}

/// Render an entry's feature flags for the `[ratatoskr] building ...`
/// log line. Distinguishes "default" (no flags), "no-default-features"
/// alone, and an explicit feature list.
fn feature_summary(entry: &CheckEntry) -> String {
    if entry.features.is_empty() {
        if entry.no_default_features {
            "no-default-features".to_owned()
        } else {
            "default".to_owned()
        }
    } else if entry.no_default_features {
        format!("no-default-features + {}", entry.features.join(","))
    } else {
        entry.features.join(",")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn entry(name: &str, features: &[&str], nodef: bool, packages: &[&str]) -> CheckEntry {
        CheckEntry {
            name: name.into(),
            features: features.iter().map(|s| (*s).to_owned()).collect(),
            no_default_features: nodef,
            build_packages: packages.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn feature_summary_default_when_empty() {
        let e = entry("a", &[], false, &[]);
        assert_eq!(feature_summary(&e), "default");
    }

    #[test]
    fn feature_summary_no_default_alone() {
        let e = entry("a", &[], true, &[]);
        assert_eq!(feature_summary(&e), "no-default-features");
    }

    #[test]
    fn feature_summary_with_features() {
        let e = entry("a", &["test-helpers", "harness"], false, &[]);
        assert_eq!(feature_summary(&e), "test-helpers,harness");
    }

    #[test]
    fn feature_summary_with_features_and_no_default() {
        let e = entry("a", &["commands"], true, &[]);
        assert_eq!(
            feature_summary(&e),
            "no-default-features + commands"
        );
    }

    #[test]
    fn build_errors_when_sweep_not_found() {
        // Pure resolver test - no cargo invocation.
        let checks = vec![entry("other", &[], false, &["app"])];
        let harness = HarnessConfig {
            sweep: "missing".into(),
            binary: "app".into(),
        };
        let err = build_for_harness(Path::new("/"), &checks, &harness, false, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("'missing'"), "got: {err}");
    }
}
