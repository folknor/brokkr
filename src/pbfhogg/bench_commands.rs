//! Benchmark: run pbfhogg CLI commands and measure wall-clock time.
//!
//! Routes every preset through `PbfhoggCommand::build_args` so argv
//! construction has a single source of truth. `preset_to_command` is the
//! only place suite-preset strings meet the typed command enum.

use std::path::{Path, PathBuf};

use crate::dispatch;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::measure::{CommandContext, CommandParams};
use crate::output;
use crate::pbfhogg::commands::{
    CatTypeFilter, DiffFormat, ExtractStrategy, InputKind, OutputKind, PbfhoggCommand,
};

pub const ALL_COMMANDS: &[&str] = &[
    "inspect",
    "inspect-nodes",
    "inspect-tags",
    "inspect-tags-way",
    "check-refs",
    "check-ids",
    "sort",
    "cat",
    "cat-way",
    "cat-relation",
    "cat-dedupe",
    "tags-filter-way",
    "tags-filter-amenity",
    "tags-filter-twopass",
    "tags-filter-osc",
    "getid",
    "getparents",
    "getid-invert",
    "renumber",
    "merge-changes",
    "apply-changes",
    "add-locations-to-ways",
    "extract-simple",
    "extract-complete",
    "extract-smart",
    "time-filter",
    "diff",
    "diff-osc",
];

/// Hardcoded bbox used by the extract-{simple,complete,smart} suite presets.
/// Kept for backwards compatibility with historical suite rows; the
/// standalone `brokkr extract` command uses the dataset's configured bbox.
const SUITE_EXTRACT_BBOX: &str = "12.4,55.6,12.7,55.8";

/// Map a suite preset string to a fully-specified `PbfhoggCommand` variant.
///
/// This is the single place where preset names are interpreted — once we
/// have a `PbfhoggCommand`, argv, metadata, input/output kind, and result
/// labels all come from the enum itself.
fn preset_to_command(name: &str) -> Result<PbfhoggCommand, DevError> {
    Ok(match name {
        "inspect" => PbfhoggCommand::Inspect {
            nodes: false,
            tags: false,
            type_filter: None,
        },
        "inspect-nodes" => PbfhoggCommand::Inspect {
            nodes: true,
            tags: false,
            type_filter: None,
        },
        "inspect-tags" => PbfhoggCommand::Inspect {
            nodes: false,
            tags: true,
            type_filter: None,
        },
        "inspect-tags-way" => PbfhoggCommand::Inspect {
            nodes: false,
            tags: true,
            type_filter: Some("way".into()),
        },
        "check-refs" => PbfhoggCommand::CheckRefs,
        "check-ids" => PbfhoggCommand::CheckIds,
        "sort" => PbfhoggCommand::Sort,
        "cat" => PbfhoggCommand::Cat {
            type_filter: None,
            dedupe: false,
            clean: false,
        },
        "cat-way" => PbfhoggCommand::Cat {
            type_filter: Some(CatTypeFilter::Way),
            dedupe: false,
            clean: false,
        },
        "cat-relation" => PbfhoggCommand::Cat {
            type_filter: Some(CatTypeFilter::Relation),
            dedupe: false,
            clean: false,
        },
        "cat-dedupe" => PbfhoggCommand::Cat {
            type_filter: None,
            dedupe: true,
            clean: false,
        },
        "tags-filter-way" => PbfhoggCommand::TagsFilter {
            filter: "w/highway=primary".into(),
            omit_referenced: true,
            input_kind_osc: false,
        },
        "tags-filter-amenity" => PbfhoggCommand::TagsFilter {
            filter: "amenity=restaurant".into(),
            omit_referenced: true,
            input_kind_osc: false,
        },
        "tags-filter-twopass" => PbfhoggCommand::TagsFilter {
            filter: "highway=primary".into(),
            omit_referenced: false,
            input_kind_osc: false,
        },
        "tags-filter-osc" => PbfhoggCommand::TagsFilter {
            filter: "highway=primary".into(),
            omit_referenced: false,
            input_kind_osc: true,
        },
        "getid" => PbfhoggCommand::Getid {
            add_referenced: false,
            invert: false,
        },
        "getparents" => PbfhoggCommand::Getparents,
        "getid-invert" => PbfhoggCommand::Getid {
            add_referenced: false,
            invert: true,
        },
        "renumber" => PbfhoggCommand::Renumber,
        "merge-changes" => PbfhoggCommand::MergeChanges,
        "apply-changes" => PbfhoggCommand::ApplyChanges,
        "add-locations-to-ways" => PbfhoggCommand::AddLocationsToWays,
        "extract-simple" => PbfhoggCommand::Extract {
            strategy: ExtractStrategy::Simple,
        },
        "extract-complete" => PbfhoggCommand::Extract {
            strategy: ExtractStrategy::Complete,
        },
        "extract-smart" => PbfhoggCommand::Extract {
            strategy: ExtractStrategy::Smart,
        },
        "time-filter" => PbfhoggCommand::TimeFilter,
        "diff" => PbfhoggCommand::Diff {
            format: DiffFormat::Default,
        },
        "diff-osc" => PbfhoggCommand::Diff {
            format: DiffFormat::Osc,
        },
        _ => return Err(DevError::Config(format!("unknown suite preset: {name}"))),
    })
}

