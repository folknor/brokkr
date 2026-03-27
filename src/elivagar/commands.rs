//! Consolidated elivagar measurable command definitions.
//!
//! Each variant of [`ElivagarCommand`] captures the identity, options, build
//! requirements, measurement capabilities, and metadata for one measurable
//! command. This is the single source of truth — bench, hotpath, profile, and
//! the future `brokkr run` surface all derive their behaviour from these
//! definitions.

use crate::db::KvPair;

use super::PipelineOpts;

// ---------------------------------------------------------------------------
// Profile kind
// ---------------------------------------------------------------------------

/// How profiling works for a command.
///
/// Elivagar uses external sampling profilers (perf/samply) rather than the
/// two-pass hotpath timing+alloc approach used by pbfhogg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileKind {
    /// pbfhogg-style: run once with hotpath timing, then once with hotpath-alloc.
    TwoPass,
    /// elivagar-style: run under an external sampling profiler.
    Sampling(SamplingTool),
}

/// External sampling profiler tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingTool {
    Perf,
    Samply,
}

impl SamplingTool {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Perf => "perf",
            Self::Samply => "samply",
        }
    }

    /// Parse from a string, defaulting to Perf for unknown values.
    pub fn from_str_or_default(s: &str) -> Self {
        match s {
            "samply" => Self::Samply,
            _ => Self::Perf,
        }
    }
}

// ---------------------------------------------------------------------------
// Build target
// ---------------------------------------------------------------------------

/// What cargo artifact a command builds, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildTarget {
    /// Build the project's main binary (release profile, optional extra
    /// features from host config). This is what `tilegen` does.
    MainBinary,

    /// Build a cargo example binary. Used by microbenchmarks (pmtiles-writer,
    /// node-store) that live in `examples/`.
    Example {
        /// The example name passed to `cargo build --example <name>`.
        name: &'static str,
    },

    /// No Rust build. Used by external tool baselines (Planetiler, Tilemaker)
    /// that invoke their own binaries.
    None,
}

// ---------------------------------------------------------------------------
// Input requirements
// ---------------------------------------------------------------------------

/// What input data a command needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// Needs a PBF file resolved from dataset + variant config.
    Pbf,

    /// No dataset input. The command generates its own workload
    /// (microbenchmarks with --tiles or --nodes).
    None,
}

