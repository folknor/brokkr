//! Consolidated pbfhogg measurable command definitions.
//!
//! Every pbfhogg command that can be measured (wall-clock, hotpath, alloc, or
//! profile) is defined here exactly once.  This replaces the scattered
//! definitions across `bench_commands.rs`, `bench_build_geocode_index.rs`,
//! and `hotpath.rs`.

use std::path::PathBuf;

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
    /// No file inputs (external tool manages its own).
    None,
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

impl ExtractStrategy {
    pub fn name(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Complete => "complete",
            Self::Smart => "smart",
        }
    }

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

/// Read mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    Sequential,
    Parallel,
    Pipelined,
    BlobReader,
}

impl ReadMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Parallel => "parallel",
            Self::Pipelined => "pipelined",
            Self::BlobReader => "blobreader",
        }
    }

    pub fn all() -> &'static [ReadMode] {
        &[Self::Sequential, Self::Parallel, Self::Pipelined, Self::BlobReader]
    }

    pub fn parse(s: &str) -> Result<Self, DevError> {
        match s.to_ascii_lowercase().as_str() {
            "sequential" => Ok(Self::Sequential),
            "parallel" => Ok(Self::Parallel),
            "pipelined" => Ok(Self::Pipelined),
            "blobreader" => Ok(Self::BlobReader),
            _ => Err(DevError::Config(format!("unknown read mode: {s}"))),
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
    CatWay,
    CatRelation,
    CatDedupe,
    TagsFilterWay,
    TagsFilterAmenity,
    TagsFilterTwopass,
    TagsFilterOsc,
    Getid,
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

    // -- Standalone commands --
    BuildGeocodeIndex,

    // -- Multi-variant benchmarks --
    Extract { strategy: ExtractStrategy },
    Read { mode: ReadMode },
    Write { compression: String, writer_mode: String },
    Merge { compression: String, io_mode: String },

    // -- Special benchmarks --
    Allocator { allocator: String },
    BlobFilter { variant: BlobFilterVariant },

    // -- External baselines --
    Planetiler,
    OsmiumCat,
    OsmiumAltw,
}

/// Blob filter comparison variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobFilterVariant {
    /// Which sub-command to run (cat-way, cat-relation, inspect-tags-way, inspect-nodes).
    pub command: String,
    /// "indexed" or "raw".
    pub pbf_label: String,
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
            Self::CatWay => "cat-way",
            Self::CatRelation => "cat-relation",
            Self::CatDedupe => "cat-dedupe",
            Self::TagsFilterWay => "tags-filter-way",
            Self::TagsFilterAmenity => "tags-filter-amenity",
            Self::TagsFilterTwopass => "tags-filter-twopass",
            Self::TagsFilterOsc => "tags-filter-osc",
            Self::Getid => "getid",
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
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::Merge { .. } => "merge",
            Self::Allocator { .. } => "allocator",
            Self::BlobFilter { .. } => "blob-filter",
            Self::Planetiler => "planetiler",
            Self::OsmiumCat => "osmium-cat",
            Self::OsmiumAltw => "osmium-altw",
        }
    }

    /// What inputs this command requires.
    pub fn input_kind(&self) -> InputKind {
        match self {
            Self::TagsFilterOsc => InputKind::PbfAndOsc,
            Self::MergeChanges => InputKind::OscOnly,
            Self::ApplyChanges => InputKind::PbfAndOsc,
            Self::Diff | Self::DiffOsc => InputKind::PbfAndMerged,
            Self::ExtractSimple | Self::ExtractComplete | Self::ExtractSmart => InputKind::PbfAndBbox,
            Self::Extract { .. } => InputKind::PbfAndBbox,
            Self::Merge { .. } => InputKind::PbfAndOsc,
            Self::Planetiler => InputKind::Pbf,
            Self::OsmiumCat | Self::OsmiumAltw => InputKind::Pbf,
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

            // OSC output.
            Self::TagsFilterOsc | Self::MergeChanges | Self::DiffOsc => {
                OutputKind::ScratchOsc("bench-output")
            }

            // Directory output.
            Self::BuildGeocodeIndex => OutputKind::ScratchDir("geocode"),

            // Read-only multi-variant (no output file).
            Self::Read { .. } => OutputKind::None,
            Self::Planetiler => OutputKind::None,
            Self::OsmiumCat => OutputKind::None,

            // Everything else writes a scratch PBF.
            _ => OutputKind::ScratchPbf("bench-output"),
        }
    }

    /// Whether this command supports hotpath profiling.
    pub fn supports_hotpath(&self) -> bool {
        match self {
            // External tools cannot be hotpath-profiled.
            Self::Planetiler | Self::OsmiumCat | Self::OsmiumAltw => false,
            // Allocator builds different binaries — not hotpath-compatible.
            Self::Allocator { .. } => false,
            // Blob-filter is a comparative benchmark, not hotpath-compatible.
            Self::BlobFilter { .. } => false,
            // Multi-variant write/merge use subprocess kv parsing, not hotpath.
            Self::Write { .. } | Self::Merge { .. } => false,
            // Multi-variant read uses subprocess kv parsing, not hotpath.
            Self::Read { .. } => false,
            _ => true,
        }
    }

    /// Default dataset for this command.
    pub fn default_dataset(&self) -> &'static str {
        match self {
            // Extract benchmarks default to "japan" for larger bbox coverage.
            Self::Extract { .. }
            | Self::ExtractSimple
            | Self::ExtractComplete
            | Self::ExtractSmart => "japan",
            _ => "denmark",
        }
    }

    /// Default PBF variant for this command.
    pub fn default_variant(&self) -> &'static str {
        "indexed"
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

    /// The result command label for the DB.
    pub fn result_command(&self) -> &'static str {
        match self {
            Self::Inspect
            | Self::InspectNodes
            | Self::InspectTags
            | Self::InspectTagsWay
            | Self::CheckRefs
            | Self::CheckIds
            | Self::Sort
            | Self::CatWay
            | Self::CatRelation
            | Self::CatDedupe
            | Self::TagsFilterWay
            | Self::TagsFilterAmenity
            | Self::TagsFilterTwopass
            | Self::TagsFilterOsc
            | Self::Getid
            | Self::Getparents
            | Self::GetidInvert
            | Self::Renumber
            | Self::MergeChanges
            | Self::ApplyChanges
            | Self::AddLocationsToWays
            | Self::ExtractSimple
            | Self::ExtractComplete
            | Self::ExtractSmart
            | Self::TimeFilter
            | Self::Diff
            | Self::DiffOsc => "bench commands",
            Self::BuildGeocodeIndex => "bench build-geocode-index",
            Self::Extract { .. } => "bench extract",
            Self::Read { .. } => "bench read",
            Self::Write { .. } => "bench write",
            Self::Merge { .. } => "bench merge",
            Self::Allocator { .. } => "bench allocator",
            Self::BlobFilter { .. } => "bench blob-filter",
            Self::Planetiler => "bench planetiler",
            Self::OsmiumCat | Self::OsmiumAltw => "bench baseline",
        }
    }

    /// The result variant label for the DB.
    pub fn result_variant(&self) -> Option<String> {
        match self {
            // Tool CLI commands use the command ID as the variant.
            Self::Inspect
            | Self::InspectNodes
            | Self::InspectTags
            | Self::InspectTagsWay
            | Self::CheckRefs
            | Self::CheckIds
            | Self::Sort
            | Self::CatWay
            | Self::CatRelation
            | Self::CatDedupe
            | Self::TagsFilterWay
            | Self::TagsFilterAmenity
            | Self::TagsFilterTwopass
            | Self::TagsFilterOsc
            | Self::Getid
            | Self::Getparents
            | Self::GetidInvert
            | Self::Renumber
            | Self::MergeChanges
            | Self::ApplyChanges
            | Self::AddLocationsToWays
            | Self::ExtractSimple
            | Self::ExtractComplete
            | Self::ExtractSmart
            | Self::TimeFilter
            | Self::Diff
            | Self::DiffOsc => Some(self.id().to_owned()),

            Self::BuildGeocodeIndex => None,

            Self::Extract { strategy } => Some(strategy.name().to_owned()),
            Self::Read { mode } => Some(mode.name().to_owned()),
            Self::Write { compression, writer_mode } => {
                Some(format!("{writer_mode}-{compression}"))
            }
            Self::Merge { compression, io_mode } => {
                Some(format!("{io_mode}+{compression}"))
            }
            Self::Allocator { allocator } => Some(allocator.clone()),
            Self::BlobFilter { variant } => {
                Some(format!("{}+{}", variant.command, variant.pbf_label))
            }
            Self::Planetiler => None, // Planetiler sets its own variant from parsed output.
            Self::OsmiumCat => Some("osmium/cat-opl".to_owned()),
            Self::OsmiumAltw => Some("osmium/add-locations-to-ways".to_owned()),
        }
    }

    /// Build metadata key-value pairs for the result DB.
    pub fn metadata(&self, ctx: &CommandContext) -> Vec<crate::db::KvPair> {
        use crate::db::KvPair;

        match self {
            Self::AddLocationsToWays => {
                if let Some(it) = ctx.param("index_type") {
                    vec![KvPair::text("meta.index_type", it)]
                } else {
                    vec![]
                }
            }
            Self::Extract { strategy } => {
                let mut meta = vec![KvPair::text("meta.strategy", strategy.name())];
                if let Some(ref bbox) = ctx.bbox {
                    meta.push(KvPair::text("meta.bbox", bbox.as_str()));
                }
                meta
            }
            Self::ExtractSimple | Self::ExtractComplete | Self::ExtractSmart => {
                // The tool CLI extract commands also carry bbox in metadata
                // when used through the extract benchmark path.
                vec![]
            }
            Self::Read { mode } => {
                vec![KvPair::text("meta.mode", mode.name())]
            }
            Self::Write { compression, writer_mode } => {
                vec![
                    KvPair::text("meta.compression", compression.as_str()),
                    KvPair::text("meta.writer_mode", writer_mode.as_str()),
                ]
            }
            Self::Merge { compression, io_mode } => {
                vec![
                    KvPair::text("meta.compression", compression.as_str()),
                    KvPair::text("meta.io_mode", io_mode.as_str()),
                ]
            }
            _ => vec![],
        }
    }

    /// Build the argument vector for this command given the resolved context.
    ///
    /// The returned `Vec<String>` contains arguments to pass to the pbfhogg
    /// binary (or external tool binary).  The binary path itself is NOT
    /// included — the caller prepends it.
    pub fn build_args(&self, ctx: &CommandContext) -> Result<Vec<String>, DevError> {
        match self {
            // -----------------------------------------------------------------
            // Tool CLI commands (26 from bench_commands.rs)
            // -----------------------------------------------------------------
            Self::Inspect => {
                Ok(vec!["inspect".into(), ctx.pbf_str()?.into()])
            }
            Self::InspectNodes => {
                Ok(vec!["inspect".into(), "--nodes".into(), ctx.pbf_str()?.into()])
            }
            Self::InspectTags => {
                Ok(vec![
                    "inspect".into(), "tags".into(), ctx.pbf_str()?.into(),
                    "--min-count".into(), "999999999".into(),
                ])
            }
            Self::InspectTagsWay => {
                Ok(vec![
                    "inspect".into(), "tags".into(), ctx.pbf_str()?.into(),
                    "--type".into(), "way".into(),
                    "--min-count".into(), "999999999".into(),
                ])
            }
            Self::CheckRefs => {
                Ok(vec!["check".into(), "--refs".into(), ctx.pbf_str()?.into()])
            }
            Self::CheckIds => {
                Ok(vec!["check".into(), "--ids".into(), ctx.pbf_str()?.into()])
            }
            Self::Sort => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "sort".into(), ctx.pbf_str()?.into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::CatWay => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "cat".into(), ctx.pbf_str()?.into(),
                    "--type".into(), "way".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::CatRelation => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "cat".into(), ctx.pbf_str()?.into(),
                    "--type".into(), "relation".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::CatDedupe => {
                let output = scratch_output_path(ctx, self);
                let pbf = ctx.pbf_str()?;
                Ok(vec![
                    "cat".into(), "--dedupe".into(), pbf.into(), pbf.into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::TagsFilterWay => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "tags-filter".into(), ctx.pbf_str()?.into(),
                    "-R".into(), "w/highway=primary".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::TagsFilterAmenity => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "tags-filter".into(), ctx.pbf_str()?.into(),
                    "-R".into(), "amenity=restaurant".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::TagsFilterTwopass => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "tags-filter".into(), ctx.pbf_str()?.into(),
                    "highway=primary".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::TagsFilterOsc => {
                let output = scratch_output_path(ctx, self);
                let osc = ctx.osc_str()?;
                Ok(vec![
                    "tags-filter".into(), "--input-kind".into(), "osc".into(),
                    osc.into(), "highway=primary".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::Getid => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "getid".into(), ctx.pbf_str()?.into(),
                    "n115722".into(), "n115723".into(), "n115724".into(),
                    "w2080".into(), "w2081".into(), "w2082".into(),
                    "r174".into(), "r213".into(), "r339".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::Getparents => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "getparents".into(), ctx.pbf_str()?.into(),
                    "n115722".into(), "n115723".into(), "w2080".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::GetidInvert => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "getid".into(), "--invert".into(), ctx.pbf_str()?.into(),
                    "n115722".into(), "n115723".into(), "n115724".into(),
                    "w2080".into(), "w2081".into(), "w2082".into(),
                    "r174".into(), "r213".into(), "r339".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::Renumber => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "renumber".into(), ctx.pbf_str()?.into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::MergeChanges => {
                let output = scratch_output_path(ctx, self);
                let osc = ctx.osc_str()?;
                Ok(vec![
                    "merge-changes".into(), osc.into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::ApplyChanges => {
                let output = scratch_output_path(ctx, self);
                let osc = ctx.osc_str()?;
                Ok(vec![
                    "apply-changes".into(), ctx.pbf_str()?.into(), osc.into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::AddLocationsToWays => {
                let output = scratch_output_path(ctx, self);
                let mut args = vec![
                    "add-locations-to-ways".into(), ctx.pbf_str()?.into(),
                    "-o".into(), path_to_string(&output)?,
                ];
                if let Some(it) = ctx.param("index_type") {
                    args.push("--index-type".into());
                    args.push(it.into());
                }
                Ok(args)
            }
            Self::ExtractSimple => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "extract".into(), ctx.pbf_str()?.into(),
                    "--simple".into(),
                    "-b".into(), "12.4,55.6,12.7,55.8".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::ExtractComplete => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "extract".into(), ctx.pbf_str()?.into(),
                    "-b".into(), "12.4,55.6,12.7,55.8".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::ExtractSmart => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "extract".into(), ctx.pbf_str()?.into(),
                    "--smart".into(),
                    "-b".into(), "12.4,55.6,12.7,55.8".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::TimeFilter => {
                let output = scratch_output_path(ctx, self);
                Ok(vec![
                    "time-filter".into(), ctx.pbf_str()?.into(),
                    "2024-01-01T00:00:00Z".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::Diff => {
                let merged = ctx.merged_pbf_str()?;
                Ok(vec![
                    "diff".into(), ctx.pbf_str()?.into(), merged.into(),
                    "-c".into(),
                ])
            }
            Self::DiffOsc => {
                let output = scratch_output_path(ctx, self);
                let merged = ctx.merged_pbf_str()?;
                Ok(vec![
                    "diff".into(), "--format".into(), "osc".into(),
                    ctx.pbf_str()?.into(), merged.into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }

            // -----------------------------------------------------------------
            // Standalone commands
            // -----------------------------------------------------------------
            Self::BuildGeocodeIndex => {
                let output_dir = ctx.scratch_dir.join(format!("geocode-{}", ctx.dataset));
                let output_dir_str = output_dir
                    .to_str()
                    .ok_or_else(|| DevError::Config("geocode output dir path is not valid UTF-8".into()))?;
                Ok(vec![
                    "build-geocode-index".into(), ctx.pbf_str()?.into(),
                    "--output-dir".into(), output_dir_str.into(),
                    "--force".into(),
                ])
            }

            // -----------------------------------------------------------------
            // Multi-variant: extract (with resolved bbox)
            // -----------------------------------------------------------------
            Self::Extract { strategy } => {
                let bbox = ctx.bbox.as_deref().ok_or_else(|| {
                    DevError::Config("extract requires a bbox".into())
                })?;
                let output = ctx.scratch_output("bench-extract-output", "osm.pbf");
                let output_str = path_to_string(&output)?;
                match strategy {
                    ExtractStrategy::Simple => Ok(vec![
                        "extract".into(), ctx.pbf_str()?.into(),
                        "--simple".into(),
                        "-b".into(), bbox.into(),
                        "-o".into(), output_str,
                    ]),
                    ExtractStrategy::Complete => Ok(vec![
                        "extract".into(), ctx.pbf_str()?.into(),
                        "-b".into(), bbox.into(),
                        "-o".into(), output_str,
                    ]),
                    ExtractStrategy::Smart => Ok(vec![
                        "extract".into(), ctx.pbf_str()?.into(),
                        "--smart".into(),
                        "-b".into(), bbox.into(),
                        "-o".into(), output_str,
                    ]),
                }
            }

            // -----------------------------------------------------------------
            // Multi-variant: read
            // -----------------------------------------------------------------
            Self::Read { mode } => {
                Ok(vec![
                    "bench-read".into(), ctx.pbf_str()?.into(),
                    "--mode".into(), mode.name().into(),
                ])
            }

            // -----------------------------------------------------------------
            // Multi-variant: write
            // -----------------------------------------------------------------
            Self::Write { compression, writer_mode } => {
                Ok(vec![
                    "bench-write".into(), ctx.pbf_str()?.into(),
                    "--compression".into(), compression.clone(),
                    "--writer".into(), writer_mode.clone(),
                ])
            }

            // -----------------------------------------------------------------
            // Multi-variant: merge
            // -----------------------------------------------------------------
            Self::Merge { compression, io_mode } => {
                let output = ctx.scratch_output("bench-merge-output", "osm.pbf");
                let output_str = path_to_string(&output)?;
                Ok(vec![
                    "bench-merge".into(), ctx.pbf_str()?.into(),
                    ctx.osc_str()?.into(),
                    "-o".into(), output_str,
                    "--compression".into(), compression.clone(),
                    "--io-mode".into(), io_mode.clone(),
                ])
            }

            // -----------------------------------------------------------------
            // Special: allocator (builds different binaries)
            // -----------------------------------------------------------------
            Self::Allocator { .. } => {
                Ok(vec!["check".into(), "--refs".into(), ctx.pbf_str()?.into()])
            }

            // -----------------------------------------------------------------
            // Special: blob-filter
            // -----------------------------------------------------------------
            Self::BlobFilter { variant } => {
                let output = ctx.scratch_output("bench-blob-filter-output", "osm.pbf");
                let output_str = path_to_string(&output)?;
                let pbf = ctx.pbf_str()?;
                let force = variant.pbf_label == "raw";
                let mut args = match variant.command.as_str() {
                    "cat-way" => vec![
                        "cat".into(), pbf.into(),
                        "--type".into(), "way".into(),
                        "-o".into(), output_str,
                    ],
                    "cat-relation" => vec![
                        "cat".into(), pbf.into(),
                        "--type".into(), "relation".into(),
                        "-o".into(), output_str,
                    ],
                    "inspect-tags-way" => vec![
                        "inspect".into(), "tags".into(), pbf.into(),
                        "--type".into(), "way".into(),
                        "--min-count".into(), "999999999".into(),
                    ],
                    "inspect-nodes" => vec![
                        "inspect".into(), "--nodes".into(), pbf.into(),
                    ],
                    _ => return Err(DevError::Config(format!(
                        "unknown blob-filter command: {}", variant.command
                    ))),
                };
                if force {
                    args.push("--force".into());
                }
                Ok(args)
            }

            // -----------------------------------------------------------------
            // External baselines
            // -----------------------------------------------------------------
            Self::OsmiumCat => {
                Ok(vec![
                    "cat".into(), ctx.pbf_str()?.into(),
                    "-o".into(), "/dev/null".into(),
                    "-f".into(), "opl".into(),
                    "--overwrite".into(),
                ])
            }
            Self::OsmiumAltw => {
                let output = ctx.scratch_output("bench-osmium-altw-output", "osm.pbf");
                let output_str = path_to_string(&output)?;
                Ok(vec![
                    "add-locations-to-ways".into(), ctx.pbf_str()?.into(),
                    "-o".into(), output_str,
                    "--overwrite".into(),
                ])
            }
            Self::Planetiler => {
                // Planetiler uses a completely different execution path (Java).
                // Args are built by the planetiler runner, not here.
                Err(DevError::Config(
                    "planetiler args are built by the planetiler runner".into(),
                ))
            }
        }
    }

    /// Build the argument vector for hotpath profiling.
    ///
    /// This produces the full command line INCLUDING the binary path as the
    /// first element, matching the format expected by `run_hotpath_capture`.
    ///
    /// Only commands where `supports_hotpath()` returns true should call this.
    pub fn build_hotpath_args(&self, ctx: &CommandContext) -> Result<Vec<String>, DevError> {
        let binary = ctx.binary_str()?;
        let mut args = vec![binary.to_owned()];

        match self {
            // Hotpath versions of commands may differ slightly from bench
            // versions (e.g. the hotpath "cat" test uses different flags).
            Self::InspectTags => {
                args.extend([
                    "inspect".into(), "tags".into(), ctx.pbf_str()?.into(),
                ]);
            }
            Self::CheckRefs => {
                args.extend([
                    "check".into(), "--refs".into(), ctx.pbf_str()?.into(),
                ]);
            }
            Self::ApplyChanges => {
                // Hotpath apply-changes needs compression param from context.
                let osc = ctx.osc_str()?;
                let compression = ctx.param("compression").unwrap_or("zlib");
                let output = ctx.scratch_output("hotpath-merged", "osm.pbf");
                args.extend([
                    "apply-changes".into(), ctx.pbf_str()?.into(), osc.into(),
                    "--compression".into(), compression.into(),
                    "-o".into(), path_to_string(&output)?,
                ]);
            }
            Self::AddLocationsToWays => {
                let index_type = ctx.param("index_type").unwrap_or("external");
                let output = ctx.scratch_output("hotpath-altw-external", "osm.pbf");
                args.extend([
                    "add-locations-to-ways".into(),
                    "--index-type".into(), index_type.into(),
                    ctx.pbf_str()?.into(),
                    "-o".into(), path_to_string(&output)?,
                ]);
            }
            Self::BuildGeocodeIndex => {
                let output_dir = ctx.scratch_dir.join(format!("geocode-{}", ctx.dataset));
                let output_dir_str = output_dir
                    .to_str()
                    .ok_or_else(|| DevError::Config("geocode output dir path is not valid UTF-8".into()))?;
                args.extend([
                    "build-geocode-index".into(), ctx.pbf_str()?.into(),
                    "--output-dir".into(), output_dir_str.into(),
                    "--force".into(),
                ]);
            }
            Self::ExtractSimple | Self::Extract { strategy: ExtractStrategy::Simple } => {
                let bbox = ctx.bbox.as_deref().ok_or_else(|| {
                    DevError::Config("extract requires a bbox".into())
                })?;
                let output = ctx.scratch_output("hotpath-extract-simple", "osm.pbf");
                args.extend([
                    "extract".into(), ctx.pbf_str()?.into(),
                    "--simple".into(),
                    "-b".into(), bbox.into(),
                    "-o".into(), path_to_string(&output)?,
                ]);
            }
            Self::ExtractComplete | Self::Extract { strategy: ExtractStrategy::Complete } => {
                let bbox = ctx.bbox.as_deref().ok_or_else(|| {
                    DevError::Config("extract requires a bbox".into())
                })?;
                let output = ctx.scratch_output("hotpath-extract-complete", "osm.pbf");
                args.extend([
                    "extract".into(), ctx.pbf_str()?.into(),
                    "-b".into(), bbox.into(),
                    "-o".into(), path_to_string(&output)?,
                ]);
            }
            Self::ExtractSmart | Self::Extract { strategy: ExtractStrategy::Smart } => {
                let bbox = ctx.bbox.as_deref().ok_or_else(|| {
                    DevError::Config("extract requires a bbox".into())
                })?;
                let output = ctx.scratch_output("hotpath-extract-smart", "osm.pbf");
                args.extend([
                    "extract".into(), ctx.pbf_str()?.into(),
                    "--smart".into(),
                    "-b".into(), bbox.into(),
                    "-o".into(), path_to_string(&output)?,
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

    /// Hotpath test label.  Used for `--test` filtering and `TEST_LABELS`.
    ///
    /// Returns `None` for commands that are not part of the standard hotpath
    /// test suite.
    pub fn hotpath_label(&self) -> Option<&'static str> {
        match self {
            Self::InspectTags => Some("inspect-tags"),
            Self::CheckRefs => Some("check-refs"),
            Self::BuildGeocodeIndex => Some("build-geocode-index"),
            Self::AddLocationsToWays => Some("altw-external"),
            Self::ExtractSimple => Some("extract-simple"),
            Self::ExtractComplete => Some("extract-complete"),
            Self::ExtractSmart => Some("extract-smart"),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Hotpath-specific commands not directly in the tool CLI
// ---------------------------------------------------------------------------

/// Special hotpath test entries that don't map 1:1 to a tool CLI benchmark
/// command.  These are composite tests defined in the hotpath suite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotpathOnlyCommand {
    /// `cat` with specific flags: `--type node,way,relation --compression zlib`
    Cat,
    /// `apply-changes` with zlib compression
    ApplyChangesZlib,
    /// `apply-changes` with no compression
    ApplyChangesNone,
}

impl HotpathOnlyCommand {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Cat => "cat",
            Self::ApplyChangesZlib => "apply-changes-zlib",
            Self::ApplyChangesNone => "apply-changes-none",
        }
    }

    /// Build the full hotpath argument vector (including binary path).
    pub fn build_hotpath_args(&self, ctx: &CommandContext) -> Result<Vec<String>, DevError> {
        let binary = ctx.binary_str()?;
        match self {
            Self::Cat => {
                let output = ctx.scratch_output("hotpath-cat-output", "osm.pbf");
                Ok(vec![
                    binary.into(),
                    "cat".into(), ctx.pbf_str()?.into(),
                    "--type".into(), "node,way,relation".into(),
                    "--compression".into(), "zlib".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::ApplyChangesZlib => {
                let osc = ctx.osc_str()?;
                let output = ctx.scratch_output("hotpath-merged", "osm.pbf");
                Ok(vec![
                    binary.into(),
                    "apply-changes".into(), ctx.pbf_str()?.into(), osc.into(),
                    "--compression".into(), "zlib".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
            Self::ApplyChangesNone => {
                let osc = ctx.osc_str()?;
                let output = ctx.scratch_output("hotpath-merged", "osm.pbf");
                Ok(vec![
                    binary.into(),
                    "apply-changes".into(), ctx.pbf_str()?.into(), osc.into(),
                    "--compression".into(), "none".into(),
                    "-o".into(), path_to_string(&output)?,
                ])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Standard hotpath test suite
// ---------------------------------------------------------------------------

/// All hotpath test labels, in the order they run.
pub const HOTPATH_TEST_LABELS: &[&str] = &[
    "inspect-tags",
    "check-refs",
    "cat",
    "apply-changes-zlib",
    "apply-changes-none",
    "altw-external",
    "build-geocode-index",
    "extract-simple",
    "extract-complete",
    "extract-smart",
];

/// An entry in the hotpath test suite — either a `PbfhoggCommand` or a
/// `HotpathOnlyCommand`.
pub enum HotpathTestEntry {
    Command(PbfhoggCommand),
    HotpathOnly(HotpathOnlyCommand),
}

impl HotpathTestEntry {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Command(cmd) => cmd.hotpath_label().expect("command must have a hotpath label"),
            Self::HotpathOnly(cmd) => cmd.label(),
        }
    }

    /// Build the full hotpath argument vector (including binary path).
    pub fn build_hotpath_args(&self, ctx: &CommandContext) -> Result<Vec<String>, DevError> {
        match self {
            Self::Command(cmd) => cmd.build_hotpath_args(ctx),
            Self::HotpathOnly(cmd) => cmd.build_hotpath_args(ctx),
        }
    }
}

/// Build the standard hotpath test suite.
///
/// Returns all test entries.  When `bbox` is `None`, extract tests are
/// omitted (matching the existing behavior in `hotpath.rs`).
pub fn hotpath_test_suite(has_bbox: bool) -> Vec<HotpathTestEntry> {
    let mut tests = vec![
        HotpathTestEntry::Command(PbfhoggCommand::InspectTags),
        HotpathTestEntry::Command(PbfhoggCommand::CheckRefs),
        HotpathTestEntry::HotpathOnly(HotpathOnlyCommand::Cat),
        HotpathTestEntry::HotpathOnly(HotpathOnlyCommand::ApplyChangesZlib),
        HotpathTestEntry::HotpathOnly(HotpathOnlyCommand::ApplyChangesNone),
        HotpathTestEntry::Command(PbfhoggCommand::AddLocationsToWays),
        HotpathTestEntry::Command(PbfhoggCommand::BuildGeocodeIndex),
    ];

    if has_bbox {
        tests.push(HotpathTestEntry::Command(PbfhoggCommand::ExtractSimple));
        tests.push(HotpathTestEntry::Command(PbfhoggCommand::ExtractComplete));
        tests.push(HotpathTestEntry::Command(PbfhoggCommand::ExtractSmart));
    }

    tests
}

// ---------------------------------------------------------------------------
// Command collections
// ---------------------------------------------------------------------------

/// All 27 tool CLI commands (the 26 from bench_commands.rs + build-geocode-index).
pub const ALL_TOOL_CLI_COMMANDS: &[&str] = &[
    "inspect",
    "inspect-nodes",
    "inspect-tags",
    "inspect-tags-way",
    "check-refs",
    "check-ids",
    "sort",
    "cat-way",
    "cat-relation",
    "cat-dedupe",
    "tags-filter-way",
    "tags-filter-amenity",
    "tags-filter-twopass",
    "tags-filter-osc",
    "getid",
    "getparents",
    "getid-invert",
    "renumber",
    "merge-changes",
    "apply-changes",
    "add-locations-to-ways",
    "extract-simple",
    "extract-complete",
    "extract-smart",
    "time-filter",
    "diff",
    "diff-osc",
    "build-geocode-index",
];

/// Parse a tool CLI command name into a `PbfhoggCommand`.
pub fn parse_tool_command(name: &str) -> Result<PbfhoggCommand, DevError> {
    match name {
        "inspect" => Ok(PbfhoggCommand::Inspect),
        "inspect-nodes" => Ok(PbfhoggCommand::InspectNodes),
        "inspect-tags" => Ok(PbfhoggCommand::InspectTags),
        "inspect-tags-way" => Ok(PbfhoggCommand::InspectTagsWay),
        "check-refs" => Ok(PbfhoggCommand::CheckRefs),
        "check-ids" => Ok(PbfhoggCommand::CheckIds),
        "sort" => Ok(PbfhoggCommand::Sort),
        "cat-way" => Ok(PbfhoggCommand::CatWay),
        "cat-relation" => Ok(PbfhoggCommand::CatRelation),
        "cat-dedupe" => Ok(PbfhoggCommand::CatDedupe),
        "tags-filter-way" => Ok(PbfhoggCommand::TagsFilterWay),
        "tags-filter-amenity" => Ok(PbfhoggCommand::TagsFilterAmenity),
        "tags-filter-twopass" => Ok(PbfhoggCommand::TagsFilterTwopass),
        "tags-filter-osc" => Ok(PbfhoggCommand::TagsFilterOsc),
        "getid" => Ok(PbfhoggCommand::Getid),
        "getparents" => Ok(PbfhoggCommand::Getparents),
        "getid-invert" => Ok(PbfhoggCommand::GetidInvert),
        "renumber" => Ok(PbfhoggCommand::Renumber),
        "merge-changes" => Ok(PbfhoggCommand::MergeChanges),
        "apply-changes" => Ok(PbfhoggCommand::ApplyChanges),
        "add-locations-to-ways" => Ok(PbfhoggCommand::AddLocationsToWays),
        "extract-simple" => Ok(PbfhoggCommand::ExtractSimple),
        "extract-complete" => Ok(PbfhoggCommand::ExtractComplete),
        "extract-smart" => Ok(PbfhoggCommand::ExtractSmart),
        "time-filter" => Ok(PbfhoggCommand::TimeFilter),
        "diff" => Ok(PbfhoggCommand::Diff),
        "diff-osc" => Ok(PbfhoggCommand::DiffOsc),
        "build-geocode-index" => Ok(PbfhoggCommand::BuildGeocodeIndex),
        _ => Err(DevError::Config(format!(
            "unknown command: {name}\nvalid commands: {}",
            ALL_TOOL_CLI_COMMANDS.join(", ")
        ))),
    }
}

/// Parse a command input string with prefix expansion support.
///
/// Accepts `"all"` to return every tool CLI command, an exact command name,
/// or a prefix that expands to all matching commands.
pub fn parse_command_input(input: &str) -> Result<Vec<PbfhoggCommand>, DevError> {
    if input == "all" {
        return Ok(all_tool_cli_commands());
    }

    // Exact match first.
    if ALL_TOOL_CLI_COMMANDS.contains(&input) {
        return Ok(vec![parse_tool_command(input)?]);
    }

    // Prefix match.
    let prefix_matches: Vec<&str> = ALL_TOOL_CLI_COMMANDS
        .iter()
        .copied()
        .filter(|cmd| cmd.starts_with(input))
        .collect();

    if !prefix_matches.is_empty() {
        let mut commands = Vec::with_capacity(prefix_matches.len());
        for name in prefix_matches {
            commands.push(parse_tool_command(name)?);
        }
        return Ok(commands);
    }

    Err(DevError::Config(format!(
        "unknown command: {input}\nvalid commands: all, {}",
        ALL_TOOL_CLI_COMMANDS.join(", ")
    )))
}

/// Return all tool CLI commands as `PbfhoggCommand` values.
fn all_tool_cli_commands() -> Vec<PbfhoggCommand> {
    vec![
        PbfhoggCommand::Inspect,
        PbfhoggCommand::InspectNodes,
        PbfhoggCommand::InspectTags,
        PbfhoggCommand::InspectTagsWay,
        PbfhoggCommand::CheckRefs,
        PbfhoggCommand::CheckIds,
        PbfhoggCommand::Sort,
        PbfhoggCommand::CatWay,
        PbfhoggCommand::CatRelation,
        PbfhoggCommand::CatDedupe,
        PbfhoggCommand::TagsFilterWay,
        PbfhoggCommand::TagsFilterAmenity,
        PbfhoggCommand::TagsFilterTwopass,
        PbfhoggCommand::TagsFilterOsc,
        PbfhoggCommand::Getid,
        PbfhoggCommand::Getparents,
        PbfhoggCommand::GetidInvert,
        PbfhoggCommand::Renumber,
        PbfhoggCommand::MergeChanges,
        PbfhoggCommand::ApplyChanges,
        PbfhoggCommand::AddLocationsToWays,
        PbfhoggCommand::ExtractSimple,
        PbfhoggCommand::ExtractComplete,
        PbfhoggCommand::ExtractSmart,
        PbfhoggCommand::TimeFilter,
        PbfhoggCommand::Diff,
        PbfhoggCommand::DiffOsc,
        PbfhoggCommand::BuildGeocodeIndex,
    ]
}

/// Return the full suite of all pbfhogg commands (for `bench all` / suite
/// support).
///
/// Includes tool CLI commands, multi-variant benchmarks (with standard
/// defaults), special benchmarks, and external baselines.
pub fn all_commands() -> Vec<PbfhoggCommand> {
    let mut commands = all_tool_cli_commands();

    // Multi-variant: extract (all strategies)
    for strategy in ExtractStrategy::all() {
        commands.push(PbfhoggCommand::Extract { strategy: *strategy });
    }

    // Multi-variant: read (all modes)
    for mode in ReadMode::all() {
        commands.push(PbfhoggCommand::Read { mode: *mode });
    }

    // Multi-variant: write (default compressions × writer modes)
    for compression in &["zlib:6", "zstd:3", "none"] {
        for writer_mode in &["sync", "pipelined"] {
            commands.push(PbfhoggCommand::Write {
                compression: (*compression).to_owned(),
                writer_mode: (*writer_mode).to_owned(),
            });
        }
    }

    // Multi-variant: merge (default compressions × io modes)
    for compression in &["zlib:6", "zstd:3", "none"] {
        commands.push(PbfhoggCommand::Merge {
            compression: (*compression).to_owned(),
            io_mode: "buffered".to_owned(),
        });
    }

    // Special benchmarks
    for allocator in &["default", "jemalloc", "mimalloc"] {
        commands.push(PbfhoggCommand::Allocator {
            allocator: (*allocator).to_owned(),
        });
    }

    for blob_cmd in &["cat-way", "cat-relation", "inspect-tags-way", "inspect-nodes"] {
        for pbf_label in &["indexed", "raw"] {
            commands.push(PbfhoggCommand::BlobFilter {
                variant: BlobFilterVariant {
                    command: (*blob_cmd).to_owned(),
                    pbf_label: (*pbf_label).to_owned(),
                },
            });
        }
    }

    // External baselines
    commands.push(PbfhoggCommand::Planetiler);
    commands.push(PbfhoggCommand::OsmiumCat);
    commands.push(PbfhoggCommand::OsmiumAltw);

    commands
}

/// Allocator names for the allocator comparison benchmark.
pub const ALL_ALLOCATORS: &[&str] = &["default", "jemalloc", "mimalloc"];

/// Blob-filter sub-commands.
pub const BLOB_FILTER_COMMANDS: &[&str] = &["cat-way", "cat-relation", "inspect-tags-way", "inspect-nodes"];

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Convert a `PathBuf` to a `String`, returning a `DevError` if not UTF-8.
fn path_to_string(path: &PathBuf) -> Result<String, DevError> {
    path.to_str()
        .map(String::from)
        .ok_or_else(|| DevError::Config(format!(
            "path is not valid UTF-8: {}", path.display()
        )))
}

/// Compute the scratch output path for a command based on its output kind.
fn scratch_output_path(ctx: &CommandContext, cmd: &PbfhoggCommand) -> PathBuf {
    let name = cmd.id();
    match cmd.output_kind() {
        OutputKind::ScratchPbf(_) => ctx.scratch_dir.join(format!("bench-{name}-output.osm.pbf")),
        OutputKind::ScratchOsc(_) => ctx.scratch_dir.join(format!("bench-{name}-output.osc.gz")),
        OutputKind::ScratchDir(dir_name) => ctx.scratch_dir.join(format!("{dir_name}-{}", ctx.dataset)),
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
            merged_pbf_path: Some(PathBuf::from("/data/scratch/merged.osm.pbf")),
            scratch_dir: PathBuf::from("/data/scratch"),
            dataset: "denmark".into(),
            bbox: Some("12.4,55.6,12.7,55.8".into()),
            params: HashMap::new(),
        }
    }

    #[test]
    fn all_commands_returns_every_tool_cli_command() {
        let result = parse_command_input("all").unwrap();
        assert_eq!(result.len(), ALL_TOOL_CLI_COMMANDS.len());
    }

    #[test]
    fn exact_match_inspect() {
        let result = parse_command_input("inspect").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id(), "inspect");
    }

    #[test]
    fn exact_match_diff() {
        let result = parse_command_input("diff").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id(), "diff");
    }

    #[test]
    fn prefix_tags_filter_expands() {
        let result = parse_command_input("tags-filter").unwrap();
        let ids: Vec<&str> = result.iter().map(|c| c.id()).collect();
        assert_eq!(ids, vec![
            "tags-filter-way", "tags-filter-amenity",
            "tags-filter-twopass", "tags-filter-osc"
        ]);
    }

    #[test]
    fn prefix_extract_expands() {
        let result = parse_command_input("extract").unwrap();
        let ids: Vec<&str> = result.iter().map(|c| c.id()).collect();
        assert_eq!(ids, vec!["extract-simple", "extract-complete", "extract-smart"]);
    }

    #[test]
    fn prefix_check_expands() {
        let result = parse_command_input("check").unwrap();
        let ids: Vec<&str> = result.iter().map(|c| c.id()).collect();
        assert_eq!(ids, vec!["check-refs", "check-ids"]);
    }

    #[test]
    fn exact_getid_wins_over_prefix() {
        let result = parse_command_input("getid").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id(), "getid");
    }

    #[test]
    fn unknown_command_errors() {
        let err = parse_command_input("nonexistent").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown command"), "got: {msg}");
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
        assert_eq!(args, vec![
            "inspect", "tags", "/data/denmark.osm.pbf",
            "--min-count", "999999999",
        ]);
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
        assert_eq!(args, vec![
            "diff", "/data/denmark.osm.pbf", "/data/scratch/merged.osm.pbf", "-c",
        ]);
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
    fn extract_default_dataset_is_japan() {
        let cmd = PbfhoggCommand::Extract { strategy: ExtractStrategy::Simple };
        assert_eq!(cmd.default_dataset(), "japan");

        let cmd2 = PbfhoggCommand::ExtractSimple;
        assert_eq!(cmd2.default_dataset(), "japan");
    }

    #[test]
    fn supports_hotpath_excludes_external() {
        assert!(!PbfhoggCommand::Planetiler.supports_hotpath());
        assert!(!PbfhoggCommand::OsmiumCat.supports_hotpath());
        assert!(!PbfhoggCommand::OsmiumAltw.supports_hotpath());
    }

    #[test]
    fn supports_hotpath_includes_tool_commands() {
        assert!(PbfhoggCommand::Inspect.supports_hotpath());
        assert!(PbfhoggCommand::CheckRefs.supports_hotpath());
        assert!(PbfhoggCommand::BuildGeocodeIndex.supports_hotpath());
        assert!(PbfhoggCommand::AddLocationsToWays.supports_hotpath());
    }

    #[test]
    fn hotpath_test_suite_without_bbox_excludes_extract() {
        let suite = hotpath_test_suite(false);
        let labels: Vec<&str> = suite.iter().map(|e| e.label()).collect();
        assert!(!labels.contains(&"extract-simple"));
        assert_eq!(labels.len(), 7);
    }

    #[test]
    fn hotpath_test_suite_with_bbox_includes_extract() {
        let suite = hotpath_test_suite(true);
        let labels: Vec<&str> = suite.iter().map(|e| e.label()).collect();
        assert!(labels.contains(&"extract-simple"));
        assert!(labels.contains(&"extract-complete"));
        assert!(labels.contains(&"extract-smart"));
        assert_eq!(labels.len(), 10);
    }

    #[test]
    fn hotpath_test_labels_match_suite() {
        let suite = hotpath_test_suite(true);
        let labels: Vec<&str> = suite.iter().map(|e| e.label()).collect();
        assert_eq!(labels, HOTPATH_TEST_LABELS);
    }

    #[test]
    fn all_tool_commands_individually_parseable() {
        for &cmd_name in ALL_TOOL_CLI_COMMANDS {
            let result = parse_command_input(cmd_name).unwrap();
            assert_eq!(result.len(), 1, "failed for command: {cmd_name}");
            assert_eq!(result[0].id(), cmd_name, "id mismatch for: {cmd_name}");
        }
    }

    #[test]
    fn result_variant_matches_id_for_tool_commands() {
        for cmd in all_tool_cli_commands() {
            let variant = cmd.result_variant();
            assert_eq!(variant.as_deref(), Some(cmd.id()));
        }
    }

    #[test]
    fn write_variant_format() {
        let cmd = PbfhoggCommand::Write {
            compression: "zlib:6".into(),
            writer_mode: "pipelined".into(),
        };
        assert_eq!(cmd.result_variant(), Some("pipelined-zlib:6".to_owned()));
    }

    #[test]
    fn merge_variant_format() {
        let cmd = PbfhoggCommand::Merge {
            compression: "zstd:3".into(),
            io_mode: "buffered".into(),
        };
        assert_eq!(cmd.result_variant(), Some("buffered+zstd:3".to_owned()));
    }

    #[test]
    fn blob_filter_variant_format() {
        let cmd = PbfhoggCommand::BlobFilter {
            variant: BlobFilterVariant {
                command: "cat-way".into(),
                pbf_label: "indexed".into(),
            },
        };
        assert_eq!(cmd.result_variant(), Some("cat-way+indexed".to_owned()));
    }

    #[test]
    fn osmium_cat_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::OsmiumCat;
        let args = cmd.build_args(&ctx).unwrap();
        assert_eq!(args, vec![
            "cat", "/data/denmark.osm.pbf",
            "-o", "/dev/null",
            "-f", "opl",
            "--overwrite",
        ]);
    }

    #[test]
    fn hotpath_cat_builds_correct_args() {
        let ctx = test_ctx();
        let entry = HotpathOnlyCommand::Cat;
        let args = entry.build_hotpath_args(&ctx).unwrap();
        assert_eq!(args[0], "/usr/bin/pbfhogg");
        assert_eq!(args[1], "cat");
        assert!(args.contains(&String::from("--compression")));
        assert!(args.contains(&String::from("zlib")));
    }

    #[test]
    fn hotpath_apply_changes_zlib_builds_correct_args() {
        let ctx = test_ctx();
        let entry = HotpathOnlyCommand::ApplyChangesZlib;
        let args = entry.build_hotpath_args(&ctx).unwrap();
        assert_eq!(args[0], "/usr/bin/pbfhogg");
        assert_eq!(args[1], "apply-changes");
        assert!(args.contains(&String::from("zlib")));
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
