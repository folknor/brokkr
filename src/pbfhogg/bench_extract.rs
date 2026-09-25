//! Benchmark: extract strategies (simple/complete/smart) with bbox.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};

pub const ALL_STRATEGIES: &[&str] = &["simple", "complete", "smart"];

/// The bbox is spelled `-b=<bbox>`, identically to `PbfhoggCommand::Extract`'s
/// argv in `commands.rs`: the two paths record the same `extract` rows, and a
/// differing spelling split their `cli_args` for the same work. The `=` form
/// is the safe one, too - a bbox west of Greenwich starts with `-`, which a
/// separate token would present to pbfhogg's parser as a flag.
///
/// An unknown name is an error rather than `unreachable!()`: the names arrive
/// as strings from the caller, and nothing at this signature proves they came
/// from [`ALL_STRATEGIES`].
fn strategy_args(name: &str, pbf: &str, bbox: &str, output: &str) -> Result<Vec<String>, DevError> {
    let bbox_arg = format!("-b={bbox}");
    Ok(match name {
        "simple" => vec![
            "extract".into(),
            pbf.into(),
            "--simple".into(),
            bbox_arg,
            "-o".into(),
            output.into(),
        ],
        "complete" => vec![
            "extract".into(),
            pbf.into(),
            bbox_arg,
            "-o".into(),
            output.into(),
        ],
        "smart" => vec![
            "extract".into(),
            pbf.into(),
            "--smart".into(),
            bbox_arg,
            "-o".into(),
            output.into(),
        ],
        _ => {
            return Err(DevError::Config(format!(
                "unknown extract strategy {name:?} (expected one of: {})",
                ALL_STRATEGIES.join(", ")
            )));
        }
    })
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    bbox: &str,
    strategies: &[&str],
    project_root: &Path,
    scratch_dir: &Path,
) -> Result<(), DevError> {
    std::fs::create_dir_all(scratch_dir)?;
    let output_path = scratch_dir.join("bench-extract-output.osm.pbf");
    let output_str = output_path.display().to_string();

    let (basename, pbf_str) = super::path_strs(pbf_path)?;

    let result = crate::harness::run_variants("strategy", strategies, |name| {
        let args = strategy_args(name, pbf_str, bbox, &output_str)?;
        let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let config = BenchConfig {
            command: "extract".into(),
            // Strategy is in cli_args (--simple/--smart/none=complete).
            // Bbox is in cli_args (-b). Measurement mode and brokkr_args
            // come from the harness.
            mode: None,
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: crate::build::CargoProfile::Release,
            runs,
            cli_args: Some(crate::harness::format_cli_args(
                &binary.display().to_string(),
                &args_refs,
            )),
            brokkr_args: None,
            metadata: vec![],
        };

        harness.run_external(&config, binary, &args_refs, project_root).map(|_| ())
    });

    std::fs::remove_file(&output_path).ok();

    result
}
