//! Consolidated elivagar measurable command definitions.
//!
//! Each variant of [`ElivagarCommand`] captures the identity, options, build
//! requirements, measurement capabilities, and metadata for one measurable
//! command. This is the single source of truth — bench, hotpath, profile, and
//! the `brokkr run` surface all derive their behaviour from these definitions.

use std::path::{Path, PathBuf};

use crate::db::KvPair;
use crate::error::DevError;

use super::PipelineOpts;

// ---------------------------------------------------------------------------
// BuildKind — how to build a command
// ---------------------------------------------------------------------------

/// Describes how to build the binary for an elivagar command.
pub enum BuildKind {
    /// Build the main project binary (e.g. `elivagar`).
    MainBinary,
    /// Build a cargo example by name (e.g. `bench_pmtiles`, `bench_node_store`).
    Example(&'static str),
    /// No Rust build needed (external tools like Planetiler, Tilemaker).
    NoBuild,
}

// ---------------------------------------------------------------------------
// ElivagarCommand — the unified command enum
// ---------------------------------------------------------------------------

/// All elivagar measurable commands.
///
/// Each variant carries the command-specific parameters that differ from one
/// invocation to another. Shared concerns (dataset, variant, runs, features,
/// measurement mode) are handled by the caller.
pub enum ElivagarCommand<'a> {
    /// Full elivagar pipeline: PBF -> PMTiles.
    ///
    /// Currently `bench self`. Builds the main binary, runs with pipeline
    /// options, parses self-reported kv metrics from stderr (total_ms, phase12_ms,
    /// ocean_ms, phase3_ms, phase4_ms, features, tiles, output_bytes).
    Tilegen {
        opts: &'a PipelineOpts<'a>,
        skip_to: Option<&'a str>,
        compression_level: Option<u32>,
    },

    /// PMTiles writer micro-benchmark.
    ///
    /// Currently `bench pmtiles`. Builds the `bench_pmtiles` cargo example.
    /// The example handles its own iteration internally.
    PmtilesWriter {
        /// Number of synthetic tiles to write. Default: 500_000.
        tiles: usize,
    },

    /// SortedNodeStore micro-benchmark.
    ///
    /// Currently `bench node-store`. Builds the `bench_node_store` cargo
    /// example. The example handles its own iteration internally.
    NodeStore {
        /// Number of nodes in millions. Default: 50.
        nodes: usize,
    },

    /// Planetiler Shortbread comparison baseline.
    ///
    /// External Java tool. Auto-downloads JDK + Planetiler JAR. No Rust
    /// build, no hotpath support.
    Planetiler,

    /// Tilemaker Shortbread comparison baseline.
    ///
    /// External C++ tool. Auto-downloads/builds Tilemaker + shortbread config.
    /// No Rust build, no hotpath support.
    Tilemaker,
}

