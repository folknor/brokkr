use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "brokkr", about = "Shared development tooling", version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Run clippy + tests
    #[command(
        display_order = 0,
        long_about = "\
Run clippy + tests. Extra args are forwarded raw to `cargo test`.

Examples:
  brokkr check                                     # clippy + all tests
  brokkr check -- --test read_paths                # run one test file
  brokkr check -- -- --ignored                     # run ignored tests
  brokkr check -- --test read_paths -- --ignored   # one file, ignored only
  brokkr check --no-default-features               # check without default features
  brokkr check --features commands                  # check with specific features
  brokkr check --package pbfhogg-cli                # check only the CLI crate
  brokkr check --package pbfhogg-cli -- --test cli  # one test file in the CLI crate"
    )]
    Check {
        /// Cargo features to enable
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,

        /// Disable default Cargo features
        #[arg(long)]
        no_default_features: bool,

        /// Target a specific package in the workspace
        #[arg(long, short)]
        package: Option<String>,

        /// Raw arguments forwarded to `cargo test`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show environment information
    #[command(display_order = 1)]
    Env,
    // ----- pbfhogg tool CLI commands (display_order = 2) -----
    /// [pbfhogg] Inspect PBF metadata
    #[command(name = "inspect", display_order = 2)]
    Inspect {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Inspect PBF node statistics
    #[command(name = "inspect-nodes", display_order = 2)]
    InspectNodes {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Inspect PBF tag frequencies
    #[command(name = "inspect-tags", display_order = 2)]
    InspectTags {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Inspect PBF tag frequencies (way type only)
    #[command(name = "inspect-tags-way", display_order = 2)]
    InspectTagsWay {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Check referential integrity
    #[command(name = "check-refs", display_order = 2)]
    CheckRefs {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Check ID ordering
    #[command(name = "check-ids", display_order = 2)]
    CheckIds {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Sort PBF
    #[command(name = "sort", display_order = 2)]
    Sort {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Cat passthrough (generate indexdata without re-encoding)
    #[command(name = "cat", display_order = 2)]
    Cat {
        #[command(flatten)]
        mode: ModeArgs,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PBF variant to use (defaults to raw — passthrough's natural input)
        #[arg(long, default_value = "raw")]
        variant: String,
        /// Use O_DIRECT for file I/O (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
        /// Use io_uring for I/O (requires linux-io-uring feature in pbfhogg)
        #[arg(long)]
        io_uring: bool,
    },
    /// [pbfhogg] Cat way elements
    #[command(name = "cat-way", display_order = 2)]
    CatWay {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Cat relation elements
    #[command(name = "cat-relation", display_order = 2)]
    CatRelation {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Cat with deduplication
    #[command(name = "cat-dedupe", display_order = 2)]
    CatDedupe {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Tags filter (way/highway=primary)
    #[command(name = "tags-filter-way", display_order = 2)]
    TagsFilterWay {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Tags filter (amenity=restaurant)
    #[command(name = "tags-filter-amenity", display_order = 2)]
    TagsFilterAmenity {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Tags filter two-pass
    #[command(name = "tags-filter-twopass", display_order = 2)]
    TagsFilterTwopass {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Tags filter OSC input
    #[command(name = "tags-filter-osc", display_order = 2)]
    TagsFilterOsc {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// OSC sequence number from brokkr.toml
        #[arg(long)]
        osc_seq: Option<String>,
    },
    /// [pbfhogg] Get elements by ID
    #[command(name = "getid", display_order = 2)]
    Getid {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Get elements by ID with referenced element collection
    #[command(name = "getid-refs", display_order = 2)]
    GetidRefs {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Get parent elements
    #[command(name = "getparents", display_order = 2)]
    Getparents {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Get elements by ID (inverted)
    #[command(name = "getid-invert", display_order = 2)]
    GetidInvert {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Renumber element IDs
    #[command(name = "renumber", display_order = 2)]
    Renumber {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Merge OSC changes
    #[command(name = "merge-changes", display_order = 2)]
    MergeChanges {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// OSC sequence number from brokkr.toml
        #[arg(long, conflicts_with = "osc_range")]
        osc_seq: Option<String>,
        /// OSC sequence range LO..HI (inclusive) to merge in a single invocation
        #[arg(long, value_parser = validate_osc_range)]
        osc_range: Option<String>,
    },
    /// [pbfhogg] Apply OSC changes to PBF
    #[command(name = "apply-changes", display_order = 2)]
    ApplyChanges {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// OSC sequence number from brokkr.toml
        #[arg(long)]
        osc_seq: Option<String>,
    },
    /// [pbfhogg] Add location data to ways
    #[command(name = "add-locations-to-ways", display_order = 2)]
    AddLocationsToWays {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Index type (dense, sparse, external; default: hash)
        #[arg(long)]
        index_type: Option<String>,
    },
    /// [pbfhogg] Extract by bounding box (simple strategy)
    #[command(name = "extract-simple", display_order = 2)]
    ExtractSimple {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Extract by bounding box (complete strategy)
    #[command(name = "extract-complete", display_order = 2)]
    ExtractComplete {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Extract by bounding box (smart strategy)
    #[command(name = "extract-smart", display_order = 2)]
    ExtractSmart {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Multi-extract benchmark (single-pass N regions)
    #[command(name = "multi-extract", display_order = 2)]
    MultiExtract {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Number of non-overlapping bbox regions to extract
        #[arg(long, default_value = "5")]
        regions: usize,
    },
    /// [pbfhogg] Filter by timestamp
    #[command(name = "time-filter", display_order = 2)]
    TimeFilter {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Diff two PBFs (summary)
    #[command(name = "diff", display_order = 2)]
    Diff {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// OSC sequence number from brokkr.toml
        #[arg(long)]
        osc_seq: Option<String>,
        /// Reuse the cached merged PBF in measured modes (default: rebuild
        /// before bench/hotpath/alloc so total invocation wall time is
        /// reproducible). No-op in run mode (cache is always reused there).
        #[arg(long)]
        keep_cache: bool,
    },
    /// [pbfhogg] Diff two snapshots of the same dataset
    #[command(
        name = "diff-snapshots",
        display_order = 2,
        long_about = "\
Diff two point-in-time snapshots of the same dataset.

Unlike `brokkr diff`, neither side is derived from apply-changes — both PBFs
come from independent snapshot resolution. Use this to measure the cost of
diffing two real weekly dumps where no blob-level byte equality is possible.

The dataset's primary (legacy top-level) pbf data is referenced as `base`.
Additional snapshots registered via `brokkr download <region> --as-snapshot <key>`
are referenced by their snapshot key.

Examples:
  brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 1
  brokkr diff-snapshots --dataset planet --from 20260411 --to 20260418 --format osc"
    )]
    DiffSnapshots {
        #[command(flatten)]
        mode: ModeArgs,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// "From" snapshot reference. Use `base` for the dataset's
        /// legacy/primary PBF, or a snapshot key registered under
        /// `[dataset.snapshot.<key>]`.
        #[arg(long)]
        from: String,
        /// "To" snapshot reference (same naming as `--from`).
        #[arg(long)]
        to: String,
        /// PBF variant to use on both sides (raw, indexed, locations).
        /// Errors if the requested variant doesn't exist on either snapshot.
        #[arg(long, default_value = "indexed")]
        variant: String,
        /// Output format: `default` (summary diff) or `osc` (OSC-format diff
        /// written to scratch).
        #[arg(long, default_value = "default")]
        format: String,
    },
    /// [pbfhogg] Diff two PBFs (OSC output)
    #[command(name = "diff-osc", display_order = 2)]
    DiffOsc {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// OSC sequence number from brokkr.toml
        #[arg(long)]
        osc_seq: Option<String>,
        /// Reuse the cached merged PBF in measured modes (default: rebuild
        /// before bench/hotpath/alloc so total invocation wall time is
        /// reproducible). No-op in run mode (cache is always reused there).
        #[arg(long)]
        keep_cache: bool,
    },
    /// [pbfhogg] Build geocode index
    #[command(name = "build-geocode-index", display_order = 2)]
    BuildGeocodeIndex {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Extract by bounding box (configurable strategy)
    #[command(name = "extract", display_order = 2)]
    Extract {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Extract strategy: simple, complete, smart, or all
        #[arg(long, default_value = "all")]
        strategy: String,
        /// Bounding box (lon_min,lat_min,lon_max,lat_max)
        #[arg(long)]
        bbox: Option<String>,
    },
    /// [pbfhogg] Read benchmark
    #[command(name = "read", display_order = 2)]
    Read {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Read modes (comma-separated: sequential,parallel,pipelined,blobreader)
        #[arg(long, default_value = "sequential,parallel,pipelined,blobreader")]
        modes: String,
    },
    /// [pbfhogg] Write benchmark
    #[command(name = "write", display_order = 2)]
    Write {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Compression (comma-separated: none,zlib:6,zstd:3)
        #[arg(long, default_value = "none,zlib:6,zstd:3")]
        compression: String,
    },
    /// [pbfhogg] Merge benchmark
    #[command(name = "merge", display_order = 2)]
    MergeBench {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Compression (comma-separated: zlib,none)
        #[arg(long, default_value = "zlib,none")]
        compression: String,
        /// Use io-uring
        #[arg(long)]
        uring: bool,
        /// OSC sequence number from brokkr.toml
        #[arg(long)]
        osc_seq: Option<String>,
    },

    // ----- elivagar commands (display_order = 3) -----
    /// [elivagar] Full tile generation pipeline
    #[command(name = "tilegen", display_order = 3)]
    Tilegen {
        #[command(flatten)]
        mode: ModeArgs,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PBF variant to use
        #[arg(long, default_value = "raw")]
        variant: String,
        /// Resume from checkpoint: ocean or sort
        #[arg(long)]
        skip_to: Option<String>,
        /// Gzip compression level 0-10
        #[arg(long)]
        compression_level: Option<u32>,
        /// Skip ocean processing
        #[arg(long)]
        no_ocean: bool,
        /// Force compact node store even without PBF sort header
        #[arg(long)]
        force_sorted: bool,
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
        /// Per-layer fanout caps (comma-separated layer=N pairs)
        #[arg(long)]
        fanout_cap: Option<String>,
        /// Polygon simplification factor (default 1.0, range 0.1-10.0)
        #[arg(long)]
        polygon_simplify_factor: Option<f64>,
    },
    /// [elivagar] PMTiles writer micro-benchmark
    #[command(name = "pmtiles-writer", display_order = 3)]
    PmtilesWriter {
        #[command(flatten)]
        mode: ModeArgs,
        /// Number of synthetic tiles
        #[arg(long, default_value = "500000")]
        tiles: usize,
    },
    /// [elivagar] SortedNodeStore micro-benchmark
    #[command(name = "node-store", display_order = 3)]
    NodeStore {
        #[command(flatten)]
        mode: ModeArgs,
        /// Nodes in millions
        #[arg(long, default_value = "50")]
        nodes: usize,
    },
    /// [elivagar] Planetiler comparison baseline
    #[command(name = "planetiler", display_order = 3)]
    ElivPlanetiler {
        #[command(flatten)]
        mode: ModeArgs,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PBF variant to use
        #[arg(long, default_value = "raw")]
        variant: String,
    },
    /// [elivagar] Tilemaker comparison baseline
    #[command(name = "tilemaker", display_order = 3)]
    ElivTilemaker {
        #[command(flatten)]
        mode: ModeArgs,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PBF variant to use
        #[arg(long, default_value = "raw")]
        variant: String,
    },

    // ----- nidhogg commands (display_order = 4) -----
    /// [nidhogg] API query benchmark
    #[command(name = "api", display_order = 4)]
    RunApi {
        #[command(flatten)]
        mode: ModeArgs,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// Specific query name to benchmark
        #[arg(long)]
        query: Option<String>,
    },
    /// [nidhogg] Ingest benchmark
    #[command(name = "nid-ingest", display_order = 4)]
    RunNidIngest {
        #[command(flatten)]
        mode: ModeArgs,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PBF variant to use
        #[arg(long, default_value = "raw")]
        variant: String,
    },
    /// [nidhogg] Tile serving benchmark
    #[command(name = "tiles", display_order = 4)]
    RunTiles {
        #[command(flatten)]
        mode: ModeArgs,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PMTiles variant from config
        #[arg(long)]
        tiles: Option<String>,
        /// Use io_uring for tile serving
        #[arg(long)]
        uring: bool,
    },

    // ----- generic commands (display_order = 5) -----
    /// Generic hotpath for projects without dedicated modules
    #[command(name = "generic-hotpath", display_order = 5)]
    GenericHotpath {
        #[command(flatten)]
        mode: ModeArgs,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PBF variant to use
        #[arg(long, default_value = "indexed")]
        variant: String,
    },

    // ----- suites (display_order = 6) -----
    /// Run a full benchmark suite (pbfhogg, elivagar, or nidhogg)
    #[command(name = "suite", display_order = 6)]
    Suite {
        #[command(flatten)]
        mode: ModeArgs,
        /// Suite name: pbfhogg, elivagar, or nidhogg
        name: String,
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PBF variant to use
        #[arg(long, default_value = "indexed")]
        variant: String,
    },
    /// Build and run with passthrough args (deprecated — use `run` subcommands instead)
    #[command(name = "passthrough", display_order = 99, hide = true)]
    Passthrough {
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
    #[command(
        display_order = 3,
        long_about = "\
Query benchmark results from .brokkr/results.db.

Examples:
  brokkr results                                    # last 20 results
  brokkr results -n 50                              # last 50 results
  brokkr results 0b74fb6f                           # look up by UUID prefix
  brokkr results --commit a65a                      # filter by commit prefix
  brokkr results --command 'bench read'             # filter by command
  brokkr results --variant pipelined                # filter by variant (substring match)
  brokkr results --dataset europe                   # filter by dataset (substring match on input file)
  brokkr results --command tags-filter --dataset eu # combine filters
  brokkr results --compare a65a 911c                # compare two commits
  brokkr results --compare a65a 911c --variant sync # compare, filtered
  brokkr results --compare-last                     # compare two most recent commits
  brokkr results --compare-last --command hotpath   # compare hotpath runs (shows function diff)"
    )]
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

        /// Filter by dataset (substring match on input filename, e.g. "europe"
        /// or "eu" matches "europe-20260301-seq4714-with-indexdata.osm")
        #[arg(long)]
        dataset: Option<String>,

        /// Filter by metadata key=value (multiple allowed, AND semantics).
        /// The key is the user-facing name without the `meta.` prefix
        /// (e.g. `--meta format=osc` matches rows with `meta.format = "osc"`).
        /// Rows missing the key are silently excluded.
        #[arg(long, value_parser = validate_meta_filter)]
        meta: Vec<String>,

        /// Maximum number of results to show
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,

        /// Maximum number of functions shown in hotpath reports (0 = all)
        #[arg(long, default_value = "10")]
        top: usize,

        /// Output sidecar samples as JSONL (requires UUID argument)
        #[arg(long, requires = "query", conflicts_with = "markers")]
        timeline: bool,

        /// Output sidecar markers as JSONL (requires UUID argument)
        #[arg(long, requires = "query", conflicts_with = "timeline")]
        markers: bool,

        /// Show per-phase summary table (use with --timeline)
        #[arg(long, requires = "timeline")]
        summary: bool,

        /// Show duration between _START/_END marker pairs (use with --markers)
        #[arg(long, requires = "markers")]
        durations: bool,

        /// Show phase pairs with duration + peak RSS/majflt from samples (use with --markers)
        #[arg(long, requires = "markers")]
        phases: bool,

        /// Show application-level counters (use with --markers)
        #[arg(long, requires = "markers")]
        counters: bool,

        /// Output only these fields (comma-separated, e.g. "t,rss,anon,majflt")
        #[arg(long, requires = "timeline", value_delimiter = ',')]
        fields: Vec<String>,

        /// Output every Nth sample (downsample)
        #[arg(long, requires = "timeline")]
        every: Option<usize>,

        /// Output only the first N samples
        #[arg(long, requires = "timeline")]
        head: Option<usize>,

        /// Output only the last N samples
        #[arg(long, requires = "timeline")]
        tail: Option<usize>,

        /// Filter samples where a field meets a condition (e.g. "majflt>0", "anon>100000")
        #[arg(long, requires = "timeline", name = "COND")]
        r#where: Option<String>,

        /// Compute min/max/avg/p50/p95 for a field
        #[arg(long, requires = "timeline")]
        stat: Option<String>,

        /// Filter to samples within a marker phase (e.g. "STAGE2")
        #[arg(long, requires = "timeline")]
        phase: Option<String>,

        /// Filter by time range in seconds (e.g. "10.0..82.0")
        #[arg(long, requires = "timeline")]
        range: Option<String>,

        /// Show a specific run index (0-based), or "all" for all runs. Defaults to the best run.
        #[arg(long, requires = "timeline")]
        run: Option<String>,

        /// Compare sidecar timelines of two results (phase-aligned summary)
        #[arg(long, num_args = 2, value_names = ["UUID_A", "UUID_B"],
              conflicts_with_all = ["query", "commit", "compare", "compare_last", "timeline", "markers"])]
        compare_timeline: Option<Vec<String>>,
    },
    /// Clean build artifacts and scratch data
    #[command(display_order = 4)]
    Clean,
    /// Show lock status (who holds the benchmark lock)
    #[command(display_order = 5)]
    Lock,
    /// Browse command history
    #[command(
        display_order = 6,
        long_about = "\
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
  brokkr history --slow 10000           # commands that took >10s"
    )]
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
    /// [pbfhogg] Download a dataset from Geofabrik or planet.openstreetmap.org
    #[command(display_order = 20)]
    Download {
        /// Region name or Geofabrik path (e.g. denmark, europe/france, asia/japan/kanto)
        region: String,

        /// Download OSC diffs up to this sequence number
        #[arg(long)]
        osc_seq: Option<u64>,

        /// Register the download as an additional snapshot of an existing
        /// dataset rather than (re-)populating the dataset's primary entry.
        ///
        /// Requires the dataset to already exist (run `brokkr download <region>`
        /// first to create the primary entry). The snapshot key must match
        /// `[a-zA-Z0-9_-]+` and cannot be `base` (reserved for the dataset's
        /// legacy/primary data).
        ///
        /// Files are written with snapshot-specific names and registered under
        /// `[<host>.datasets.<region>.snapshot.<key>]` in `brokkr.toml`.
        #[arg(long, value_parser = validate_snapshot_key_arg)]
        as_snapshot: Option<String>,
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
    /// [elivagar] Download Natural Earth shapefiles for low-zoom layers
    #[command(display_order = 32)]
    DownloadNaturalEarth,
    /// Print PMTiles v3 file statistics
    #[command(display_order = 19)]
    PmtilesStats {
        /// PMTiles file(s) to analyze
        #[arg(required = true)]
        files: Vec<String>,
    },
    /// [nidhogg] Start the server
    #[command(
        display_order = 40,
        long_about = "\
Start the nidhogg server. Builds the binary, kills any existing instance, \
spawns a background process, and waits for the health endpoint. \
Stop it with `brokkr stop`.

Use --dataset to select which dataset from brokkr.toml to serve. The \
dataset determines what features are available:

  brokkr serve --dataset denmark    query + geocode + tiles (has both)
  brokkr serve --dataset norway     tiles only (has pmtiles, no data_dir)

The other flags (--data-dir, --tiles) override or supplement what the \
dataset provides. You almost always just need --dataset.

Tiles (--tiles):
  Omitted        Auto-selects if the dataset has exactly one pmtiles \
entry, skipped if none configured
  <variant>      Looks up pmtiles.<variant> in the dataset's config \
(e.g. \"elivagar\")
  <path>         Direct file path (detected by / or .pmtiles extension)
  none           Explicitly disables tile serving even if dataset has pmtiles

Data directory (--data-dir):
  Omitted        Resolved from the dataset's data_dir field in brokkr.toml
  <dir>          Override with an explicit directory path

If neither data_dir nor tiles are available (from the dataset or overrides), \
the server has nothing to serve and will error.

Examples:
  brokkr serve                                  # denmark (default), auto-detect
  brokkr serve --dataset norway                 # tiles only (no data_dir)
  brokkr serve --dataset denmark --tiles none   # query + geocode, no tiles
  brokkr serve --tiles elivagar                 # explicit pmtiles variant
  brokkr serve --tiles ./data/custom.pmtiles    # direct file path
  brokkr serve --data-dir /mnt/fast/nidhogg     # override data directory"
    )]
    Serve {
        /// Override data directory path (ingested disk format).
        /// If omitted, resolved from the dataset's data_dir in brokkr.toml.
        /// When the dataset has no data_dir, the server starts without a
        /// disk store (tiles-only mode).
        #[arg(long, value_name = "DIR")]
        data_dir: Option<String>,

        /// Dataset name from brokkr.toml
        #[arg(long, value_name = "NAME", default_value = "denmark")]
        dataset: String,

        /// PMTiles to serve: a variant name from config, a file path, or
        /// "none" to disable. Auto-selects if the dataset has exactly one
        /// pmtiles entry; skipped if none are configured.
        #[arg(long, value_name = "VARIANT|PATH|none")]
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
    // ----- visual testing commands (litehtml + sluggrs, display_order = 50) -----
    /// [litehtml/sluggrs] Run visual tests against reference artifacts
    #[command(display_order = 50)]
    Test {
        /// Fixture or snapshot ID (or unique prefix)
        #[arg(value_name = "ID")]
        fixture: Option<String>,

        /// Run all fixtures tagged with this suite name (litehtml only)
        #[arg(long, conflicts_with = "all")]
        suite: Option<String>,

        /// Run all fixtures/snapshots
        #[arg(long)]
        all: bool,

        /// Force-regenerate Chrome reference artifacts before comparing (litehtml only)
        #[arg(long)]
        recapture: bool,
    },
    /// [litehtml/sluggrs] List fixtures/snapshots and approval state
    #[command(display_order = 50)]
    List,
    /// [litehtml/sluggrs] Record current output as accepted baseline (requires clean git tree)
    #[command(display_order = 50)]
    Approve {
        /// Fixture/snapshot ID (or unique prefix)
        fixture: String,
    },
    /// [litehtml/sluggrs] Show detailed results for a past run
    #[command(display_order = 50)]
    Report {
        /// Run ID (or prefix)
        run_id: String,
    },
    /// [litehtml/sluggrs] Show current state of all fixtures/snapshots
    #[command(name = "visual-status", display_order = 50)]
    VisualStatus,

    // ----- litehtml-only commands (display_order = 51) -----
    /// [litehtml] Normalize raw email HTML into a self-contained fixture
    #[command(display_order = 51)]
    Prepare {
        /// Input HTML file (raw email)
        input: String,
        /// Output HTML file (self-contained fixture)
        output: String,
    },
    /// [litehtml] Extract a sub-fixture from a prepared HTML file
    #[command(name = "html-extract", display_order = 51)]
    HtmlExtract {
        /// Input HTML file (already prepared)
        input: String,
        /// CSS selector to extract (single element)
        #[arg(long, conflicts_with_all = ["from", "to"])]
        selector: Option<String>,
        /// Start of sibling range to extract (inclusive)
        #[arg(long, requires = "to", conflicts_with = "selector")]
        from: Option<String>,
        /// End of sibling range to extract (inclusive)
        #[arg(long, requires = "from", conflicts_with = "selector")]
        to: Option<String>,
        /// Output HTML file (extracted sub-fixture)
        output: String,
    },
    /// [litehtml] Print structural outline of a prepared HTML file
    #[command(display_order = 51)]
    Outline {
        /// Input HTML file (prepared)
        input: String,
        /// Maximum nesting depth before collapsing (default: 4)
        #[arg(long, default_value = "4")]
        depth: usize,
        /// Show full tree with no depth limit
        #[arg(long)]
        full: bool,
        /// Print suggested CSS selectors for top-level sections
        #[arg(long)]
        selectors: bool,
    },

    // ----- sluggrs-only commands (display_order = 55) -----
    /// [sluggrs] Rendering hotpath (defaults to --hotpath 1, use --alloc for allocation tracking)
    #[command(name = "hotpath", display_order = 55)]
    Hotpath {
        /// Per-function allocation tracking instead of timing
        #[arg(long)]
        alloc: bool,

        /// Number of runs
        #[arg(long, short = 'n', default_value = "1")]
        runs: usize,

        /// Example binary to build and run (default: hotpath)
        #[arg(long, default_value = "hotpath")]
        target: String,

        /// Print full output
        #[arg(short, long)]
        verbose: bool,

        /// Run even if the git tree is dirty (results will not be stored)
        #[arg(long)]
        force: bool,

        /// Skip memory availability check
        #[arg(long)]
        no_mem_check: bool,

        /// Wait for the lock instead of failing immediately
        #[arg(long)]
        wait: bool,
    },
}

// ---------------------------------------------------------------------------
// Shared mode args (measurement/build flags for all measurable commands)
// ---------------------------------------------------------------------------

#[derive(Args, Clone)]
pub(crate) struct ModeArgs {
    /// Full benchmark: lockfile, N runs (default 3), DB storage
    #[arg(long, num_args = 0..=1, default_missing_value = "3")]
    pub(crate) bench: Option<usize>,

    /// Function-level timing via hotpath feature (optional run count, default 1)
    #[arg(long, num_args = 0..=1, default_missing_value = "1")]
    pub(crate) hotpath: Option<usize>,

    /// Per-function allocation tracking via hotpath-alloc feature (optional run count, default 1)
    #[arg(long, num_args = 0..=1, default_missing_value = "1")]
    pub(crate) alloc: Option<usize>,

    /// Print full build/bench/result output
    #[arg(short, long)]
    pub(crate) verbose: bool,

    /// Build and benchmark an old commit via git worktree
    #[arg(long)]
    pub(crate) commit: Option<String>,

    /// Cargo features to enable (e.g. linux-io-uring)
    #[arg(long, value_delimiter = ',')]
    pub(crate) features: Vec<String>,

    /// Run even if the git tree is dirty (results will not be stored)
    #[arg(long)]
    pub(crate) force: bool,

    /// Skip memory availability check
    #[arg(long)]
    pub(crate) no_mem_check: bool,

    /// Wait for the lock instead of failing immediately
    #[arg(long)]
    pub(crate) wait: bool,
}

// ---------------------------------------------------------------------------
// Shared args for pbfhogg measured commands
// ---------------------------------------------------------------------------

#[derive(Args, Clone)]
pub(crate) struct PbfArgs {
    /// Dataset name from brokkr.toml
    #[arg(long, default_value = "denmark")]
    pub(crate) dataset: String,
    /// PBF variant to use (raw, indexed, locations)
    #[arg(long, default_value = "indexed")]
    pub(crate) variant: String,
    /// Use O_DIRECT for file I/O (requires linux-direct-io feature in pbfhogg)
    #[arg(long)]
    pub(crate) direct_io: bool,
    /// Use io_uring for I/O (requires linux-io-uring feature in pbfhogg)
    #[arg(long)]
    pub(crate) io_uring: bool,
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
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
    },
    /// [pbfhogg] Cross-validate cat (type filters) against osmium cat
    #[command(display_order = 1)]
    Cat {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
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
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
    },
    /// [pbfhogg] Cross-validate multi-extract (single-pass vs sequential)
    #[command(name = "multi-extract", display_order = 2)]
    MultiExtract {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        bbox: Option<String>,
        /// Number of non-overlapping bbox regions
        #[arg(long, default_value = "5")]
        regions: usize,
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
    },
    /// [pbfhogg] Cross-validate tags-filter against osmium tags-filter
    #[command(display_order = 3)]
    TagsFilter {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
    },
    /// [pbfhogg] Cross-validate getid/getid --invert against osmium getid
    #[command(display_order = 4)]
    GetidRemoveid {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
    },
    /// [pbfhogg] Cross-validate add-locations-to-ways against osmium
    #[command(display_order = 5)]
    AddLocationsToWays {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
    },
    /// [pbfhogg] Cross-validate check --refs against osmium check-refs
    #[command(display_order = 6)]
    CheckRefs {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        /// Use O_DIRECT for reads (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
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
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
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
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
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
        /// Use O_DIRECT for writes (requires linux-direct-io feature in pbfhogg)
        #[arg(long)]
        direct_io: bool,
    },

    /// [elivagar] Verify PMTiles output integrity
    #[command(name = "pmtiles", display_order = 15)]
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


/// Validate `--as-snapshot` key: matches `[a-zA-Z0-9_-]+` and is not the
/// reserved sentinel `base`. Delegates to `config::validate_snapshot_key`
/// so the parse-time and CLI-time rules stay in sync.
fn validate_snapshot_key_arg(s: &str) -> Result<String, String> {
    crate::config::validate_snapshot_key(s)?;
    Ok(s.to_owned())
}

/// Validate `--meta key=value`: must contain exactly one `=`. Both sides may
/// be empty (the empty-value case is legitimate for filtering rows where the
/// stored value is the empty string).
fn validate_meta_filter(s: &str) -> Result<String, String> {
    if !s.contains('=') {
        return Err(format!(
            "expected key=value, got '{s}' (use --meta KEY=VALUE)"
        ));
    }
    Ok(s.to_owned())
}

/// Validate `--osc-range` format: `LO..HI` where both are non-negative integers and LO <= HI.
fn validate_osc_range(s: &str) -> Result<String, String> {
    let (lo_s, hi_s) = s
        .split_once("..")
        .ok_or_else(|| format!("expected LO..HI, got '{s}'"))?;
    let lo: u64 = lo_s
        .parse()
        .map_err(|e| format!("invalid LO '{lo_s}': {e}"))?;
    let hi: u64 = hi_s
        .parse()
        .map_err(|e| format!("invalid HI '{hi_s}': {e}"))?;
    if lo > hi {
        return Err(format!("LO ({lo}) must be <= HI ({hi})"));
    }
    Ok(s.to_owned())
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
        Err(format!(
            "invalid date format '{s}', expected YYYY-MM-DD or YYYY-MM-DD HH:MM:SS"
        ))
    }
}

// ---------------------------------------------------------------------------
// Pbfhogg command extraction
// ---------------------------------------------------------------------------

use crate::pbfhogg::commands::PbfhoggCommand;
use std::collections::HashMap;

impl Command {
    /// Extract the pbfhogg measured-command parts from a CLI command variant.
    ///
    /// Returns `None` for non-pbfhogg commands (elivagar, nidhogg, shared, etc.).
    /// The returned tuple is `(mode, pbf, command, osc_seq, extra_params)`.
    #[allow(clippy::too_many_lines, clippy::type_complexity)]
    pub(crate) fn as_pbfhogg(
        &self,
    ) -> Option<(
        &ModeArgs,
        &PbfArgs,
        PbfhoggCommand,
        Option<&str>,
        HashMap<String, String>,
    )> {
        let empty = HashMap::new();
        match self {
            // Simple commands: mode + pbf, no extras
            Self::Inspect { mode, pbf } => Some((mode, pbf, PbfhoggCommand::Inspect, None, empty)),
            Self::InspectNodes { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::InspectNodes, None, empty))
            }
            Self::InspectTags { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::InspectTags, None, empty))
            }
            Self::InspectTagsWay { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::InspectTagsWay, None, empty))
            }
            Self::CheckRefs { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::CheckRefs, None, empty))
            }
            Self::CheckIds { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::CheckIds, None, empty))
            }
            Self::Sort { mode, pbf } => Some((mode, pbf, PbfhoggCommand::Sort, None, empty)),
            Self::CatWay { mode, pbf } => Some((mode, pbf, PbfhoggCommand::CatWay, None, empty)),
            Self::CatRelation { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::CatRelation, None, empty))
            }
            Self::CatDedupe { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::CatDedupe, None, empty))
            }
            Self::TagsFilterWay { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::TagsFilterWay, None, empty))
            }
            Self::TagsFilterAmenity { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::TagsFilterAmenity, None, empty))
            }
            Self::TagsFilterTwopass { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::TagsFilterTwopass, None, empty))
            }
            Self::Getid { mode, pbf } => Some((mode, pbf, PbfhoggCommand::Getid, None, empty)),
            Self::GetidRefs { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::GetidRefs, None, empty))
            }
            Self::Getparents { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::Getparents, None, empty))
            }
            Self::GetidInvert { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::GetidInvert, None, empty))
            }
            Self::Renumber { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::Renumber, None, empty))
            }
            Self::ExtractSimple { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::ExtractSimple, None, empty))
            }
            Self::ExtractComplete { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::ExtractComplete, None, empty))
            }
            Self::ExtractSmart { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::ExtractSmart, None, empty))
            }
            Self::MultiExtract {
                mode,
                pbf,
                regions,
            } => {
                let mut params = HashMap::new();
                params.insert("regions".into(), regions.to_string());
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::MultiExtract { regions: *regions },
                    None,
                    params,
                ))
            }
            Self::TimeFilter { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::TimeFilter, None, empty))
            }
            Self::BuildGeocodeIndex { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::BuildGeocodeIndex, None, empty))
            }

            // Commands with OSC sequence
            Self::TagsFilterOsc { mode, pbf, osc_seq } => Some((
                mode,
                pbf,
                PbfhoggCommand::TagsFilterOsc,
                osc_seq.as_deref(),
                empty,
            )),
            Self::MergeChanges {
                mode,
                pbf,
                osc_seq,
                osc_range,
            } => {
                let mut params = HashMap::new();
                if let Some(r) = osc_range {
                    params.insert("osc_range".into(), r.clone());
                }
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::MergeChanges,
                    osc_seq.as_deref(),
                    params,
                ))
            }
            Self::ApplyChanges { mode, pbf, osc_seq } => Some((
                mode,
                pbf,
                PbfhoggCommand::ApplyChanges,
                osc_seq.as_deref(),
                empty,
            )),
            Self::Diff {
                mode,
                pbf,
                osc_seq,
                keep_cache,
            } => {
                let mut params = HashMap::new();
                if *keep_cache {
                    params.insert("keep_cache".into(), "true".into());
                }
                Some((mode, pbf, PbfhoggCommand::Diff, osc_seq.as_deref(), params))
            }
            Self::DiffOsc {
                mode,
                pbf,
                osc_seq,
                keep_cache,
            } => {
                let mut params = HashMap::new();
                if *keep_cache {
                    params.insert("keep_cache".into(), "true".into());
                }
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::DiffOsc,
                    osc_seq.as_deref(),
                    params,
                ))
            }

            // Command with extra params
            Self::AddLocationsToWays {
                mode,
                pbf,
                index_type,
            } => {
                let mut params = HashMap::new();
                if let Some(it) = index_type {
                    params.insert("index_type".into(), it.clone());
                }
                Some((mode, pbf, PbfhoggCommand::AddLocationsToWays, None, params))
            }

            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_compare_last_conflicts_with_query() {
        let parsed = Cli::try_parse_from(["brokkr", "results", "abc123", "--compare-last"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn results_compare_requires_two_commits() {
        let parsed = Cli::try_parse_from(["brokkr", "results", "--compare", "abc123"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn pmtiles_stats_requires_at_least_one_file() {
        let parsed = Cli::try_parse_from(["brokkr", "pmtiles-stats"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn inspect_tags_accepts_mode_flags() {
        let parsed =
            Cli::try_parse_from(["brokkr", "inspect-tags", "--hotpath", "--dataset", "japan"])
                .expect("parse");

        let Command::InspectTags { mode, pbf } = parsed.command else {
            panic!("expected inspect-tags command");
        };
        assert!(mode.hotpath.is_some());
        assert_eq!(pbf.dataset, "japan");
    }
}
