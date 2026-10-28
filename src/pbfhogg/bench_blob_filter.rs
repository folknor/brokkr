//! Benchmark: compare indexed (with indexdata) vs raw (without) PBF performance.

use std::path::Path;

use crate::clean::layout::PbfhoggLayout;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::pbfhogg::argv::PbfArgv;
use crate::pbfhogg::binary::PbfhoggBinary;

const COMMANDS: &[&str] = &[
    "cat-way",
    "cat-relation",
    "inspect-tags-way",
    "inspect-nodes",
];

/// The argv for one of [`COMMANDS`], the cat ones writing the shared output
/// file. An unknown name is an error rather than `unreachable!()`: the name
/// is parsed back out of a variant string, and nothing at this signature
/// proves it came from [`COMMANDS`].
fn command_args(name: &str, pbf: &str, output: &str, force: bool) -> Result<PbfArgv, DevError> {
    let argv = match name {
        "cat-way" => PbfArgv::new("cat")
            .input(pbf)?
            .arg("--type")
            .arg("way")
            .output(output)?,
        "cat-relation" => PbfArgv::new("cat")
            .input(pbf)?
            .arg("--type")
            .arg("relation")
            .output(output)?,
        "inspect-tags-way" => PbfArgv::new("inspect")
            .arg("tags")
            .input(pbf)?
            .arg("--type")
            .arg("way")
            .arg("--min-count")
            .arg(super::commands::INSPECT_ALL_TAGS_MIN_COUNT),
        "inspect-nodes" => PbfArgv::new("inspect").arg("--nodes").input(pbf)?,
        _ => {
            return Err(DevError::Config(format!(
                "unknown blob-filter command {name:?} (expected one of: {})",
                COMMANDS.join(", ")
            )));
        }
    };
    Ok(argv.arg_if(force, "--force"))
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &PbfhoggBinary,
    pbf_indexed: &Path,
    pbf_raw: &Path,
    file_mb: f64,
    runs: usize,
    project_root: &Path,
    scratch_dir: &Path,
) -> Result<(), DevError> {
    std::fs::create_dir_all(scratch_dir)?;
    let output_path =
        PbfhoggLayout::fixed_scratch_file(scratch_dir, "bench-blob-filter-output.osm.pbf");
    let output_str = output_path.display().to_string();

    let (indexed_basename, indexed_str) = crate::utf8_path::path_strings(pbf_indexed)?;
    let (raw_basename, raw_str) = crate::utf8_path::path_strings(pbf_raw)?;

    let variants: &[(&str, &str, &str, bool)] = &[
        ("indexed", indexed_str, &indexed_basename, false),
        ("raw", raw_str, &raw_basename, true),
    ];

    let variant_names: Vec<String> = COMMANDS
        .iter()
        .flat_map(|cmd| {
            variants
                .iter()
                .map(move |(label, ..)| format!("{cmd}+{label}"))
        })
        .collect();
    let variant_refs: Vec<&str> = variant_names.iter().map(String::as_str).collect();

    let result = crate::bench_variants::run_variants("variant", &variant_refs, |variant| {
        let (cmd, label_suffix) = variant
            .split_once('+')
            .ok_or_else(|| DevError::Config(format!("invalid variant '{variant}'")))?;
        let &(_, pbf_str, basename, force) = variants
            .iter()
            .find(|&&(l, ..)| l == label_suffix)
            .ok_or_else(|| DevError::Config(format!("unknown variant label '{label_suffix}'")))?;

        let invocation = binary.invocation(command_args(cmd, pbf_str, &output_str, force)?)?;

        let config = BenchConfig {
            command: "blob-filter".into(),
            // Sub-command + indexed/raw discriminator is visible in
            // cli_args (different argv per row - e.g. `cat --type way`
            // with `--force` only on raw) and in input_file (different
            // PBF per row). Measurement mode and brokkr_args come from
            // the harness.
            input_file: Some(basename.to_owned()),
            input_mb: Some(file_mb),
            cargo_features: None,
            external_build: None,
            runs,
            cli_args: None,
            metadata: vec![],
            arm_qualifier: None,
        };

        harness
            .run_external(&config, &invocation, project_root)
            .map(|_| ())
    });

    crate::reservations::remove_file(&output_path).ok();

    result
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::{COMMANDS, command_args};

    #[test]
    fn raw_variant_appends_force() {
        let argv = command_args("inspect-nodes", "in.osm.pbf", "out.osm.pbf", true).unwrap();
        assert_eq!(argv.args(), ["inspect", "--nodes", "in.osm.pbf", "--force"]);
    }

    #[test]
    fn indexed_variant_has_no_force() {
        let argv = command_args("inspect-nodes", "in.osm.pbf", "out.osm.pbf", false).unwrap();
        assert_eq!(argv.args(), ["inspect", "--nodes", "in.osm.pbf"]);
    }

    #[test]
    fn exactly_the_commands_naming_the_output_declare_it_written() {
        // The shared output file is reused by every variant and iteration, so
        // a command that names it must let pbfhogg replace it.
        for name in COMMANDS {
            let argv = command_args(name, "in.osm.pbf", "out.osm.pbf", false).unwrap();
            let names_output = argv.args().iter().any(|a| a == "out.osm.pbf");
            assert_eq!(argv.writes_output(), names_output, "{name}");
        }
    }

    #[test]
    fn an_unknown_command_is_an_error_not_a_panic() {
        assert!(command_args("cat-node", "in.osm.pbf", "out.osm.pbf", false).is_err());
    }
}
