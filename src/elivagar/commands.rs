//! Consolidated elivagar measurable command definitions.
//!
//! The enum shape records whether a command builds Rust code and, for built
//! commands, whether it builds the main binary or a cargo example.

use std::path::PathBuf;

use crate::clean::layout::ElivagarLayout;
use crate::config;
use crate::error::DevError;

use super::PipelineOpts;

/// A measurable elivagar command, separated by how it is executed.
pub enum ElivagarCommand<'a> {
    Built(BuiltCommand<'a>),
    External(ExternalTool),
}

/// Rust-backed commands. The outer variant is the build kind: main binary or
/// cargo example.
pub enum BuiltCommand<'a> {
    /// Full elivagar pipeline: PBF -> PMTiles, using the main binary.
    MainBinary {
        tilegen: &'a config::project_config::elivagar::TilegenConfig,
        /// Resume from a checkpoint. Per-invocation, not part of the contract:
        /// `execution.resumed_from` sits in provenance's diagnostic group, and
        /// elivagar validates the checkpoint's input identity and producer
        /// config against the current run itself.
        skip_to: Option<&'a str>,
    },
    Example(ExampleCommand),
}

/// Cargo examples used by elivagar's synthetic micro-benchmarks.
#[derive(Clone, Copy)]
pub enum ExampleCommand {
    /// PMTiles writer micro-benchmark.
    PmtilesWriter { tiles: usize },
    /// SortedNodeStore micro-benchmark.
    NodeStore { nodes: usize },
}

/// Tools that have their own build and execution setup.
pub enum ExternalTool {
    /// Planetiler Shortbread comparison baseline.
    Planetiler,
    /// Tilemaker Shortbread comparison baseline.
    Tilemaker,
}

impl ElivagarCommand<'_> {
    /// The command ID used in the CLI.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Built(command) => command.id(),
            Self::External(tool) => tool.id(),
        }
    }

    /// Whether this command needs a PBF input file.
    pub fn needs_pbf(&self) -> bool {
        match self {
            Self::Built(BuiltCommand::MainBinary { .. }) | Self::External(_) => true,
            Self::Built(BuiltCommand::Example(_)) => false,
        }
    }
}

impl BuiltCommand<'_> {
    /// Build argv for Rust commands. Main-binary tilegen requires assertions
    /// from the same PBF entry that supplied its resolved path.
    pub fn build_args(
        &self,
        pbf_str: &str,
        layout: &ElivagarLayout<'_>,
        opts: Option<&PipelineOpts>,
    ) -> Result<Vec<String>, DevError> {
        match self {
            Self::MainBinary { tilegen, skip_to } => {
                let opts = opts.ok_or_else(|| {
                    DevError::Config("tilegen arguments require resolved PBF assertions".into())
                })?;

                let output_path = layout.tilegen_output();
                let output_str = output_path.display().to_string();

                let tmp_dir = layout.tilegen_tmp();
                let tmp_dir_str = tmp_dir.display().to_string();

                let mut args: Vec<String> = vec![
                    "run".into(),
                    pbf_str.into(),
                    "-o".into(),
                    output_str,
                    "--tmp-dir".into(),
                    tmp_dir_str,
                ];

                if let Some(phase) = skip_to {
                    args.push("--skip-to".into());
                    args.push((*phase).into());
                }
                opts.push_args(tilegen, &mut args, layout.data_dir())?;

                Ok(args)
            }
            Self::Example(_) => Err(DevError::Internal(
                "an example's argv depends on its internal run count; use ExampleCommand::args"
                    .into(),
            )),
        }
    }

    /// The pairing qualifier of this command's results rows: the tilegen
    /// block's flags ([`PipelineOpts::arm_qualifier`]) for the main binary,
    /// `None` for an example, whose argv is all in the brokkr argv.
    pub fn arm_qualifier(&self, opts: Option<&PipelineOpts>) -> Result<Option<String>, DevError> {
        match self {
            Self::MainBinary { tilegen, .. } => {
                let opts = opts.ok_or_else(|| {
                    DevError::Config("tilegen arguments require resolved PBF assertions".into())
                })?;
                opts.arm_qualifier(tilegen).map(Some)
            }
            Self::Example(_) => Ok(None),
        }
    }

    /// The CLI command ID.
    pub fn id(&self) -> &'static str {
        match self {
            Self::MainBinary { .. } => "tilegen",
            Self::Example(command) => command.id(),
        }
    }

    /// Files this command may produce in the scratch directory.
    pub fn output_files(&self, layout: &ElivagarLayout<'_>) -> Vec<PathBuf> {
        match self {
            Self::MainBinary { .. } => vec![layout.tilegen_output()],
            Self::Example(_) => vec![],
        }
    }
}

