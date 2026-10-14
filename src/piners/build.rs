//! Cargo builds for piners' command-owned validator harnesses.
//!
//! Keep piners on the shared build primitives directly. This small adapter
//! resolves the shared `[*.harness]` shape without reaching into another
//! project's command module.

use std::path::{Path, PathBuf};

use crate::build::{BuildConfig, CargoProfile};
use crate::config::HarnessConfig;
use crate::error::DevError;

/// The executable selected by Cargo and the details piners reports.
#[derive(Debug)]
pub(crate) struct HarnessBuild {
    pub binary: PathBuf,
    pub bin_dir: PathBuf,
    pub features_label: String,
}

/// Build the configured validator harness using brokkr's shared Cargo runner.
pub(crate) fn build_for_harness(
    project_root: &Path,
    harness_cfg: &HarnessConfig,
    profile: CargoProfile,
    on_spawn: Option<&dyn Fn(u32)>,
    on_done: Option<&dyn Fn()>,
    isolation: crate::shutdown::ChildIsolation<'_>,
) -> Result<HarnessBuild, DevError> {
    let features_label = if harness_cfg.features.is_empty() {
        "default".to_owned()
    } else {
        harness_cfg.features.join(",")
    };
    let config = BuildConfig::for_harness_with_profile(harness_cfg, profile);
    let result = crate::build::cargo_build_observed(&config, project_root, on_spawn, isolation);
    if let Some(done) = on_done {
        done();
    }
    let binary = result?.path;
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
