//! Consolidated pbfhogg measurable command definitions.
//!
//! Every pbfhogg command that can be measured (wall-clock, hotpath, alloc, or
//! profile) is defined here exactly once.  This replaces the scattered
//! definitions across `bench_commands.rs`, `bench_build_geocode_index.rs`,
//! and `hotpath.rs`.

use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::measure::CommandContext;

// ---------------------------------------------------------------------------
// Input / output descriptors
// ---------------------------------------------------------------------------

/// What inputs a command requires beyond the binary itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// PBF file only.
    Pbf,
    /// PBF + OSC diff file.
    PbfAndOsc,
    /// PBF + bbox string.
    PbfAndBbox,
    /// PBF + merged PBF (generated from PBF + OSC).
    PbfAndMerged,
    /// OSC file only (no PBF).
    OscOnly,
}

/// What output a command produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputKind {
    /// Writes a scratch `.osm.pbf` file.
    ScratchPbf,
    /// Writes a scratch `.osc.gz` file.
    ScratchOsc,
    /// Writes to a scratch directory. The payload is the directory name stem
    /// (e.g. `"geocode"`) — suffixed with the dataset at resolution time.
    ScratchDir(&'static str),
    /// No output file (read-only / stdout-only commands).
    None,
}

/// Which build-args flavor to produce: the default wallclock/suite args,
/// or the hotpath-profile args (binary prepended, hotpath-prefixed scratch
/// filenames, a few simplified argv forms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgMode {
    Bench,
    Hotpath,
}

impl ArgMode {
    /// Scratch output filename prefix. Keeps bench/hotpath runs in separate
    /// files so a hotpath run on the same command doesn't clobber a bench
    /// cache file between sequential invocations.
    fn scratch_prefix(self) -> &'static str {
        match self {
            Self::Bench => "bench",
            Self::Hotpath => "hotpath",
        }
    }
}

// ---------------------------------------------------------------------------
// Sub-enums for multi-variant commands
// ---------------------------------------------------------------------------

/// Extract strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractStrategy {
    Simple,
    Complete,
    Smart,
}

/// `cat --type` filter — restrict passthrough to a specific object type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatTypeFilter {
    Way,
    Relation,
}

impl CatTypeFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Way => "way",
            Self::Relation => "relation",
        }
    }

    pub fn parse(s: &str) -> Result<Self, DevError> {
        match s {
            "way" => Ok(Self::Way),
            "relation" => Ok(Self::Relation),
            _ => Err(DevError::Config(format!(
                "unknown cat --type '{s}' (expected: way, relation)"
            ))),
        }
    }
}

/// Output format for `diff` / `diff-snapshots` (`pbfhogg diff` accepts both
/// a default summary format and `--format osc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
#[value(rename_all = "lowercase")]
pub enum DiffFormat {
    /// Default summary diff (`pbfhogg diff <a> <b> -c`).
    #[default]
    Default,
    /// OSC-format diff (`pbfhogg diff --format osc <a> <b> -o <out>`).
    Osc,
}

