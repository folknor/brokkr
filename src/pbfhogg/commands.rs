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
    ScratchPbf(&'static str),
    /// Writes a scratch `.osc.gz` file.
    ScratchOsc(&'static str),
    /// Writes to a scratch directory.
    ScratchDir(&'static str),
    /// No output file (read-only / stdout-only commands).
    None,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFormat {
    /// Default summary diff (`pbfhogg diff <a> <b> -c`).
    Default,
    /// OSC-format diff (`pbfhogg diff --format osc <a> <b> -o <out>`).
    Osc,
}

impl DiffFormat {
    pub fn parse(s: &str) -> Result<Self, DevError> {
        match s {
            "default" => Ok(Self::Default),
            "osc" => Ok(Self::Osc),
            _ => Err(DevError::Config(format!(
                "unknown diff format '{s}' (expected: default, osc)"
            ))),
        }
    }
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
// PbfhoggCommand — the unified command enum
// ---------------------------------------------------------------------------

/// Every measurable pbfhogg command.
///
/// Tool CLI commands, standalone benchmarks, multi-variant benchmarks,
/// and external baselines are all variants of this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PbfhoggCommand {
    // -- Tool CLI commands (26 commands from bench_commands.rs) --
    Inspect,
    InspectNodes,
    InspectTags,
    InspectTagsWay,
    CheckRefs,
    CheckIds,
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
    TagsFilterWay,
    TagsFilterAmenity,
    TagsFilterTwopass,
    TagsFilterOsc,
    Getid,
    /// Like Getid but with `--add-referenced` (two-pass with ref collection).
    GetidRefs,
    Getparents,
    GetidInvert,
    Renumber,
    MergeChanges,
    ApplyChanges,
    AddLocationsToWays,
    ExtractSimple,
    ExtractComplete,
    ExtractSmart,
    TimeFilter,
    Diff,
    DiffOsc,

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
            Self::Inspect => "inspect",
            Self::InspectNodes => "inspect-nodes",
            Self::InspectTags => "inspect-tags",
            Self::InspectTagsWay => "inspect-tags-way",
            Self::CheckRefs => "check-refs",
            Self::CheckIds => "check-ids",
            Self::Sort => "sort",
            Self::Cat { .. } => "cat",
            Self::TagsFilterWay => "tags-filter-way",
            Self::TagsFilterAmenity => "tags-filter-amenity",
            Self::TagsFilterTwopass => "tags-filter-twopass",
            Self::TagsFilterOsc => "tags-filter-osc",
            Self::Getid => "getid",
            Self::GetidRefs => "getid-refs",
            Self::Getparents => "getparents",
            Self::GetidInvert => "getid-invert",
            Self::Renumber => "renumber",
            Self::MergeChanges => "merge-changes",
            Self::ApplyChanges => "apply-changes",
            Self::AddLocationsToWays => "add-locations-to-ways",
            Self::ExtractSimple => "extract-simple",
            Self::ExtractComplete => "extract-complete",
            Self::ExtractSmart => "extract-smart",
            Self::TimeFilter => "time-filter",
            Self::Diff => "diff",
            Self::DiffOsc => "diff-osc",
            Self::BuildGeocodeIndex => "build-geocode-index",
            Self::Extract { .. } => "extract",
            Self::MultiExtract { .. } => "multi-extract",
            Self::DiffSnapshots { .. } => "diff-snapshots",
        }
    }

    /// What inputs this command requires.
    pub fn input_kind(&self) -> InputKind {
        match self {
            Self::TagsFilterOsc => InputKind::PbfAndOsc,
            Self::MergeChanges => InputKind::OscOnly,
            Self::ApplyChanges => InputKind::PbfAndOsc,
            Self::Diff | Self::DiffOsc => InputKind::PbfAndMerged,
            Self::ExtractSimple | Self::ExtractComplete | Self::ExtractSmart => {
                InputKind::PbfAndBbox
            }
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
            Self::Inspect
            | Self::InspectNodes
            | Self::InspectTags
            | Self::InspectTagsWay
            | Self::CheckRefs
            | Self::CheckIds
            | Self::Diff => OutputKind::None,

            // DiffSnapshots: same as Diff/DiffOsc — None for default format
            // (stdout summary), ScratchOsc for osc format.
            Self::DiffSnapshots { format: DiffFormat::Default } => OutputKind::None,
            Self::DiffSnapshots { format: DiffFormat::Osc } => {
                OutputKind::ScratchOsc("bench-output")
            }

            // OSC output.
            Self::TagsFilterOsc | Self::MergeChanges | Self::DiffOsc => {
                OutputKind::ScratchOsc("bench-output")
            }

            // Directory output.
            Self::BuildGeocodeIndex => OutputKind::ScratchDir("geocode"),
            Self::MultiExtract { .. } => OutputKind::ScratchDir("multi-extract"),

            // Everything else writes a scratch PBF.
            _ => OutputKind::ScratchPbf("bench-output"),
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
            Self::ApplyChanges | Self::Sort | Self::DiffOsc | Self::Cat { dedupe: true, .. }
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
            Self::Diff | Self::DiffOsc | Self::DiffSnapshots { .. } => &[1],
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
            Self::Diff | Self::DiffOsc => {
                // Merged-PBF cache state is observed at dispatch time
                // (the caller sets the params based on whether the cached
                // merged file was reused). Lets `brokkr results <uuid>`
                // distinguish runs that paid the setup cost from runs that
                // reused a cached file.
                let mut meta = Vec::new();
                if let Some(state) = ctx.param("merged_cache_state") {
                    meta.push(KvPair::text("meta.merged_cache", state));
                }
                if let Some(age) = ctx.param("merged_cache_age_s") {
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
                if let Some(file) = ctx.param("to_snapshot_file") {
                    meta.push(KvPair::text("meta.to_snapshot_file", file));
                }
                if let Some(mb) = ctx.param("to_snapshot_file_mb") {
                    meta.push(KvPair::text("meta.to_snapshot_file_mb", mb));
                }
                meta
            }
            _ => vec![],
        }
    }

    /// Build the argument vector for this command given the resolved context.
    ///
    /// The returned `Vec<String>` contains arguments to pass to the pbfhogg
    /// binary (or external tool binary).  The binary path itself is NOT
    /// included — the caller prepends it.
    #[allow(clippy::too_many_lines)]
    pub fn build_args(&self, ctx: &CommandContext) -> Result<Vec<String>, DevError> {
        match self {
            // -----------------------------------------------------------------
            // Tool CLI commands (26 from bench_commands.rs)
            // -----------------------------------------------------------------
            Self::Inspect => Ok(vec!["inspect".into(), ctx.pbf_str()?.into()]),
            Self::InspectNodes => Ok(vec![
                "inspect".into(),
                "--nodes".into(),
                ctx.pbf_str()?.into(),
            ]),
            Self::InspectTags => Ok(vec![
                "inspect".into(),
                "tags".into(),
                ctx.pbf_str()?.into(),
                "--min-count".into(),
                "999999999".into(),
            ]),
            Self::InspectTagsWay => Ok(vec![
                "inspect".into(),
                "tags".into(),
                ctx.pbf_str()?.into(),
                "--type".into(),
                "way".into(),
                "--min-count".into(),
                "999999999".into(),
            ]),
            Self::CheckRefs => Ok(vec!["check".into(), "--refs".into(), ctx.pbf_str()?.into()]),
            Self::CheckIds => Ok(vec!["check".into(), "--ids".into(), ctx.pbf_str()?.into()]),
            Self::Sort => {
                let output = scratch_output_path(ctx, self);
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
                let output = scratch_output_path(ctx, self);
                let pbf = ctx.pbf_str()?;
                let mut args: Vec<String> = vec!["cat".into()];
                if *dedupe {
                    args.push("--dedupe".into());
                }
                if *clean {
                    args.push("--clean".into());
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
            Self::TagsFilterWay => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "tags-filter".into(),
                    ctx.pbf_str()?.into(),
                    "-R".into(),
                    "w/highway=primary".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::TagsFilterAmenity => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "tags-filter".into(),
                    ctx.pbf_str()?.into(),
                    "-R".into(),
                    "amenity=restaurant".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::TagsFilterTwopass => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "tags-filter".into(),
                    ctx.pbf_str()?.into(),
                    "highway=primary".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::TagsFilterOsc => {
                let output = scratch_output_path(ctx, self);
                let osc = ctx.osc_str()?;
                Ok(vec![
                    "tags-filter".into(),
                    "--input-kind".into(),
                    "osc".into(),
                    osc.into(),
                    "highway=primary".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::Getid => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "getid".into(),
                    ctx.pbf_str()?.into(),
                    "n115722".into(),
                    "n115723".into(),
                    "n115724".into(),
                    "w2080".into(),
                    "w2081".into(),
                    "w2082".into(),
                    "r174".into(),
                    "r213".into(),
                    "r339".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::GetidRefs => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "getid".into(),
                    ctx.pbf_str()?.into(),
                    "--add-referenced".into(),
                    "n115722".into(),
                    "n115723".into(),
                    "n115724".into(),
                    "w2080".into(),
                    "w2081".into(),
                    "w2082".into(),
                    "r174".into(),
                    "r213".into(),
                    "r339".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::Getparents => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "getparents".into(),
                    ctx.pbf_str()?.into(),
                    "n115722".into(),
                    "n115723".into(),
                    "w2080".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::GetidInvert => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "getid".into(),
                    "--invert".into(),
                    ctx.pbf_str()?.into(),
                    "n115722".into(),
                    "n115723".into(),
                    "n115724".into(),
                    "w2080".into(),
                    "w2081".into(),
                    "w2082".into(),
                    "r174".into(),
                    "r213".into(),
                    "r339".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::Renumber => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "renumber".into(),
                    ctx.pbf_str()?.into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::MergeChanges => {
                let output = scratch_output_path(ctx, self);
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
                let output = scratch_output_path(ctx, self);
                let osc = ctx.osc_str()?;
                Ok(vec![
                    "apply-changes".into(),
                    ctx.pbf_str()?.into(),
                    osc.into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::AddLocationsToWays => {
                let output = scratch_output_path(ctx, self);
                let mut args = vec![
                    "add-locations-to-ways".into(),
                    ctx.pbf_str()?.into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ];
                if let Some(it) = ctx.param("index_type") {
                    args.push("--index-type".into());
                    args.push(it.into());
                }
                if let Some(s) = ctx.param("start_stage") {
                    args.push("--start-stage".into());
                    args.push(s.into());
                }
                if ctx.param("keep_scratch").is_some() {
                    args.push("--keep-scratch".into());
                }
                Ok(args)
            }
            Self::ExtractSimple => {
                let bbox = ctx
                    .bbox
                    .as_deref()
                    .ok_or_else(|| DevError::Config("extract requires a bbox".into()))?;
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "extract".into(),
                    ctx.pbf_str()?.into(),
                    "--simple".into(),
                    format!("-b={bbox}"),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::ExtractComplete => {
                let bbox = ctx
                    .bbox
                    .as_deref()
                    .ok_or_else(|| DevError::Config("extract requires a bbox".into()))?;
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "extract".into(),
                    ctx.pbf_str()?.into(),
                    format!("-b={bbox}"),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::ExtractSmart => {
                let bbox = ctx
                    .bbox
                    .as_deref()
                    .ok_or_else(|| DevError::Config("extract requires a bbox".into()))?;
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "extract".into(),
                    ctx.pbf_str()?.into(),
                    "--smart".into(),
                    format!("-b={bbox}"),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::TimeFilter => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "time-filter".into(),
                    ctx.pbf_str()?.into(),
                    "2024-01-01T00:00:00Z".into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
            }
            Self::Diff => {
                let merged = ctx.pbf_b_str()?;
                Ok(vec![
                    "diff".into(),
                    ctx.pbf_str()?.into(),
                    merged.into(),
                    "-c".into(),
                ])
            }
            Self::DiffOsc => {
                let output = scratch_output_path(ctx, self);
                let merged = ctx.pbf_b_str()?;
                Ok(vec![
                    "diff".into(),
                    "--format".into(),
                    "osc".into(),
                    ctx.pbf_str()?.into(),
                    merged.into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ])
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
                        let output = scratch_output_path(ctx, self);
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
                let output = ctx.scratch_output("bench-extract-output", "osm.pbf");
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

    /// Build the argument vector for hotpath profiling.
    ///
    /// This produces the full command line INCLUDING the binary path as the
    /// first element, matching the format expected by `run_hotpath_capture`.
    ///
    /// Only commands where `supports_hotpath()` returns true should call this.
    #[allow(clippy::too_many_lines)]
    pub fn build_hotpath_args(&self, ctx: &CommandContext) -> Result<Vec<String>, DevError> {
        let binary = ctx.binary_str()?;
        let mut args = vec![binary.to_owned()];

        match self {
            // Hotpath versions of commands may differ slightly from bench
            // versions (e.g. the hotpath "cat" test uses different flags).
            Self::InspectTags => {
                args.extend(["inspect".into(), "tags".into(), ctx.pbf_str()?.into()]);
            }
            Self::CheckRefs => {
                args.extend(["check".into(), "--refs".into(), ctx.pbf_str()?.into()]);
            }
            Self::ApplyChanges => {
                // Hotpath apply-changes needs compression param from context.
                let osc = ctx.osc_str()?;
                let compression = ctx.param("compression").unwrap_or("zlib");
                let output = ctx.scratch_output("hotpath-merged", "osm.pbf");
                args.extend([
                    "apply-changes".into(),
                    ctx.pbf_str()?.into(),
                    osc.into(),
                    "--compression".into(),
                    compression.into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ]);
            }
            Self::AddLocationsToWays => {
                let output = ctx.scratch_output("hotpath-altw", "osm.pbf");
                args.extend([
                    "add-locations-to-ways".into(),
                    ctx.pbf_str()?.into(),
                    "-o".into(),
                    path_to_string(&output)?,
                ]);
                if let Some(it) = ctx.param("index_type") {
                    args.push("--index-type".into());
                    args.push(it.into());
                }
                if let Some(s) = ctx.param("start_stage") {
                    args.push("--start-stage".into());
                    args.push(s.into());
                }
                if ctx.param("keep_scratch").is_some() {
                    args.push("--keep-scratch".into());
                }
            }
            Self::BuildGeocodeIndex => {
                let output_dir = ctx.scratch_dir.join(format!("geocode-{}", ctx.dataset));
                let output_dir_str = output_dir.to_str().ok_or_else(|| {
                    DevError::Config("geocode output dir path is not valid UTF-8".into())
                })?;
                args.extend([
                    "build-geocode-index".into(),
                    ctx.pbf_str()?.into(),
                    "--output-dir".into(),
                    output_dir_str.into(),
                    "--force".into(),
                ]);
            }
            Self::ExtractSimple
            | Self::Extract {
                strategy: ExtractStrategy::Simple,
            } => {
                let bbox = ctx
                    .bbox
                    .as_deref()
                    .ok_or_else(|| DevError::Config("extract requires a bbox".into()))?;
                let output = ctx.scratch_output("hotpath-extract-simple", "osm.pbf");
                args.extend([
                    "extract".into(),
                    ctx.pbf_str()?.into(),
                    "--simple".into(),
                    format!("-b={bbox}"),
                    "-o".into(),
                    path_to_string(&output)?,
                ]);
            }
            Self::ExtractComplete
            | Self::Extract {
                strategy: ExtractStrategy::Complete,
            } => {
                let bbox = ctx
                    .bbox
                    .as_deref()
                    .ok_or_else(|| DevError::Config("extract requires a bbox".into()))?;
                let output = ctx.scratch_output("hotpath-extract-complete", "osm.pbf");
                args.extend([
                    "extract".into(),
                    ctx.pbf_str()?.into(),
                    format!("-b={bbox}"),
                    "-o".into(),
                    path_to_string(&output)?,
                ]);
            }
            Self::ExtractSmart
            | Self::Extract {
                strategy: ExtractStrategy::Smart,
            } => {
                let bbox = ctx
                    .bbox
                    .as_deref()
                    .ok_or_else(|| DevError::Config("extract requires a bbox".into()))?;
                let output = ctx.scratch_output("hotpath-extract-smart", "osm.pbf");
                args.extend([
                    "extract".into(),
                    ctx.pbf_str()?.into(),
                    "--smart".into(),
                    format!("-b={bbox}"),
                    "-o".into(),
                    path_to_string(&output)?,
                ]);
            }
            // For all other hotpath-capable commands, the args are identical
            // to the bench version, just prefixed with the binary path.
            other => {
                let bench_args = other.build_args(ctx)?;
                args.extend(bench_args);
            }
        }

        Ok(args)
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
fn scratch_output_path(ctx: &CommandContext, cmd: &PbfhoggCommand) -> PathBuf {
    let name = cmd.id();
    match cmd.output_kind() {
        OutputKind::ScratchPbf(_) => ctx.scratch_dir.join(format!("bench-{name}-output.osm.pbf")),
        OutputKind::ScratchOsc(_) => ctx.scratch_dir.join(format!("bench-{name}-output.osc.gz")),
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
    use super::*;
    use std::collections::HashMap;

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
            params: HashMap::new(),
        }
    }

    #[test]
    fn inspect_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Inspect;
        let args = cmd.build_args(&ctx).unwrap();
        assert_eq!(args, vec!["inspect", "/data/denmark.osm.pbf"]);
    }

    #[test]
    fn inspect_tags_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::InspectTags;
        let args = cmd.build_args(&ctx).unwrap();
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
        let args = cmd.build_args(&ctx).unwrap();
        assert_eq!(args[0], "apply-changes");
        assert_eq!(args[1], "/data/denmark.osm.pbf");
        assert_eq!(args[2], "/data/denmark-4705.osc.gz");
        assert_eq!(args[3], "-o");
    }

    #[test]
    fn diff_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Diff;
        let args = cmd.build_args(&ctx).unwrap();
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
        ctx.params.insert("index_type".into(), "external".into());
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx).unwrap();
        assert!(args.contains(&String::from("--index-type")));
        assert!(args.contains(&String::from("external")));
    }

    #[test]
    fn altw_build_args_with_start_stage() {
        let mut ctx = test_ctx();
        ctx.params.insert("index_type".into(), "external".into());
        ctx.params.insert("start_stage".into(), "3".into());
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx).unwrap();
        assert!(args.contains(&String::from("--start-stage")));
        assert!(args.contains(&String::from("3")));
    }

    #[test]
    fn altw_build_args_with_keep_scratch() {
        let mut ctx = test_ctx();
        ctx.params.insert("index_type".into(), "external".into());
        ctx.params.insert("keep_scratch".into(), "true".into());
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx).unwrap();
        assert!(args.contains(&String::from("--keep-scratch")));
    }

    #[test]
    fn altw_hotpath_no_index_type_default() {
        // Hotpath should NOT default to --index-type external when omitted.
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_hotpath_args(&ctx).unwrap();
        assert!(!args.contains(&String::from("--index-type")));
    }

    #[test]
    fn altw_hotpath_with_start_stage() {
        let mut ctx = test_ctx();
        ctx.params.insert("index_type".into(), "external".into());
        ctx.params.insert("start_stage".into(), "4".into());
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_hotpath_args(&ctx).unwrap();
        assert!(args.contains(&String::from("--index-type")));
        assert!(args.contains(&String::from("external")));
        assert!(args.contains(&String::from("--start-stage")));
        assert!(args.contains(&String::from("4")));
    }

    #[test]
    fn altw_metadata_has_no_axis_mirrors() {
        // After v13 the index_type / start_stage / keep_scratch axes
        // live in cli_args and brokkr_args (grep-able from there); the
        // metadata builder is reserved for runtime observations, so it
        // no longer mirrors user-supplied flags.
        let mut ctx = test_ctx();
        ctx.params.insert("index_type".into(), "external".into());
        ctx.params.insert("start_stage".into(), "3".into());
        ctx.params.insert("keep_scratch".into(), "true".into());
        let cmd = PbfhoggCommand::AddLocationsToWays;
        assert!(cmd.metadata(&ctx).is_empty());
    }

    #[test]
    fn build_geocode_index_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::BuildGeocodeIndex;
        let args = cmd.build_args(&ctx).unwrap();
        assert_eq!(args[0], "build-geocode-index");
        assert_eq!(args[1], "/data/denmark.osm.pbf");
        assert_eq!(args[2], "--output-dir");
        assert!(args[3].contains("geocode-denmark"));
        assert_eq!(args[4], "--force");
    }

    #[test]
    fn supports_hotpath_includes_tool_commands() {
        assert!(PbfhoggCommand::Inspect.supports_hotpath());
        assert!(PbfhoggCommand::CheckRefs.supports_hotpath());
        assert!(PbfhoggCommand::BuildGeocodeIndex.supports_hotpath());
        assert!(PbfhoggCommand::AddLocationsToWays.supports_hotpath());
    }

    #[test]
    fn tags_filter_osc_requires_osc() {
        let cmd = PbfhoggCommand::TagsFilterOsc;
        assert!(cmd.needs_osc());
        assert_eq!(cmd.input_kind(), InputKind::PbfAndOsc);
    }

    #[test]
    fn merge_changes_uses_osc_only() {
        let cmd = PbfhoggCommand::MergeChanges;
        assert_eq!(cmd.input_kind(), InputKind::OscOnly);
    }
}
