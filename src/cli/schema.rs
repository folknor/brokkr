use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

/// Index mode selection for `verify add-locations-to-ways`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum AltwMode {
    Hash,
    Sparse,
    Dense,
    External,
    All,
}

/// Version banner: semver plus the short git hash (with a `-dirty` suffix for
/// an unclean tree) and the UTC build time, stamped at compile time by
/// `build.rs`. e.g. `0.1.0 (abc123def 2026-07-12 12:34:56 UTC)`. Fed to clap's
/// `--version` so a stale installed `brokkr` names the commit it was built from.
const LONG_VERSION: &str = env!("BROKKR_LONG_VERSION");

#[derive(Parser)]
#[command(name = "brokkr", about = "Shared development tooling", version = LONG_VERSION)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Run gremlins + clippy + tests
    #[command(
        display_order = 0,
        long_about = "\
Three phases in order: gremlin scan, clippy, then tests. Each phase
short-circuits the next - gremlins fail the run before clippy starts,
clippy warnings are denied by the project's Cargo.toml lints so a
clippy failure short-circuits before tests run. Extra args after `--`
are forwarded raw to `cargo test` (invoke `cargo test` directly to skip
clippy for a targeted test run).

Output (default text mode, no flags):
  - Gremlins: one line per banned-Unicode hit. `--fix-gremlins`
    rewrites them in place before the scan.
  - Clippy: one line per diagnostic in the form
    `error[CODE] file:line:col message` /
    `warning[rule] file:line:col message`. Cargo runs with
    `--message-format=json` so every warning carries its lint code,
    not just the first occurrence per rule.
  - Tests: one line per failure on failure, compact summary on pass.

Capping and scoping (clippy + gremlins):
  Output is capped at `--limit N` (default 20). When the cap kicks in,
  diagnostics in files changed on the current branch (vs upstream /
  origin/master / origin/main) are surfaced first, and a trailer
  summarises what's hidden.
  - `--all` shows everything, sorted by (level, lint code, file, line)
    so every hit of a single rule clumps together for bulk triage.

Output mode:
  - `--raw`: reconstruct cargo's terminal-style output by concatenating
    each diagnostic's `rendered` field (full source annotations and
    help suggestions).

Examples:
  brokkr check                                     # gremlins + clippy + all tests
  brokkr check --all                               # bulk-triage view, sorted by lint
  brokkr check --fix-gremlins                      # rewrite banned chars before checking
  brokkr check --raw                               # full terminal-style cargo output
  brokkr check -- --test read_paths                # run one test file
  brokkr check -- -- --ignored                     # run ignored tests
  brokkr check -- --test read_paths -- --ignored   # one file, ignored only
  brokkr check --no-default-features               # check without default features
  brokkr check --features commands                 # check with specific features
  brokkr check --package pbfhogg-cli               # check only the CLI crate
  brokkr check --package pbfhogg-cli -- --test cli # one test file in the CLI crate"
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

        /// Validation profile from `[test.profiles]` in brokkr.toml.
        /// Selects sweeps, libtest filters (--include-ignored, --skip,
        /// --test-threads, etc.), and env vars for the test phase.
        /// Falls back to `[test] default_profile` if unset, then to
        /// today's single-sweep behaviour. Conflicts with `--features` /
        /// `--no-default-features` (those override the sweep set).
        #[arg(long, conflicts_with_all = ["features", "no_default_features"])]
        profile: Option<String>,

        /// Run the gate profile named by `[test] gate_profile` (validated
        /// at load time to certify "complete"). The stable pre-commit
        /// invocation: docs and hooks can say `brokkr check --gate` and
        /// survive profile renames. Conflicts with every flag that would
        /// narrow the run.
        #[arg(long, conflicts_with_all = ["profile", "features", "no_default_features", "package"])]
        gate: bool,

        /// Reconstruct cargo's terminal-style output (full source
        /// annotations, help suggestions) by concatenating each
        /// diagnostic's `rendered` field.
        #[arg(long)]
        raw: bool,

        /// Append one machine-readable summary line (a JSON object) as
        /// the last line of stdout: `schema`, `certifies`, `verdict`,
        /// `profile`, `sweeps`, `failed_phase`, `elapsed_ms`. The object
        /// is versioned and additive (`schema: 1`) - consumers must
        /// tolerate unknown fields. Human output is unchanged; parse the
        /// final stdout line.
        #[arg(long)]
        json: bool,

        /// Maximum diagnostics printed per phase (gremlins, clippy). Ignored
        /// with `--raw` or `--all`.
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Show every diagnostic without capping or scoping to changed
        /// files. Sorted by (level, lint code, file, line) so every hit
        /// of a single rule clumps together for bulk triage.
        #[arg(long)]
        all: bool,

        /// Before checking, rewrite banned Unicode in tracked source files
        /// with their ASCII equivalents (em/en dash -> `-`, smart quotes ->
        /// straight, NBSP -> space, zero-width/bidi deleted). Writes files
        /// in place.
        #[arg(long)]
        fix_gremlins: bool,

        /// After the check is otherwise done, print every test that ran in
        /// descending order by wall-clock time. Capped at `--limit` (or
        /// uncapped with `--all`). Build time is excluded - timing
        /// starts when libtest emits the per-test start marker.
        #[arg(long)]
        timings: bool,

        /// Log the full `cargo clippy` / `cargo test` command for every
        /// sweep instead of the collapsed `<phase> <name>: <shape>` form.
        /// The default collapses because the command is mostly profile
        /// boilerplate repeated identically per sweep; a *failing* sweep
        /// reprints its command either way.
        #[arg(long)]
        commands: bool,

        /// Raw arguments forwarded to the test phase. Tokens before a
        /// literal `--` are passed to `cargo test` (before cargo's own
        /// `--`); tokens after the second `--` are passed to libtest
        /// (after brokkr's enforced `--test-threads=1`). Examples:
        /// `brokkr check -- --test read_paths` (cargo-level filter),
        /// `brokkr check -- -- --ignored` (libtest-level flag).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run ONLY the clippy phase against an ad-hoc target (investigative).
    #[command(
        display_order = 0,
        long_about = "\
Run ONLY clippy - no gremlins/style/textlint/manifest/dependency/test.
An investigative probe, not a gate: it allows a real `--all-features` and
free feature sweeps a `[[check]]` gate never would, run under brokkr's env
and toolchain discipline (disable_toolchain honoured, [[check]] env applied,
global lock held) so a differential lint hunt no longer forces raw
`cargo +nightly clippy` (which drops HIGH_PRECISION=1 and the toolchain pin).

Two modes:
  - Ad-hoc: `-p` (repeatable) + `--all-features` / `--features` /
    `--no-default-features`. Env is the union of every [[check]] entry's env
    (a project invariant a probe must not drop); a key two entries set to
    different values is a config error unless `--env` picks one.
  - `--sweep NAME`: borrow one [[check]] entry's packages/features/env verbatim.