// ---------------------------------------------------------------------------
// Command enum
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
    // -- Identity -----------------------------------------------------------

    /// The command ID used in the CLI and as the result DB command label.
    ///
    /// These are the canonical names from CLI-REDESIGN.md:
    /// - `bench self` -> `tilegen`
    /// - `bench pmtiles` -> `pmtiles-writer`
    /// - `bench node-store` -> `node-store`
    /// - `bench planetiler` -> `planetiler`
    /// - `bench tilemaker` -> `tilemaker`
    pub fn id(&self) -> &'static str {
        match self {
            Self::Tilegen { .. } => "tilegen",
            Self::PmtilesWriter { .. } => "pmtiles-writer",
            Self::NodeStore { .. } => "node-store",
            Self::Planetiler => "planetiler",
            Self::Tilemaker => "tilemaker",
        }
    }

    /// The legacy `BenchConfig.command` value used in the current DB schema.
    ///
    /// Preserves backwards compatibility with existing stored results until
    /// the schema is migrated to use canonical IDs.
    pub fn legacy_bench_command(&self) -> &'static str {
        match self {
            Self::Tilegen { .. } => "bench self",
            Self::PmtilesWriter { .. } => "bench pmtiles",
            Self::NodeStore { .. } => "bench node-store",
            Self::Planetiler => "bench planetiler",
            Self::Tilemaker => "bench tilemaker",
        }
    }

    /// The legacy `BenchConfig.command` value used for hotpath runs.
    pub fn legacy_hotpath_command(&self) -> &'static str {
        // All hotpath runs currently use "hotpath" as the command.
        "hotpath"
    }

    /// The legacy `BenchConfig.command` value used for profile runs.
    pub fn legacy_profile_command(&self) -> &'static str {
        "profile"
    }

    // -- Measurement capabilities -------------------------------------------

    /// Whether this command supports hotpath instrumentation
    /// (function-level timing and allocation tracking).
    pub fn supports_hotpath(&self) -> bool {
        match self {
            Self::Tilegen { .. } | Self::PmtilesWriter { .. } | Self::NodeStore { .. } => true,
            Self::Planetiler | Self::Tilemaker => false,
        }
    }

    /// Whether this command supports profiling.
    ///
    /// Only the full pipeline supports sampling profiler integration.
    /// Microbenchmarks and external baselines do not.
    pub fn supports_profile(&self) -> bool {
        matches!(self, Self::Tilegen { .. })
    }

    /// The kind of profiling this command uses, if profiling is supported.
    ///
    /// Elivagar always uses external sampling profilers (perf or samply),
    /// never the two-pass hotpath timing+alloc approach.
    pub fn profile_kind(&self) -> Option<ProfileKind> {
        if self.supports_profile() {
            Some(ProfileKind::Sampling(SamplingTool::Perf))
        } else {
            None
        }
    }

    // -- Build configuration ------------------------------------------------

    /// What cargo artifact this command builds.
    pub fn build_target(&self) -> BuildTarget {
        match self {
            Self::Tilegen { .. } => BuildTarget::MainBinary,
            Self::PmtilesWriter { .. } => BuildTarget::Example {
                name: "bench_pmtiles",
            },
            Self::NodeStore { .. } => BuildTarget::Example {
                name: "bench_node_store",
            },
            Self::Planetiler | Self::Tilemaker => BuildTarget::None,
        }
    }

    /// The cargo profile used for benchmark builds.
    ///
    /// Rust commands use `"release"`. External tools report their own build
    /// system in the results DB.
    pub fn cargo_profile(&self) -> &'static str {
        match self {
            Self::Tilegen { .. } | Self::PmtilesWriter { .. } | Self::NodeStore { .. } => {
                "release"
            }
            Self::Planetiler => "java",
            Self::Tilemaker => "cmake",
        }
    }

    /// The cargo profile used for profiling builds (release + debug symbols).
    pub fn profiling_cargo_profile(&self) -> &'static str {
        "profiling"
    }

    // -- Input requirements -------------------------------------------------

    /// What input data this command needs.
    pub fn input_kind(&self) -> InputKind {
        match self {
            Self::Tilegen { .. } | Self::Planetiler | Self::Tilemaker => InputKind::Pbf,
            Self::PmtilesWriter { .. } | Self::NodeStore { .. } => InputKind::None,
        }
    }

    // -- Defaults -----------------------------------------------------------

    /// Default dataset name. Only meaningful for commands with
    /// `InputKind::Pbf`.
    pub fn default_dataset(&self) -> &'static str {
        "denmark"
    }

    /// Default PBF variant. Elivagar uses `"raw"` by default (unlike
    /// pbfhogg's `"indexed"`).
    pub fn default_variant(&self) -> &'static str {
        "raw"
    }

    /// Default number of benchmark runs.
    pub fn default_runs(&self) -> usize {
        match self {
            Self::Tilegen { .. } | Self::Planetiler | Self::Tilemaker => 3,
            // Microbenchmarks handle their own internal iteration; the harness
            // runs them once and records the aggregate.
            Self::PmtilesWriter { .. } | Self::NodeStore { .. } => 1,
        }
    }

    // -- Harness run style --------------------------------------------------

    /// Whether the harness runs this command as an external process
    /// (`run_external` / `run_external_with_kv`) or as an internal closure
    /// (`run_internal`).
    ///
    /// - `Tilegen` uses `run_external_with_kv_raw` because elivagar emits
    ///   structured kv metrics to stderr and we need the raw stderr to detect
    ///   LocationsOnWays.
    /// - `Planetiler` and `Tilemaker` use `run_external` (plain wall-clock).
    /// - `PmtilesWriter` and `NodeStore` use `run_internal` because the
    ///   example binary manages its own iterations.
    pub fn is_external(&self) -> bool {
        match self {
            Self::Tilegen { .. } | Self::Planetiler | Self::Tilemaker => true,
            Self::PmtilesWriter { .. } | Self::NodeStore { .. } => false,
        }
    }

    // -- Hotpath variant naming ---------------------------------------------

    /// The hotpath variant name stored in `BenchConfig.variant`.
    ///
    /// Combines the command's hotpath base name with the alloc suffix.
    pub fn hotpath_variant(&self, alloc: bool) -> String {
        let suffix = if alloc { "/alloc" } else { "" };
        let base = match self {
            Self::Tilegen { .. } => "tilegen",
            Self::PmtilesWriter { .. } => "pmtiles",
            Self::NodeStore { .. } => "node-store",
            Self::Planetiler | Self::Tilemaker => unreachable!("no hotpath for external tools"),
        };
        format!("{base}{suffix}")
    }

    // -- Bench result variant -----------------------------------------------

    /// The variant stored in `BenchConfig.variant` for wall-clock benchmark
    /// runs.
    pub fn bench_variant(&self) -> Option<&'static str> {
        match self {
            Self::Tilegen { .. } | Self::PmtilesWriter { .. } | Self::NodeStore { .. } => None,
            Self::Planetiler => Some("shortbread"),
            Self::Tilemaker => Some("shortbread"),
        }
    }

    // -- Metadata -----------------------------------------------------------

    /// Build the command-specific metadata `KvPair`s for benchmark storage.
    ///
    /// These are the `meta.*` keys that describe the command's configuration.
    /// Common metadata (dataset, variant, runs) is added by the harness.
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
                    #[allow(clippy::cast_possible_wrap)]
                    m.push(KvPair::int("meta.compression_level", *v as i64));
                }
                m
            }
            #[allow(clippy::cast_possible_wrap)]
            Self::PmtilesWriter { tiles } => vec![
                KvPair::int("meta.tiles", *tiles as i64),
            ],
            #[allow(clippy::cast_possible_wrap)]
            Self::NodeStore { nodes } => vec![
                KvPair::int("meta.nodes_millions", *nodes as i64),
            ],
            Self::Planetiler | Self::Tilemaker => vec![],
        }
    }

    /// Build the run-level kv pairs emitted alongside each benchmark result.
    ///
    /// Distinct from `metadata()` which describes the configuration; these
    /// describe the specific run parameters.
    pub fn run_kv(&self) -> Vec<KvPair> {
        match self {
            #[allow(clippy::cast_possible_wrap)]
            Self::PmtilesWriter { tiles } => vec![
                KvPair::int("tiles", *tiles as i64),
            ],
            #[allow(clippy::cast_possible_wrap)]
            Self::NodeStore { nodes } => vec![
                KvPair::int("nodes_millions", *nodes as i64),
            ],
            Self::Tilegen { .. } | Self::Planetiler | Self::Tilemaker => vec![],
        }
    }

    /// Build metadata for hotpath runs, extending the base metadata with
    /// alloc tracking info and internal run count.
    pub fn hotpath_metadata(&self, alloc: bool, runs: usize) -> Vec<KvPair> {
        let mut m = Vec::new();
        match self {
            Self::Tilegen { opts, .. } => {
                m = opts.metadata();
                m.push(KvPair::text("meta.alloc", alloc.to_string()));
            }
            #[allow(clippy::cast_possible_wrap)]
            Self::PmtilesWriter { tiles } => {
                m.push(KvPair::int("meta.tiles", *tiles as i64));
                m.push(KvPair::int("meta.internal_runs", runs as i64));
                m.push(KvPair::text("meta.alloc", alloc.to_string()));
            }
            #[allow(clippy::cast_possible_wrap)]
            Self::NodeStore { nodes } => {
                m.push(KvPair::int("meta.nodes_millions", *nodes as i64));
                m.push(KvPair::int("meta.internal_runs", runs as i64));
                m.push(KvPair::text("meta.alloc", alloc.to_string()));
            }
            Self::Planetiler | Self::Tilemaker => {}
        }
        m
    }

    // -- CLI argument construction ------------------------------------------

    /// Build the arguments for the elivagar binary when running the tilegen
    /// pipeline.
    ///
    /// Returns the argument vector for the elivagar binary: `run <pbf> -o
    /// <output> --tmp-dir <tmp> [pipeline flags]`. The caller is responsible
    /// for resolving the PBF path, output path, and tmp dir.
    pub fn tilegen_args(
        pbf_str: &str,
        output_str: &str,
        tmp_dir_str: &str,
        skip_to: Option<&str>,
        compression_level: Option<u32>,
        opts: &PipelineOpts,
        data_dir: &std::path::Path,
    ) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "run".into(),
            pbf_str.into(),
            "-o".into(),
            output_str.into(),
            "--tmp-dir".into(),
            tmp_dir_str.into(),
        ];

        if let Some(phase) = skip_to {
            args.push("--skip-to".into());
            args.push(phase.into());
        }
        if let Some(level) = compression_level {
            args.push("--compression-level".into());
            args.push(level.to_string());
        }
        opts.push_args(&mut args, data_dir);
        args
    }

    /// Build the arguments for microbenchmark example binaries.
    ///
    /// For PmtilesWriter: `--tiles <N> --runs <N>`
    /// For NodeStore: `--nodes <N> --runs <N>`
    pub fn example_args(&self, runs: usize) -> Vec<String> {
        match self {
            Self::PmtilesWriter { tiles } => {
                vec![
                    "--tiles".into(),
                    tiles.to_string(),
                    "--runs".into(),
                    runs.to_string(),
                ]
            }
            Self::NodeStore { nodes } => {
                vec![
                    "--nodes".into(),
                    nodes.to_string(),
                    "--runs".into(),
                    runs.to_string(),
                ]
            }
            _ => vec![],
        }
    }

    // -- Suite membership ---------------------------------------------------

    /// Whether this command is part of the `elivagar` benchmark suite
    /// (currently `bench all`).
    pub fn in_suite(&self) -> bool {
        // All five commands are part of the elivagar suite.
        true
    }

    // -- Display helpers ----------------------------------------------------

    /// Human-readable description for log messages.
    pub fn display_label(&self, file_mb: Option<f64>, runs: usize) -> String {
        match self {
            Self::Tilegen { .. } => {
                let mb = file_mb.map_or(String::new(), |mb| format!(" ({mb:.0} MB)"));
                format!("elivagar pipeline{mb}, {runs} run(s)")
            }
            Self::PmtilesWriter { tiles } => {
                format!("bench_pmtiles: {tiles} tiles, {runs} runs")
            }
            Self::NodeStore { nodes } => {
                format!("bench_node_store: {nodes}M nodes, {runs} runs")
            }
            Self::Planetiler => {
                let mb = file_mb.map_or(String::new(), |mb| format!(" ({mb:.0} MB)"));
                format!("Planetiler Shortbread{mb}, {runs} run(s)")
            }
            Self::Tilemaker => {
                let mb = file_mb.map_or(String::new(), |mb| format!(" ({mb:.0} MB)"));
                format!("Tilemaker Shortbread{mb}, {runs} run(s)")
            }
        }
    }

    /// The lock command label used when acquiring the benchmark lockfile.
    pub fn lock_command(&self) -> &'static str {
        match self {
            Self::Tilegen { .. } => "bench self",
            Self::PmtilesWriter { .. } => "bench pmtiles",
            Self::NodeStore { .. } => "bench node-store",
            Self::Planetiler => "bench planetiler",
            Self::Tilemaker => "bench tilemaker",
        }
    }

    /// The lock command label for hotpath runs.
    pub fn hotpath_lock_command(&self) -> &'static str {
        match self {
            Self::Tilegen { .. } => "hotpath",
            Self::PmtilesWriter { .. } => "hotpath pmtiles",
            Self::NodeStore { .. } => "hotpath node-store",
            Self::Planetiler | Self::Tilemaker => unreachable!("no hotpath for external tools"),
        }
    }

    // -- Output / cleanup ---------------------------------------------------

    /// The scratch output filename for tilegen benchmark runs.
    pub fn tilegen_output_filename() -> &'static str {
        "bench-self-output.pmtiles"
    }

    /// The scratch output filename for hotpath tilegen runs.
    pub fn tilegen_hotpath_output_filename(alloc: bool) -> String {
        let suffix = if alloc { "alloc-" } else { "" };
        format!("hotpath-{suffix}output.pmtiles")
    }

    /// The scratch output filename for profile tilegen runs.
    pub fn tilegen_profile_output_filename() -> &'static str {
        "profile-output.pmtiles"
    }

    /// The tmp directory name used by the tilegen pipeline.
    pub fn tilegen_tmp_dir_name() -> &'static str {
        "tilegen_tmp"
    }

    /// Environment variables to set when running hotpath captures.
    pub fn hotpath_env(&self) -> &[(&str, &str)] {
        match self {
            // Tilegen sets ELIVAGAR_NODE_STATS=1 during hotpath to collect
            // node store statistics.
            Self::Tilegen { .. } => &[("ELIVAGAR_NODE_STATS", "1")],
            Self::PmtilesWriter { .. } | Self::NodeStore { .. } => &[],
            Self::Planetiler | Self::Tilemaker => &[],
        }
    }

    /// Whether this command detects LocationsOnWays from binary stderr.
    ///
    /// Only the tilegen pipeline emits this signal. When detected, a
    /// `meta.locations_on_ways_detected` kv pair is added to results.
    pub fn detects_locations_on_ways(&self) -> bool {
        matches!(self, Self::Tilegen { .. })
    }
}

// ---------------------------------------------------------------------------
// Iteration over all commands
// ---------------------------------------------------------------------------

/// All command IDs, in suite execution order.
pub const ALL_COMMAND_IDS: &[&str] = &[
    "tilegen",
    "planetiler",
    "node-store",
    "pmtiles-writer",
    "tilemaker",
];

/// All command IDs that support hotpath instrumentation.
pub const HOTPATH_COMMAND_IDS: &[&str] = &[
    "tilegen",
    "pmtiles-writer",
    "node-store",
];
