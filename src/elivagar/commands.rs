//! Consolidated elivagar measurable command definitions.
//!
//! Each variant of [`ElivagarCommand`] captures the identity, options, build
//! requirements, measurement capabilities, and metadata for one measurable
//! command. This is the single source of truth — bench, hotpath, profile, and
//! the `brokkr run` surface all derive their behaviour from these definitions.

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;

use super::PipelineOpts;

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
    pub fn needs_scratch(&self) -> bool {
        matches!(self, Self::Tilegen { .. })
    }

    /// The DB command label for result storage.
    pub fn result_command(&self) -> &'static str {
        match self {
            Self::Tilegen { .. } => "bench self",
            Self::PmtilesWriter { .. } => "bench pmtiles",
            Self::NodeStore { .. } => "bench node-store",
            Self::Planetiler => "bench planetiler",
            Self::Tilemaker => "bench tilemaker",
        }
    }

    /// The DB variant label for result storage.
    pub fn result_variant(&self) -> Option<String> {
        None
    }

    /// Build the argument vector for the tilegen pipeline command.
    ///
    /// `pbf_str` is the resolved PBF path, `scratch_dir` is the output
    /// directory, and `data_dir` is the data directory for ocean shapefiles
    /// and tmp storage.
    pub fn build_tilegen_args(
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
            _ => Err(DevError::Config(format!(
                "build_tilegen_args called on non-tilegen command '{}'",
                self.id()
            ))),
        }
    }

    /// Build metadata KV pairs for result storage.
    #[allow(clippy::cast_possible_wrap)]
    pub fn metadata(&self) -> Vec<KvPair> {
        match self {
            Self::Tilegen {
                opts,
                skip_to,
                compression_level,
            } => {
                let mut m = opts.metadata();
                if let Some(v) = skip_to {
                    m.push(KvPair::text("meta.skip_to", *v));
                }
                if let Some(v) = compression_level {
                    m.push(KvPair::int("meta.compression_level", *v as i64));
                }
                m
            }
            Self::PmtilesWriter { tiles } => {
                vec![
                    KvPair::int("meta.tiles", *tiles as i64),
                    KvPair::int("meta.internal_runs", 1),
                ]
            }
            Self::NodeStore { nodes } => {
                vec![
                    KvPair::int("meta.nodes_millions", *nodes as i64),
                    KvPair::int("meta.internal_runs", 1),
                ]
            }
            Self::Planetiler | Self::Tilemaker => vec![],
        }
    }
}