`--env KEY=VALUE` (repeatable) overrides either env source and wins last.
Output modes match `brokkr check`'s clippy phase: default capped text,
`--all` bulk-triage, `--limit N`, `--raw` (cargo's terminal-style rendering).
Exit 0 iff zero diagnostics; 1 on any lint or build error.

Examples:
  brokkr clippy                              # default selection, default features
  brokkr clippy -p mycrate --all-features    # one crate, all features
  brokkr clippy --features a,b -p mycrate    # a virtual workspace needs -p
  brokkr clippy --sweep ffi                  # replay the 'ffi' [[check]] entry
  brokkr clippy --sweep ffi --env HIGH_PRECISION=0
  brokkr clippy --all                        # bulk-triage, sorted by lint"
    )]
    Clippy {
        /// Target package (repeatable). No `-p` uses cargo's default package
        /// selection: every member of a virtual workspace, or the root package
        /// of a package-rooted one.
        #[arg(long, short, value_name = "PKG")]
        package: Vec<String>,

        /// Enable all Cargo features. Allowed here because clippy is a probe,
        /// not a gate (the `[[check]]` `features = "all"` ban still stands).
        #[arg(long, conflicts_with_all = ["features", "no_default_features", "sweep"])]
        all_features: bool,

        /// Cargo features to enable (comma-separated).
        #[arg(long, value_delimiter = ',', conflicts_with = "sweep")]
        features: Vec<String>,

        /// Disable default Cargo features.
        #[arg(long, conflicts_with = "sweep")]
        no_default_features: bool,

        /// Borrow one `[[check]]` entry's packages/features/env and run just
        /// its clippy invocation. Conflicts with the ad-hoc target flags.
        #[arg(long, value_name = "NAME", conflicts_with = "package")]
        sweep: Option<String>,

        /// Extra env var KEY=VALUE (repeatable, non-empty KEY). Overrides the
        /// merged `[[check]]` env (ad-hoc) or the entry env (`--sweep`).
        #[arg(long, value_parser = validate_env_kv)]
        env: Vec<String>,

        /// Reconstruct cargo's terminal-style output (human-rendered, not JSON).
        #[arg(long)]
        raw: bool,

        /// Maximum diagnostics printed. Ignored with `--raw` or `--all`.
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Show every diagnostic, sorted by (level, lint, file, line).
        #[arg(long)]
        all: bool,
    },
    /// Run `cargo fmt`. All arguments are forwarded raw.
    #[command(display_order = 0)]
    Fmt {
        /// Raw arguments forwarded to `cargo fmt`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run `cargo run`. All arguments are forwarded raw.
    #[command(display_order = 0)]
    Run {
        /// Raw arguments forwarded to `cargo run`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List rust source files above a line-count threshold (default 800).
    ///
    /// Scans tracked and untracked-not-ignored `.rs` files and prints those
    /// with more than THRESHOLD lines, largest first.
    #[command(display_order = 0)]
    Wc {
        /// Only list files with more than this many lines.
        #[arg(default_value_t = crate::wc::DEFAULT_THRESHOLD)]
        threshold: usize,
    },
    /// Read the bundled documentation (`brokkr man` lists the topics).
    ///
    /// The `docs/**.md` files are compiled into the binary and rendered to the
    /// terminal. Topics are filtered by the detected project, except the
    /// project-agnostic ones (check, clippy, deps, config, measure,
    /// output-channels), which are listed everywhere.
    #[command(display_order = 0)]
    Man {
        /// Topic to read; omit to list the topics available here.
        topic: Option<String>,
    },
    /// Audit Cargo.lock for dependency smells (duplicate versions, etc.).
    ///
    /// Phase-based; each phase emits zero or more findings. v1 ships
    /// `duplicate_version` with blame attribution (which of your direct
    /// deps is anchoring an old version). See `docs/commands/deps.md`.
    #[command(display_order = 0)]
    Deps {
        /// Emit NDJSON events on stdout (one JSON object per line). No
        /// prefixed log output, no terminal colors.
        #[arg(long)]
        json: bool,

        /// Maximum findings printed per phase. Ignored with `--json` or
        /// `--all`.
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Show every finding without capping.
        #[arg(long)]
        all: bool,

        /// Always exit 0, even when findings exist. Useful for
        /// report-only invocations in CI.
        #[arg(long)]
        no_fail: bool,

        /// Optional package spec (`name` or `name@version`). When set,
        /// suppress the other phases and print every Normal-kind chain
        /// from a workspace member down to the named package. Useful
        /// for answering "who is pulling in this crate?" and for
        /// disambiguating one specific version of a duplicate.
        #[arg(value_name = "PKG")]
        focus: Option<String>,
    },
    /// Show environment information
    #[command(display_order = 1)]
    Env,
    // ----- pbfhogg tool CLI commands (display_order = 2) -----
    /// [pbfhogg] Inspect PBF. Flags select mode:
    ///   no flag       → metadata (block count / bbox / stats)
    ///   `--nodes`     → node statistics
    ///   `--tags`      → tag frequencies (optionally narrowed by
    ///                   `--type node|way|relation`)
    ///   `--extended`  → extended metadata scan (timestamp range, data
    ///                   bbox, metadata coverage, ordering). Default-mode
    ///                   only - rejected with `--nodes` / `--tags`.
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
        /// Extended scan of default inspect (timestamp range, data bbox,
        /// metadata coverage, ordering). Only valid in default mode -
        /// clap rejects the combination with `--nodes` / `--tags`.
        #[arg(long, conflicts_with_all = ["nodes", "tags"])]
        extended: bool,
        /// Parallel worker threads for pbfhogg inspect. Only applies to
        /// `--nodes` / `--tags` modes (pbfhogg's default inspect doesn't
        /// accept `-j`). `0` = auto (`available_parallelism()`); omit to
        /// use pbfhogg's default (1).
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Check referential integrity
    #[command(name = "check-refs", display_order = 2)]
    CheckRefs {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Check ID ordering
    #[command(name = "check-ids", display_order = 2)]
    CheckIds {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Additionally detect duplicate IDs per type. Allocates
        /// RoaringTreemap sets; higher memory + CPU than the streaming
        /// default.
        #[arg(long)]
        full: bool,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Sort PBF
    #[command(name = "sort", display_order = 2)]
    Sort {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot (e.g. one
        /// produced by `brokkr degrade --unsort --as-snapshot ...`).
        #[arg(long)]
        snapshot: Option<String>,
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
        #[arg(long, conflicts_with = "type_filter")]
        dedupe: bool,
        /// Force the full-decode / re-frame Framed path (cat_filtered).
        #[arg(long)]
        clean: bool,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Tags filter. Orthogonal flags:
    ///   `--filter EXPR` - pbfhogg filter expression (default
    ///     `w/highway=primary`). Examples: `amenity=restaurant`,
    ///     `highway=primary`, `w/building=yes`.
    ///   `-R` / `--omit-referenced` - single-pass; drop referenced
    ///     objects (default: two-pass with references).
    ///   `-i` / `--invert-match` - flip match sense: keep non-matching,
    ///     drop matching.
    ///   `-t` / `--remove-tags` - strip tags from referenced-but-unmatched
    ///     objects (meaningful only in the two-pass path, i.e. without `-R`).
    ///   `--input-kind osc` - read an OSC diff instead of a PBF.
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
        /// Invert match sense: drop matching objects, keep non-matching.
        /// Not valid with `--input-kind osc`.
        #[arg(short = 'i', long = "invert-match", conflicts_with = "input_kind")]
        invert_match: bool,
        /// Remove tags from referenced-but-unmatched objects in the
        /// two-pass path. No-op under `-R` (referenced objects are
        /// dropped entirely). Not valid with `--input-kind osc`.
        #[arg(short = 't', long = "remove-tags", conflicts_with = "input_kind")]
        remove_tags: bool,
        /// Read an OSC diff as input instead of a PBF.
        #[arg(long = "input-kind", value_parser = ["pbf", "osc"])]
        input_kind: Option<String>,
        /// OSC sequence number from brokkr.toml (only used with
        /// `--input-kind osc`).
        #[arg(long)]
        osc_seq: Option<String>,
        /// Snapshot key to read input from (PBF or OSC, depending on
        /// `--input-kind`). Use `base` (or omit) for the dataset's
        /// primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
        /// Parallel worker threads for pbfhogg tags-filter. `0` = auto
        /// (`available_parallelism()`); omit to use pbfhogg's default.
        /// Requires a pbfhogg build that exposes `-j` on tags-filter;
        /// older builds will reject it.
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
    },
    /// [pbfhogg] Get elements by hardcoded ID set. Flags:
    ///   `--add-referenced` - also pull in referenced objects (two-pass);
    ///   `--invert` - select everything NOT in the ID set.
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
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Get parent elements
    #[command(name = "getparents", display_order = 2)]
    Getparents {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Renumber element IDs
    #[command(name = "renumber", display_order = 2)]
    Renumber {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Merge OSC changes. `--simplify` picks the BTreeMap dedupe
    /// path (keep only the last change per object) instead of the default
    /// streaming concat path - a different code path worth measuring.
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
        /// Keep only the last change per (type, id) - BTreeMap dedupe path.
        #[arg(long)]
        simplify: bool,
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
        /// Pass `--locations-on-ways` through to pbfhogg apply-changes.
        #[arg(long)]
        locations_on_ways: bool,
        /// Parallel worker threads for pbfhogg apply-changes. `0` = auto
        /// (`available_parallelism()`); omit to use pbfhogg's default
        /// (nproc-2 at time of writing). Requires a pbfhogg build that
        /// exposes `-j` on apply-changes; older builds will reject it.
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
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
        /// Emit the injected-prepass wire extensions (BlobHeader field 5
        /// way-member bitmaps, Way field 20 shared-node pins; declared via
        /// the `pbfhogg.WayMembers-v1` / `pbfhogg.SharedNodePins-v1` header
        /// feature strings). Forwarded verbatim to pbfhogg, which hard-errors
        /// on invalid combinations. Enriched output is osmium-incompatible by
        /// design, so `brokkr verify add-locations-to-ways` refuses this flag.
        #[arg(long)]
        inject_prepass: bool,
        /// Pass `--force` through to pbfhogg's `add-locations-to-ways`
        /// (skips the indexdata requirement, forcing the full decode-all
        /// fallback on raw / non-indexed input). Named `--force-altw` to
        /// disambiguate from brokkr's own per-subcommand `--force`
        /// dirty-tree override, mirroring `--force-repack`.
        #[arg(long)]
        force_altw: bool,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot (e.g. one
        /// produced by `brokkr degrade --strip-locations --as-snapshot ...`).
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Re-encode a PBF with a configurable elements-per-blob cap.
    ///
    /// Default writes to scratch and is overwritten on each `--bench`
    /// iteration. Pass `--as-snapshot KEY` to promote the final iteration's
    /// artifact into the dataset graph (registered under
    /// `[<host>.datasets.<dataset>.snapshot.<KEY>.pbf.indexed]` in
    /// brokkr.toml). `KEY=base` is reserved.
    #[command(name = "repack", display_order = 2)]
    Repack {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
        /// Cap on elements per output blob (passes through to pbfhogg).
        /// Omit to use pbfhogg's default (8000).
        #[arg(long, value_name = "N")]
        elements_per_blob: Option<u32>,
        /// Promote the final iteration's artifact to a snapshot under the
        /// dataset (writes a [..snapshot.<KEY>.pbf.indexed] entry).
        #[arg(long, value_name = "KEY")]
        as_snapshot: Option<String>,
        /// Overwrite an existing snapshot of the same key. Without this,
        /// `--as-snapshot KEY` errors when KEY is already registered.
        #[arg(long, requires = "as_snapshot")]
        replace_snapshot: bool,
        /// Pass `--force` through to pbfhogg repack (skips the indexdata
        /// requirement).
        #[arg(long)]
        force_repack: bool,
    },
    /// [pbfhogg] Produce an adversarial PBF by stripping properties or
    /// perturbing structure.
    ///
    /// Flags compose; pbfhogg requires at least one transformation flag and
    /// brokkr defers to pbfhogg's own validation. Default writes to scratch
    /// and is overwritten on each `--bench` iteration. With
    /// `--as-snapshot KEY`, the final artifact is promoted into the dataset
    /// graph - under `pbf.raw` when `--strip-indexdata` is set, otherwise
    /// `pbf.indexed`.
    #[command(name = "degrade", display_order = 2)]
    Degrade {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
        /// Clear `Sort.Type_then_ID` and produce one adjacent same-kind blob
        /// pair per kind with overlapping ID ranges.
        #[arg(long)]
        unsort: bool,
        /// Drop `LocationsOnWays` (clears the header feature and re-encodes
        /// ways without inline coordinates).
        #[arg(long)]
        strip_locations: bool,
        /// Clear `BlobHeader.indexdata` on every OsmData blob.
        #[arg(long)]
        strip_indexdata: bool,
        /// Promote the final iteration's artifact to a snapshot under the
        /// dataset. Written under `pbf.raw` if `--strip-indexdata` is set,
        /// otherwise `pbf.indexed`.
        #[arg(long, value_name = "KEY")]
        as_snapshot: Option<String>,
        /// Overwrite an existing snapshot of the same key. Without this,
        /// `--as-snapshot KEY` errors when KEY is already registered.
        #[arg(long, requires = "as_snapshot")]
        replace_snapshot: bool,
    },
    /// [pbfhogg] Multi-extract benchmark (N regions in one pbfhogg
    /// invocation). `--strategy` picks pbfhogg's extract algorithm
    /// (simple / smart / complete / all).
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
        /// Extract strategy: simple, complete, smart, or all (runs all
        /// three back-to-back, like `brokkr extract --strategy all`).
        #[arg(long, default_value = "simple")]
        strategy: String,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// [pbfhogg] Filter by timestamp
    #[command(name = "time-filter", display_order = 2)]
    TimeFilter {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
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
        /// Parallel worker threads for pbfhogg diff. `0` = auto
        /// (`available_parallelism()`); omit to use pbfhogg's default (1).
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
    },
    /// [pbfhogg] Diff two snapshots of the same dataset
    #[command(
        name = "diff-snapshots",
        display_order = 2,
        long_about = "\
Diff two point-in-time snapshots of the same dataset.

Unlike `brokkr diff`, neither side is derived from apply-changes - both PBFs
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
        /// Parallel worker threads for pbfhogg diff. `0` = auto
        /// (`available_parallelism()`); omit to use pbfhogg's default (1).
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
    },
    /// [pbfhogg] Diff two PBFs (OSC output)
    /// [pbfhogg] Build geocode index
    #[command(name = "build-geocode-index", display_order = 2)]
    BuildGeocodeIndex {
        #[command(flatten)]
        mode: ModeArgs,
        #[command(flatten)]
        pbf: PbfArgs,
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
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
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical snapshot.
        #[arg(long)]
        snapshot: Option<String>,
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
        /// Snapshot key to read input from. Use `base` (or omit) for the
        /// dataset's primary data; pass a key registered under
        /// `[dataset.snapshot.<key>]` for a historical or re-encoded snapshot
        /// (e.g. `repack --as-snapshot`). Lets pure decode throughput be
        /// measured against blob count independent of any command layer.
        #[arg(long)]
        snapshot: Option<String>,
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
    ///
    /// Pipeline configuration lives entirely in `[<host>.tilegen.default]` in
    /// brokkr.toml - ocean inputs, tile format, budgets, geometry. There are
    /// no override flags: either it is explicit in the block, or it is not
    /// set. What remains here is the input axis (dataset/variant), the
    /// measurement mode, and the per-invocation resume point.
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
        /// Resume from checkpoint: ocean, sort or assemble
        #[arg(long)]
        skip_to: Option<String>,
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
    /// Build and run with passthrough args (deprecated - use `run` subcommands instead)
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

In a piners project this queries the same .brokkr/results.db (hotpath/alloc
runs). The corpus run store has its own command - see `brokkr corpus-results`."
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

        /// Filter by captured env var KEY=VALUE (multiple allowed, AND
        /// semantics). Keys are bare env var names (no `env.` prefix),
        /// e.g. `--env PBFHOGG_USE_NEW_PATH=1`. Rows without the key
        /// are excluded - use an explicit baseline value (e.g. `=0`)
        /// on the off runs rather than relying on absence.
        #[arg(long, value_parser = validate_meta_filter)]
        env: Vec<String>,

        /// Substring match against the run's invocation: the subprocess
        /// invocation (`cli_args`), the brokkr invocation (`brokkr_args`), or a
        /// captured env var as `NAME=VALUE`. Repeatable - each `--grep` must
        /// match (AND). `git log --grep` style. E.g. `--grep apply-changes
        /// --grep zstd:1 --grep uring` to find apply-changes runs that used
        /// both zstd:1 and io_uring, or `--grep LAYER_STATS=1` for an
        /// env-gated arm. Use `--env` for an exact key=value match.
        #[arg(long)]
        grep: Vec<String>,

        /// Inverse of `--grep`: exclude rows whose invocation contains the
        /// term, across the same three sources (`cli_args`, `brokkr_args`,
        /// captured env). Repeatable - a row is excluded if it matches ANY term.
        /// Composes with `--grep`. The A/B case: `--grep apply-changes
        /// --grep-v uring` selects the arm distinguished only by an absent
        /// flag, which `--grep` alone cannot express.
        #[arg(long = "grep-v")]
        grep_v: Vec<String>,

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
`--hotpath`, and `--alloc` runs. A UUID prefix is required - use
`brokkr results` to find one. `--run N|all` picks a specific run
within the result (default: best run).

The `dirty` pseudo-UUID resolves to the most recent forced or failed
run - runs produced via `--force` (dirty tree) or that exited non-zero
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
  brokkr sidecar --compare a65a 911c                  # two results, phase-aligned",
        group(ArgGroup::new("sample_view").args(&["samples", "stat"]).multiple(false)),
        group(ArgGroup::new("phased_view").args(&["samples", "stat", "counters"]).multiple(false)),
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
        #[arg(long, conflicts_with_all = ["samples", "markers", "durations", "stat", "compare", "stalls"])]
        counters: bool,

        /// Roll up cumulative `*_wait_ns` counters by category.
        ///
        /// Convention: a target accumulates blocking time per category into a
        /// strictly-monotonic counter named `<category>_wait_ns` (one atomic add
        /// per blocking event). This view takes the max per name, strips the
        /// `_wait_ns` suffix for the category, and reports total stall time as a
        /// fraction of run wall-clock - which can exceed 100% for waits summed
        /// across concurrent threads (it's the avg threads parked in that
        /// category). Runs with no `*_wait_ns` counters produce an informative
        /// empty result.
        #[arg(long, conflicts_with_all = ["samples", "markers", "durations", "counters", "stat", "compare"])]
        stalls: bool,

        /// Compute min/max/avg/p50/p95 for a /proc field (e.g. `--stat rss`)
        #[arg(long, conflicts_with_all = ["samples", "markers", "durations", "counters", "compare"])]
        stat: Option<String>,

        /// Compare two results phase-by-phase (no UUID argument)
        #[arg(long, num_args = 2, value_names = ["UUID_A", "UUID_B"],
              conflicts_with_all = ["query", "samples", "markers", "durations", "counters", "stat"])]
        compare: Option<Vec<String>>,

        /// Render as a fixed-width table instead of JSONL. Applies to the
        /// default view, --durations, --counters, and --compare. No-op for
        /// --samples / --markers / --stat (always JSONL).
        #[arg(long)]
        human: bool,

        /// Show a specific run index (0-based), or "all" for all runs. Defaults to the best run.
        #[arg(long)]
        run: Option<String>,

        /// Filter to a marker phase (e.g. "STAGE2"). Requires --samples,
        /// --stat, or --counters.
        #[arg(long, requires = "phased_view")]
        phase: Option<String>,

        /// Keep only counters whose name contains this substring (e.g.
        /// `--grep s1a_`). Requires --counters. A run emitting a progress
        /// counter every 64 blobs otherwise buries the ~30 lines that
        /// matter under a full dump.
        #[arg(long, requires = "counters", value_name = "SUBSTR")]
        grep: Option<String>,

        /// Filter samples by time range in seconds (e.g. "10.0..82.0"). Requires --samples or --stat.
        #[arg(long, requires = "sample_view")]
        range: Option<String>,

        /// Filter samples where a field meets a condition (e.g. "majflt>0"). Requires --samples or --stat.
        #[arg(long, name = "COND", requires = "sample_view")]
        r#where: Option<String>,

        /// Output only these fields (comma-separated, e.g. "t,rss,anon,majflt"). Only with --samples.
        #[arg(long, value_delimiter = ',', requires = "samples", conflicts_with = "stat")]
        fields: Vec<String>,

        /// Output every Nth sample (downsample). Requires --samples or --stat.
        #[arg(long, requires = "sample_view")]
        every: Option<usize>,

        /// Output only the first N samples. Requires --samples or --stat.
        #[arg(long, requires = "sample_view")]
        head: Option<usize>,

        /// Output only the last N samples. Requires --samples or --stat.
        #[arg(long, requires = "sample_view")]
        tail: Option<usize>,
    },
    /// Clean build artifacts and scratch data
    #[command(display_order = 5)]
    Clean {
        /// Also remove all persistent benchmark worktrees
        /// (sibling `.brokkr-worktree-<project>-*` dirs created by --commit).
        #[arg(long)]
        worktrees: bool,

        /// Also run `cargo clean -p <PKG>` - wipe this project's own build
        /// artifacts (all profiles) while keeping dependency artifacts
        /// cached. The fix for stale-incremental linker failures. PKG
        /// defaults to the brokkr.toml project name; pass a value to clean
        /// a different package (e.g. `--cargo pbfhogg-cli`).
        #[arg(long, num_args = 0..=1, value_name = "PKG")]
        cargo: Option<Option<String>>,
    },
    /// Show lock status (who holds the benchmark lock)
    #[command(display_order = 6)]
    Lock,
    /// Gracefully stop the active bench (SIGTERM → clean shutdown + scratch cleanup)
    #[command(
        display_order = 6,
        long_about = "\
Ask the brokkr process holding the lock to wrap up cleanly.

Default: sends SIGTERM to the brokkr process. Brokkr catches it,
SIGKILLs the tool being measured, flushes partial sidecar data
(reachable via `brokkr sidecar dirty`), releases the lock, and
runs `brokkr clean` for the project.

--hard: sends SIGKILL to both the brokkr PID and the recorded
child PID without any cleanup. Use when the default path is
wedged; follow up with `brokkr clean` manually."
    )]
    Kill {
        /// SIGKILL immediately (brokkr + child) without graceful cleanup.
        #[arg(long)]
        hard: bool,
    },
    /// Browse command history
    #[command(
        display_order = 7,
        long_about = "\
Browse the global command history log (~/.local/share/brokkr/history.db).

Every brokkr invocation is recorded with timing and exit status.

Examples:
  brokkr history                        # last 25 entries
  brokkr history 1234                   # detail view for a single entry
  brokkr history -n 50                  # last 50
  brokkr history --all                  # everything
  brokkr history --command bench        # filter by command substring
  brokkr history --project pbfhogg      # filter by project
  brokkr history --project-dir /clone2  # filter by cwd substring
  brokkr history --failed               # only non-zero exit
  brokkr history --status 130           # filter by exit code (e.g. 130 = interrupt)
  brokkr history --since 2026-03-01     # from date (YYYY-MM-DD)
  brokkr history --until 2026-03-05     # up to date (YYYY-MM-DD)
  brokkr history --slow 10000           # commands that took >10s"
    )]
    History {
        /// Look up a single history entry by id (shown in the leftmost column of the default view)
        id: Option<i64>,

        /// Filter by command (substring match)
        #[arg(long, conflicts_with = "id")]
        command: Option<String>,

        /// Filter by project name
        #[arg(long, conflicts_with = "id")]
        project: Option<String>,

        /// Filter by cwd substring (useful for multiple clones of the same project)
        #[arg(long, conflicts_with = "id")]
        project_dir: Option<String>,

        /// Show only failed commands (non-zero exit)
        #[arg(long, conflicts_with_all = ["id", "status"])]
        failed: bool,

        /// Filter by exact exit status code
        #[arg(long, conflicts_with = "id")]
        status: Option<i32>,

        /// Show entries from this date onward (YYYY-MM-DD or YYYY-MM-DD HH:MM:SS)
        #[arg(long, value_parser = validate_since, conflicts_with = "id")]
        since: Option<String>,

        /// Show entries up to this date (YYYY-MM-DD or YYYY-MM-DD HH:MM:SS)
        #[arg(long, value_parser = validate_since, conflicts_with = "id")]
        until: Option<String>,

        /// Show commands that took at least this many milliseconds
        #[arg(long, conflicts_with = "id")]
        slow: Option<i64>,

        /// Maximum number of entries to show
        #[arg(long, short = 'n', default_value = "25", conflicts_with_all = ["all", "id"])]
        limit: usize,

        /// Show all entries (ignores -n)
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Delete results and sidecar data by UUID or commit prefix
    #[command(
        display_order = 8,
        long_about = "\
Delete rows from .brokkr/results.db (runs + FK children) and
.brokkr/sidecar.db (samples, markers, summary, counters, meta, latest)
matching a UUID prefix or commit prefix. Useful when a benchmark was
run under wrong pretences and the numbers are garbage.

Dry-run by default: lists the rows that would be deleted. Pass -f
to actually delete.

Examples:
  brokkr invalidate 0b74fb6f           # preview deletion by UUID prefix
  brokkr invalidate 0b74fb6f -f        # perform deletion
  brokkr invalidate --commit a65a      # preview: every row on commit a65a...
  brokkr invalidate --commit a65a -f   # perform"
    )]
    Invalidate {
        /// UUID prefix to invalidate (mutually exclusive with --commit)
        #[arg(value_name = "UUID", conflicts_with = "commit", required_unless_present = "commit")]
        uuid: Option<String>,

        /// Commit hash prefix - invalidate every result rooted at matching commits
        #[arg(long)]
        commit: Option<String>,

        /// Actually delete (without this flag, the command is a dry-run)
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Cross-validate output against reference tools
    #[command(display_order = 11)]
    Verify {
        /// Stream every check's full detail. Default is quiet on pass (one
        /// result line per check) and loud on fail (the failing check's
        /// detail is replayed). `-v` also shows build output.
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
    /// [elivagar] Inspect PMTiles header, tile stats, and metadata
    #[command(name = "pmtiles-inspect", display_order = 33)]
    PmtilesInspect {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// Commit short hash selecting which tilegen output to open (default: current HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Explicit PMTiles path, skips dataset/commit resolution
        #[arg(long)]
        file: Option<String>,
    },
    /// [elivagar] Per-ring winding/area diagnosis for one tile
    #[command(display_order = 34)]
    Diag {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// Commit short hash selecting which tilegen output to open (default: current HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Explicit PMTiles path, skips dataset/commit resolution
        #[arg(long)]
        file: Option<String>,
        #[arg(short = 'z', long)]
        z: u8,
        #[arg(short = 'x', long)]
        x: u32,
        #[arg(short = 'y', long)]
        y: u32,
    },
    /// [elivagar] Render one tile to SVG
    #[command(display_order = 35)]
    Svg {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// Commit short hash selecting which tilegen output to open (default: current HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Explicit PMTiles path, skips dataset/commit resolution
        #[arg(long)]
        file: Option<String>,
        #[arg(short = 'z', long)]
        z: u8,
        #[arg(short = 'x', long)]
        x: u32,
        #[arg(short = 'y', long)]
        y: u32,
        #[arg(short = 'W', long, default_value_t = 1)]
        width: u32,
        #[arg(short = 'H', long, default_value_t = 1)]
        height: u32,
        /// Comma-separated layer names to render (default: all)
        #[arg(short = 'l', long)]
        layers: Option<String>,
        /// Output file path (default: elivagar's own default)
        #[arg(short = 'o', long)]
        output: Option<std::path::PathBuf>,
    },
    /// [elivagar] Diff current tilegen output against the blessed archive
    #[command(display_order = 36)]
    Regress {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// Commit short hash selecting which current output to diff (default: current HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Explicit CURRENT PMTiles path, skips dataset/commit resolution
        #[arg(long)]
        file: Option<String>,
        /// Explicit BLESSED PMTiles path, skips brokkr.toml blessed resolution
        #[arg(long)]
        against: Option<String>,
        /// Geometry tolerance in the layer's extent units (default 0)
        #[arg(long, default_value_t = 0)]
        tol: i32,
        /// tolerance_moved budget before exit 1 (default 0: any move fails)
        #[arg(long, default_value_t = 0)]
        max_moved: u64,
        /// Per-class example cap (default 20)
        #[arg(long, default_value_t = 20)]
        max_examples: usize,
        /// Dump side-by-side SVG pairs for the worst structural diffs to DIR
        #[arg(long)]
        svg_dump: Option<std::path::PathBuf>,
        /// Emit a machine-readable report to stdout
        #[arg(long)]
        json: bool,
    },
    /// [elivagar] Bless the current tilegen output as the regress reference
    #[command(display_order = 37)]
    Bless {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// Commit short hash selecting which output to bless (default: current HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Explicit PMTiles path to bless, skips dataset/commit resolution
        #[arg(long)]
        file: Option<String>,
    },
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
    Visual {
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
    /// Run one specific cargo test (release by default; --debug for dev)
    ///
    /// Always: --include-ignored, --nocapture, --test-threads=1.
    /// Adds --release unless dev profile is selected. Profile precedence:
    /// `--debug` / `--release` on the CLI win; otherwise `[test] debug` in
    /// brokkr.toml decides (default release).
    /// Feature selection matches `brokkr check` - defaults to --all-features,
    /// and runs a second sweep with [check].consumer_features if configured.
    /// Streams the test's own stdout/stderr live and prints a [test]
    /// PASS/FAIL footer with wall time per sweep. Use --raw for unfiltered
    /// cargo output. Gated off for litehtml/sluggrs (use `brokkr visual`).
    ///
    /// The package passed to `cargo test -p` is resolved in order:
    ///   1. `-p/--package` on the command line
    ///   2. `[test] default_package` in brokkr.toml
    ///   3. The project's built-in default (pbfhogg-cli, nidhogg)
    ///
    /// Projects without any of these (e.g. a workspace) must pass -p.
    ///
    /// Env vars exported to the test process:
    ///   BROKKR_TEST_BIN_DIR=<target>/{release,debug} - directory of any
    ///     `build_packages` artefacts rebuilt for this sweep. Tests that
    ///     spawn the just-rebuilt binary should read this rather than
    ///     guessing via `cfg!(debug_assertions)` (which lies under
    ///     `[profile.test]` overrides). Tracks --debug.
    ///
    /// Example:
    ///   brokkr test merge_basic_create_modify_delete_uring
    ///   brokkr test -p calendar extract_tag_value_flattens_nested_text
    ///   brokkr test roundtrip_uring_tiny_output -N 5
    ///   brokkr test some_unit_test --debug
    #[command(display_order = 10)]
    Test {
        /// Exact test name to run (substring filter, case-sensitive)
        name: String,
        /// Cargo package to test (`cargo test -p <pkg>`). Overrides the
        /// project default and any `[test] default_package` in brokkr.toml.
        #[arg(short = 'p', long = "package")]
        package: Option<String>,
        /// Repeat the test this many times per sweep (flaky-test hunting)
        #[arg(short = 'N', long = "repeat", default_value = "1")]
        repeat: u32,
        /// Parallel cargo compile jobs (`cargo test -j N`)
        #[arg(short = 'j', long = "jobs")]
        jobs: Option<u32>,
        /// Bypass filtering - print everything cargo emits
        #[arg(long)]
        raw: bool,
        /// Build and run the test in dev profile instead of release.
        /// BROKKR_TEST_BIN_DIR points at <target>/debug accordingly.
        /// Overrides `[test] debug` from brokkr.toml.
        #[arg(long, conflicts_with = "release")]
        debug: bool,
        /// Force the release profile, overriding `[test] debug = true`
        /// from brokkr.toml. Mutually exclusive with --debug.
        #[arg(long)]
        release: bool,
        /// Override the per-test watchdog ceiling, in seconds (1-280).
        /// Only honored when `<name>` matches exactly one test per sweep;
        /// if it matches more than one, `brokkr test` errors before
        /// running. Without this flag the ceiling is the standard 20s.
        #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..=280))]
        timeout: Option<u64>,
        /// Run only the named sweep from the resolved `default_profile`
        /// (e.g. `--sweep all` or `--sweep consumer`), instead of every
        /// sweep the profile lists. Errors if no sweep carries that label.
        #[arg(long, value_name = "LABEL")]
        sweep: Option<String>,
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
    },

    // ----- ratatoskr-only commands (display_order = 60) -----
    /// [ratatoskr] Run a Service-subprocess test script (deterministic harness)
    ///
    /// Accepts either a single `.lua` script or a directory under
    /// `crates/app/tests/service-harness/`. The directory form is sugar
    /// for `service-suite --filter <rel>/`: same code path, same artefact
    /// layout, and `-N` becomes cohort cycles instead of per-script
    /// repeats. Builds the configured `[[check]]` sweep via
    /// `[ratatoskr.harness]` (same feature contract `brokkr check`
    /// enforces), allocates a per-run artefact dir at
    /// `.brokkr/ratatoskr/<test>/run-N/`, then spawns
    /// `<binary> --test-harness <SCRIPT>` with
    /// `BROKKR_HARNESS_ARTEFACT_DIR` and `BROKKR_TEST_BIN_DIR` set in
    /// the env. Captures stdout/stderr into the artefact dir alongside
    /// `run.toml` and a copy of the script. Preserves the dir on
    /// failure; deletes it on success unless `--keep-artefacts` is set.
    /// `-N <count>` on a single script repeats that script `<count>`
    /// times; on a directory it runs the cohort `<count>` times in
    /// order. Default is stop-on-first-failure; `--keep-going` runs
    /// every iteration and the summary lists the failures.
    /// The harness binary itself (Lua VM via dellingr, ServiceClient
    /// userdata, wait combinator, frame-log tap, /proc snapshot writer)
    /// lives in ratatoskr's `app` crate and lands in Phase 8; until
    /// then `app --test-harness` errors out and brokkr captures that
    /// faithfully.
    #[command(name = "service-test", display_order = 60)]
    ServiceTest {
        /// Path to a Lua test script, or a directory under
        /// `crates/app/tests/service-harness/` to run as a cohort
        /// (sugar for `service-suite --filter <rel>/`).
        script: String,

        /// Preserve the artefact directory even on success
        #[arg(long)]
        keep_artefacts: bool,

        /// Build the harness binary with the dev profile (`<target>/debug/`).
        /// Default is release for parity with `brokkr test` and to match
        /// what users will run in production. Overrides
        /// `[ratatoskr.harness] debug` from `brokkr.toml`.
        #[arg(long, conflicts_with = "release")]
        debug: bool,

        /// Force the release profile, overriding `[ratatoskr.harness] debug`
        /// from `brokkr.toml`. Mutually exclusive with `--debug`.
        #[arg(long)]
        release: bool,

        /// Repeat count. For a single script: number of iterations
        /// (each gets its own `run-N/`). For a directory: number of
        /// cohort cycles, where each cycle invokes every matched
        /// script once. Build is shared across iterations.
        #[arg(short = 'N', long = "repeat", default_value = "1", value_name = "COUNT")]
        repeat: u32,

        /// Keep going after a failed iteration. Default is to stop on
        /// the first failure so the artefact dir lands fast for triage.
        #[arg(long)]
        keep_going: bool,
    },

    /// [ratatoskr] List discovered service-test scripts with descriptions
    ///
    /// Scans `crates/app/tests/service-harness/*.lua` under the project
    /// root, parses a top-of-file `-- key: value` frontmatter for
    /// `description` and `expected` (`pass` / `ignored`), and prints a
    /// table. Empty-state output points at the expected directory.
    #[command(name = "service-list", display_order = 61)]
    ServiceList,

    /// [ratatoskr] Run every discovered service-test script in sequence
    ///
    /// Discovers `crates/app/tests/service-harness/**/*.lua`, optionally
    /// filters by substring against the script's relative name, builds the
    /// harness binary once, then runs each script through the same path
    /// `service-test` uses (per-script artefact dir, ceiling-bounded spawn,
    /// preserve-on-failure). Scripts marked `expected = ignored` in the
    /// frontmatter are skipped unless `--include-ignored` is set. Default
    /// is stop-on-first-failure; `--keep-going` runs every selected script
    /// and reports a summary listing the failed names. Exits non-zero if
    /// any selected script failed. `-N <count>` runs the whole cohort
    /// `<count>` times in order (50 cycles over 11 scripts = 550 runs).
    #[command(name = "service-suite", display_order = 62)]
    ServiceSuite {
        /// Substring filter against the script's relative name (e.g.
        /// `t1/` to run only the T1 cohort, `boot` for boot-related tests).
        /// Matches scripts whose name *contains* the substring.
        #[arg(long, value_name = "SUBSTRING")]
        filter: Option<String>,

        /// Preserve each script's artefact directory even on success
        #[arg(long)]
        keep_artefacts: bool,

        /// Build the harness binary with the dev profile (`<target>/debug/`).
        /// Default is release. Overrides `[ratatoskr.harness] debug` from
        /// `brokkr.toml`.
        #[arg(long, conflicts_with = "release")]
        debug: bool,

        /// Force the release profile, overriding `[ratatoskr.harness] debug`
        /// from `brokkr.toml`. Mutually exclusive with `--debug`.
        #[arg(long)]
        release: bool,

        /// Keep going after a failed script. Default is to stop on the
        /// first failure so the artefact dir lands fast for triage.
        #[arg(long)]
        keep_going: bool,

        /// Include scripts marked `expected = ignored` in the frontmatter.
        /// By default these are skipped (they reproduce known-broken
        /// Service behaviour and would block clean suite runs).
        #[arg(long)]
        include_ignored: bool,

        /// Run the cohort this many times in order. Each cycle invokes
        /// every selected script once. Default 1.
        #[arg(short = 'N', long = "repeat", default_value = "1", value_name = "COUNT")]
        repeat: u32,
    },

    /// [ratatoskr] List discovered sync-test scripts (plan 3)
    ///
    /// Walks `[ratatoskr] sync_script_dir` (default
    /// `crates/app/tests/sync-harness`), parses top-of-file frontmatter
    /// (`description`, `expected`, `fixture`, `protocol`, `ceiling`),
    /// prints a sorted table. Empty-state output names the expected
    /// directory so a fresh checkout (no harness module yet) gets a
    /// useful response.
    #[command(name = "sync-list", display_order = 66)]
    SyncList,

    /// [ratatoskr] Run a sync-test script against sæhrimnir (plan 3)
    ///
    /// Builds the harness binary declared by `[ratatoskr.harness]`,
    /// spawns sæhrimnir against the script's `-- fixture: <NAME>`
    /// frontmatter, parses the per-protocol ports out of the readiness
    /// sentinel, then spawns `<harness binary> --test-harness <SCRIPT>`
    /// with `BROKKR_HARNESS_ARTEFACT_DIR` and `BROKKR_TEST_BIN_DIR` set
    /// plus one `RATATOSKR_TEST_<PROTO>_ENDPOINT` per protocol whose
    /// env-var spelling is configured under `[ratatoskr]`. Tears
    /// sæhrimnir down with a 1.5s SIGTERM budget after the harness
    /// exits. PASS/FAIL on the harness binary's exit code; on FAIL the
    /// per-run dir at `.brokkr/ratatoskr/sync/<test>/run-N/` is
    /// preserved with `harness/`, `mock/`, and `run.toml` for triage.
    #[command(name = "sync-smoke", display_order = 67)]
    SyncSmoke {
        /// Path to the sync-test `.lua` script. Frontmatter must declare
        /// `-- fixture: <NAME>` so brokkr can resolve which fixture to
        /// load into sæhrimnir.
        script: String,

        /// Preserve the artefact directory even on success
        #[arg(long)]
        keep_artefacts: bool,

        /// Build the harness binary with the dev profile (`<target>/debug/`).
        /// Default is release. Overrides `[ratatoskr.harness] debug` from
        /// `brokkr.toml`.
        #[arg(long, conflicts_with = "release")]
        debug: bool,

        /// Force the release profile, overriding `[ratatoskr.harness] debug`
        /// from `brokkr.toml`. Mutually exclusive with `--debug`.
        #[arg(long)]
        release: bool,
    },

    /// [ratatoskr] Bench a sync-test script against sæhrimnir (plan 3)
    ///
    /// Same spawn shape as `sync-smoke`, measured. Spawns sæhrimnir
    /// once and reuses it across iterations; spawns the harness binary
    /// `--bench` times with `BROKKR_MARKER_FIFO` set so the script can
    /// emit `SYNC_START` / `SYNC_END` markers around the measured
    /// region. Best-of-N is selected on that marker span (falls back
    /// to wall-clock elapsed when the script doesn't emit those
    /// markers); the best iteration's `summary.json` (if the script
    /// writes one into `BROKKR_HARNESS_ARTEFACT_DIR`) gets ingested as
    /// `meta.<key>` rows alongside the result. Stored in
    /// `.brokkr/results.db` via the standard `BenchHarness` so
    /// `brokkr results --compare` works.
    #[command(name = "sync-bench", display_order = 68)]
    SyncBench {
        /// Path to the sync-test `.lua` script (frontmatter must
        /// declare `-- fixture: <NAME>`).
        script: String,

        /// Number of measured iterations. Default 3.
        #[arg(long, default_value = "3", value_name = "COUNT")]
        bench: usize,

        /// Allow recording on a dirty git tree (results land under the
        /// `dirty` alias instead of being skipped).
        #[arg(long)]
        force: bool,

        /// Preserve the artefact directory even on success
        #[arg(long)]
        keep_artefacts: bool,

        /// Build the harness binary with the dev profile (`<target>/debug/`).
        /// Default is release for parity with what users will run.
        /// Overrides `[ratatoskr.harness] debug` from `brokkr.toml`.
        #[arg(long, conflicts_with = "release")]
        debug: bool,

        /// Force the release profile, overriding `[ratatoskr.harness] debug`
        /// from `brokkr.toml`. Mutually exclusive with `--debug`.
        #[arg(long)]
        release: bool,

        /// Run the named `[ratatoskr.gate.<name>]` gate after the bench
        /// completes. Records a row in `.brokkr/ratatoskr/gate.db`,
        /// looks up the per-hostname baseline, and exits non-zero if any
        /// metric rule fails. See `docs/commands/ratatoskr-gate.md`.
        #[arg(long, value_name = "NAME")]
        gate: Option<String>,

        /// Record this run as a baseline candidate for `--gate <name>`,
        /// suppress gate evaluation, and print the new UUID plus the TOML
        /// line to paste under `[ratatoskr.gate.<name>.baseline]`.
        /// Requires `--gate`.
        #[arg(long, requires = "gate")]
        as_baseline: bool,
    },

    /// [ratatoskr] Spawn sæhrimnir against a fixture, print endpoints, run until Ctrl-C
    ///
    /// Manual-exploration tool for plan 3. Resolves
    /// `[ratatoskr] mock_server_binary` and `[ratatoskr] fixtures_dir`
    /// from `brokkr.toml` (both required), spawns sæhrimnir with
    /// `--fixture <PATH> --readiness-file <PATH>`, waits for the
    /// readiness sentinel, prints the per-protocol listening
    /// endpoints, and runs until SIGINT/SIGTERM. SIGTERMs sæhrimnir
    /// with a 1.5s budget on shutdown before escalating to SIGKILL.
    /// Auto-build of sæhrimnir is not yet wired - the binary must
    /// already exist at `mock_server_binary`.
    #[command(name = "mock-serve", display_order = 65)]
    MockServe {
        /// Fixture name. Resolves to `<fixtures_dir>/<NAME>.toml` or
        /// `<fixtures_dir>/<NAME>.lua` (whichever exists; both is an
        /// error). To disambiguate when both exist, pass the name with
        /// its extension (e.g. `--fixture jmap-small.lua`).
        #[arg(long, value_name = "NAME")]
        fixture: String,
    },

    /// [piners] Run a keyword-selected slice of the parity corpus
    ///
    /// Resolves probes from the piners-owned registry (`pins.toml` +
    /// `<keyword>.toml` files under `[piners] registry_dir`), hard-verifies
    /// each selected probe's `strategy.pine` + `tv_trades.csv` (and the
    /// selection's referenced feed groups) against the corpus tree under
    /// `[piners] corpus_root` by xxh128, writes a manifest, builds the
    /// `[piners.harness]` binary once, and invokes it with `--manifest
    /// <path>`. The harness emits NDJSON per-probe disposition lines that
    /// brokkr renders.
    ///
    /// Selection is over the pinned universe: `--keyword` (repeatable)
    /// unions groupings, `--probe <id>` (repeatable) picks individual
    /// probes, `--all` takes everything (the slow characterization pass). A
    /// bare invocation with
    /// no selection is an error - the full corpus never runs by accident.
    /// `--verify-only` walks and verifies the whole universe without
    /// building or running. A missing pinned path or a hash mismatch is a
    /// hard error. The run fails on a real break (compile/runtime) or a
    /// non-zero harness exit; parity tiers are reported but do not fail the
    /// run yet (baseline work is deferred). Default profile is debug.
    #[command(name = "corpus", display_order = 70)]
    Corpus {
        /// Keyword grouping to select (repeatable or comma-separated;
        /// union of matched probes). Resolves through `pins.toml`, so
        /// every selected probe is verified.
        #[arg(long, value_name = "KEYWORD", value_delimiter = ',')]
        keyword: Vec<String>,

        /// Select a probe by id (repeatable or comma-separated; the union
        /// of the listed probes), resolved directly against `pins.toml`. A
        /// probe pinned but absent from every keyword file is still
        /// selectable this way.
        #[arg(long, value_name = "ID", value_delimiter = ',')]
        probe: Vec<String>,

        /// Select the whole pinned universe (slow characterization pass).
        #[arg(long)]
        all: bool,

        /// Verify every pinned probe's files against the submodule and
        /// exit, without building or running the harness. Use after a
        /// submodule re-pin to catch drift.
        #[arg(long, conflicts_with_all = ["bench", "hotpath", "alloc"])]
        verify_only: bool,

        /// Stamp `pins.toml` from the corpus filesystem (no build, no
        /// harness). Sibling to `--verify-only`, and the only way
        /// `pins.toml` is created or re-stamped. Discovers probe dirs
        /// anywhere under `corpus_root` by the marker (`strategy.pine` +
        /// `tv_trades.csv`), not the pinned universe. `--reseed --all`
        /// regenerates the whole file from the tree (probes whose dirs
        /// vanished drop out); `--reseed --probe <id>` (repeatable)
        /// upserts each. Re-stamps `[feeds]` hashes, preserves `[roots]`
        /// and the hand-maintained probe fields, assigns feeds to new
        /// probes by longest `[roots]` prefix. Prints added/changed/removed;
        /// review the result with `git diff pins.toml`. Not usable with
        /// `--keyword` or `--verify-only`.
        #[arg(long, conflicts_with_all = ["verify_only", "keyword", "bench", "hotpath", "alloc"])]
        reseed: bool,

        /// Run the selection, then stamp each selected probe's current
        /// disposition into its `expected` field in `pins.toml` (the gate's
        /// pinned contract). Sibling to `--reseed`: reseed adopts new corpus
        /// content, bless adopts new dispositions. Records reality, including
        /// `compile_fail`/`runtime_fail`/`no_tv_data`/`no_overlap` outcomes.
        /// Prints `blessed N (changed M)`; review with `git diff pins.toml`.
        /// Combine with `--all`/`--keyword`/`--probe` to scope what is
        /// blessed. Not usable with `--verify-only` or `--reseed`.
        #[arg(long, conflicts_with_all = ["verify_only", "reseed", "bench", "hotpath", "alloc"])]
        bless: bool,

        /// Run, aggregate, and report the per-probe expected-disposition gate
        /// diff, but do not fail on it. The harness exit code still governs
        /// pass/fail. Use during the bless-everything rollout (before
        /// expectations exist) or for ad-hoc "just show me the breakdown"
        /// runs.
        #[arg(long, conflicts_with_all = ["bench", "hotpath", "alloc"])]
        no_gate: bool,

        /// Build the harness with the dev profile (`<target>/debug/`).
        /// This is already the default for `corpus`; the flag is here to
        /// override `[piners.harness] debug = false`. Mutually exclusive
        /// with `--release`.
        #[arg(long, conflicts_with = "release")]
        debug: bool,

        /// Build the harness with the release profile (the slow
        /// characterization build). Overrides the debug default.
        #[arg(long)]
        release: bool,

        /// Preserve the run dir (manifest + harness output) even on
        /// success. Failures are always preserved. (Parity runs only;
        /// measured runs use the bench scratch dir.)
        #[arg(long, conflicts_with_all = ["bench", "hotpath", "alloc"])]
        keep_artefacts: bool,

        /// Extra flags forwarded verbatim to the harness binary, appended
        /// after `--manifest <path>`: everything after a literal `--`, e.g.
        /// `brokkr corpus --probe x --no-gate -- --scan-signal-extra`. The
        /// allowlist-friendly replacement for env-var-prefixed invocations.
        /// Works for parity and measured runs; recorded in the run row's
        /// selector (runs.db) / cli_args (results.db). Conflicts with
        /// `--verify-only`/`--reseed` (no harness runs) and `--bless` (pins
        /// must record default-behavior dispositions only). The gate stays
        /// active - pair with `--no-gate` when the flags change dispositions.
        #[arg(last = true, value_name = "HARNESS_FLAGS",
              conflicts_with_all = ["verify_only", "reseed", "bless"])]
        harness_args: Vec<String>,

        /// Measurement mode (`--hotpath`/`--alloc`) and shared build flags.
        /// With no measurement flag this is a bare parity run (gate +
        /// runs.db); `--hotpath`/`--alloc` build the harness with the hotpath
        /// feature and record to results.db via `brokkr results`. NOTE:
        /// `--force` is dual-purpose here - in a parity run it bypasses the
        /// ~270s pre-run runtime ceiling; in a measured run it carries its
        /// usual "run on a dirty tree" meaning (the ceiling is a parity-only
        /// concept). `--bench` is not yet supported for corpus.
        #[command(flatten)]
        mode: ModeArgs,
    },

    /// [piners] Query the corpus run store (.brokkr/piners/corpus/runs.db)
    #[command(
        name = "corpus-results",
        display_order = 71,
        long_about = "\
Query the corpus run store written by `brokkr corpus`
(.brokkr/piners/corpus/runs.db). This is piners' parity-corpus query
surface; `brokkr results` stays the benchmark store (hotpath/alloc).

Examples:
  brokkr corpus-results                                    # table of recent corpus runs
  brokkr corpus-results 42                                 # run 42's per-probe dispositions
  brokkr corpus-results --probe magnifier-tick-dist-endpoints-01   # disposition + trade_diff rows
  brokkr corpus-results --diffs --probe a --probe b                # multi-probe diff table (latest run)
  brokkr corpus-results --diffs --probe a --columns our_qty,tv_entry_qty,our_pnl,tv_pnl  # projected
  brokkr corpus-results --diffs --probe a --columns all            # every column, rendered vertically
  brokkr corpus-results --diffs --where 'exit_price_delta > 0.05'  # filtered trade rows (latest run)
  brokkr corpus-results --runtimes --over 269                      # probes whose runtime nears the wall
  brokkr corpus-results --trend magnifier-tick-dist-endpoints-01   # tier/p90 over recent runs
  brokkr corpus-results --sql 'SELECT probe, p90_exit FROM disposition'  # read-only escape hatch"
    )]
    CorpusResults {
        /// Corpus run id (default: latest). A bare positional, also accepted
        /// as `--run`.
        #[arg(value_name = "RUN_ID")]
        run_id: Option<i64>,

        /// Select a specific corpus run id (default: latest). Equivalent to the
        /// bare positional.
        #[arg(long, value_name = "N")]
        run: Option<i64>,

        /// Maximum number of rows to show (recent-runs table; --trend history)
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,

        /// Probe selector. A single `--probe` (without `--diffs`) shows that
        /// probe's combo view - its disposition plus `trade_diff` rows.
        /// Repeatable or comma-separated under `--diffs` to narrow the diff
        /// table to several probes at once.
        #[arg(long, value_name = "ID", value_delimiter = ',')]
        probe: Vec<String>,

        /// List `trade_diff` rows across the run (latest run by default). Shape
        /// it with `--probe`, `--columns`, and/or `--where`.
        #[arg(long)]
        diffs: bool,

        /// Project the `--diffs` table onto these columns (comma-separated or
        /// repeated). Default is a curated set covering the time/price/qty/pnl
        /// axes; `all` selects every trade_diff column and renders vertically.
        /// An unknown name lists the valid columns.
        #[arg(long, value_name = "COLS", value_delimiter = ',')]
        columns: Vec<String>,

        /// Show each probe's most-recent runtime, slowest first (shares the
        /// pre-run ceiling's per-probe estimate, so it can't drift from the
        /// wall).
        #[arg(long)]
        runtimes: bool,

        /// With `--runtimes`, keep only probes above this many seconds (e.g.
        /// `--over 269` for what nears the ceiling).
        #[arg(long, value_name = "SECS")]
        over: Option<f64>,

        /// Trend a probe's disposition/tier/p90 over recent runs.
        #[arg(long, value_name = "ID")]
        trend: Option<String>,

        /// Raw SQL boolean filter for `--diffs` (trusted local input; the DB is
        /// opened read-only). E.g. `--diffs --where "exit_price_delta > 0.05"`.
        #[arg(long = "where", value_name = "EXPR")]
        where_expr: Option<String>,

        /// Run a read-only `SELECT`/`WITH` query against runs.db (the escape
        /// hatch for anything the canned views don't cover).
        #[arg(long, value_name = "SQL")]
        sql: Option<String>,

        /// In the run-detail view (`--run`/bare id), show every probe instead
        /// of only the ones that deviate from their pin.
        #[arg(long)]
        full: bool,
    },

    /// [piners] Differential-lint corpus: piners vs pine-lint over .pine snippets
    #[command(
        name = "lint-corpus",
        display_order = 72,
        long_about = "\
Run a keyword-selected slice of the lint corpus through two offline
validators - piners (this dirty tree) and pine-lint - diff their
diagnostics on a (line, col, severity) grain, and gate on a pinned
agreement disposition per snippet. `--reanchor` consults TradingView
(pine-lint --tv) to re-ground the pins. See docs/commands/lint-corpus.md."
    )]
    LintCorpus {
        /// Keyword grouping to select (repeatable or comma-separated; union
        /// of matched probes). Resolves through `lints.toml`.
        #[arg(long, value_name = "KEYWORD", value_delimiter = ',')]
        keyword: Vec<String>,

        /// Select a snippet by id (repeatable or comma-separated; the union
        /// of the listed probes), resolved against `lints.toml`.
        #[arg(long, value_name = "ID", value_delimiter = ',')]
        probe: Vec<String>,

        /// Select the whole pinned universe (full characterization pass).
        #[arg(long)]
        all: bool,

        /// Hash-verify every selected snippet against the corpus tree and
        /// exit, without building or running either validator.
        #[arg(long, conflicts_with_all = ["reanchor", "bless", "reseed"])]
        verify_only: bool,

        /// Stamp `lints.toml` from the snippet tree (the bootstrap and
        /// after-edit re-stamp; the only way the file is created or its
        /// hashes refreshed). No build, no run. `--reseed --all` regenerates
        /// from the snippet dir; `--reseed --probe <id>` upserts. Preserves
        /// each surviving probe's `expected` and TV anchor.
        #[arg(long, conflicts_with_all = ["reanchor", "bless", "verify_only"])]
        reseed: bool,

        /// Refresh the TV anchor: drive `pine-lint --tv` over the selection
        /// and re-stamp each probe's TV fingerprint + timestamp into
        /// `lints.toml`. The periodic, network-touching registry writer.
        /// Not usable with `--bless` or `--verify-only`.
        #[arg(long, conflicts_with_all = ["bless", "verify_only"])]
        reanchor: bool,

        /// Run the selection, then stamp each probe's current disposition
        /// into its `expected` field in `lints.toml`. Never gates. Not
        /// usable with `--reanchor` or `--verify-only`.
        #[arg(long, conflicts_with_all = ["reanchor", "verify_only"])]
        bless: bool,

        /// Run and report the per-probe gate diff, but do not fail on it.
        #[arg(long)]
        no_gate: bool,

        /// Compare type/analysis diagnostics too, not just parser/syntax. The
        /// default is syntax-only: the two validators' type/semantic
        /// diagnostics diverge enough that comparing them is mostly noise.
        #[arg(long)]
        all_stages: bool,

        /// Include warning diagnostics in the gated diff. Default is errors
        /// only.
        #[arg(long)]
        warnings: bool,

        /// Build the piners validator with the dev profile (the default for
        /// lint-corpus; overrides `[piners.lint] debug = false`). Mutually
        /// exclusive with `--release`.
        #[arg(long, conflicts_with = "release")]
        debug: bool,

        /// Build the piners validator with the release profile.
        #[arg(long)]
        release: bool,
    },

    /// [piners] Query the lint corpus run store (.brokkr/piners/lint/runs.db)
    #[command(
        name = "lint-results",
        display_order = 73,
        long_about = "\
Query the lint corpus run store written by `brokkr lint-corpus`
(.brokkr/piners/lint/runs.db).

