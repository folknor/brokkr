//! Shared command argument resolution.
use crate::{cli, config, error::DevError, measure};

pub(crate) fn profile_override(debug: bool, release: bool) -> Option<bool> {
    if debug {
        Some(true)
    } else if release {
        Some(false)
    } else {
        None
    }
}

/// The CLI profile choice of a command that takes `--profile` beside
/// `--debug`/`--release` (all three clap-exclusive): the two flags are
/// spellings of `--profile dev` and `--profile release`.
pub(crate) fn profile_choice(
    debug: bool,
    release: bool,
    profile: Option<&crate::build::RustProfile>,
) -> Option<crate::build::RustProfile> {
    profile
        .cloned()
        .or_else(|| profile_override(debug, release).map(crate::build::RustProfile::for_debug))
}

pub(crate) fn parse_env_overrides(pairs: &[String]) -> Result<Vec<(String, String)>, DevError> {
    pairs
        .iter()
        .map(|p| {
            let (k, v) = p
                .split_once('=')
                .ok_or_else(|| DevError::Usage(format!("--env expects KEY=VALUE, got '{p}'")))?;
            Ok((k.to_owned(), v.to_owned()))
        })
        .collect()
}

pub(crate) fn resolve_features(
    dev_config: &config::DevConfig,
    cli_features: &[String],
) -> Vec<String> {
    let host_features = config::host_features(dev_config);
    if host_features.is_empty() {
        return cli_features.to_vec();
    }
    let mut merged = host_features;
    for f in cli_features {
        if !merged.iter().any(|existing| existing == f) {
            merged.push(f.clone());
        }
    }
    merged
}

pub(crate) fn resolve_mode(mode: &cli::ModeArgs) -> measure::MeasureMode {
    if let Some(runs) = mode.bench {
        measure::MeasureMode::Bench { runs }
    } else if let Some(runs) = mode.hotpath {
        measure::MeasureMode::Hotpath { runs }
    } else if let Some(runs) = mode.alloc {
        measure::MeasureMode::Alloc { runs }
    } else {
        measure::MeasureMode::Run
    }
}
