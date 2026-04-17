use clap::{Args, Parser, Subcommand, ValueEnum};

/// Index mode selection for `verify add-locations-to-ways`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum AltwMode {
    Hash,
    Sparse,
    Dense,
    External,
    All,
}

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

        /// Show unfiltered cargo output
        #[arg(long, conflicts_with = "json")]
        raw: bool,

        /// Emit NDJSON diagnostics and summaries
        #[arg(long, conflicts_with = "raw")]
        json: bool,

        /// Raw arguments forwarded to `cargo test`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show environment information
    #[command(display_order = 1)]
    Env,
    // ----- pbfhogg tool CLI commands (display_order = 2) -----
    /// [pbfhogg] Inspect PBF. Flags select mode:
    ///   no flag   → metadata (block count / bbox / stats)
    ///   `--nodes` → node statistics
    ///   `--tags`  → tag frequencies (optionally narrowed by
    ///               `--type node|way|relation`)
    #[command(name = "inspect", display_order = 2)]
    Inspect {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Show node statistics (mutually exclusive with `--tags`).
        #[arg(long, conflicts_with = "tags")]
        nodes: bool,
        /// Show tag frequencies (mutually exclusive with `--nodes`).
        #[arg(long)]
        tags: bool,
        /// Restrict `--tags` to a single object type.
        #[arg(
            long = "type",
            value_name = "KIND",
            value_parser = ["node", "way", "relation"],
            requires = "tags",
        )]
        type_filter: Option<String>,
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
    /// [pbfhogg] Cat passthrough. Flags are orthogonal:
    ///   `--type way|relation` restricts output to one object kind;
    ///   `--dedupe` runs the two-input dedupe path (and only this
    ///     combination supports `--io-uring`);
    ///   `--clean` forces the full-decode / re-frame Framed path
    ///     instead of Raw passthrough.
    #[command(name = "cat", display_order = 2)]
    Cat {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Restrict output to a single object kind (way or relation).
        #[arg(
            long = "type",
            value_name = "KIND",
            value_parser = ["way", "relation"],
        )]
        type_filter: Option<String>,
        /// Run `cat --dedupe` with two PBF inputs.
        #[arg(long)]
        dedupe: bool,
        /// Force the full-decode / re-frame Framed path (cat_filtered).
        #[arg(long)]
        clean: bool,
    },
    /// [pbfhogg] Tags filter. Orthogonal flags:
    ///   `--filter EXPR` — pbfhogg filter expression (default
    ///     `w/highway=primary`). Examples: `amenity=restaurant`,
    ///     `highway=primary`, `w/building=yes`.
    ///   `-R` / `--omit-referenced` — single-pass; drop referenced
    ///     objects (default: two-pass with references).
    ///   `--input-kind osc` — read an OSC diff instead of a PBF.
    #[command(name = "tags-filter", display_order = 2)]
    TagsFilter {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Filter expression passed through to pbfhogg tags-filter.
        #[arg(long, default_value = "w/highway=primary")]
        filter: String,
        /// Single-pass filter: match objects only, drop referenced ones.
        /// Not valid with `--input-kind osc` (pbfhogg rejects it at runtime).
        #[arg(short = 'R', long = "omit-referenced", conflicts_with = "input_kind")]
        omit_referenced: bool,
        /// Read an OSC diff as input instead of a PBF.
        #[arg(long = "input-kind", value_parser = ["pbf", "osc"])]
        input_kind: Option<String>,
        /// OSC sequence number from brokkr.toml (only used with
        /// `--input-kind osc`).
        #[arg(long)]
        osc_seq: Option<String>,
        /// Snapshot key to read from (OSC input). Use `base` (or omit)
        /// for the primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Get elements by hardcoded ID set. Flags:
    ///   `--add-referenced` — also pull in referenced objects (two-pass);
    ///   `--invert` — select everything NOT in the ID set.
    #[command(name = "getid", display_order = 2)]
    Getid {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Two-pass: include objects referenced by the matched set.
        /// Mutually exclusive with `--invert` (pbfhogg rejects the combo).
        #[arg(long = "add-referenced", conflicts_with = "invert")]
        add_referenced: bool,
        /// Select everything NOT in the hardcoded ID set.
        #[arg(long)]
        invert: bool,
    },
    /// [pbfhogg] Get parent elements
    #[command(name = "getparents", display_order = 2)]
    Getparents {
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
        /// Snapshot key to read OSCs from. Use `base` (or omit) for the
        /// dataset's primary/legacy OSC chain; pass a snapshot key to read
        /// from a historical snapshot's OSC table.
        #[arg(long)]
        snapshot: Option<String>,
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
        /// Snapshot key to read PBF and OSC from. Use `base` (or omit) for
        /// the dataset's primary/legacy data; pass a snapshot key registered
        /// under `[dataset.snapshot.<key>]` to read from a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
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
        /// Start from stage N, skipping earlier stages (2-4; requires a prior --keep-scratch run)
        #[arg(long, value_parser = validate_start_stage, requires = "index_type")]
        start_stage: Option<String>,
        /// Preserve the external join scratch directory for subsequent --start-stage invocations
        #[arg(long, requires = "index_type")]
        keep_scratch: bool,
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
        /// Source bounding box to carve regions from (lon_min,lat_min,lon_max,lat_max).
        /// Falls back to the dataset's configured bbox if omitted.
        #[arg(long)]
        bbox: Option<String>,
    },
    /// [pbfhogg] Filter by timestamp
    #[command(name = "time-filter", display_order = 2)]
    TimeFilter {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
    },
    /// [pbfhogg] Diff base PBF against the applied-changes merged PBF.
    /// `--format osc` switches output from summary (stdout) to an OSC
    /// file. The brokkr runner generates the merged PBF from base +
    /// OSC before diffing.
    #[command(name = "diff", display_order = 2)]
    Diff {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Output format: `default` (summary diff) or `osc` (OSC-format
        /// diff written to scratch).
        #[arg(long, default_value_t, value_enum)]
        format: crate::pbfhogg::commands::DiffFormat,
        /// OSC sequence number from brokkr.toml
        #[arg(long)]
        osc_seq: Option<String>,
        /// Reuse the cached merged PBF in measured modes (default: rebuild
        /// before bench/hotpath/alloc so total invocation wall time is
        /// reproducible). No-op in run mode (cache is always reused there).
        #[arg(long)]
        keep_cache: bool,
        /// Snapshot key to read PBF and OSC from. Use `base` (or omit) for
        /// the dataset's primary/legacy data; pass a snapshot key to read
        /// from a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
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
        #[arg(long, default_value_t, value_enum)]
        format: crate::pbfhogg::commands::DiffFormat,
    },
    /// [pbfhogg] Diff two PBFs (OSC output)
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
        /// Compressions to benchmark (comma-separated: none,zlib:6,zstd:3)
        #[arg(long, default_value = "none,zlib:6,zstd:3")]
        compressions: String,
    },
    /// [pbfhogg] Merge benchmark
    #[command(name = "merge", display_order = 2)]
    MergeBench {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Compressions to benchmark (comma-separated: zlib,none)
        #[arg(long, default_value = "zlib,none")]
        compressions: String,
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
  brokkr results --command read                     # last 20 matching 'read'
  brokkr results 0b74fb6f                           # look up by UUID prefix
  brokkr results --commit a65a                      # filter by commit prefix
  brokkr results --command 'bench read'             # filter by command
  brokkr results --mode hotpath                     # filter by measurement mode
  brokkr results --dataset europe                   # filter by dataset (substring match on input file)
  brokkr results --command tags-filter --dataset eu # combine filters
  brokkr results --compare a65a 911c                # compare two commits
  brokkr results --compare a65a 911c --mode bench   # compare, filtered
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

        /// Compare the two most recent commits (use with --command/--mode to narrow)
        #[arg(long, conflicts_with_all = ["query", "commit", "compare"])]
        compare_last: bool,

        /// Filter by command (substring match, e.g. "read" matches "bench read")
        #[arg(long)]
        command: Option<String>,

        /// Filter by measurement mode (substring match; exact values are
        /// `bench`, `hotpath`, `alloc`). `--variant` accepted as a legacy
        /// alias for muscle memory from the pre-rename days.
        #[arg(long, alias = "variant")]
        mode: Option<String>,

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

        /// Substring match against either the subprocess invocation
        /// (`cli_args`) or the brokkr invocation (`brokkr_args`). Think
        /// `git log --grep`: a single token that scans both
        /// freeform-invocation columns. E.g. `--grep zstd:1`.
        #[arg(long)]
        grep: Option<String>,

        /// Maximum number of results to show
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,

        /// Maximum number of functions shown in hotpath reports (0 = all)
        #[arg(long, default_value = "10")]
        top: usize,
    },
    /// Query sidecar /proc timelines, markers, and phase summaries
    #[command(
        display_order = 4,
        long_about = "\
Query sidecar data captured in .brokkr/sidecar.db during `--bench`,
`--hotpath`, and `--alloc` runs. A UUID prefix is required — use
`brokkr results` to find one. `--run N|all` picks a specific run
within the result (default: best run).

The `dirty` pseudo-UUID resolves to the most recent forced or failed
run — runs produced via `--force` (dirty tree) or that exited non-zero
have no results.db row, but their sidecar data is still stored and
reachable this way.

Examples:
  brokkr sidecar <uuid>                               # per-phase summary (default view)
  brokkr sidecar dirty                                # the last forced/failed run
  brokkr sidecar <uuid> --human                       # same, as a fixed-width table
  brokkr sidecar <uuid> --samples                     # raw /proc sample stream (JSONL)
  brokkr sidecar <uuid> --samples --phase STAGE2      # samples within a marker phase
  brokkr sidecar <uuid> --markers                     # raw marker events (JSONL)
  brokkr sidecar <uuid> --durations                   # START/END pair timings
  brokkr sidecar <uuid> --counters                    # application counters
  brokkr sidecar <uuid> --stat rss                    # min/max/avg/p50/p95 for a field
  brokkr sidecar --compare a65a 911c                  # two results, phase-aligned"
    )]
    Sidecar {
        /// UUID prefix to look up (required; use `brokkr results` to find one)
        #[arg(required_unless_present = "compare")]
        query: Option<String>,

        /// Raw /proc samples as JSONL (one record per 100ms sample)
        #[arg(long, conflicts_with_all = ["markers", "durations", "counters", "stat", "compare"])]
        samples: bool,

        /// Raw marker events as JSONL
        #[arg(long, conflicts_with_all = ["samples", "durations", "counters", "stat", "compare"])]
        markers: bool,

        /// START/END marker-pair durations
        #[arg(long, conflicts_with_all = ["samples", "markers", "counters", "stat", "compare"])]
        durations: bool,

        /// Application-level counters
        #[arg(long, conflicts_with_all = ["samples", "markers", "durations", "stat", "compare"])]
        counters: bool,

        /// Compute min/max/avg/p50/p95 for a /proc field (e.g. `--stat rss`)
        #[arg(long, conflicts_with_all = ["samples", "markers", "durations", "counters", "compare"])]
        stat: Option<String>,

        /// Compare two results phase-by-phase (no UUID argument)
        #[arg(long, num_args = 2, value_names = ["UUID_A", "UUID_B"],
              conflicts_with_all = ["query", "samples", "markers", "durations", "counters", "stat"])]
        compare: Option<Vec<String>>,

        /// Render as a fixed-width table where a human layout exists
        /// (default view and --compare). JSONL views are unaffected.
        #[arg(long)]
        human: bool,

        /// Show a specific run index (0-based), or "all" for all runs. Defaults to the best run.
        #[arg(long)]
        run: Option<String>,

        /// Filter samples to a marker phase (e.g. "STAGE2")
        #[arg(long, conflicts_with_all = ["markers", "durations", "counters", "compare"])]
        phase: Option<String>,

        /// Filter samples by time range in seconds (e.g. "10.0..82.0")
        #[arg(long, conflicts_with_all = ["markers", "durations", "counters", "compare"])]
        range: Option<String>,

        /// Filter samples where a field meets a condition (e.g. "majflt>0", "anon>100000")
        #[arg(long, name = "COND",
              conflicts_with_all = ["markers", "durations", "counters", "compare"])]
        r#where: Option<String>,

        /// Output only these fields (comma-separated, e.g. "t,rss,anon,majflt"). Only with --samples.
        #[arg(long, value_delimiter = ',', requires = "samples")]
        fields: Vec<String>,

        /// Output every Nth sample (downsample). Only with --samples.
        #[arg(long, requires = "samples")]
        every: Option<usize>,

        /// Output only the first N samples. Only with --samples.
        #[arg(long, requires = "samples")]
        head: Option<usize>,

        /// Output only the last N samples. Only with --samples.
        #[arg(long, requires = "samples")]
        tail: Option<usize>,
    },
    /// Clean build artifacts and scratch data
    #[command(display_order = 5)]
    Clean,
    /// Show lock status (who holds the benchmark lock)
    #[command(display_order = 6)]
    Lock,
    /// Browse command history
    #[command(
        display_order = 7,
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
        #[arg(long, value_parser = validate_snapshot_key_arg, conflicts_with = "refresh")]
        as_snapshot: Option<String>,

        /// Rotate the dataset to a newer upstream snapshot. Archives the
        /// existing primary pbf/osc data into a `[snapshot.<key>]` block
        /// (key derived from download_date or file mtime), then downloads
        /// the new PBF and resets the OSC chain. HEAD-checks upstream
        /// `Last-Modified` first and no-ops if the upstream isn't newer
        /// (use `--force` to rotate anyway).
        ///
        /// Mutually exclusive with `--as-snapshot`.
        #[arg(long, conflicts_with = "as_snapshot")]
        refresh: bool,

        /// Force `--refresh` to rotate even when the upstream Last-Modified
        /// header is not newer than the existing pbf.raw's mtime. Use when
        /// the heuristic gets it wrong (e.g. file mtime was touched by an
        /// rsync, or you want to re-download for some other reason).
        ///
        /// Only meaningful with `--refresh`. Clap rejects it on plain
        /// `download` and `download --as-snapshot` to avoid silently
        /// ignoring a flag the user explicitly typed.
        #[arg(long, requires = "refresh")]
        force: bool,
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

    /// Validate argv, config, and path resolution without building or running.
    /// Short-circuits after path/arg-vector construction. Skips cargo build,
    /// lock acquisition, and process execution. Useful for sanity-checking a
    /// script of queued benches before leaving it overnight.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Kill the child process when this marker is emitted via the sidecar FIFO.
    /// Useful for benchmarking only a specific phase of execution.
    #[arg(long)]
    pub(crate) stop: Option<String>,
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
    /// Output compression: zlib:N (N=1-9), zstd:N, or none
    #[arg(long, value_parser = validate_compression)]
    pub(crate) compression: Option<String>,
}

