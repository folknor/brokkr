//! Self-contained builds for ratatoskr's orchestration commands.
//!
//! Reads `[ratatoskr.harness]` from `brokkr.toml` (package, optional
//! binary override, optional features, optional debug) and invokes
//! `cargo build` directly. Decoupled from `[[check]]`: the orchestration
//! build no longer participates in `brokkr check`'s sweep matrix, so
//! everyday checks don't compile orchestration-only feature graphs.
//!
//! The actual cargo invocation goes through [`crate::build::cargo_build`],
//! which already knows how to pass `--message-format=json` and pick the
//! produced executable out of cargo's stdout.

use std::path::{Path, PathBuf};

use crate::build::{self, BuildConfig};
use crate::config::HarnessConfig;
use crate::error::DevError;
use crate::output;

/// Result of a successful harness build.
#[derive(Debug)]
pub struct HarnessBuild {
    /// Path to the binary the harness will spawn. Always under
    /// `<target>/<profile>/`.
    pub binary: PathBuf,
    /// Directory containing every binary the package produced. The
    /// harness exports this as `BROKKR_TEST_BIN_DIR` so spawned-binary
    /// tests can find sibling helpers (`parent_death_helper`, etc.)
    /// without re-deriving the cargo target dir.
    pub bin_dir: PathBuf,
    /// Human-readable features summary (e.g. `default`, `feat-a,feat-b`).
    /// Surfaced in log lines and `run.toml`.
    pub features_label: String,
}

/// Build the harness per `[ratatoskr.harness]`. `debug = true` produces
/// a dev-profile build at `<target>/debug/`; the default is release for
/// parity with `brokkr test`'s default.
pub fn build_for_harness(
    project_root: &Path,
    harness_cfg: &HarnessConfig,
    debug: bool,
    on_spawn: Option<&dyn Fn(u32)>,
    on_done: Option<&dyn Fn()>,
    isolate_pg: bool,
) -> Result<HarnessBuild, DevError> {
    let profile_name = if debug { "dev" } else { "release" };
    let features_label = feature_summary(&harness_cfg.features);

    output::harness_msg(&format!(
        "building package '{}' (features: {features_label}, profile: {profile_name})",
        harness_cfg.package,
    ));

    let cfg = BuildConfig::for_harness(harness_cfg, debug);
    let result = build::cargo_build_observed(&cfg, project_root, on_spawn, isolate_pg);
    if let Some(cb) = on_done {
        cb();
    }
    let binary = result?;

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
        features_label,
    })
}

/// Render a features list for the `[ratatoskr] building ...` log line.
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