impl ExtractStrategy {
    pub fn all() -> &'static [ExtractStrategy] {
        &[Self::Simple, Self::Complete, Self::Smart]
    }

    pub fn parse(s: &str) -> Result<Self, DevError> {
        match s {
            "simple" => Ok(Self::Simple),
            "complete" => Ok(Self::Complete),
            "smart" => Ok(Self::Smart),
            _ => Err(DevError::Config(format!("unknown extract strategy: {s}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Hardcoded bench fixtures
// ---------------------------------------------------------------------------

/// Fixed ID set used by both `getid` and `getparents` suite presets. Includes
/// three IDs of each object type (n/w/r) so the bench shape is reproducible.
/// Some entries may not exist in very small test datasets — the pbfhogg
/// binary tolerates missing IDs without erroring.
const GETID_BENCH_IDS: &[&str] = &[
    "n115722", "n115723", "n115724",
    "w2080", "w2081", "w2082",
    "r174", "r213", "r339",
];

/// Subset of `GETID_BENCH_IDS` used by the `getparents` preset — fewer
/// ids because getparents is a superset operation and more inputs would
/// dominate with parent-traversal cost.
const GETPARENTS_BENCH_IDS: &[&str] = &["n115722", "n115723", "w2080"];

/// Min-count threshold used by the bench version of `inspect tags`. Set
/// impossibly high so pbfhogg's default tag-frequency filter never trims
/// anything — we want the full decode cost measured, not just popular tags.
const INSPECT_ALL_TAGS_MIN_COUNT: &str = "999999999";

// ---------------------------------------------------------------------------
// PbfhoggCommand — the unified command enum
// ---------------------------------------------------------------------------

/// Every measurable pbfhogg command.
///
/// Tool CLI commands, standalone benchmarks, multi-variant benchmarks,
/// and external baselines are all variants of this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PbfhoggCommand {
    // -- Tool CLI commands (26 commands from bench_commands.rs) --
    /// Unified `inspect`. Flags:
    ///   - `nodes` → `--nodes` (PBF node statistics; mutually exclusive
    ///     with `tags`).
    ///   - `tags` → `tags` subcommand for tag-frequency (mutually
    ///     exclusive with `nodes`). Always emits `--min-count 999999999`
    ///     internally so every tag is reported.
    ///   - `type_filter` → `--type <node|way|relation>` for the tags
    ///     subcommand only.
    Inspect {
        nodes: bool,
        tags: bool,
        type_filter: Option<String>,
    },
    CheckRefs,
    /// `check --ids`. `full` adds `--full` — per-type duplicate-ID detection
    /// via RoaringTreemap sets, in addition to the streaming monotonicity /
    /// type-order checks.
    CheckIds { full: bool },
    Sort,
    /// Unified `cat` — all flags orthogonal:
    ///   - `type_filter` → `--type way|relation` (restricts output to one
    ///     object type; single-pass filter).
    ///   - `dedupe` → `--dedupe` with two PBF inputs (merge-style dedupe,
    ///     the only Cat flavour that supports `--io-uring`).
    ///   - `clean` → `--clean`, forces the full-decode / re-frame Framed
    ///     path (`cat_filtered`) rather than Raw passthrough.
    Cat {
        type_filter: Option<CatTypeFilter>,
        dedupe: bool,
        clean: bool,
    },
    /// Unified `tags-filter`. Orthogonal flags:
    ///   - `filter` — the pbfhogg filter expression (e.g.
    ///     `w/highway=primary`, `amenity=restaurant`); defaults to
    ///     `w/highway=primary`.
    ///   - `omit_referenced` → `-R` (single-pass, matched objects only;
    ///     default off = two-pass, pull in referenced objects).
    ///   - `input_kind_osc` → `--input-kind osc` (read an OSC diff
    ///     instead of a PBF as input).
    TagsFilter {
        filter: String,
        omit_referenced: bool,
        input_kind_osc: bool,
    },
    /// Unified `getid`. Flags:
    ///   - `add_referenced` → `--add-referenced` (two-pass with
    ///     referenced-element collection).
    ///   - `invert` → `--invert` (select everything NOT matching).
    /// The ID list is hardcoded (same fixed set used by all three
    /// previous presets) to keep the bench shape reproducible.
    Getid {
        add_referenced: bool,
        invert: bool,
    },
    Getparents,
    Renumber,
    MergeChanges,
    ApplyChanges,
    AddLocationsToWays,
    TimeFilter,
    /// Unified `diff`. `format` selects summary (`Default`) or OSC-format
    /// output. Reuses `DiffFormat` with `DiffSnapshots`.
    Diff { format: DiffFormat },

    /// Two-snapshot diff: compares two PBFs from different point-in-time
    /// snapshots of the same dataset (e.g. `planet-20260223` vs
    /// `planet-20260411`). Unlike `Diff`, neither side is derived from
    /// `apply-changes` — both come from independent snapshot resolution.
    /// The format flag selects between summary diff and OSC-format diff.
    DiffSnapshots { format: DiffFormat },

    // -- Standalone commands --
    BuildGeocodeIndex,

    // -- Multi-variant benchmarks --
    Extract { strategy: ExtractStrategy },

    /// Multi-extract: single-pass N-region extract benchmark.
    MultiExtract { regions: usize },
}

impl PbfhoggCommand {
    /// The command ID string used in CLI and result DB.
    pub fn id(&self) -> &str {
        match self {
            Self::Inspect { .. } => "inspect",
            Self::CheckRefs => "check-refs",
            Self::CheckIds { .. } => "check-ids",
            Self::Sort => "sort",
            Self::Cat { .. } => "cat",
            Self::TagsFilter { .. } => "tags-filter",
            Self::Getid { .. } => "getid",
            Self::Getparents => "getparents",
            Self::Renumber => "renumber",
            Self::MergeChanges => "merge-changes",
            Self::ApplyChanges => "apply-changes",
            Self::AddLocationsToWays => "add-locations-to-ways",
            Self::TimeFilter => "time-filter",
            Self::Diff { .. } => "diff",
            Self::BuildGeocodeIndex => "build-geocode-index",
            Self::Extract { .. } => "extract",
            Self::MultiExtract { .. } => "multi-extract",
            Self::DiffSnapshots { .. } => "diff-snapshots",
        }
    }

    /// What inputs this command requires.
    pub fn input_kind(&self) -> InputKind {
        match self {
            Self::TagsFilter { input_kind_osc: true, .. } => InputKind::PbfAndOsc,
            Self::MergeChanges => InputKind::OscOnly,
            Self::ApplyChanges => InputKind::PbfAndOsc,
            Self::Diff { .. } => InputKind::PbfAndMerged,
            Self::Extract { .. } | Self::MultiExtract { .. } => InputKind::PbfAndBbox,
            // DiffSnapshots resolves both PBFs via the snapshot path resolver.
            // Doesn't need OSC or merged-PBF setup; the dispatch layer handles
            // the snapshot resolution outside the InputKind dispatch.
            Self::DiffSnapshots { .. } => InputKind::Pbf,
            _ => InputKind::Pbf,
        }
    }

    /// What output this command produces.
    pub fn output_kind(&self) -> OutputKind {
        match self {
            // No output file (read-only / stdout commands).
            Self::Inspect { .. }
            | Self::CheckRefs
            | Self::CheckIds { .. }
            | Self::Diff { format: DiffFormat::Default }
            | Self::DiffSnapshots { format: DiffFormat::Default } => OutputKind::None,

            // OSC output (includes OSC-format diff and diff-snapshots).
            Self::TagsFilter { input_kind_osc: true, .. }
            | Self::MergeChanges
            | Self::Diff { format: DiffFormat::Osc }
            | Self::DiffSnapshots { format: DiffFormat::Osc } => OutputKind::ScratchOsc,

            // Directory output.
            Self::BuildGeocodeIndex => OutputKind::ScratchDir("geocode"),
            Self::MultiExtract { .. } => OutputKind::ScratchDir("multi-extract"),

            // Everything else writes a scratch PBF.
            _ => OutputKind::ScratchPbf,
        }
    }

    /// Whether this command supports hotpath profiling.
    pub fn supports_hotpath(&self) -> bool {
        true
    }

    /// Whether this command accepts `--io-uring`.
    ///
    /// Only commands whose pbfhogg binary clap definition includes `--io-uring`
    /// are listed here.  Passing the flag to other commands causes clap exit
    /// code 2.
    pub fn supports_io_uring(&self) -> bool {
        matches!(
            self,
            Self::ApplyChanges
                | Self::Sort
                | Self::Diff { format: DiffFormat::Osc }
                | Self::Cat { dedupe: true, .. }
        )
    }

    /// Whether this command needs an OSC file.
    pub fn needs_osc(&self) -> bool {
        matches!(
            self.input_kind(),
            InputKind::PbfAndOsc | InputKind::OscOnly | InputKind::PbfAndMerged
        )
    }

    /// Whether this command needs a bbox.
    pub fn needs_bbox(&self) -> bool {
        matches!(self.input_kind(), InputKind::PbfAndBbox)
    }

    /// Non-zero exit codes that should be treated as success for this command.
    /// `diff` follows standard diff convention: exit 1 = differences found (not an error).
    pub fn ok_exit_codes(&self) -> &'static [i32] {
        match self {
            Self::Diff { .. } | Self::DiffSnapshots { .. } => &[1],
            _ => &[],
        }
    }

    /// The result command label for the DB — just the subcommand id
    /// (`"add-locations-to-ways"`, `"cat"`, `"diff-snapshots"`, ...). The
    /// measurement mode (`bench`/`hotpath`/`alloc`) lives in the
    /// `variant` column; axes (direct-io, compression, snapshot, …) live
    /// in `cli_args` and `brokkr_args`.
    pub fn result_command(&self) -> String {
        self.id().to_owned()
    }

    /// Build metadata key-value pairs for the result DB.
    ///
    /// Post-v13, this holds only *runtime observations* — things the
    /// harness learned that cli_args/brokkr_args can't tell you (e.g. the
    /// Diff merged-PBF cache state observed at dispatch time, the
    /// resolved filename/size for a snapshot key, …). Axis-like fields
    /// (index_type, snapshot key, strategy, bbox, …) are passed on the
    /// brokkr command line and recorded in the brokkr_args/cli_args
    /// columns, so they don't need mirroring here.
    pub fn metadata(&self, ctx: &CommandContext) -> Vec<crate::db::KvPair> {
        use crate::db::KvPair;

        match self {
            Self::Diff { .. } => {
                // Merged-PBF cache state is observed at dispatch time
                // (the caller sets the params based on whether the cached
                // merged file was reused). Lets `brokkr results <uuid>`
                // distinguish runs that paid the setup cost from runs that
                // reused a cached file.
                let mut meta = Vec::new();
                if let Some(state) = &ctx.params.merged_cache_state {
                    meta.push(KvPair::text("meta.merged_cache", state));
                }
                if let Some(age) = &ctx.params.merged_cache_age_s {
                    meta.push(KvPair::text("meta.merged_cache_age_s", age));
                }
                meta
            }
            Self::DiffSnapshots { .. } => {
                // The --to snapshot's resolved filename and size are
                // observations — the user passed a key like `20260411`,
                // the dispatch layer resolved it to an actual file via
                // brokkr.toml. Record the resolved info so queries can
                // identify which B-side file was actually consumed.
                let mut meta = Vec::new();
                if let Some(file) = &ctx.params.to_snapshot_file {
                    meta.push(KvPair::text("meta.to_snapshot_file", file));
                }
                if let Some(mb) = &ctx.params.to_snapshot_file_mb {
                    meta.push(KvPair::text("meta.to_snapshot_file_mb", mb));
                }
                meta
            }
            _ => vec![],
        }
    }

    /// Build the argument vector for this command given the resolved context.
    ///
    /// `ArgMode::Bench` produces argv without the binary path (the caller
    /// passes the binary separately when spawning). `ArgMode::Hotpath`
    /// prepends the binary path (matching the format expected by
    /// `run_hotpath_capture`) and picks hotpath-prefixed scratch filenames.
    /// A few commands have small argv differences between modes — those are
    /// called out inline.
    #[allow(clippy::too_many_lines)]
    pub fn build_args(
        &self,
        ctx: &CommandContext,
        mode: ArgMode,
    ) -> Result<Vec<String>, DevError> {
        let mut prefix: Vec<String> = Vec::new();
        if mode == ArgMode::Hotpath {
            prefix.push(ctx.binary_str()?.to_owned());
        }
        let body = self.build_body(ctx, mode)?;
        prefix.extend(body);
        Ok(prefix)
    }

    /// Build the body of the argument vector (no binary prefix).
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    fn build_body(
        &self,
        ctx: &CommandContext,
        mode: ArgMode,
    ) -> Result<Vec<String>, DevError> {
        match self {
            // -----------------------------------------------------------------
            // Tool CLI commands (26 from bench_commands.rs)
            // -----------------------------------------------------------------
            Self::Inspect {
                nodes,
                tags,
                type_filter,
            } => {
                let mut args: Vec<String> = vec!["inspect".into()];
                if *tags {
                    args.push("tags".into());
                    args.push(ctx.pbf_str()?.into());
                    // Hotpath legacy: the pre-unification build_hotpath_args
                    // dropped --type and --min-count for `inspect tags`.
                    // Preserved here so hotpath result rows don't shift.
                    if mode == ArgMode::Bench {
                        if let Some(tf) = type_filter {
                            args.push("--type".into());
                            args.push(tf.clone());
                        }
                        args.push("--min-count".into());
                        args.push(INSPECT_ALL_TAGS_MIN_COUNT.into());
                    }
                } else if *nodes {
                    args.push("--nodes".into());
                    args.push(ctx.pbf_str()?.into());
                } else {
                    args.push(ctx.pbf_str()?.into());
                }
                Ok(args)
            }
            Self::CheckRefs => Ok(vec!["check".into(), "--refs".into(), ctx.pbf_str()?.into()]),
            Self::CheckIds { full } => {
                let mut args = vec!["check".into(), "--ids".into(), ctx.pbf_str()?.into()];
                if *full {
                    args.push("--full".into());
                }
                Ok(args)
            }
            Self::Sort => {
                let output = scratch_output_path(ctx, self, mode);
                Ok(vec![
                    "sort".into(),
                    ctx.pbf_str()?.into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::Cat {
                type_filter,
                dedupe,
                clean,
            } => {
                let output = scratch_output_path(ctx, self, mode);
                let pbf = ctx.pbf_str()?;
                let mut args: Vec<String> = vec!["cat".into()];
                if *dedupe {
                    args.push("--dedupe".into());
                }
                if *clean {
                    // pbfhogg's --clean takes an ATTR value
                    // (version|changeset|timestamp|uid|user). For bench purposes
                    // we just need to force the full-decode / Framed path —
                    // `version` is the lightest-weight strip and always present.
                    args.push("--clean".into());
                    args.push("version".into());
                }
                // Input(s). `--dedupe` takes two PBFs (we pass the same
                // file twice for a deterministic bench shape).
                args.push(pbf.into());
                if *dedupe {
                    args.push(pbf.into());
                }
                if let Some(tf) = type_filter {
                    args.push("--type".into());
                    args.push(tf.as_str().into());
                }
                args.push("-o".into());
                args.push(path_to_string(&output)?);
                Ok(args)
            }
            Self::TagsFilter {
                filter,
                omit_referenced,
                input_kind_osc,
            } => {
                let output = scratch_output_path(ctx, self, mode);
                let mut args: Vec<String> = vec!["tags-filter".into()];
                if *input_kind_osc {
                    args.push("--input-kind".into());
                    args.push("osc".into());
                }
                if *omit_referenced {
                    args.push("-R".into());
                }
                // Input: OSC file when --input-kind osc, otherwise PBF.
                if *input_kind_osc {
                    args.push(ctx.osc_str()?.into());
                } else {
                    args.push(ctx.pbf_str()?.into());
                }
                args.push(filter.clone());
                args.push("-o".into());
                args.push(path_to_string(&output)?);
                Ok(args)
            }
            Self::Getid {
                add_referenced,
                invert,
            } => {
                let output = scratch_output_path(ctx, self, mode);
                let mut args: Vec<String> = vec!["getid".into()];
                if *invert {
                    args.push("--invert".into());
                }
                args.push(ctx.pbf_str()?.into());
                if *add_referenced {
                    args.push("--add-referenced".into());
                }
                for id in GETID_BENCH_IDS {
                    args.push((*id).into());
                }
                args.push("-o".into());
                args.push(path_to_string(&output)?);
                Ok(args)
            }
            Self::Getparents => {
                let output = scratch_output_path(ctx, self, mode);
                let mut args: Vec<String> = vec!["getparents".into(), ctx.pbf_str()?.into()];
                for id in GETPARENTS_BENCH_IDS {
                    args.push((*id).into());
                }
                args.push("-o".into());
                args.push(path_to_string(&output)?);
                Ok(args)
            }
            Self::Renumber => {
                let output = scratch_output_path(ctx, self, mode);
                Ok(vec![
                    "renumber".into(),
                    ctx.pbf_str()?.into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::MergeChanges => {
                let output = scratch_output_path(ctx, self, mode);
                let oscs = ctx.osc_strs()?;
                let mut args = vec!["merge-changes".into()];
                for o in oscs {
                    args.push(o.into());
                }
                args.push("-o".into());
                args.push(path_to_string(&output)?);
                Ok(args)
            }
            Self::ApplyChanges => {
                let output = scratch_output_path(ctx, self, mode);
                let osc = ctx.osc_str()?;
                let mut args = vec![
                    "apply-changes".into(),
                    ctx.pbf_str()?.into(),
                    osc.into(),
                ];
                // Hotpath legacy: pre-unification build_hotpath_args always
                // emitted --compression (default zlib) for apply-changes. The
                // dispatch layer also appends --compression from CLI, so
                // hotpath runs with an explicit --compression CLI flag end up
                // with it twice (pbfhogg takes the last one).
                if mode == ArgMode::Hotpath {
                    let compression = ctx.params.compression.as_deref().unwrap_or("zlib");
                    args.push("--compression".into());
                    args.push(compression.into());
                }
                args.push("-o".into());
                args.push(path_to_string(&output)?);
                Ok(args)
            }
            Self::AddLocationsToWays => {
                let output = scratch_output_path(ctx, self, mode);
                let mut args = vec![
                    "add-locations-to-ways".into(),
                    ctx.pbf_str()?.into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ];
                if let Some(it) = &ctx.params.index_type {
                    args.push("--index-type".into());
                    args.push(it.clone());
                }
                Ok(args)
            }
            Self::TimeFilter => {
                let output = scratch_output_path(ctx, self, mode);
                Ok(vec![
                    "time-filter".into(),
                    ctx.pbf_str()?.into(),
                    "2024-01-01T00:00:00Z".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::Diff { format } => {
                let merged = ctx.pbf_b_str()?;
                let pbf = ctx.pbf_str()?;
                match format {
                    DiffFormat::Default => Ok(vec![
                        "diff".into(),
                        pbf.into(),
                        merged.into(),
                        "-c".into(),
                    ]),
                    DiffFormat::Osc => {
                        let output = scratch_output_path(ctx, self, mode);
                        Ok(vec![
                            "diff".into(),
                            "--format".into(),
                            "osc".into(),
                            pbf.into(),
                            merged.into(),
                            "-o".into(),
                            path_to_string(&output)?,
                        ])
                    }
                }
            }
            Self::DiffSnapshots { format } => {
                // Both PBFs come from snapshot resolution; pbf_path is the
                // --from side, pbf_b_path is the --to side.
                let from = ctx.pbf_str()?;
                let to = ctx.pbf_b_str()?;
                match format {
                    DiffFormat::Default => Ok(vec![
                        "diff".into(),
                        from.into(),
                        to.into(),
                        "-c".into(),
                    ]),
                    DiffFormat::Osc => {
                        let output = scratch_output_path(ctx, self, mode);
                        Ok(vec![
                            "diff".into(),
                            "--format".into(),
                            "osc".into(),
                            from.into(),
                            to.into(),
                            "-o".into(),
                            path_to_string(&output)?,
                        ])
                    }
                }
            }

            // -----------------------------------------------------------------
            // Standalone commands
            // -----------------------------------------------------------------
            Self::BuildGeocodeIndex => {
                let output_dir = ctx.scratch_dir.join(format!("geocode-{}", ctx.dataset));
                let output_dir_str = output_dir.to_str().ok_or_else(|| {
                    DevError::Config("geocode output dir path is not valid UTF-8".into())
                })?;
                Ok(vec![
                    "build-geocode-index".into(),
                    ctx.pbf_str()?.into(),
                    "--output-dir".into(),
                    output_dir_str.into(),
                    "--force".into(),
                ])
            }

            // -----------------------------------------------------------------
            // Multi-extract: single-pass N-region extract benchmark
            // -----------------------------------------------------------------
            Self::MultiExtract { regions } => {
                if *regions == 0 {
                    return Err(DevError::Config(
                        "multi-extract requires at least 1 region".into(),
                    ));
                }
                let bbox_str = ctx
                    .bbox
                    .as_deref()
                    .ok_or_else(|| DevError::Config("multi-extract requires a bbox".into()))?;
                let parts: Vec<f64> = bbox_str
                    .split(',')
                    .map(|s| s.trim().parse::<f64>())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| DevError::Config(format!("invalid bbox: {e}")))?;
                if parts.len() != 4 {
                    return Err(DevError::Config(format!(
                        "bbox must have 4 values, got {}",
                        parts.len()
                    )));
                }
                let (min_lon, min_lat, max_lon, max_lat) =
                    (parts[0], parts[1], parts[2], parts[3]);

                let output_dir = ctx.scratch_dir.join("multi-extract");
                std::fs::create_dir_all(&output_dir)?;

                let strip_width = (max_lon - min_lon) / *regions as f64;
                let mut extracts = Vec::new();
                for i in 0..*regions {
                    let strip_min = min_lon + strip_width * i as f64;
                    let strip_max = if i + 1 == *regions {
                        max_lon
                    } else {
                        min_lon + strip_width * (i + 1) as f64
                    };
                    extracts.push(format!(
                        r#"    {{ "output": "strip-{i}.osm.pbf", "bbox": [{strip_min}, {min_lat}, {strip_max}, {max_lat}] }}"#
                    ));
                }

                let config_json = format!(
                    "{{\n  \"directory\": \"{}\",\n  \"extracts\": [\n{}\n  ]\n}}",
                    output_dir.display(),
                    extracts.join(",\n"),
                );

                let config_path = ctx.scratch_dir.join("multi-extract-config.json");
                std::fs::write(&config_path, &config_json)?;

                Ok(vec![
                    "extract".into(),
                    ctx.pbf_str()?.into(),
                    "--config".into(),
                    path_to_string(&config_path)?,
                    "--simple".into(),
                ])
            }

            // -----------------------------------------------------------------
            // Multi-variant: extract (with resolved bbox)
            // -----------------------------------------------------------------
            Self::Extract { strategy } => {
                let bbox = ctx
                    .bbox
                    .as_deref()
                    .ok_or_else(|| DevError::Config("extract requires a bbox".into()))?;
                let output = scratch_output_path(ctx, self, mode);
                let output_str = path_to_string(&output)?;
                match strategy {
                    ExtractStrategy::Simple => Ok(vec![
                        "extract".into(),
                        ctx.pbf_str()?.into(),
                        "--simple".into(),
                        format!("-b={bbox}"),
                        "-o".into(),
                        output_str,
                    ]),
                    ExtractStrategy::Complete => Ok(vec![
                        "extract".into(),
                        ctx.pbf_str()?.into(),
                        format!("-b={bbox}"),
                        "-o".into(),
                        output_str,
                    ]),
                    ExtractStrategy::Smart => Ok(vec![
                        "extract".into(),
                        ctx.pbf_str()?.into(),
                        "--smart".into(),
                        format!("-b={bbox}"),
                        "-o".into(),
                        output_str,
                    ]),
                }
            }
        }
    }

}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Convert a `PathBuf` to a `String`, returning a `DevError` if not UTF-8.
fn path_to_string(path: &Path) -> Result<String, DevError> {
    path.to_str()
        .map(String::from)
        .ok_or_else(|| DevError::Config(format!("path is not valid UTF-8: {}", path.display())))
}

/// Compute the scratch output path for a command based on its output kind.
///
/// Bench and hotpath runs land in distinct filenames so back-to-back runs
/// in the same scratch dir don't clobber each other between invocations.
pub(crate) fn scratch_output_path(
    ctx: &CommandContext,
    cmd: &PbfhoggCommand,
    mode: ArgMode,
) -> PathBuf {
    let name = cmd.id();
    let prefix = mode.scratch_prefix();
    match cmd.output_kind() {
        OutputKind::ScratchPbf => ctx
            .scratch_dir
            .join(format!("{prefix}-{name}-output.osm.pbf")),
        OutputKind::ScratchOsc => ctx
            .scratch_dir
            .join(format!("{prefix}-{name}-output.osc.gz")),
        OutputKind::ScratchDir(dir_name) => {
            ctx.scratch_dir.join(format!("{dir_name}-{}", ctx.dataset))
        }
        OutputKind::None => PathBuf::new(), // Should not be used.
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;
    use crate::measure::CommandParams;

    fn test_ctx() -> CommandContext {
        CommandContext {
            binary: PathBuf::from("/usr/bin/pbfhogg"),
            pbf_path: PathBuf::from("/data/denmark.osm.pbf"),
            osc_path: Some(PathBuf::from("/data/denmark-4705.osc.gz")),
            osc_paths: vec![PathBuf::from("/data/denmark-4705.osc.gz")],
            pbf_b_path: Some(PathBuf::from("/data/scratch/merged.osm.pbf")),
            scratch_dir: PathBuf::from("/data/scratch"),
            dataset: "denmark".into(),
            bbox: Some("12.4,55.6,12.7,55.8".into()),
            params: CommandParams::default(),
        }
    }

    #[test]
    fn inspect_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Inspect {
            nodes: false,
            tags: false,
            type_filter: None,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap();
        assert_eq!(args, vec!["inspect", "/data/denmark.osm.pbf"]);
    }

    #[test]
    fn inspect_tags_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Inspect {
            nodes: false,
            tags: true,
            type_filter: None,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap();
        assert_eq!(
            args,
            vec![
                "inspect",
                "tags",
                "/data/denmark.osm.pbf",
                "--min-count",
                "999999999",
            ]
        );
    }

    #[test]
    fn apply_changes_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::ApplyChanges;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap();
        assert_eq!(args[0], "apply-changes");
        assert_eq!(args[1], "/data/denmark.osm.pbf");
        assert_eq!(args[2], "/data/denmark-4705.osc.gz");
        assert_eq!(args[3], "-o");
    }

    #[test]
    fn diff_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Diff {
            format: DiffFormat::Default,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap();
        assert_eq!(
            args,
            vec![
                "diff",
                "/data/denmark.osm.pbf",
                "/data/scratch/merged.osm.pbf",
                "-c",
            ]
        );
    }

    #[test]
    fn add_locations_to_ways_with_index_type() {
        let mut ctx = test_ctx();
        ctx.params.index_type = Some("external".into());
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap();
        assert!(args.contains(&String::from("--index-type")));
        assert!(args.contains(&String::from("external")));
    }

    #[test]
    fn altw_hotpath_no_index_type_default() {
        // Hotpath should NOT default to --index-type external when omitted.
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Hotpath).unwrap();
        assert!(!args.contains(&String::from("--index-type")));
    }

    #[test]
    fn altw_metadata_has_no_axis_mirrors() {
        // After v13 the index_type axis lives in cli_args and brokkr_args
        // (grep-able from there); the metadata builder is reserved for
        // runtime observations, so it no longer mirrors user-supplied flags.
        let mut ctx = test_ctx();
        ctx.params.index_type = Some("external".into());
        let cmd = PbfhoggCommand::AddLocationsToWays;
        assert!(cmd.metadata(&ctx).is_empty());
    }

    #[test]
    fn build_geocode_index_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::BuildGeocodeIndex;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap();
        assert_eq!(args[0], "build-geocode-index");
        assert_eq!(args[1], "/data/denmark.osm.pbf");
        assert_eq!(args[2], "--output-dir");
        assert!(args[3].contains("geocode-denmark"));
        assert_eq!(args[4], "--force");
    }

    #[test]
    fn supports_hotpath_includes_tool_commands() {
        assert!(
            PbfhoggCommand::Inspect {
                nodes: false,
                tags: false,
                type_filter: None
            }
            .supports_hotpath()
        );
        assert!(PbfhoggCommand::CheckRefs.supports_hotpath());
        assert!(PbfhoggCommand::BuildGeocodeIndex.supports_hotpath());
        assert!(PbfhoggCommand::AddLocationsToWays.supports_hotpath());
    }

    #[test]
    fn tags_filter_osc_requires_osc() {
        let cmd = PbfhoggCommand::TagsFilter {
            filter: "highway=primary".into(),
            omit_referenced: false,
            input_kind_osc: true,
        };
        assert!(cmd.needs_osc());
        assert_eq!(cmd.input_kind(), InputKind::PbfAndOsc);
    }

    #[test]
    fn tags_filter_pbf_does_not_require_osc() {
        let cmd = PbfhoggCommand::TagsFilter {
            filter: "w/highway=primary".into(),
            omit_referenced: true,
            input_kind_osc: false,
        };
        assert!(!cmd.needs_osc());
        assert_eq!(cmd.input_kind(), InputKind::Pbf);
    }

    #[test]
    fn merge_changes_uses_osc_only() {
        let cmd = PbfhoggCommand::MergeChanges;
        assert_eq!(cmd.input_kind(), InputKind::OscOnly);
    }
}
