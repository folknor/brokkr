//! Benchmark: compare indexed (with indexdata) vs raw (without) PBF performance.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};

const COMMANDS: &[&str] = &[
    "cat-way",
    "cat-relation",
    "inspect-tags-way",
    "inspect-nodes",
];

fn command_args(name: &str, pbf: &str, output: &str, force: bool) -> Vec<String> {
    let mut args = match name {
        "cat-way" => vec![
            "cat".into(),
            pbf.into(),
            "--type".into(),
            "way".into(),
            "-o".into(),
            output.into(),
        ],
        "cat-relation" => vec![
            "cat".into(),
            pbf.into(),
            "--type".into(),
            "relation".into(),
            "-o".into(),
            output.into(),
        ],
        "inspect-tags-way" => vec![
            "inspect".into(),
            "tags".into(),
            pbf.into(),
            "--type".into(),
            "way".into(),
            "--min-count".into(),
            "999999999".into(),
        ],
        "inspect-nodes" => vec!["inspect".into(), "--nodes".into(), pbf.into()],
        _ => unreachable!("unknown command: {name}"),
    };
    if force {
        args.push("--force".into());
    }
    args
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_indexed: &Path,
    pbf_raw: &Path,
    file_mb: f64,
    runs: usize,
    project_root: &Path,
    scratch_dir: &Path,
) -> Result<(), DevError> {
    std::fs::create_dir_all(scratch_dir)?;
    let output_path = scratch_dir.join("bench-blob-filter-output.osm.pbf");
    let output_str = output_path.display().to_string();

    let (indexed_basename, indexed_str) = super::path_strs(pbf_indexed)?;
    let (raw_basename, raw_str) = super::path_strs(pbf_raw)?;

    let variants: &[(&str, &str, &str, bool)] = &[
        ("indexed", indexed_str, &indexed_basename, false),
        ("raw", raw_str, &raw_basename, true),
    ];

    let variant_names: Vec<String> = COMMANDS
        .iter()
        .flat_map(|cmd| variants.iter().map(move |(label, ..)| format!("{cmd}+{label}")))
        .collect();
    let variant_refs: Vec<&str> = variant_names.iter().map(String::as_str).collect();

    let result = crate::harness::run_variants("variant", &variant_refs, |variant| {
        let (cmd, label_suffix) = variant
            .split_once('+')
            .ok_or_else(|| DevError::Config(format!("invalid variant '{variant}'")))?;
        let &(_, pbf_str, basename, force) = variants
            .iter()
            .find(|&&(l, ..)| l == label_suffix)
            .ok_or_else(|| DevError::Config(format!("unknown variant label '{label_suffix}'")))?;

        let args = command_args(cmd, pbf_str, &output_str, force);
        let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let config = BenchConfig {
            command: "blob-filter".into(),
            // Sub-command + indexed/raw discriminator is visible in
            // cli_args (different argv per row — e.g. `cat --type way`
            // with `--force` only on raw) and in input_file (different
            // PBF per row). Measurement mode and brokkr_args come from
            // the harness.
            mode: None,
            input_file: Some(basename.to_owned()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
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

#[cfg(test)]
mod tests {
    use super::command_args;

    #[test]
    fn raw_variant_appends_force() {
        let args = command_args("inspect-nodes", "in.osm.pbf", "out.osm.pbf", true);
        assert_eq!(args, vec!["inspect", "--nodes", "in.osm.pbf", "--force"]);
    }

    #[test]
    fn indexed_variant_has_no_force() {
        let args = command_args("inspect-nodes", "in.osm.pbf", "out.osm.pbf", false);
        assert_eq!(args, vec!["inspect", "--nodes", "in.osm.pbf"]);
    }
}