/// Whether any requested preset needs the merged PBF (apply-changes output).
fn suite_needs_merged_pbf(commands: &[&str]) -> Result<bool, DevError> {
    commands
        .iter()
        .map(|c| preset_to_command(c))
        .try_fold(false, |acc, cmd| {
            Ok(acc || matches!(cmd?.input_kind(), InputKind::PbfAndMerged))
        })
}

/// Whether any requested preset needs an OSC file directly as input.
fn suite_needs_osc(commands: &[&str]) -> Result<bool, DevError> {
    commands
        .iter()
        .map(|c| preset_to_command(c))
        .try_fold(false, |acc, cmd| {
            let kind = cmd?.input_kind();
            Ok(acc
                || matches!(
                    kind,
                    InputKind::PbfAndOsc | InputKind::OscOnly | InputKind::PbfAndMerged
                ))
        })
}

/// Ensure a merged PBF exists in the scratch directory. Returns the path.
/// Skips merge if the file already exists.
fn ensure_merged_pbf(
    binary: &Path,
    pbf_path: &Path,
    osc_path: &Path,
    scratch_dir: &Path,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let stem = pbf_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("input");
    let merged_name = format!("{stem}-bench-merged.osm.pbf");
    let merged_path = scratch_dir.join(&merged_name);

    if merged_path.exists() {
        output::bench_msg(&format!("using cached merged PBF: {merged_name}"));
        return Ok(merged_path);
    }

    std::fs::create_dir_all(scratch_dir)
        .map_err(|e| DevError::Config(format!("failed to create scratch dir: {e}")))?;

    output::bench_msg(&format!("generating merged PBF: {merged_name}"));
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path not UTF-8".into()))?;
    let osc_str = osc_path
        .to_str()
        .ok_or_else(|| DevError::Config("OSC path not UTF-8".into()))?;
    let merged_str = merged_path
        .to_str()
        .ok_or_else(|| DevError::Config("merged path not UTF-8".into()))?;
    let binary_str = binary.display().to_string();

    let captured = output::run_captured(
        &binary_str,
        &["apply-changes", pbf_str, osc_str, "-o", merged_str],
        project_root,
    )?;

    captured.check_success(&binary_str)?;

    Ok(merged_path)
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    osc_path: Option<&Path>,
    scratch_dir: Option<&Path>,
    file_mb: f64,
    runs: usize,
    commands: &[&str],
    project_root: &Path,
    index_type: Option<&str>,
) -> Result<(), DevError> {
    let (basename, _) = super::path_strs(pbf_path)?;

    // Resolve OSC path once if any preset needs it.
    let osc_pathbuf = if suite_needs_osc(commands)? {
        Some(
            osc_path
                .ok_or_else(|| {
                    DevError::Config(
                        "tags-filter-osc/merge-changes/apply-changes/diff/diff-osc require an OSC file (dataset must have osc configured)".into(),
                    )
                })?
                .to_path_buf(),
        )
    } else {
        None
    };

    // Generate merged PBF if any requested command needs it.
    let merged_pbf = if suite_needs_merged_pbf(commands)? {
        let osc = osc_path.ok_or_else(|| {
            DevError::Config(
                "diff/diff-osc require an OSC file (dataset must have osc configured)".into(),
            )
        })?;
        let scratch = scratch_dir
            .ok_or_else(|| DevError::Config("diff/diff-osc require a scratch directory".into()))?;
        Some(ensure_merged_pbf(
            binary,
            pbf_path,
            osc,
            scratch,
            project_root,
        )?)
    } else {
        None
    };

    // Ensure scratch dir exists up front so per-preset contexts can reference it.
    let scratch_dir = scratch_dir.ok_or_else(|| {
        DevError::Config("bench_commands::run requires a scratch directory".into())
    })?;
    std::fs::create_dir_all(scratch_dir)
        .map_err(|e| DevError::Config(format!("failed to create scratch dir: {e}")))?;

    crate::harness::run_variants("command", commands, |name| {
        let cmd = preset_to_command(name)?;

        let mut params = CommandParams::default();
        if let Some(it) = index_type {
            params.index_type = Some(it.to_owned());
        }

        let osc_pathbuf_for_ctx = osc_pathbuf.clone();
        let osc_paths = osc_pathbuf_for_ctx.clone().map(|p| vec![p]).unwrap_or_default();
        let bbox = if matches!(cmd, PbfhoggCommand::Extract { .. }) {
            Some(SUITE_EXTRACT_BBOX.into())
        } else {
            None
        };

        let ctx = CommandContext {
            binary: binary.to_path_buf(),
            pbf_path: pbf_path.to_path_buf(),
            osc_path: osc_pathbuf_for_ctx,
            osc_paths,
            pbf_b_path: merged_pbf.clone(),
            scratch_dir: scratch_dir.to_path_buf(),
            dataset: String::new(),
            bbox,
            params,
        };

        let args = cmd.build_args(&ctx)?;
        let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let config = BenchConfig {
            command: cmd.result_command(),
            mode: None,
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
            runs,
            cli_args: Some(crate::harness::format_cli_args(
                &binary.display().to_string(),
                &args_refs,
            )),
            brokkr_args: None,
            metadata: cmd.metadata(&ctx),
        };

        harness.run_external_ok(&config, binary, &args_refs, project_root, cmd.ok_exit_codes())?;

        dispatch::cleanup_pbfhogg_output(&cmd, &ctx);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_maps_to_a_command() {
        for name in ALL_COMMANDS {
            preset_to_command(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn unknown_preset_errors() {
        assert!(preset_to_command("not-a-command").is_err());
    }

    #[test]
    fn cat_variants_share_result_command() {
        // All four cat presets collapse to the consolidated "cat" command
        // label in the DB (axes live in cli_args).
        for name in ["cat", "cat-way", "cat-relation", "cat-dedupe"] {
            assert_eq!(preset_to_command(name).unwrap().result_command(), "cat");
        }
    }

    #[test]
    fn extract_variants_use_hardcoded_bbox() {
        let cmd = preset_to_command("extract-simple").unwrap();
        assert!(matches!(
            cmd,
            PbfhoggCommand::Extract {
                strategy: ExtractStrategy::Simple
            }
        ));
    }

    #[test]
    fn output_kinds_match_legacy_classification() {
        // No-output-file presets (inspect, check-*, diff default).
        for name in [
            "inspect",
            "inspect-nodes",
            "inspect-tags",
            "inspect-tags-way",
            "check-refs",
            "check-ids",
            "diff",
        ] {
            let cmd = preset_to_command(name).unwrap();
            assert!(matches!(cmd.output_kind(), OutputKind::None), "{name}");
        }
        // OSC-output presets.
        for name in ["tags-filter-osc", "merge-changes", "diff-osc"] {
            let cmd = preset_to_command(name).unwrap();
            assert!(
                matches!(cmd.output_kind(), OutputKind::ScratchOsc(_)),
                "{name}"
            );
        }
    }
}
