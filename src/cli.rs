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
  brokkr check -- --test read_paths -- --ignored   # one file, ignored only")]
    Check {
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
    /// [pbfhogg] Benchmark allocators (default/jemalloc/mimalloc) via check-refs
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
    /// [pbfhogg] Cross-validate getid/removeid against osmium getid
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
    /// [pbfhogg] Cross-validate check-refs against osmium check-refs
    #[command(display_order = 6)]
    CheckRefs {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
    },
    /// [pbfhogg] Cross-validate merge against osmium/osmosis/osmconvert
    #[command(display_order = 7)]
    Merge {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        osc_seq: Option<String>,
    },
    /// [pbfhogg] Cross-validate derive-changes roundtrip against osmium
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
    Batch,
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
