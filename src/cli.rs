use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "brokkr", about = "Shared development tooling", version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Run clippy + tests
    #[command(display_order = 0, long_about = "\
Run clippy + tests. Extra args are forwarded raw to `cargo test`.

Examples:
  brokkr check                                     # clippy + all tests
  brokkr check -- --test read_paths                # run one test file
  brokkr check -- -- --ignored                     # run ignored tests
  brokkr check -- --test read_paths -- --ignored   # one file, ignored only
  brokkr check --no-default-features               # check without default features
  brokkr check --features commands                  # check with specific features")]
    Check {
        /// Cargo features to enable
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,

        /// Disable default Cargo features
        #[arg(long)]
        no_default_features: bool,

        /// Raw arguments forwarded to `cargo test`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show environment information
    #[command(display_order = 1)]
    Env,
    /// Build and run the project binary
    #[command(display_order = 2)]
    Run {
        /// Cargo features to enable (e.g. linux-io-uring)
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,
        /// Print machine-readable timing line (key=value pairs)
        #[arg(long)]
        time: bool,
        /// Print machine-readable JSON timing summary
        #[arg(long)]
        json: bool,
        /// Number of times to run the command (build happens once)
        #[arg(long, default_value_t = 1)]
        runs: usize,
        /// Skip build step and run existing release binary
        #[arg(long)]
        no_build: bool,
        /// Arguments passed to the binary
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Query benchmark results
    #[command(display_order = 3, long_about = "\
Query benchmark results from .brokkr/results.db.

Examples:
  brokkr results                                    # last 20 results
  brokkr results -n 50                              # last 50 results
  brokkr results 0b74fb6f                           # look up by UUID prefix
  brokkr results --commit a65a                      # filter by commit prefix
  brokkr results --command 'bench read'             # filter by command
  brokkr results --variant pipelined                # filter by variant (substring match)
  brokkr results --compare a65a 911c                # compare two commits
  brokkr results --compare a65a 911c --variant sync # compare, filtered
  brokkr results --compare-last                     # compare two most recent commits
  brokkr results --compare-last --command hotpath   # compare hotpath runs (shows function diff)")]
    Results {
        /// UUID prefix to look up specific result(s)
        #[arg(conflicts_with_all = ["commit", "compare"])]
        query: Option<String>,

        /// Show results for a specific commit (prefix match)
        #[arg(long, conflicts_with = "compare")]
        commit: Option<String>,

        /// Compare two commits side-by-side
        #[arg(long, num_args = 2, value_names = ["COMMIT_A", "COMMIT_B"])]
        compare: Option<Vec<String>>,

        /// Compare the two most recent commits (use with --command/--variant to narrow)
        #[arg(long, conflicts_with_all = ["query", "commit", "compare"])]
        compare_last: bool,

        /// Filter by command (substring match, e.g. "read" matches "bench read")
        #[arg(long)]
        command: Option<String>,

        /// Filter by variant (substring match, e.g. "zlib" matches "buffered+zlib")
        #[arg(long)]
        variant: Option<String>,

        /// Maximum number of results to show
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,

        /// Maximum number of functions shown in hotpath reports (0 = all)
        #[arg(long, default_value = "10")]
        top: usize,
    },
    /// Clean build artifacts and scratch data
    #[command(display_order = 4)]
    Clean,
    /// Show lock status (who holds the benchmark lock)
    #[command(display_order = 5)]
    Lock,
    /// Browse command history
    #[command(display_order = 6, long_about = "\
Browse the global command history log (~/.local/share/brokkr/history.db).

Every brokkr invocation is recorded with timing and exit status.

Examples:
  brokkr history                        # last 25 entries
  brokkr history -n 50                  # last 50
  brokkr history --all                  # everything
  brokkr history --command bench        # filter by command substring
  brokkr history --project pbfhogg      # filter by project
  brokkr history --failed               # only non-zero exit
  brokkr history --since 2026-03-01     # from date (YYYY-MM-DD)
  brokkr history --slow 10000           # commands that took >10s")]
    History {
        /// Filter by command (substring match)
        #[arg(long)]
        command: Option<String>,

        /// Filter by project name
        #[arg(long)]
        project: Option<String>,

        /// Show only failed commands (non-zero exit)
        #[arg(long)]
        failed: bool,

        /// Show entries from this date onward (YYYY-MM-DD or YYYY-MM-DD HH:MM:SS)
        #[arg(long, value_parser = validate_since)]
        since: Option<String>,

        /// Show commands that took at least this many milliseconds
        #[arg(long)]
        slow: Option<i64>,

        /// Maximum number of entries to show
        #[arg(long, short = 'n', default_value = "25", conflicts_with = "all")]
        limit: usize,

        /// Show all entries (ignores -n)
        #[arg(long)]
        all: bool,
    },
    /// Run benchmarks
    #[command(display_order = 10)]
    Bench {
        /// Print full build/bench/result output
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Build and benchmark an old commit via git worktree
        #[arg(long)]
        commit: Option<String>,

        /// Cargo features to enable (e.g. libdeflater)
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,

        /// Run even if the git tree is dirty (results will not be stored)
        #[arg(long)]
        force: bool,

        #[command(subcommand)]
        bench: BenchCommand,
    },
    /// Cross-validate output against reference tools
    #[command(display_order = 11)]
    Verify {
        /// Print full build/verify output
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Build and verify an old commit via git worktree
        #[arg(long)]
        commit: Option<String>,

        #[command(subcommand)]
        verify: VerifyCommand,
    },
    /// Run hotpath profiling (timing or allocation instrumentation)
    #[command(display_order = 12)]
    Hotpath {
        /// Target to profile (default: main pipeline; elivagar also supports pmtiles, node-store)
        target: Option<String>,

        /// Print full build/bench/result output
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Build and benchmark an old commit via git worktree
        #[arg(long)]
        commit: Option<String>,

        /// Cargo features to enable (e.g. linux-io-uring)
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,

        /// Run even if the git tree is dirty (results will not be stored)
        #[arg(long)]
        force: bool,

        /// Dataset name from brokkr.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// PBF variant to use (raw, indexed, locations)
        #[arg(long, default_value = "indexed")]
        variant: String,

        /// OSC sequence number(s) from brokkr.toml (comma-separated)
        #[arg(long)]
        osc_seq: Option<String>,

        /// Run allocation profiling instead of timing
        #[arg(long)]
        alloc: bool,

        /// Skip ocean shapefile detection (elivagar only)
        #[arg(long)]
        no_ocean: bool,

        /// Force compact node store even without PBF sort header (elivagar only)
        #[arg(long)]
        force_sorted: bool,

        /// Bypass elivagar flat-index safety guardrails (unsafe)
        #[arg(long)]
        allow_unsafe_flat_index: bool,

        /// Tile output format (mvt or mlt, elivagar only)
        #[arg(long)]
        tile_format: Option<String>,
        /// Tile compression (gzip or brotli, elivagar only)
        #[arg(long)]
        tile_compression: Option<String>,
        /// Compress sort chunks (lz4 or snappy, elivagar only)
        #[arg(long)]
        compress_sort_chunks: Option<String>,
        /// Keep tile blob in memory (elivagar only)
        #[arg(long)]
        in_memory: bool,
        /// Input PBF has locations on ways (elivagar only)
        #[arg(long)]
        locations_on_ways: bool,
        /// Default fanout cap for all layers (elivagar only)
        #[arg(long)]
        fanout_cap_default: Option<u32>,
        /// Per-layer fanout caps (elivagar only, comma-separated layer=N pairs)
        #[arg(long)]
        fanout_cap: Option<String>,
        /// Polygon simplification factor (elivagar only, default 1.0, range 0.1–10.0)
        #[arg(long)]
        polygon_simplify_factor: Option<f64>,

        /// Number of runs; best-of-N is stored (default: 1)
        #[arg(long, default_value = "1")]
        runs: usize,

        /// Number of tiles (pmtiles variant only)
        #[arg(long, default_value = "500000")]
        tiles: usize,

        /// Nodes in millions (node-store variant only)
        #[arg(long, default_value = "50")]
        nodes: usize,

        /// Run only this test (pbfhogg: inspect-tags, check-refs, cat, apply-changes-zlib, apply-changes-none)
        #[arg(long)]
        test: Option<String>,

        /// Skip memory availability check
        #[arg(long)]
        no_mem_check: bool,
    },
    /// Run two-pass profiling (timing + allocation) for a dataset
    #[command(display_order = 13)]
    Profile {
        /// Print full build/bench/result output
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Build and profile an old commit via git worktree
        #[arg(long)]
        commit: Option<String>,

        /// Cargo features to enable (e.g. linux-io-uring)
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,

        /// Dataset name from brokkr.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// PBF variant to use (raw, indexed, locations)
        #[arg(long, default_value = "indexed")]
        variant: String,

        /// OSC sequence number(s) from brokkr.toml (comma-separated)
        #[arg(long)]
        osc_seq: Option<String>,

        /// Profiling tool: perf or samply (elivagar only)
        #[arg(long)]
        tool: Option<String>,

        /// Skip ocean shapefile detection (elivagar only)
        #[arg(long)]
        no_ocean: bool,

        /// Force compact node store even without PBF sort header (elivagar only)
        #[arg(long)]
        force_sorted: bool,

        /// Bypass elivagar flat-index safety guardrails (unsafe)
        #[arg(long)]
        allow_unsafe_flat_index: bool,

        /// Tile output format (mvt or mlt, elivagar only)
        #[arg(long)]
        tile_format: Option<String>,
        /// Tile compression (gzip or brotli, elivagar only)
        #[arg(long)]
        tile_compression: Option<String>,
        /// Compress sort chunks (lz4 or snappy, elivagar only)
        #[arg(long)]
        compress_sort_chunks: Option<String>,
        /// Keep tile blob in memory (elivagar only)
        #[arg(long)]
        in_memory: bool,
        /// Input PBF has locations on ways (elivagar only)
        #[arg(long)]
        locations_on_ways: bool,
        /// Default fanout cap for all layers (elivagar only)
        #[arg(long)]
        fanout_cap_default: Option<u32>,
        /// Per-layer fanout caps (elivagar only, comma-separated layer=N pairs)
        #[arg(long)]
        fanout_cap: Option<String>,
        /// Polygon simplification factor (elivagar only, default 1.0, range 0.1–10.0)
        #[arg(long)]
        polygon_simplify_factor: Option<f64>,

        /// Skip memory availability check
        #[arg(long)]
        no_mem_check: bool,
    },
    /// [pbfhogg] Download a region dataset from Geofabrik
    #[command(display_order = 20)]
    Download {
        /// Region name (malta, greater-london, switzerland, norway, japan, denmark, germany, north-america)
        region: String,

        /// URL for the OSC diff file
        #[arg(long)]
        osc_url: Option<String>,
    },
    /// [elivagar] Compare feature counts between two PMTiles archives
    #[command(display_order = 30)]
    CompareTiles {
        /// First PMTiles file
        file_a: String,
        /// Second PMTiles file
        file_b: String,
        /// Sample size per zoom level
        #[arg(long)]
        sample: Option<usize>,
    },
    /// [elivagar] Download ocean shapefiles
    #[command(display_order = 31)]
    DownloadOcean,
    /// Print PMTiles v3 file statistics
    #[command(display_order = 19)]
    PmtilesStats {
        /// PMTiles file(s) to analyze
        #[arg(required = true)]
        files: Vec<String>,
    },
    /// [nidhogg] Start the server
    #[command(display_order = 40)]
    Serve {
        /// Data directory (ingested disk format)
        #[arg(long)]
        data_dir: Option<String>,

        /// Dataset name from brokkr.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// PMTiles variant from config (auto-selects if only one configured)
        #[arg(long)]
        tiles: Option<String>,
    },
    /// [nidhogg] Stop the server
    #[command(display_order = 41)]
    Stop,
    /// [nidhogg] Check server status
    #[command(display_order = 42)]
    Status,
    /// [nidhogg] Ingest a PBF into disk format
    #[command(display_order = 43)]
    Ingest {
        /// PBF variant to use (raw, indexed, locations)
        #[arg(long, default_value = "raw")]
        variant: String,

        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
    },
    /// [nidhogg] Run diff application
    #[command(display_order = 44)]
    Update {
        /// Arguments passed to nidhogg-update
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// [nidhogg] Send a test query
    #[command(display_order = 45)]
    Query {
        /// JSON query body (default: Copenhagen highways)
        json: Option<String>,
    },
    /// [nidhogg] Test geocoding
    #[command(display_order = 46)]
    Geocode {
        /// Search term (default: Kobenhavn)
        #[arg(default_value = "København")]
        term: String,
    },
    /// Run the full pipeline and open a map viewer for visual inspection
    #[command(display_order = 50)]
    Preview {
        /// Start from a specific pipeline step (enrich, tilegen, ingest, serve)
        #[arg(long, value_enum)]
        from: Option<PreviewStep>,

        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// PBF variant to use (raw, indexed, locations)
        #[arg(long, default_value = "indexed")]
        variant: String,

        /// Don't open browser after starting the server
        #[arg(long)]
        no_open: bool,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum PreviewStep {
    Enrich,
    Tilegen,
    Ingest,
    Serve,
}

#[derive(Subcommand)]
pub(crate) enum BenchCommand {
    /// [pbfhogg] Benchmark CLI commands (external timing)
    #[command(display_order = 0)]
    Commands {
        #[arg(default_value = "all")]
        command: String,
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        osc_seq: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// [pbfhogg] Benchmark extract strategies (simple/complete/smart)
    #[command(display_order = 1)]
    Extract {
        #[arg(long, default_value = "japan")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long)]
        bbox: Option<String>,
        #[arg(long, default_value = "simple,complete,smart")]
        strategies: String,
    },
    /// [pbfhogg] Benchmark allocators (default/jemalloc/mimalloc) via check --refs
    #[command(display_order = 2)]
    Allocator {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// [pbfhogg] Benchmark indexed vs non-indexed PBF performance
    #[command(display_order = 3)]
    BlobFilter {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        indexed_variant: String,
        #[arg(long, default_value = "raw")]
        raw_variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// [pbfhogg] Benchmark Planetiler Java PBF read performance
    #[command(display_order = 4)]
    Planetiler {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// [pbfhogg] Read benchmark (5 modes)
    #[command(display_order = 5)]
    Read {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long, default_value = "sequential,parallel,pipelined,blobreader")]
        modes: String,
    },
    /// [pbfhogg] Write benchmark (sync + pipelined x compression)
    #[command(display_order = 6)]
    Write {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long, default_value = "none,zlib:6,zstd:3")]
        compression: String,
    },
    /// [pbfhogg] Merge benchmark (I/O modes x compression)
    #[command(display_order = 7)]
    Merge {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        osc_seq: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long)]
        uring: bool,
        #[arg(long, default_value = "zlib,none")]
        compression: String,
    },
    /// [pbfhogg] Run full benchmark suite (commands + baselines)
    #[command(display_order = 8)]
    All {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
    },

    /// [elivagar] Full pipeline benchmark
    #[command(name = "self", display_order = 10)]
    ElivSelf {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "raw")]
        variant: String,
        #[arg(long, default_value = "1")]
        runs: usize,
        /// Resume from checkpoint: ocean or sort
        #[arg(long)]
        skip_to: Option<String>,
        /// Skip ocean processing
        #[arg(long)]
        no_ocean: bool,
        /// Force compact node store even without PBF sort header
        #[arg(long)]
        force_sorted: bool,
        /// Gzip compression level 0-10
        #[arg(long)]
        compression_level: Option<u32>,
        /// Bypass elivagar flat-index safety guardrails (unsafe)
        #[arg(long)]
        allow_unsafe_flat_index: bool,
        /// Tile output format (mvt or mlt)
        #[arg(long)]
        tile_format: Option<String>,
        /// Tile compression (gzip or brotli)
        #[arg(long)]
        tile_compression: Option<String>,
        /// Compress sort chunks (lz4 or snappy)
        #[arg(long)]
        compress_sort_chunks: Option<String>,
        /// Keep tile blob in memory
        #[arg(long)]
        in_memory: bool,
        /// Input PBF has locations on ways
        #[arg(long)]
        locations_on_ways: bool,
        /// Default fanout cap for all layers
        #[arg(long)]
        fanout_cap_default: Option<u32>,
        /// Per-layer fanout caps (comma-separated layer=N pairs, e.g. water_polygons=2048,boundaries=4096)
        #[arg(long)]
        fanout_cap: Option<String>,
        /// Polygon simplification factor (default 1.0, range 0.1–10.0)
        #[arg(long)]
        polygon_simplify_factor: Option<f64>,
    },
    /// [elivagar] SortedNodeStore benchmark
    #[command(display_order = 11)]
    NodeStore {
        /// Nodes in millions
        #[arg(long, default_value = "50")]
        nodes: usize,
        #[arg(long, default_value = "5")]
        runs: usize,
    },
    /// [elivagar] PMTiles writer benchmark
    #[command(display_order = 12)]
    Pmtiles {
        /// Number of tiles
        #[arg(long, default_value = "500000")]
        tiles: usize,
        #[arg(long, default_value = "5")]
        runs: usize,
    },
    /// [elivagar] Planetiler comparison benchmark
    #[command(display_order = 13)]
    ElivPlanetiler {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "raw")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// [elivagar] Tilemaker comparison benchmark
    #[command(display_order = 14)]
    Tilemaker {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "raw")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// [elivagar] Full benchmark suite
    #[command(display_order = 15)]
    ElivAll {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "raw")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
    },

    /// [nidhogg] API query benchmark
    #[command(display_order = 20)]
    Api {
        /// Dataset the server is loaded with (for metadata recording)
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// Run count
        #[arg(long, default_value = "10")]
        runs: usize,
        /// Only run this specific query (cph_highways, cph_large, cph_small_nofilter, cph_buildings)
        #[arg(long)]
        query: Option<String>,
    },
    /// [nidhogg] Ingest benchmark
    #[command(display_order = 21)]
    NidIngest {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "raw")]
        variant: String,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// [nidhogg] Tile serving lifecycle benchmark
    #[command(display_order = 22)]
    Tiles {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PMTiles variant from config (auto-selects if only one configured)
        #[arg(long)]
        tiles: Option<String>,
        /// Runs (full server lifecycle per run)
        #[arg(long, default_value = "1")]
        runs: usize,
        #[arg(long)]
        uring: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum VerifyCommand {
    /// [pbfhogg] Cross-validate sort against osmium sort
    #[command(display_order = 0)]
    Sort {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
    },
    /// [pbfhogg] Cross-validate cat (type filters) against osmium cat
    #[command(display_order = 1)]
    Cat {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
    },
    /// [pbfhogg] Cross-validate extract (bbox strategies) against osmium extract
    #[command(display_order = 2)]
    Extract {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        bbox: Option<String>,
    },
    /// [pbfhogg] Cross-validate tags-filter against osmium tags-filter
    #[command(display_order = 3)]
    TagsFilter {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
    },
    /// [pbfhogg] Cross-validate getid/getid --invert against osmium getid
    #[command(display_order = 4)]
    GetidRemoveid {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
    },
    /// [pbfhogg] Cross-validate add-locations-to-ways against osmium
    #[command(display_order = 5)]
    AddLocationsToWays {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
    },
    /// [pbfhogg] Cross-validate check --refs against osmium check-refs
    #[command(display_order = 6)]
    CheckRefs {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
    },
    /// [pbfhogg] Cross-validate apply-changes against osmium/osmosis/osmconvert
    #[command(display_order = 7)]
    Merge {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        osc_seq: Option<String>,
    },
    /// [pbfhogg] Cross-validate diff --format osc roundtrip against osmium
    #[command(display_order = 8)]
    DeriveChanges {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        osc_seq: Option<String>,
    },
    /// [pbfhogg] Cross-validate diff summary against osmium diff
    #[command(display_order = 9)]
    Diff {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        osc_seq: Option<String>,
    },
    /// [pbfhogg] Run all verify commands sequentially
    #[command(display_order = 10)]
    All {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        osc_seq: Option<String>,
        #[arg(long)]
        bbox: Option<String>,
    },

    /// [elivagar] Verify PMTiles output integrity
    #[command(display_order = 15)]
    ElivVerify {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PMTiles variant from config (auto-selects if only one configured)
        #[arg(long)]
        tiles: Option<String>,
    },

    /// [nidhogg] Batch query verification
    #[command(display_order = 20)]
    Batch {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
    },
    /// [nidhogg] Geocode verification
    #[command(display_order = 21)]
    NidGeocode {
        /// Search terms to test
        #[arg(trailing_var_arg = true)]
        queries: Vec<String>,
    },
    /// [nidhogg] Read-only filesystem verification
    #[command(display_order = 22)]
    Readonly {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
    },
}

/// Validate `--since` format: YYYY-MM-DD or YYYY-MM-DD HH:MM:SS.
fn validate_since(s: &str) -> Result<String, String> {
    let date_ok = s.len() == 10
        && s.as_bytes().get(4) == Some(&b'-')
        && s.as_bytes().get(7) == Some(&b'-')
        && s[..4].chars().all(|c| c.is_ascii_digit())
        && s[5..7].chars().all(|c| c.is_ascii_digit())
        && s[8..10].chars().all(|c| c.is_ascii_digit());

    let datetime_ok = s.len() == 19
        && !date_ok
        && s[..10].len() == 10
        && validate_since(&s[..10]).is_ok()
        && s.as_bytes().get(10) == Some(&b' ')
        && s[11..13].chars().all(|c| c.is_ascii_digit())
        && s.as_bytes().get(13) == Some(&b':')
        && s[14..16].chars().all(|c| c.is_ascii_digit())
        && s.as_bytes().get(16) == Some(&b':')
        && s[17..19].chars().all(|c| c.is_ascii_digit());

    if date_ok || datetime_ok {
        Ok(s.to_owned())
    } else {
        Err(format!("invalid date format '{s}', expected YYYY-MM-DD or YYYY-MM-DD HH:MM:SS"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_compare_last_conflicts_with_query() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "results",
            "abc123",
            "--compare-last",
        ]);
        assert!(parsed.is_err());
    }

    #[test]
    fn results_compare_requires_two_commits() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "results",
            "--compare",
            "abc123",
        ]);
        assert!(parsed.is_err());
    }

    #[test]
    fn bench_blob_filter_defaults_are_stable() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "bench",
            "blob-filter",
        ]).expect("parse");

        let Command::Bench { bench, .. } = parsed.command else {
            panic!("expected bench command");
        };
        let BenchCommand::BlobFilter { dataset, indexed_variant, raw_variant, runs } = bench else {
            panic!("expected blob-filter subcommand");
        };
        assert_eq!(dataset, "denmark");
        assert_eq!(indexed_variant, "indexed");
        assert_eq!(raw_variant, "raw");
        assert_eq!(runs, 3);
    }

    #[test]
    fn pmtiles_stats_requires_at_least_one_file() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "pmtiles-stats",
        ]);
        assert!(parsed.is_err());
    }

    #[test]
    fn preview_defaults_are_stable() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "preview",
        ]).expect("parse");

        let Command::Preview { from, dataset, variant, no_open } = parsed.command else {
            panic!("expected preview command");
        };
        assert!(from.is_none());
        assert_eq!(dataset, "denmark");
        assert_eq!(variant, "indexed");
        assert!(!no_open);
    }

    #[test]
    fn preview_from_accepts_all_steps() {
        for step in ["enrich", "tilegen", "ingest", "serve"] {
            let parsed = Cli::try_parse_from([
                "brokkr",
                "preview",
                "--from", step,
            ]).expect(&format!("parse --from {step}"));

            let Command::Preview { from, .. } = parsed.command else {
                panic!("expected preview command");
            };
            assert!(from.is_some(), "--from {step} should parse");
        }
    }

    #[test]
    fn preview_no_open_flag() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "preview",
            "--no-open",
        ]).expect("parse");

        let Command::Preview { no_open, .. } = parsed.command else {
            panic!("expected preview command");
        };
        assert!(no_open);
    }

    #[test]
    fn preview_from_rejects_invalid_step() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "preview",
            "--from", "bogus",
        ]);
        assert!(parsed.is_err());
    }

    #[test]
    fn run_supports_passthrough_args_after_double_dash() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "run",
            "--",
            "query",
            "--json",
        ]).expect("parse");

        let Command::Run { args, .. } = parsed.command else {
            panic!("expected run command");
        };
        assert_eq!(args, vec!["query", "--json"]);
    }
}