/// Shared dataset/variant/direct_io args for pbfhogg verify subcommands.
/// (Verify doesn't take `--io-uring` or `--compression` — different surface
/// than `PbfArgs`, hence a separate struct.)
#[derive(Args, Clone)]
pub(crate) struct VerifyPbfArgs {
    /// Dataset name from brokkr.toml
    #[arg(long, default_value = "denmark")]
    pub(crate) dataset: String,
    /// PBF variant to use (raw, indexed, locations)
    #[arg(long, default_value = "indexed")]
    pub(crate) variant: String,
    /// Use O_DIRECT for file I/O (requires linux-direct-io feature in pbfhogg)
    #[arg(long)]
    pub(crate) direct_io: bool,
}

#[derive(Subcommand)]
pub(crate) enum VerifyCommand {
    /// [pbfhogg] Cross-validate sort against osmium sort
    #[command(display_order = 0)]
    Sort {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
    },
    /// [pbfhogg] Cross-validate cat (type filters) against osmium cat
    #[command(display_order = 1)]
    Cat {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
    },
    /// [pbfhogg] Cross-validate extract (bbox strategies) against osmium extract
    #[command(display_order = 2)]
    Extract {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
        #[arg(long)]
        bbox: Option<String>,
    },
    /// [pbfhogg] Cross-validate multi-extract (single-pass vs sequential)
    #[command(name = "multi-extract", display_order = 2)]
    MultiExtract {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
        #[arg(long)]
        bbox: Option<String>,
        /// Number of non-overlapping bbox regions
        #[arg(long, default_value = "5")]
        regions: usize,
    },
    /// [pbfhogg] Cross-validate tags-filter against osmium tags-filter
    #[command(display_order = 3)]
    TagsFilter {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
    },
    /// [pbfhogg] Cross-validate getid/getid --invert against osmium getid
    #[command(display_order = 4)]
    GetidRemoveid {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
    },
    /// [pbfhogg] Cross-validate add-locations-to-ways against osmium
    #[command(display_order = 5)]
    AddLocationsToWays {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
        /// Which index modes to verify. `all` runs hash, sparse, dense, external.
        #[arg(long, value_enum, default_value = "all")]
        mode: AltwMode,
    },
    /// [pbfhogg] Cross-validate check --refs against osmium check-refs
    #[command(display_order = 6)]
    CheckRefs {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
    },
    /// [pbfhogg] Cross-validate apply-changes against osmium/osmosis/osmconvert
    #[command(display_order = 7)]
    Merge {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
        #[arg(long)]
        osc_seq: Option<String>,
    },
    /// [pbfhogg] Cross-validate diff --format osc roundtrip against osmium
    #[command(display_order = 8)]
    DeriveChanges {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
        #[arg(long)]
        osc_seq: Option<String>,
    },
    /// [pbfhogg] Cross-validate renumber against osmium renumber
    #[command(display_order = 9)]
    Renumber {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        /// Comma-separated starting IDs (forwarded to both pbfhogg and osmium)
        #[arg(long = "start-id", value_name = "IDS")]
        start_id: Option<String>,
        /// Print detail from the diff log when mismatches are found
        #[arg(long)]
        verbose: bool,
    },
    /// [pbfhogg] Cross-validate diff summary against osmium diff
    #[command(display_order = 10)]
    Diff {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long, default_value = "indexed")]
        variant: String,
        #[arg(long)]
        osc_seq: Option<String>,
    },
    /// [pbfhogg] Run all verify commands sequentially
    #[command(display_order = 11)]
    All {
        #[command(flatten)]
        pbf: VerifyPbfArgs,
        #[arg(long)]
        osc_seq: Option<String>,
        #[arg(long)]
        bbox: Option<String>,
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

fn validate_compression(s: &str) -> Result<String, String> {
    if s == "none" {
        return Ok(s.to_owned());
    }
    if let Some(level) = s.strip_prefix("zlib:") {
        let n: u8 = level
            .parse()
            .map_err(|_| format!("invalid zlib level '{level}', expected 1-9"))?;
        if (1..=9).contains(&n) {
            return Ok(s.to_owned());
        }
        return Err(format!("zlib level {n} out of range, expected 1-9"));
    }
    if let Some(level) = s.strip_prefix("zstd:") {
        level
            .parse::<u32>()
            .map_err(|_| format!("invalid zstd level '{level}', expected a positive integer"))?;
        return Ok(s.to_owned());
    }
    Err(format!(
        "invalid compression '{s}', expected 'none', 'zlib:N' (N=1-9), or 'zstd:N'"
    ))
}

/// Validate `--start-stage`: must be 2, 3, or 4.
fn validate_start_stage(s: &str) -> Result<String, String> {
    let n: u8 = s
        .parse()
        .map_err(|_| format!("expected stage number 2-4, got '{s}'"))?;
    if !(2..=4).contains(&n) {
        return Err(format!("stage must be 2-4, got {n}"));
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
//
// `impl Command { fn as_pbfhogg(...) }` lives next to the pbfhogg command
// definitions in `src/pbfhogg/cli_adapter.rs` — it's the bridge between
// the CLI shape and the typed `PbfhoggCommand`, and grouping it with the
// target type keeps both surfaces easy to change together.

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
        let parsed = Cli::try_parse_from([
            "brokkr",
            "inspect",
            "--tags",
            "--hotpath",
            "--dataset",
            "japan",
        ])
        .expect("parse");

        let Command::Inspect {
            mode,
            pbf,
            tags,
            type_filter,
            ..
        } = parsed.command
        else {
            panic!("expected inspect command");
        };
        assert!(mode.hotpath.is_some());
        assert_eq!(pbf.dataset, "japan");
        assert!(tags);
        assert_eq!(type_filter, None);
    }

    #[test]
    fn validate_start_stage_accepts_valid() {
        assert!(validate_start_stage("2").is_ok());
        assert!(validate_start_stage("3").is_ok());
        assert!(validate_start_stage("4").is_ok());
    }

    #[test]
    fn validate_start_stage_rejects_invalid() {
        assert!(validate_start_stage("0").is_err());
        assert!(validate_start_stage("1").is_err());
        assert!(validate_start_stage("5").is_err());
        assert!(validate_start_stage("255").is_err());
        assert!(validate_start_stage("abc").is_err());
    }

    #[test]
    fn altw_start_stage_requires_index_type() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "add-locations-to-ways",
            "--start-stage",
            "4",
        ]);
        assert!(parsed.is_err());
    }

    #[test]
    fn altw_start_stage_with_index_type_parses() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "add-locations-to-ways",
            "--index-type",
            "external",
            "--start-stage",
            "4",
        ])
        .expect("parse");

        let Command::AddLocationsToWays { start_stage, .. } = parsed.command else {
            panic!("expected add-locations-to-ways command");
        };
        assert_eq!(start_stage.as_deref(), Some("4"));
    }

    #[test]
    fn altw_keep_scratch_requires_index_type() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "add-locations-to-ways",
            "--keep-scratch",
        ]);
        assert!(parsed.is_err());
    }

    #[test]
    fn altw_keep_scratch_with_index_type_parses() {
        let parsed = Cli::try_parse_from([
            "brokkr",
            "add-locations-to-ways",
            "--index-type",
            "external",
            "--keep-scratch",
        ])
        .expect("parse");

        let Command::AddLocationsToWays { keep_scratch, .. } = parsed.command else {
            panic!("expected add-locations-to-ways command");
        };
        assert!(keep_scratch);
    }
}
