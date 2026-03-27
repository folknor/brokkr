//! Consolidated elivagar measurable command definitions.
//!
//! Each variant of [`ElivagarCommand`] captures the identity, options, build
//! requirements, measurement capabilities, and metadata for one measurable
//! command. This is the single source of truth — bench, hotpath, profile, and
//! the future `brokkr run` surface all derive their behaviour from these
//! definitions.

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
    /// options, parses self-reported kv metrics from stderr.
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
}
