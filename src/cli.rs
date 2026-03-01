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
    #[command(long_about = "\
Run clippy + tests. Extra args are forwarded raw to `cargo test`.

Examples:
  brokkr check                                     # clippy + all tests
  brokkr check -- --test read_paths                # run one test file
  brokkr check -- -- --ignored                     # run ignored tests
  brokkr check -- --test read_paths -- --ignored   # one file, ignored only")]
    Check {
        /// Raw arguments forwarded to `cargo test`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show environment information
    Env,
    /// Build and run the project binary
    Run {
        /// Cargo features to enable (e.g. linux-io-uring)
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,
        /// Arguments passed to the binary
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Query benchmark results
    #[command(long_about = "\
Query benchmark results from .brokkr/results.db.

Examples:
  brokkr results                                    # last 20 results
  brokkr results -n 50                              # last 50 results
  brokkr results 0b74fb6f                           # look up by UUID prefix
  brokkr results --commit a65a                      # filter by commit prefix
  brokkr results --command 'bench read'             # filter by command
  brokkr results --variant pipelined                # filter by variant prefix
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

        /// Filter by command name (e.g. "bench read", "bench merge")
        #[arg(long)]
        command: Option<String>,

        /// Filter by variant prefix (e.g. "tags-filter" matches all tags-filter-* variants)
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
    Clean,
    /// Show lock status (who holds the benchmark lock)
    Lock,
    /// Run benchmarks
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

        #[command(subcommand)]
        bench: BenchCommand,
    },
    /// Cross-validate pbfhogg output against reference tools
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
    Hotpath {
        /// Variant to profile (default: main pipeline; elivagar also supports pmtiles, node-store)
        variant: Option<String>,

        /// Print full build/bench/result output
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Build and benchmark an old commit via git worktree
        #[arg(long)]
        commit: Option<String>,

        /// Cargo features to enable (e.g. linux-io-uring)
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,

        /// Dataset name from brokkr.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,

        /// Run allocation profiling instead of timing
        #[arg(long)]
        alloc: bool,

        /// Skip ocean shapefile detection (elivagar only)
        #[arg(long)]
        no_ocean: bool,

        /// Number of runs; best-of-N is stored (default: 1)
        #[arg(long, default_value = "1")]
        runs: usize,

        /// Number of tiles (pmtiles variant only)
        #[arg(long, default_value = "500000")]
        tiles: usize,

        /// Nodes in millions (node-store variant only)
        #[arg(long, default_value = "50")]
        nodes: usize,

        /// Skip memory availability check
        #[arg(long)]
        no_mem_check: bool,
    },
    /// Run two-pass profiling (timing + allocation) for a dataset
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

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,

        /// Profiling tool: perf or samply (elivagar only)
        #[arg(long)]
        tool: Option<String>,

        /// Skip ocean shapefile detection (elivagar only)
        #[arg(long)]
        no_ocean: bool,

        /// Skip memory availability check
        #[arg(long)]
        no_mem_check: bool,
    },
    /// Download a region dataset from Geofabrik
    Download {
        /// Region name (malta, greater-london, switzerland, norway, japan, denmark, germany, north-america)
        region: String,

        /// URL for the OSC diff file
        #[arg(long)]
        osc_url: Option<String>,
    },
    /// Compare feature counts between two PMTiles archives (elivagar)
    CompareTiles {
        /// First PMTiles file
        file_a: String,
        /// Second PMTiles file
        file_b: String,
        /// Sample size per zoom level
        #[arg(long)]
        sample: Option<usize>,
    },
    /// Download ocean shapefiles (elivagar)
    DownloadOcean,
    /// Print PMTiles v3 file statistics
    PmtilesStats {
        /// PMTiles file(s) to analyze
        #[arg(required = true)]
        files: Vec<String>,
    },
    /// Start the nidhogg server (nidhogg only)
    Serve {
        /// Data directory (ingested disk format)
        #[arg(long)]
        data_dir: Option<String>,

        /// Dataset name from brokkr.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Path to PMTiles file for tile serving
        #[arg(long)]
        tiles: Option<String>,
    },
    /// Stop the nidhogg server (nidhogg only)
    Stop,
    /// Check nidhogg server status (nidhogg only)
    Status,
    /// Ingest a PBF into nidhogg disk format (nidhogg only)
    Ingest {
        /// Explicit PBF file path
        #[arg(long)]
        pbf: Option<String>,

        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
    },
    /// Run nidhogg-update for diff application (nidhogg only)
    Update {
        /// Arguments passed to nidhogg-update
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send a test query to the nidhogg server (nidhogg only)
    Query {
        /// JSON query body (default: Copenhagen highways)
        json: Option<String>,
    },
    /// Test geocoding on the nidhogg server (nidhogg only)
    Geocode {
        /// Search term (default: Kobenhavn)
        #[arg(default_value = "København")]
        term: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum BenchCommand {
    /// Benchmark CLI commands (external timing)
    Commands {
        #[arg(default_value = "all")]
        command: String,
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Benchmark extract strategies (simple/complete/smart)
    Extract {
        #[arg(long, default_value = "japan")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long)]
        bbox: Option<String>,
        #[arg(long, default_value = "simple,complete,smart")]
        strategies: String,
    },
    /// Benchmark allocators (default/jemalloc/mimalloc) via check-refs
    Allocator {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Benchmark indexed vs non-indexed PBF performance
    BlobFilter {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf_indexed: Option<String>,
        #[arg(long)]
        pbf_raw: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Benchmark Planetiler Java PBF read performance
    Planetiler {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Read benchmark (5 modes)
    Read {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long, default_value = "sequential,parallel,pipelined,mmap,blobreader")]
        modes: String,
    },
    /// Write benchmark (sync + pipelined x compression)
    Write {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long, default_value = "none,zlib:6,zstd:3")]
        compression: String,
    },
    /// Merge benchmark (I/O modes x compression)
    Merge {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long)]
        uring: bool,
        #[arg(long, default_value = "zlib,none")]
        compression: String,
    },
    /// Run full benchmark suite (commands + baselines)
    All {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },

    // ----- Elivagar bench variants -----

    /// Elivagar: full pipeline benchmark
    #[command(name = "self")]
    ElivSelf {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "1")]
        runs: usize,
        /// Resume from checkpoint: ocean or sort
        #[arg(long)]
        skip_to: Option<String>,
        /// Skip ocean processing
        #[arg(long)]
        no_ocean: bool,
        /// Gzip compression level 0-10
        #[arg(long)]
        compression_level: Option<u32>,
    },
    /// Elivagar: SortedNodeStore benchmark
    NodeStore {
        /// Nodes in millions
        #[arg(long, default_value = "50")]
        nodes: usize,
        #[arg(long, default_value = "5")]
        runs: usize,
    },
    /// Elivagar: PMTiles writer benchmark
    Pmtiles {
        /// Number of tiles
        #[arg(long, default_value = "500000")]
        tiles: usize,
        #[arg(long, default_value = "5")]
        runs: usize,
    },
    /// Elivagar: Planetiler comparison benchmark
    ElivPlanetiler {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Elivagar: Tilemaker comparison benchmark
    Tilemaker {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Elivagar: full benchmark suite
    ElivAll {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },

    // ----- Nidhogg bench variants -----

    /// Nidhogg: API query benchmark
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
    /// Nidhogg: ingest benchmark
    NidIngest {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
}

#[derive(Subcommand)]
pub(crate) enum VerifyCommand {
    /// Cross-validate sort against osmium sort
    Sort {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate cat (type filters) against osmium cat
    Cat {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate extract (bbox strategies) against osmium extract
    Extract {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        bbox: Option<String>,
    },
    /// Cross-validate tags-filter against osmium tags-filter
    TagsFilter {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate getid/removeid against osmium getid
    GetidRemoveid {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate add-locations-to-ways against osmium
    AddLocationsToWays {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate check-refs against osmium check-refs
    CheckRefs {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate merge against osmium/osmosis/osmconvert
    Merge {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
    },
    /// Cross-validate derive-changes roundtrip against osmium
    DeriveChanges {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
    },
    /// Cross-validate diff summary against osmium diff
    Diff {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
    },
    /// Run all verify commands sequentially
    All {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
        #[arg(long)]
        bbox: Option<String>,
    },

    // ----- Nidhogg verify variants -----

    /// Nidhogg: batch query verification
    Batch,
    /// Nidhogg: geocode verification
    NidGeocode {
        /// Search terms to test
        #[arg(trailing_var_arg = true)]
        queries: Vec<String>,
    },
    /// Nidhogg: read-only filesystem verification
    Readonly {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
    },
}