impl<'a> ElivagarCommand<'a> {
    /// The command ID used in the CLI and as the result DB command label.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Tilegen { .. } => "tilegen",
            Self::PmtilesWriter { .. } => "pmtiles-writer",
            Self::NodeStore { .. } => "node-store",
            Self::Planetiler => "planetiler",
            Self::Tilemaker => "tilemaker",
        }
    }

    /// Whether this command supports hotpath instrumentation
    /// (function-level timing and allocation tracking).
    pub fn supports_hotpath(&self) -> bool {
        match self {
            Self::Tilegen { .. } | Self::PmtilesWriter { .. } | Self::NodeStore { .. } => true,
            Self::Planetiler | Self::Tilemaker => false,
        }
    }

    /// Whether this command is an external tool (not a Rust binary we build).
    ///
    /// Planetiler and Tilemaker are external tools with their own complex
    /// setup that doesn't fit the unified dispatch pattern.
    pub fn is_external(&self) -> bool {
        matches!(self, Self::Planetiler | Self::Tilemaker)
    }

    /// Whether this command needs a PBF input file.
    ///
    /// Tilegen and external tools process PBF files. Micro-benchmarks
    /// (PmtilesWriter, NodeStore) are synthetic and need no PBF.
    pub fn needs_pbf(&self) -> bool {
        match self {
            Self::Tilegen { .. } | Self::Planetiler | Self::Tilemaker => true,
            Self::PmtilesWriter { .. } | Self::NodeStore { .. } => false,
        }
    }

    /// Whether this command needs a data directory (for ocean shapefiles, tmp, etc.).
    ///
    /// Tilegen needs it for ocean data and tmp storage. External tools need it
    /// for their own data. Micro-benchmarks are self-contained.
    #[allow(dead_code)] // prepared for future nidhogg-style dispatch unification
    pub fn needs_data_dir(&self) -> bool {
        match self {
            Self::Tilegen { .. } | Self::Planetiler | Self::Tilemaker => true,
            Self::PmtilesWriter { .. } | Self::NodeStore { .. } => false,
        }
    }

    /// Files to clean up after a run.
    ///
    /// Returns paths relative to the given scratch directory that should be
    /// removed after execution completes.
    pub fn output_files(&self, scratch_dir: &Path) -> Vec<PathBuf> {
        match self {
            Self::Tilegen { .. } => {
                vec![scratch_dir.join("bench-self-output.pmtiles")]
            }
            Self::PmtilesWriter { .. }
            | Self::NodeStore { .. }
            | Self::Planetiler
            | Self::Tilemaker => vec![],
        }
    }

    /// Describes how to build the binary for this command.
    pub fn build_config(&self) -> BuildKind {
        match self {
            Self::Tilegen { .. } => BuildKind::MainBinary,
            Self::PmtilesWriter { .. } => BuildKind::Example("bench_pmtiles"),
            Self::NodeStore { .. } => BuildKind::Example("bench_node_store"),
            Self::Planetiler | Self::Tilemaker => BuildKind::NoBuild,
        }
    }

    /// Cargo package to build for this command (main binary).
    ///
    /// Returns `None` for the default package and external tools.
    pub fn package(&self) -> Option<&'static str> {
        None
    }

    /// Cargo example to build for this command.
    ///
    /// Returns `Some` for micro-benchmarks that are cargo examples.
    pub fn example(&self) -> Option<&'static str> {
        match self {
            Self::PmtilesWriter { .. } => Some("bench_pmtiles"),
            Self::NodeStore { .. } => Some("bench_node_store"),
            _ => None,
        }
    }

    /// Whether this command needs the scratch directory to be created.
    #[allow(dead_code)]
    pub fn needs_scratch(&self) -> bool {
        matches!(self, Self::Tilegen { .. })
    }

    /// The DB command label for result storage — the bare subcommand id.
    /// The measurement mode (`bench`/`hotpath`/`alloc`) is recorded in the
    /// `variant` column.
    pub fn result_command(&self) -> &'static str {
        match self {
            Self::Tilegen { .. } => "self",
            Self::PmtilesWriter { .. } => "pmtiles",
            Self::NodeStore { .. } => "node-store",
            Self::Planetiler => "planetiler",
            Self::Tilemaker => "tilemaker",
        }
    }

    /// Build the argument vector for this command.
    ///
    /// - **Tilegen**: full pipeline args (`run <pbf> -o <output> --tmp-dir <tmp> [flags]`).
    ///   `pbf_str` is the resolved PBF path, `scratch_dir` is the output
    ///   directory, and `data_dir` is the data directory for ocean shapefiles
    ///   and tmp storage.
    /// - **PmtilesWriter**: `["--tiles", "<N>", "--runs", "1"]`.
    /// - **NodeStore**: `["--nodes", "<N>", "--runs", "1"]`.
    /// - **Planetiler/Tilemaker**: returns an error (external tools have their own setup).
    pub fn build_args(
        &self,
        pbf_str: &str,
        scratch_dir: &Path,
        data_dir: &Path,
    ) -> Result<Vec<String>, DevError> {
        match self {
            Self::Tilegen {
                opts,
                skip_to,
                compression_level,
            } => {
                std::fs::create_dir_all(scratch_dir)?;

                let output_path = scratch_dir.join("bench-self-output.pmtiles");
                let output_str = output_path.display().to_string();

                let tmp_dir = data_dir.join("tilegen_tmp");
                std::fs::create_dir_all(&tmp_dir)?;
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
                if let Some(level) = compression_level {
                    args.push("--compression-level".into());
                    args.push(level.to_string());
                }
                opts.push_args(&mut args, data_dir);

                Ok(args)
            }
            Self::PmtilesWriter { tiles } => Ok(vec![
                "--tiles".into(),
                tiles.to_string(),
                "--runs".into(),
                "1".into(),
            ]),
            Self::NodeStore { nodes } => Ok(vec![
                "--nodes".into(),
                nodes.to_string(),
                "--runs".into(),
                "1".into(),
            ]),
            Self::Planetiler | Self::Tilemaker => Err(DevError::Config(format!(
                "build_args called on external command '{}' — external tools have their own arg construction",
                self.id()
            ))),
        }
    }

    /// Build metadata KV pairs for result storage.
    ///
    /// Post-v13 this is reserved for runtime observations. Axis-like
    /// fields (skip_to, compression_level, tiles count, nodes count,
    /// pipeline opts like ocean/tile_format/fanout-cap/etc.) are all
    /// captured in `brokkr_args`/`cli_args` so don't need mirroring
    /// here. `locations_on_ways_detected` is observed after the run
    /// and added by the dispatch layer.
    pub fn metadata(&self) -> Vec<KvPair> {
        Vec::new()
    }
}
