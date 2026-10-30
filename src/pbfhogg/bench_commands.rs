//! Benchmark: run pbfhogg CLI commands and measure wall-clock time.
//!
//! Routes every preset through `PbfhoggCommand::build_args` so argv
//! construction has a single source of truth. `preset_to_command` is the
//! only place suite-preset strings meet the typed command enum.

use std::path::Path;

use crate::error::DevError;
use crate::harness::BenchHarness;
use crate::pbfhogg::commands::{
    CatMode, CatTypeFilter, DiffFormat, InspectMode, PbfhoggCommand, TagsFilterInput,
};
use crate::pbfhogg::context::{
    BenchSelection, CommandContext, CommandObservations, CommandParams, ResolvedInputs,
    TwoPbfOrigin,
};

/// Preset commands included in the suite. Extract is benchmarked separately
/// by `bench_all` so it uses the selected dataset's bbox and one strategy axis.
pub const ALL_COMMANDS: &[&str] = &[
    "inspect",
    "inspect-nodes",
    "inspect-tags",
    "inspect-tags-way",
    "check-refs",
    "check-ids",
    "check-index",
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
    "time-filter",
    "diff",
    "diff-osc",
];

/// Map a suite preset string to a fully-specified `PbfhoggCommand` variant.
///
/// This is the single place where preset names are interpreted - once we
/// have a `PbfhoggCommand`, argv, metadata, input/output kind, and result
/// labels all come from the enum itself.
#[allow(clippy::too_many_lines)]
pub(crate) fn preset_to_command(name: &str) -> Result<PbfhoggCommand, DevError> {
    Ok(match name {
        "inspect" => PbfhoggCommand::Inspect {
            mode: InspectMode::Default { extended: false },
        },
        "inspect-nodes" => PbfhoggCommand::Inspect {
            mode: InspectMode::Nodes,
        },
        "inspect-tags" => PbfhoggCommand::Inspect {
            mode: InspectMode::Tags { type_filter: None },
        },
        "inspect-tags-way" => PbfhoggCommand::Inspect {
            mode: InspectMode::Tags {
                type_filter: Some(super::commands::InspectTypeFilter::Way),
            },
        },
        "check-refs" => PbfhoggCommand::CheckRefs,
        "check-ids" => PbfhoggCommand::CheckIds { full: false },
        "check-index" => PbfhoggCommand::CheckIndex,
        "sort" => PbfhoggCommand::Sort,
        "cat" => PbfhoggCommand::Cat(CatMode::Plain {
            type_filter: None,
            clean: false,
        }),
        "cat-way" => PbfhoggCommand::Cat(CatMode::Plain {
            type_filter: Some(CatTypeFilter::Way),
            clean: false,
        }),
        "cat-relation" => PbfhoggCommand::Cat(CatMode::Plain {
            type_filter: Some(CatTypeFilter::Relation),
            clean: false,
        }),
        "cat-dedupe" => PbfhoggCommand::Cat(CatMode::Dedupe),
        "tags-filter-way" => PbfhoggCommand::TagsFilter {
            filter: "w/highway=primary".into(),
            input: TagsFilterInput::Pbf {
                omit_referenced: true,
                invert: false,
                remove_tags: false,
            },
        },
        "tags-filter-amenity" => PbfhoggCommand::TagsFilter {
            filter: "amenity=restaurant".into(),
            input: TagsFilterInput::Pbf {
                omit_referenced: true,
                invert: false,
                remove_tags: false,
            },
        },
        "tags-filter-twopass" => PbfhoggCommand::TagsFilter {
            filter: "highway=primary".into(),
            input: TagsFilterInput::Pbf {
                omit_referenced: false,
                invert: false,
                remove_tags: false,
            },
        },
        "tags-filter-osc" => PbfhoggCommand::TagsFilter {
            filter: "highway=primary".into(),
            input: TagsFilterInput::Osc,
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
        "merge-changes" => PbfhoggCommand::MergeChanges { simplify: false },
        "apply-changes" => PbfhoggCommand::ApplyChanges,
        "add-locations-to-ways" => PbfhoggCommand::AddLocationsToWays,
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
        .try_fold(false, |acc, cmd| Ok(acc || cmd?.needs_merged_pbf()))
}

/// Whether any requested preset needs an OSC file directly as input.
fn suite_needs_osc(commands: &[&str]) -> Result<bool, DevError> {
    commands
        .iter()
        .map(|c| preset_to_command(c))
        .try_fold(false, |acc, cmd| Ok(acc || cmd?.needs_osc_for_setup()))
}

/// The answers the suite's input entry declares, and the label of the table
/// that declares them.
pub struct SuiteExpect<'a> {
    pub expect: &'a crate::config::PbfExpect,
    pub table: String,
}

/// Every preset's declared answer and accepted exits, before anything runs:
/// a preset that could not pass refuses the suite before the merged PBF is
/// built. The suite always rebuilds the merge with this build, so a declared
/// diff answer is checked against this build's merge.
fn plan_answers<'n>(
    binary: &crate::pbfhogg::binary::PbfhoggBinary,
    commands: &[&'n str],
    osc_seq: Option<&str>,
    expect: Option<&SuiteExpect<'_>>,
) -> Result<
    std::collections::HashMap<
        &'n str,
        (
            crate::output::child::Exits,
            Option<crate::pbfhogg::expect::Expectation>,
        ),
    >,
    DevError,
> {
    let mut answers = std::collections::HashMap::new();
    for name in commands {
        let cmd = preset_to_command(name)?;
        let expectation = expect.and_then(|suite| {
            crate::pbfhogg::expect::for_entry(&cmd, suite.expect, osc_seq, &suite.table)
        });
        let exits = crate::pbfhogg::expect::completed_exits(&cmd, binary, expectation.as_ref())?;
        answers.insert(*name, (exits, expectation));
    }
    Ok(answers)
}

/// Run the suite presets.
///
/// `osc_seq` is the replication seq `osc_path` was registered under; together
/// with the snapshot key it names the merged-PBF cache. The suite always runs
/// against the dataset's primary snapshot, so that key is `base`. The merged
/// PBF comes from the same `plan::ensure_merged_pbf` the single-command
/// path uses, and is always rebuilt: the suite is a measured mode, and the
/// cache key cannot see a primary PBF refreshed in place under the same stem.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &crate::pbfhogg::binary::PbfhoggBinary,
    pbf_path: &Path,
    osc_path: Option<&Path>,
    osc_seq: Option<&str>,
    scratch_dir: &Path,
    runs: usize,
    commands: &[&str],
    selection: &BenchSelection,
    project_root: &Path,
    expect: Option<&SuiteExpect<'_>>,
) -> Result<(), DevError> {
    let answers = plan_answers(binary, commands, osc_seq, expect)?;

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
    let (merged_pbf, merged_cache) = if suite_needs_merged_pbf(commands)? {
        let osc = osc_path.ok_or_else(|| {
            DevError::Config(
                "diff/diff-osc require an OSC file (dataset must have osc configured)".into(),
            )
        })?;
        let seq = osc_seq.ok_or_else(|| {
            DevError::Config(
                "diff/diff-osc require the OSC file's replication seq (dataset osc entry)".into(),
            )
        })?;
        let (merged, state) = crate::pbfhogg::plan::ensure_merged_pbf(
            binary,
            pbf_path,
            osc,
            "base",
            seq,
            scratch_dir,
            project_root,
            true,
        )?;
        (Some(merged), Some(state))
    } else {
        (None, None)
    };

    // Ensure scratch dir exists up front so per-preset contexts can reference it.
    std::fs::create_dir_all(scratch_dir)
        .map_err(|e| DevError::Config(format!("failed to create scratch dir: {e}")))?;

    crate::bench_variants::run_variants("command", commands, |name| {
        let cmd = preset_to_command(name)?;

        // The dataset's configured getid/getparents IDs reach every preset
        // context, getid-invert included; nothing else is configured here.
        let params = CommandParams {
            selection: selection.clone(),
            ..CommandParams::default()
        };
        let primary_pbf = pbf_path.to_path_buf();
        let inputs = match &cmd {
            PbfhoggCommand::MergeChanges { .. }
            | PbfhoggCommand::TagsFilter {
                input: TagsFilterInput::Osc,
                ..
            } => ResolvedInputs::OscOnly {
                oscs: osc_pathbuf.clone().into_iter().collect(),
            },
            PbfhoggCommand::ApplyChanges => ResolvedInputs::PbfAndOsc {
                pbf: primary_pbf,
                osc: osc_pathbuf.clone().ok_or_else(|| {
                    DevError::Config("apply-changes requires a resolved OSC file".into())
                })?,
            },
            PbfhoggCommand::Diff { .. } => ResolvedInputs::TwoPbfs {
                a: primary_pbf,
                b: merged_pbf.clone().ok_or_else(|| {
                    DevError::Config("diff requires a resolved merged PBF".into())
                })?,
                origin: TwoPbfOrigin::Merged,
            },
            _ => ResolvedInputs::Pbf { pbf: primary_pbf },
        };
        let ctx = CommandContext {
            inputs,
            scratch_dir: scratch_dir.to_path_buf(),
            dataset: String::new(),
            params,
            observations: CommandObservations {
                merged_cache,
                snapshot_to: None,
            },
        };

        let (exits, expectation) = answers
            .get(name)
            .ok_or_else(|| DevError::Internal(format!("suite preset {name} was not planned")))?;
        crate::pbfhogg::plan::run_wallclock_core(
            harness,
            binary,
            &cmd,
            &ctx,
            runs,
            project_root,
            false,
            *exits,
            expectation.as_ref(),
        )?;
        crate::pbfhogg::plan::cleanup_output(&cmd, &ctx, crate::pbfhogg::commands::ArgMode::Bench)?;
        Ok(())
    })
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
    use super::*;
    use crate::pbfhogg::commands::OutputKind;

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
    fn cat_variants_share_result_id() {
        // All four cat presets collapse to the consolidated "cat" command
        // label in the DB (axes live in cli_args).
        for name in ["cat", "cat-way", "cat-relation", "cat-dedupe"] {
            assert_eq!(preset_to_command(name).unwrap().id(), "cat");
        }
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
            "check-index",
        ] {
            let cmd = preset_to_command(name).unwrap();
            assert!(matches!(cmd.output_kind(), OutputKind::None), "{name}");
        }
        // OSC-output presets.
        for name in ["tags-filter-osc", "merge-changes", "diff-osc"] {
            let cmd = preset_to_command(name).unwrap();
            assert!(
                matches!(cmd.output_kind(), OutputKind::ScratchOsc),
                "{name}"
            );
        }
        // The text diff writes its text to a scratch file, never stdout.
        let cmd = preset_to_command("diff").unwrap();
        assert!(matches!(cmd.output_kind(), OutputKind::ScratchText));
    }
}
