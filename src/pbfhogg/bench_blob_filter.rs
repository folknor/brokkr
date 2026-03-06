//! Benchmark: compare indexed (with indexdata) vs raw (without) PBF performance.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

const COMMANDS: &[&str] = &["cat-way", "cat-relation", "inspect-tags-way", "inspect-nodes"];

fn command_args(name: &str, pbf: &str, force: bool) -> Vec<String> {
    let mut args = match name {
        "cat-way" => vec!["cat".into(), pbf.into(), "--type".into(), "way".into(), "-o".into(), "/dev/null".into()],
        "cat-relation" => vec!["cat".into(), pbf.into(), "--type".into(), "relation".into(), "-o".into(), "/dev/null".into()],
        "inspect-tags-way" => vec!["inspect".into(), "tags".into(), pbf.into(), "--type".into(), "way".into(), "--min-count".into(), "999999999".into()],
        "inspect-nodes" => vec!["inspect".into(), "--nodes".into(), pbf.into()],
        _ => unreachable!("unknown command: {name}"),
    };
    if force {
        args.push("--force".into());
    }
    args
}

pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_indexed: &Path,
    pbf_raw: &Path,
    file_mb: f64,
    runs: usize,
    project_root: &Path,
) -> Result<(), DevError> {
    let (indexed_basename, indexed_str) = super::path_strs(pbf_indexed)?;
    let (raw_basename, raw_str) = super::path_strs(pbf_raw)?;

    let variants: &[(&str, &str, &str, bool)] = &[
        ("indexed", indexed_str, &indexed_basename, false),
        ("raw", raw_str, &raw_basename, true),
    ];

    for &cmd in COMMANDS {
        for &(label_suffix, pbf_str, basename, force) in variants {
            let variant = format!("{cmd}+{label_suffix}");
            output::bench_msg(&format!("variant: {variant}"));

            let args = command_args(cmd, pbf_str, force);
            let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let config = BenchConfig {
                command: "bench blob-filter".into(),
                variant: Some(variant),
                input_file: Some(basename.to_owned()),
                input_mb: Some(file_mb),
                cargo_features: None,
                cargo_profile: "release".into(),
                runs,
                cli_args: Some(crate::harness::format_cli_args(&binary.display().to_string(), &args_refs)),
                metadata: vec![],
            };

            harness.run_external(&config, binary, &args_refs, project_root)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::command_args;

    #[test]
    fn raw_variant_appends_force() {
        let args = command_args("inspect-nodes", "in.osm.pbf", true);
        assert_eq!(args, vec!["inspect", "--nodes", "in.osm.pbf", "--force"]);
    }

    #[test]
    fn indexed_variant_has_no_force() {
        let args = command_args("inspect-nodes", "in.osm.pbf", false);
        assert_eq!(args, vec!["inspect", "--nodes", "in.osm.pbf"]);
    }
}