Examples:
  brokkr lint-results            # table of recent lint runs
  brokkr lint-results 42         # run 42's per-probe dispositions (deviations only)
  brokkr lint-results 42 --full  # every probe in run 42"
    )]
    LintResults {
        /// Lint run id (default: latest). A bare positional, also accepted as
        /// `--run`.
        #[arg(value_name = "RUN_ID")]
        run_id: Option<i64>,

        /// Select a specific lint run id (default: latest). Equivalent to the
        /// bare positional.
        #[arg(long, value_name = "N")]
        run: Option<i64>,

        /// Maximum number of rows to show in the recent-runs table.
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,

        /// In the run-detail view, show every probe instead of only the ones
        /// that deviate from their pin.
        #[arg(long)]
        full: bool,
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

impl ModeArgs {
    /// Whether a measurement mode (`--bench`/`--hotpath`/`--alloc`) is set.
    /// Used by commands like `corpus` that flatten `ModeArgs` and route bare
    /// (no-measurement) invocations to a different, non-measured handler.
    pub(crate) fn is_measured(&self) -> bool {
        self.bench.is_some() || self.hotpath.is_some() || self.alloc.is_some()
    }
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
/// (Verify doesn't take `--io-uring` or `--compression` - different surface
/// than `PbfArgs`, hence a separate struct.)
///
/// `--input <path>` is the escape hatch for handcrafted fixtures that no
/// real-world dataset exercises (e.g. overlapping-blob PBFs from
/// `pbfhogg/examples/`). When set, dataset / variant resolution is
/// skipped and the path is used directly. Mutually exclusive with
/// user-provided `--dataset` / `--variant` (their defaults still
/// populate the struct, but clap's `conflicts_with` only triggers on
/// user-supplied flags).
#[derive(Args, Clone)]
pub(crate) struct VerifyPbfArgs {
    /// Dataset name from brokkr.toml
    #[arg(long, default_value = "denmark", conflicts_with = "input")]
    pub(crate) dataset: String,
    /// PBF variant to use (raw, indexed, locations)
    #[arg(long, default_value = "indexed", conflicts_with = "input")]
    pub(crate) variant: String,
    /// Path to a handcrafted input PBF (skips dataset resolution).
    /// Mutually exclusive with `--dataset` / `--variant`.
    #[arg(long, value_name = "PATH")]
    pub(crate) input: Option<std::path::PathBuf>,
    /// Snapshot key to cross-validate. Use `base` (or omit) for the
    /// dataset's primary data; pass a key registered under
    /// `[dataset.snapshot.<key>]` for a historical or adversarial encoding
    /// (e.g. one produced by `brokkr degrade --unsort --as-snapshot` or
    /// `brokkr repack --as-snapshot`). Overrides only the PBF input; OSC
    /// changes (for merge/derive-changes verifies) still resolve from the
    /// dataset's primary chain. Mutually exclusive with `--input`.
    #[arg(long, conflicts_with = "input")]
    pub(crate) snapshot: Option<String>,
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
        /// Accepted only so the refusal is explicit: enriched (injected-prepass)
        /// output is osmium-incompatible by design (BlobHeader field 5 headers
        /// run ~1-8 KB; libosmium 2.23 rejects any BlobHeader over 127 bytes,
        /// their issue 405). brokkr cannot cross-validate it against osmium, so
        /// passing this flag errors with a pointer to flag-off verify. Enriched
        /// correctness is covered by pbfhogg's own oracle-roundtrip + backend
        /// parity suite.
        #[arg(long)]
        inject_prepass: bool,
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
        #[arg(long, default_value = "denmark", conflicts_with = "input")]
        dataset: String,
        #[arg(long, default_value = "indexed", conflicts_with = "input")]
        variant: String,
        /// Path to a handcrafted input PBF (skips dataset resolution).
        #[arg(long, value_name = "PATH")]
        input: Option<std::path::PathBuf>,
        /// Snapshot key to cross-validate (see `--snapshot` on other verify
        /// subcommands). Overrides only the PBF input. Mutually exclusive
        /// with `--input`.
        #[arg(long, conflicts_with = "input")]
        snapshot: Option<String>,
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
        #[arg(long, default_value = "denmark", conflicts_with = "input")]
        dataset: String,
        #[arg(long, default_value = "indexed", conflicts_with = "input")]
        variant: String,
        /// Path to a handcrafted input PBF (skips dataset resolution).
        #[arg(long, value_name = "PATH")]
        input: Option<std::path::PathBuf>,
        /// Snapshot key to cross-validate (see `--snapshot` on other verify
        /// subcommands). Overrides only the PBF input; the OSC still
        /// resolves from the dataset's primary chain. Mutually exclusive
        /// with `--input`.
        #[arg(long, conflicts_with = "input")]
        snapshot: Option<String>,
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
        /// Print per-zoom ocean ring geometry statistics
        #[arg(long)]
        geometry_stats: bool,
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