impl ExternalTool {
    /// The CLI command ID.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Planetiler => "planetiler",
            Self::Tilemaker => "tilemaker",
        }
    }
}

impl ExampleCommand {
    /// The CLI command ID.
    pub fn id(&self) -> &'static str {
        match self {
            Self::PmtilesWriter { .. } => "pmtiles-writer",
            Self::NodeStore { .. } => "node-store",
        }
    }

    /// Cargo example target used to build this command.
    pub fn cargo_example(&self) -> &'static str {
        match self {
            Self::PmtilesWriter { .. } => "bench_pmtiles",
            Self::NodeStore { .. } => "bench_node_store",
        }
    }

    /// The internal repetition count an example process is given: the
    /// requested runs under `--bench`, where the process is the one aggregate
    /// sample, and 1 for run and profiled modes, where each process is one
    /// pass.
    pub fn internal_runs(bench: bool, runs: usize) -> usize {
        if bench { runs } else { 1 }
    }

    /// The example's argv for `internal_runs` internal repetitions. The
    /// examples do their setup once per process and repeat only their timed
    /// loops: `bench_pmtiles` generates its tiles, then runs each writer
    /// (elivagar's and pmtiles-rs's) that many times; `bench_node_store`
    /// generates nodes and builds the store once, then runs the way-like and
    /// the random lookup loops that many times each.
    pub fn args(&self, internal_runs: usize) -> Vec<String> {
        let (flag, size) = match self {
            Self::PmtilesWriter { tiles } => ("--tiles", *tiles),
            Self::NodeStore { nodes } => ("--nodes", *nodes),
        };
        vec![
            flag.into(),
            size.to_string(),
            "--runs".into(),
            internal_runs.to_string(),
        ]
    }

    /// The observations a bench row records beside the process wall: the
    /// example's size parameter and the internal count it was given.
    pub fn observations(&self, internal_runs: usize) -> Vec<crate::db::KvPair> {
        use crate::db::KvPair;
        let (key, value) = match self {
            Self::PmtilesWriter { tiles } => ("tiles", *tiles),
            Self::NodeStore { nodes } => ("nodes_millions", *nodes),
        };
        vec![
            KvPair::int(key, i64::try_from(value).unwrap_or(i64::MAX)),
            KvPair::int(
                "internal_runs",
                i64::try_from(internal_runs).unwrap_or(i64::MAX),
            ),
        ]
    }
}

#[cfg(test)]
mod example_tests {
    use super::ExampleCommand;

    #[test]
    fn only_a_bench_repeats_inside_the_example() {
        assert_eq!(ExampleCommand::internal_runs(true, 5), 5);
        assert_eq!(ExampleCommand::internal_runs(false, 5), 1);
        assert_eq!(
            ExampleCommand::PmtilesWriter { tiles: 9 }.args(5),
            ["--tiles", "9", "--runs", "5"]
        );
        assert_eq!(
            ExampleCommand::NodeStore { nodes: 50 }.args(1),
            ["--nodes", "50", "--runs", "1"]
        );
    }
}
