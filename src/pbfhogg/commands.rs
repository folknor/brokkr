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

            // Everything else writes a scratch PBF.
            _ => OutputKind::ScratchPbf("bench-output"),
        }
    }

    /// Whether this command supports hotpath profiling.
    pub fn supports_hotpath(&self) -> bool {
        true
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

}


// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Convert a `PathBuf` to a `String`, returning a `DevError` if not UTF-8.
fn path_to_string(path: &Path) -> Result<String, DevError> {
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
