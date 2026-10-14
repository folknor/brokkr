//! Consolidated elivagar measurable command definitions.
//!
//! The enum shape records whether a command builds Rust code and, for built
//! commands, whether it builds the main binary or a cargo example.

use std::path::{Path, PathBuf};

use crate::config;
use crate::db::KvPair;
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
        scratch_dir: &Path,
        data_dir: &Path,
        opts: Option<&PipelineOpts>,
    ) -> Result<Vec<String>, DevError> {
        match self {
            Self::MainBinary { tilegen, skip_to } => {
                let opts = opts.ok_or_else(|| {
                    DevError::Config("tilegen arguments require resolved PBF assertions".into())
                })?;

                let output_path = scratch_dir.join("bench-self-output.pmtiles");
                let output_str = output_path.display().to_string();

                let tmp_dir = data_dir.join("tilegen_tmp");
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
                if let Some(level) = tilegen.compression_level {
                    args.push("--compression-level".into());
                    args.push(level.to_string());
                }
                opts.push_args(tilegen, &mut args, data_dir)?;

                Ok(args)
            }
            Self::Example(ExampleCommand::PmtilesWriter { tiles }) => Ok(vec![
                "--tiles".into(),
                tiles.to_string(),
                "--runs".into(),
                "1".into(),
            ]),
            Self::Example(ExampleCommand::NodeStore { nodes }) => Ok(vec![
                "--nodes".into(),
                nodes.to_string(),
                "--runs".into(),
                "1".into(),
            ]),
        }
    }

    /// Build metadata KV pairs for result storage.
    ///
    /// Post-v13 this is reserved for runtime observations. Axis-like fields
    /// are captured in `brokkr_args`/`cli_args`; `locations_on_ways_detected`
    /// is observed after the run and added by the dispatch layer.
    pub fn metadata(&self) -> Vec<KvPair> {
        Vec::new()
    }

    /// The CLI command ID.
    pub fn id(&self) -> &'static str {
        match self {
            Self::MainBinary { .. } => "tilegen",
            Self::Example(command) => command.id(),
        }
    }

    /// Files this command may produce in the scratch directory.
    pub fn output_files(&self, scratch_dir: &Path) -> Vec<PathBuf> {
        match self {
            Self::MainBinary { .. } => vec![scratch_dir.join("bench-self-output.pmtiles")],
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
}
