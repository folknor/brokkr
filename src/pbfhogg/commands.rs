//! Consolidated pbfhogg measurable command definitions.
//!
//! Every measurable pbfhogg command and its argv shape is defined here exactly
//! once. `bench_commands.rs` maps suite preset names to these typed commands.

use std::path::{Path, PathBuf};

use crate::clean::layout::{PbfhoggFileKind, PbfhoggLayout};
use crate::datasets::{Bbox, ElementIdList};
use crate::error::DevError;
use crate::pbfhogg::context::{CommandContext, CommandObservations, CommandParams};

// ---------------------------------------------------------------------------
// Input / output descriptors
// ---------------------------------------------------------------------------

/// Valid inspect modes. The shape prevents contradictory node/tag switches
/// and keeps `extended` out of modes where pbfhogg ignores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectMode {
    Default {
        extended: bool,
    },
    Nodes,
    Tags {
        type_filter: Option<InspectTypeFilter>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectTypeFilter {
    Node,
    Way,
    Relation,
    Invalid(String),
}

impl InspectTypeFilter {
    pub fn from_cli(value: &str) -> Self {
        match value {
            "node" => Self::Node,
            "way" => Self::Way,
            "relation" => Self::Relation,
            _ => Self::Invalid(format!(
                "unknown inspect tags --type '{value}' (expected: node, way, relation)"
            )),
        }
    }

    fn as_str(&self) -> Result<&'static str, DevError> {
        match self {
            Self::Node => Ok("node"),
            Self::Way => Ok("way"),
            Self::Relation => Ok("relation"),
            Self::Invalid(message) => Err(DevError::Config(message.clone())),
        }
    }
}

/// Valid tags-filter input policies. OSC input cannot carry PBF-only switches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagsFilterInput {
    Pbf {
        omit_referenced: bool,
        invert: bool,
        remove_tags: bool,
    },
    Osc,
    Invalid(String),
}

/// What output a command produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// Writes a scratch `.osm.pbf` file.
    ScratchPbf,
    /// Writes a scratch `.osc.gz` file.
    ScratchOsc,
    /// Writes a scratch `.txt` file: a text `diff`, whose text would
    /// overflow the measurement's stdout capture at planet scale.
    ScratchText,
    /// Writes to a scratch directory. `dataset_suffix` distinguishes output
    /// directories such as `geocode-<dataset>` from fixed paths such as
    /// multi-extract's `multi-extract`.
    ScratchDir {
        name: &'static str,
        dataset_suffix: bool,
    },
    /// No output file (read-only / stdout-only commands).
    None,
}

/// Variant-specific output routing for snapshot promotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionVariant {
    Indexed,
    Raw,
    Unsupported,
}

/// Facts that must stay aligned with a command's argv behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDescriptor {
    pub id: &'static str,
    pub output: OutputKind,
    pub promotion_variant: PromotionVariant,
    pub supports_io_uring: bool,
}

/// Which build-args flavor to produce. The command arguments are shared;
/// the mode selects only a separate scratch filename namespace.
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

/// A typed command's argv: the argv and what it reads, writes and
/// scratches, all decided by the `build_body` arm that built it.
pub(crate) use crate::pbfhogg::argv::PbfArgv as CommandArgv;

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

/// `cat --type` filter - restrict passthrough to a specific object type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatTypeFilter {
    Way,
    Relation,
    Invalid(String),
}

/// What a `cat` run does. pbfhogg's `--dedupe` is blob-level passthrough and
/// refuses `--type` and `--clean`, so the two shapes are separate variants and
/// no `PbfhoggCommand::Cat` can carry the combination pbfhogg rejects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatMode {
    /// Single-input `cat`.
    ///   - `type_filter` → `--type way|relation` (restricts output to one
    ///     object kind).
    ///   - `clean` → `--clean version`: decode and re-encode each data
    ///     blob cat keeps (every one without `type_filter`), stripping the
    ///     version attribute.
    Plain {
        type_filter: Option<CatTypeFilter>,
        clean: bool,
    },
    /// `cat --dedupe` with two PBF inputs (the same file twice), the only
    /// `cat` flavour that supports `--io-uring`.
    Dedupe,
}

impl CatTypeFilter {
    pub fn as_str(&self) -> Result<&'static str, DevError> {
        match self {
            Self::Way => Ok("way"),
            Self::Relation => Ok("relation"),
            Self::Invalid(message) => Err(DevError::Config(message.clone())),
        }
    }

    pub fn from_cli(value: &str) -> Self {
        match Self::parse(value) {
            Ok(filter) => filter,
            Err(error) => Self::Invalid(error.to_string()),
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
    /// Text diff (`pbfhogg diff <a> <b> --suppress-common --output <out>`).
    #[default]
    Default,
    /// OSC-format diff (`pbfhogg diff --format osc <a> <b> --output <out>`).
    Osc,
}

/// Canonical form for `digest --form` (`full` is v2, `content` is v1, diff's
/// equality).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum DigestForm {
    Full,
    Content,
}

impl DigestForm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Content => "content",
        }
    }
}

impl ExtractStrategy {
    pub fn all() -> &'static [ExtractStrategy] {
        &[Self::Simple, Self::Complete, Self::Smart]
    }

    pub fn parse(s: &str) -> Result<Self, DevError> {
        Self::all()
            .iter()
            .copied()
            .find(|strategy| strategy.name() == s)
            .ok_or_else(|| DevError::Config(format!("unknown extract strategy: {s}")))
    }

    /// Stable strategy label used by command selectors and result output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Complete => "complete",
            Self::Smart => "smart",
        }
    }

    /// pbfhogg's optional strategy flag. `Complete` is its default path.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Self::Simple => Some("--simple"),
            Self::Complete => None,
            Self::Smart => Some("--smart"),
        }
    }

    /// osmium's name for the equivalent extraction strategy.
    pub fn osmium_name(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Complete => "complete_ways",
            Self::Smart => "smart",
        }
    }
}

// ---------------------------------------------------------------------------
// Hardcoded bench fixtures
// ---------------------------------------------------------------------------

/// Built-in ID list for the getid bench (`brokkr getid` and the suite's
/// getid presets) on a dataset without `getid_ids`. Three IDs of each object
/// type (n/w/r), chosen in Denmark: a dataset that does not contain Denmark
/// may lack most of them, and its bench then times a mostly empty selection,
/// which is why a dataset can configure its own. pbfhogg tolerates missing IDs. Changing
/// this list changes what every existing unconfigured row measured without
/// splitting the pairing, so it stays as it is.
const GETID_BENCH_IDS: &[&str] = &[
    "n115722", "n115723", "n115724", "w2080", "w2081", "w2082", "r174", "r213", "r339",
];

/// Built-in ID list for the getparents bench on a dataset without
/// `getparents_ids`: a subset of `GETID_BENCH_IDS`, fewer because
/// getparents is a superset operation and more inputs would dominate with
/// parent-traversal cost. Fixed for the same reason: a changed list would
/// pair unlike rows under one key unless the key moved too. `w2080` had no
/// relation parent on the europe and planet inputs checked on 2026-10-28, so
/// there it adds lookup work but selects nothing; a dataset that needs a way
/// seed with parents configures `getparents_ids`.
const GETPARENTS_BENCH_IDS: &[&str] = &["n115722", "n115723", "w2080"];

/// The ID argv a getid or getparents bench passes, and the pairing qualifier
/// that keeps a configured list's rows apart: `None` when the resolved list
/// is the built-in one (configured or not), so those rows keep the identity
/// every existing row has.
fn bench_ids(
    configured: Option<&ElementIdList>,
    builtin: &[&str],
) -> (Vec<String>, Option<String>) {
    let ids = configured.map_or_else(
        || builtin.iter().map(|&id| id.to_owned()).collect(),
        ElementIdList::args,
    );
    let qualifier = (ids.iter().map(String::as_str).ne(builtin.iter().copied()))
        .then(|| format!("ids={}", ids.join(",")));
    (ids, qualifier)
}

/// A bbox's pairing identity: its parsed coordinates in Rust's shortest
/// round-trip form, signed zero folded, so two spellings of one box (`12` and
/// `12.0`) pair. pbfhogg parses each component as an f64 too, so equal
/// coordinates here are equal boundaries in the child.
fn canonical_bbox(bbox: &Bbox) -> String {
    bbox.coordinates()
        .iter()
        // `-0.0 + 0.0` is `+0.0`; every other value is unchanged.
        .map(|&c| (c + 0.0).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Min-count threshold used by the bench version of `inspect tags`. Set
/// impossibly high so pbfhogg's default tag-frequency filter never trims
/// anything - we want the full decode cost measured, not just popular tags.
pub(crate) const INSPECT_ALL_TAGS_MIN_COUNT: &str = "999999999";

/// Default `verify multi-extract` region count, shared with the CLI schema.
pub const DEFAULT_MULTI_EXTRACT_REGIONS: usize = 5;

// ---------------------------------------------------------------------------
// PbfhoggCommand - the unified command enum
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
        mode: InspectMode,
    },
    CheckRefs,
    /// `check --ids`. `full` adds `--full` - per-type duplicate-ID detection
    /// via RoaringTreemap sets, in addition to the streaming monotonicity /
    /// type-order checks.
    CheckIds {
        full: bool,
    },
    /// `check --index`: re-derives every OSMData blob's indexdata and
    /// tagdata from its decompressed payload. Passing `--index` alone
    /// suppresses pbfhogg's default `--ids --refs` pair.
    CheckIndex,
    /// `digest`. `form` → `--form <full|content>`; `None` passes no flag, so
    /// the build's own default is measured - a baseline from before `--form`
    /// existed digests its only form.
    Digest {
        form: Option<DigestForm>,
    },
    Sort,
    /// `cat`, in one of the shapes `CatMode` admits.
    Cat(CatMode),
    /// Unified `tags-filter`. Orthogonal flags:
    ///   - `filter` - the pbfhogg filter expression (e.g.
    ///     `w/highway=primary`, `amenity=restaurant`); defaults to
    ///     `w/highway=primary`.
    ///
    /// Input-specific flags are encoded by `TagsFilterInput`.
    TagsFilter {
        filter: String,
        input: TagsFilterInput,
    },
    /// Unified `getid`. Flags:
    ///   - `add_referenced` → `--add-referenced` (two-pass with
    ///     referenced-element collection).
    ///   - `invert` → `--invert` (select everything NOT matching).
    ///
    /// The ID list is hardcoded (same fixed set used by all three
    /// previous presets) to keep the bench shape reproducible.
    Getid {
        add_referenced: bool,
        invert: bool,
    },
    Getparents,
    Renumber,
    /// `merge-changes`. `simplify` → `--simplify` (BTreeMap dedupe path,
    /// keeping only the last change per object). Off = default streaming
    /// concat path.
    MergeChanges {
        simplify: bool,
    },
    ApplyChanges,
    AddLocationsToWays,
    TimeFilter,
    /// Unified `diff`. `format` selects summary (`Default`) or OSC-format
    /// output. Reuses `DiffFormat` with `DiffSnapshots`.
    Diff {
        format: DiffFormat,
    },

    /// Two-snapshot diff: compares two PBFs from different point-in-time
    /// snapshots of the same dataset (e.g. `planet-20260223` vs
    /// `planet-20260411`). Unlike `Diff`, neither side is derived from
    /// `apply-changes` - both come from independent snapshot resolution.
    /// The format flag selects between summary diff and OSC-format diff.
    DiffSnapshots {
        format: DiffFormat,
    },

    /// `repack` - re-encode a PBF with a configurable elements-per-blob
    /// cap. `elements_per_blob = None` defers to pbfhogg's default (8000);
    /// `Some(N)` emits `--elements-per-blob N`. `force` adds `--force`,
    /// skipping pbfhogg's indexdata requirement.
    Repack {
        elements_per_blob: Option<u32>,
        force: bool,
    },
    /// `degrade` - emit an adversarial PBF. At least one of the three
    /// transformation flags is required (pbfhogg enforces this; brokkr
    /// forwards as-is). `strip_indexdata` is the only flag that affects
    /// snapshot-promotion routing (produces `pbf.raw`, not `pbf.indexed`).
    Degrade {
        unsort: bool,
        strip_locations: bool,
        strip_indexdata: bool,
    },

    // -- Standalone commands --
    BuildGeocodeIndex,

    // -- Multi-variant benchmarks --
    Extract {
        strategy: ExtractStrategy,
    },

    /// Multi-extract: N-region extract benchmark. `strategy` picks the
    /// pbfhogg extract strategy (`Simple` → `--simple`, `Smart` → `--smart`,
    /// `Complete` → no flag) - the axis lets us measure multipolygon/
    /// boundary-relation cost on the same region set.
    MultiExtract {
        regions: usize,
        strategy: ExtractStrategy,
    },
}

impl PbfhoggCommand {
    /// Describe command identity, output routing and I/O support in one match.
    pub fn descriptor(&self) -> CommandDescriptor {
        use OutputKind::{None, ScratchDir, ScratchOsc, ScratchPbf, ScratchText};
        let (id, output, promotion_variant, supports_io_uring) = match self {
            Self::Inspect { .. } => ("inspect", None, PromotionVariant::Unsupported, false),
            Self::CheckRefs => ("check-refs", None, PromotionVariant::Unsupported, false),
            Self::CheckIds { .. } => ("check-ids", None, PromotionVariant::Unsupported, false),
            Self::CheckIndex => ("check-index", None, PromotionVariant::Unsupported, false),
            Self::Digest { .. } => ("digest", None, PromotionVariant::Unsupported, false),
            Self::Sort => ("sort", ScratchPbf, PromotionVariant::Indexed, true),
            Self::Cat(cat) => (
                "cat",
                ScratchPbf,
                PromotionVariant::Indexed,
                matches!(cat, CatMode::Dedupe),
            ),
            Self::TagsFilter { input, .. } => (
                "tags-filter",
                if matches!(input, TagsFilterInput::Osc) {
                    ScratchOsc
                } else {
                    ScratchPbf
                },
                PromotionVariant::Unsupported,
                false,
            ),
            Self::Getid { .. } => ("getid", ScratchPbf, PromotionVariant::Indexed, false),
            Self::Getparents => ("getparents", ScratchPbf, PromotionVariant::Indexed, false),
            Self::Renumber => ("renumber", ScratchPbf, PromotionVariant::Indexed, false),
            Self::MergeChanges { .. } => (
                "merge-changes",
                ScratchOsc,
                PromotionVariant::Unsupported,
                false,
            ),
            Self::ApplyChanges => ("apply-changes", ScratchPbf, PromotionVariant::Indexed, true),
            Self::AddLocationsToWays => (
                "add-locations-to-ways",
                ScratchPbf,
                PromotionVariant::Indexed,
                false,
            ),
            Self::TimeFilter => ("time-filter", ScratchPbf, PromotionVariant::Indexed, false),
            Self::Diff {
                format: DiffFormat::Default,
            } => ("diff", ScratchText, PromotionVariant::Unsupported, false),
            Self::Diff {
                format: DiffFormat::Osc,
            } => ("diff", ScratchOsc, PromotionVariant::Unsupported, true),
            Self::DiffSnapshots {
                format: DiffFormat::Default,
            } => (
                "diff-snapshots",
                ScratchText,
                PromotionVariant::Unsupported,
                false,
            ),
            Self::DiffSnapshots {
                format: DiffFormat::Osc,
            } => (
                "diff-snapshots",
                ScratchOsc,
                PromotionVariant::Unsupported,
                false,
            ),
            Self::Repack { .. } => ("repack", ScratchPbf, PromotionVariant::Indexed, true),
            Self::Degrade {
                strip_indexdata: true,
                ..
            } => ("degrade", ScratchPbf, PromotionVariant::Raw, true),
            Self::Degrade { .. } => ("degrade", ScratchPbf, PromotionVariant::Indexed, true),
            Self::BuildGeocodeIndex => (
                "build-geocode-index",
                ScratchDir {
                    name: "geocode",
                    dataset_suffix: true,
                },
                PromotionVariant::Unsupported,
                false,
            ),
            Self::Extract { .. } => ("extract", ScratchPbf, PromotionVariant::Indexed, false),
            Self::MultiExtract { .. } => (
                "multi-extract",
                ScratchDir {
                    name: "multi-extract",
                    dataset_suffix: false,
                },
                PromotionVariant::Unsupported,
                false,
            ),
        };
        CommandDescriptor {
            id,
            output,
            promotion_variant,
            supports_io_uring,
        }
    }

    /// The command ID string used in CLI and result DB.
    pub fn id(&self) -> &'static str {
        self.descriptor().id
    }

    /// What output this command produces.
    pub fn output_kind(&self) -> OutputKind {
        self.descriptor().output
    }

    /// Whether this command accepts `--io-uring`.
    ///
    /// Only commands whose pbfhogg binary clap definition includes `--io-uring`
    /// are listed here.  Passing the flag to other commands causes clap exit
    /// code 2.
    /// pbfhogg is an external binary here, so this local compatibility set is
    /// not generated from its clap schema; callers should not repeat it in
    /// error text.
    pub fn supports_io_uring(&self) -> bool {
        self.descriptor().supports_io_uring
    }

    /// Whether this command consumes an OSC file as an argv input.
    pub fn needs_osc_input(&self) -> bool {
        matches!(self, Self::MergeChanges { .. } | Self::ApplyChanges)
            || matches!(
                self,
                Self::TagsFilter {
                    input: TagsFilterInput::Osc,
                    ..
                }
            )
    }

    /// Whether setup must resolve an OSC file, including for the merged-PBF
    /// preparation that `diff` consumes.
    pub fn needs_osc_for_setup(&self) -> bool {
        self.needs_osc_input() || self.needs_merged_pbf()
    }

    /// Whether this command requires a PBF after its resolved-input shape is known.
    pub fn needs_pbf(&self) -> bool {
        !matches!(self, Self::MergeChanges { .. })
            && !matches!(
                self,
                Self::TagsFilter {
                    input: TagsFilterInput::Osc,
                    ..
                }
            )
    }

    /// Whether the plan needs to build the merged PBF consumed by `diff`.
    pub fn needs_merged_pbf(&self) -> bool {
        matches!(self, Self::Diff { .. })
    }

    /// Non-zero exit codes that should be treated as a completed run.
    ///
    /// pbfhogg exits 1 for a completed negative answer and 2 for a failure:
    /// `diff` found differences, `check` found violations (`check-refs` on a
    /// ways-only file, `check-ids` on a duplicated one, `check-index` on a
    /// file whose blob metadata disagrees with its payload). Those runs are
    /// stored, with `meta.exit_code` saying which answer was measured. Only
    /// the commands that give such an answer accept 1, and only on a build
    /// that has the contract: pbfhogg commits from before it (854f9465)
    /// exited 1 on every failure, so `expect::completed_exits` drops 1 for a
    /// build its probe finds predates the split, and a `--commit` baseline
    /// never stores a crash as an answer; an input that declares its answer
    /// narrows the set to that answer. `inspect` would answer
    /// negatively only under `--indexed` or `--show`, which brokkr never
    /// passes.
    pub fn ok_exit_codes(&self) -> &'static [i32] {
        match self {
            Self::Diff { .. }
            | Self::DiffSnapshots { .. }
            | Self::CheckRefs
            | Self::CheckIds { .. }
            | Self::CheckIndex => &[1],
            // Exhaustive on purpose: a new command must decide whether it
            // answers negatively, or a violating run is dropped as a failure.
            Self::Inspect { .. }
            | Self::Digest { .. }
            | Self::Sort
            | Self::Cat(_)
            | Self::TagsFilter { .. }
            | Self::Getid { .. }
            | Self::Getparents
            | Self::Renumber
            | Self::MergeChanges { .. }
            | Self::ApplyChanges
            | Self::AddLocationsToWays
            | Self::TimeFilter
            | Self::Repack { .. }
            | Self::Degrade { .. }
            | Self::BuildGeocodeIndex
            | Self::Extract { .. }
            | Self::MultiExtract { .. } => &[],
        }
    }

    /// The pairing qualifier a results row of this command carries beyond the
    /// brokkr argv: the measured element selection when a dataset configures
    /// getid or getparents IDs that differ from the built-in list. The IDs
    /// live in the subprocess argv, which `arm_key` never sees, so without
    /// this a configured list's rows would pair with rows that measured
    /// another selection. `None` for the built-in lists, whose rows keep the
    /// identity they always had.
    ///
    /// Extract and multi-extract likewise qualify by what the brokkr argv
    /// does not show: the bbox when it came from the dataset rather than
    /// `--bbox`, and the strategy when one argv fans out over every strategy
    /// (`CommandParams::strategy_fanout`). An explicit `--bbox` with a single
    /// `--strategy` stays unqualified. `None` for every other command.
    pub fn arm_qualifier(&self, ctx: &CommandContext) -> Option<String> {
        match self {
            Self::Extract { strategy } | Self::MultiExtract { strategy, .. } => {
                let mut parts = Vec::new();
                if ctx.params.bbox.is_none()
                    && let Some(bbox) = ctx.inputs.bbox()
                {
                    parts.push(format!("bbox={}", canonical_bbox(bbox)));
                }
                if ctx.params.strategy_fanout {
                    parts.push(format!("strategy={}", strategy.name()));
                }
                (!parts.is_empty()).then(|| parts.join(";"))
            }
            Self::Getid { .. } => bench_ids(ctx.params.selection.getid.as_ref(), GETID_BENCH_IDS).1,
            Self::Getparents => {
                bench_ids(
                    ctx.params.selection.getparents.as_ref(),
                    GETPARENTS_BENCH_IDS,
                )
                .1
            }
            _ => None,
        }
    }

    /// Build metadata key-value pairs for the result DB.
    ///
    /// Post-v13, this holds only *runtime observations* - things the
    /// harness learned that cli_args/brokkr_args can't tell you (e.g. the
    /// Diff merged-PBF cache state observed at dispatch time, the
    /// resolved filename/size for a snapshot key, …). Axis-like fields
    /// (index_type, snapshot key, strategy, bbox, …) are passed on the
    /// brokkr command line and recorded in the brokkr_args/cli_args
    /// columns, so they don't need mirroring here.
    pub fn metadata(&self, observations: &CommandObservations) -> Vec<crate::db::KvPair> {
        use crate::db::KvPair;

        match self {
            Self::Diff { .. } => {
                // Merged-PBF cache state is observed while resolving the
                // command input. Lets `brokkr results <uuid>`
                // distinguish runs that paid the setup cost from runs that
                // reused a cached file.
                let mut meta = Vec::new();
                if let Some(state) = observations.merged_cache {
                    match state {
                        crate::pbfhogg::context::MergedCacheState::Hit { age_secs } => {
                            meta.push(KvPair::text("meta.merged_cache", "hit"));
                            meta.push(KvPair::text(
                                "meta.merged_cache_age_s",
                                age_secs.to_string(),
                            ));
                        }
                        crate::pbfhogg::context::MergedCacheState::Miss => {
                            meta.push(KvPair::text("meta.merged_cache", "miss"));
                        }
                    }
                }
                meta
            }
            Self::DiffSnapshots { .. } => {
                // The --to snapshot's resolved filename and size are
                // observations - the user passed a key like `20260411`,
                // the dispatch layer resolved it to an actual file via
                // brokkr.toml. Record the resolved info so queries can
                // identify which B-side file was actually consumed.
                let mut meta = Vec::new();
                if let Some(snapshot) = &observations.snapshot_to {
                    if let Some(file) = &snapshot.file_name {
                        meta.push(KvPair::text("meta.to_snapshot_file", file));
                    }
                    if let Some(mb) = snapshot.size_mb {
                        meta.push(KvPair::real("meta.to_snapshot_file_mb", mb));
                    }
                }
                meta
            }
            _ => vec![],
        }
    }

    /// Build the argument vector for this command given the resolved context.
    ///
    /// Returns the command argv without the binary path; the caller passes
    /// the binary separately when spawning. The mode only selects the scratch
    /// filename namespace, so benchmark and profiling runs execute the same
    /// command arguments.
    /// I/O and explicit compression options are appended here so every caller,
    /// including dry-run, sees the same command shape. The one flag not here
    /// is `--overwrite`: whether it is passed depends on the executable, so
    /// [`PbfhoggBinary`](crate::pbfhogg::binary::PbfhoggBinary) adds it when
    /// resolving the returned [`CommandArgv`].
    #[allow(clippy::too_many_lines)]
    pub fn build_args(&self, ctx: &CommandContext, mode: ArgMode) -> Result<CommandArgv, DevError> {
        let mut argv = self.build_body(ctx, mode)?;
        for arg in ctx.params.io.args() {
            argv = argv.arg(arg);
        }
        if let Some(compression) = &ctx.params.compression {
            argv = argv.arg("--compression").arg(compression.to_string());
        }
        Ok(argv)
    }

    /// Build the core argv for an extract command with an explicit output
    /// path. Shared by measured commands, suite benchmarks and verification,
    /// keeping the strategy flag and single-token `-b=<bbox>` spelling alike.
    pub(crate) fn extract_args_for(
        &self,
        pbf: &Path,
        bbox: &Bbox,
        output: &Path,
    ) -> Result<CommandArgv, DevError> {
        let Self::Extract { strategy } = self else {
            return Err(DevError::Config(
                "extract_args_for requires an extract command".into(),
            ));
        };

        let mut argv = CommandArgv::new("extract").input(pbf)?;
        if let Some(flag) = strategy.flag() {
            argv = argv.arg(flag);
        }
        argv.arg(bbox.to_pbfhogg_arg()).output(output)
    }

    /// Build the body of the argument vector (no binary prefix), with what it
    /// reads, writes and scratches, all from the same calls.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    fn build_body(&self, ctx: &CommandContext, mode: ArgMode) -> Result<CommandArgv, DevError> {
        let jobs = |argv: CommandArgv| match ctx.params.jobs {
            Some(j) => argv.arg("--jobs").arg(j.to_string()),
            None => argv,
        };
        match self {
            // -----------------------------------------------------------------
            // Tool CLI commands (26 from bench_commands.rs)
            // -----------------------------------------------------------------
            Self::Inspect { mode: inspect_mode } => {
                let mut argv = CommandArgv::new("inspect");
                match inspect_mode {
                    InspectMode::Tags { type_filter } => {
                        argv = argv.arg("tags").input(ctx.pbf_str()?)?;
                        if let Some(tf) = type_filter {
                            argv = argv.arg("--type").arg(tf.as_str()?);
                        }
                        argv = argv.arg("--min-count").arg(INSPECT_ALL_TAGS_MIN_COUNT);
                    }
                    InspectMode::Nodes => {
                        argv = argv.arg("--nodes").input(ctx.pbf_str()?)?;
                    }
                    InspectMode::Default { extended } => {
                        argv = argv.input(ctx.pbf_str()?)?;
                        if *extended {
                            argv = argv.arg("--extended");
                        }
                    }
                }
                // pbfhogg's default inspect (metadata) doesn't accept -j;
                // drop the flag silently rather than let pbfhogg reject.
                if matches!(inspect_mode, InspectMode::Nodes | InspectMode::Tags { .. }) {
                    argv = jobs(argv);
                }
                Ok(argv)
            }
            Self::CheckRefs => CommandArgv::new("check")
                .arg("--refs")
                .input(ctx.pbf_str()?),
            Self::CheckIds { full } => {
                let argv = CommandArgv::new("check")
                    .arg("--ids")
                    .input(ctx.pbf_str()?)?;
                Ok(if *full { argv.arg("--full") } else { argv })
            }
            Self::CheckIndex => CommandArgv::new("check")
                .arg("--index")
                .input(ctx.pbf_str()?),
            Self::Digest { form } => {
                let argv = CommandArgv::new("digest").input(ctx.pbf_str()?)?;
                Ok(match form {
                    Some(form) => argv.arg("--form").arg(form.as_str()),
                    None => argv,
                })
            }
            Self::Sort => {
                let output = scratch_output_path(ctx, self, mode)?;
                CommandArgv::new("sort")
                    .input(ctx.pbf_str()?)?
                    .output(&output)
            }
            Self::Cat(cat) => {
                let output = scratch_output_path(ctx, self, mode)?;
                let pbf = ctx.pbf_str()?;
                let argv = CommandArgv::new("cat");
                match cat {
                    // `--dedupe` takes two PBFs; the same file twice gives a
                    // deterministic bench shape.
                    CatMode::Dedupe => argv.arg("--dedupe").input(pbf)?.input(pbf)?,
                    CatMode::Plain { type_filter, clean } => {
                        let mut argv = argv;
                        if *clean {
                            // pbfhogg's --clean takes an ATTR value
                            // (version|changeset|timestamp|uid|user). Any one
                            // makes cat decode and re-encode each blob it keeps;
                            // `version` is the one brokkr strips.
                            argv = argv.arg("--clean").arg("version");
                        }
                        argv = argv.input(pbf)?;
                        if let Some(tf) = type_filter {
                            argv = argv.arg("--type").arg(tf.as_str()?);
                        }
                        argv
                    }
                }
                .output(&output)
            }
            Self::TagsFilter { filter, input } => {
                let output = scratch_output_path(ctx, self, mode)?;
                let mut argv = CommandArgv::new("tags-filter");
                let input_path = match input {
                    TagsFilterInput::Osc => {
                        argv = argv.arg("--input-kind").arg("osc");
                        ctx.osc_str()?
                    }
                    TagsFilterInput::Pbf {
                        omit_referenced,
                        invert,
                        remove_tags,
                    } => {
                        if *omit_referenced {
                            argv = argv.arg("--omit-referenced");
                        }
                        if *invert {
                            argv = argv.arg("--invert-match");
                        }
                        if *remove_tags {
                            argv = argv.arg("--remove-tags");
                        }
                        ctx.pbf_str()?
                    }
                    TagsFilterInput::Invalid(message) => {
                        return Err(DevError::Config(message.clone()));
                    }
                };
                let argv = argv
                    .input(input_path)?
                    .arg(filter.clone())
                    .output(&output)?;
                Ok(jobs(argv))
            }
            Self::Getid {
                add_referenced,
                invert,
            } => {
                let output = scratch_output_path(ctx, self, mode)?;
                let mut argv = CommandArgv::new("getid");
                if *invert {
                    argv = argv.arg("--invert");
                }
                argv = argv.input(ctx.pbf_str()?)?;
                if *add_referenced {
                    argv = argv.arg("--add-referenced");
                }
                let (ids, _) = bench_ids(ctx.params.selection.getid.as_ref(), GETID_BENCH_IDS);
                for id in ids {
                    argv = argv.arg(id);
                }
                argv.output(&output)
            }
            Self::Getparents => {
                let output = scratch_output_path(ctx, self, mode)?;
                let mut argv = CommandArgv::new("getparents").input(ctx.pbf_str()?)?;
                let (ids, _) = bench_ids(
                    ctx.params.selection.getparents.as_ref(),
                    GETPARENTS_BENCH_IDS,
                );
                for id in ids {
                    argv = argv.arg(id);
                }
                argv.output(&output)
            }
            Self::Renumber => {
                let output = scratch_output_path(ctx, self, mode)?;
                CommandArgv::new("renumber")
                    .input(ctx.pbf_str()?)?
                    .output(&output)
            }
            Self::MergeChanges { simplify } => {
                let output = scratch_output_path(ctx, self, mode)?;
                let mut argv = CommandArgv::new("merge-changes");
                for o in ctx.osc_strs()? {
                    argv = argv.input(o)?;
                }
                let argv = argv.output(&output)?;
                Ok(if *simplify {
                    argv.arg("--simplify")
                } else {
                    argv
                })
            }
            Self::ApplyChanges => {
                let output = scratch_output_path(ctx, self, mode)?;
                Self::apply_changes_args(
                    Path::new(ctx.pbf_str()?),
                    Path::new(ctx.osc_str()?),
                    &output,
                    &ctx.params,
                )
            }
            Self::AddLocationsToWays => {
                let output = scratch_output_path(ctx, self, mode)?;
                let mut argv = CommandArgv::new("add-locations-to-ways")
                    .input(ctx.pbf_str()?)?
                    .output(&output)?
                    .scratch(&ctx.scratch_dir);
                if let Some(it) = &ctx.params.index_type {
                    argv = argv.arg("--index-type").arg(it.clone());
                }
                if ctx.params.inject_prepass {
                    argv = argv.arg("--inject-prepass");
                }
                if ctx.params.force_altw {
                    argv = argv.arg("--force");
                }
                Ok(argv)
            }
            Self::TimeFilter => {
                let output = scratch_output_path(ctx, self, mode)?;
                CommandArgv::new("time-filter")
                    .input(ctx.pbf_str()?)?
                    .arg("2024-01-01T00:00:00Z")
                    .output(&output)
            }
            Self::Diff { format } | Self::DiffSnapshots { format } => {
                let from = ctx.pbf_str()?;
                let to = ctx.pbf_b_str()?;
                let argv = match format {
                    DiffFormat::Default => {
                        // The text goes to a file, never stdout: at planet it
                        // overflows the measurement's stdout capture.
                        let output = scratch_output_path(ctx, self, mode)?;
                        let argv = CommandArgv::new("diff")
                            .input(from)?
                            .input(to)?
                            .arg("--suppress-common")
                            .output(&output)?;
                        if crate::pbfhogg::argv::text_diff_writes_scratch(ctx.params.jobs, false) {
                            argv.scratch(&ctx.scratch_dir)
                        } else {
                            argv
                        }
                    }
                    DiffFormat::Osc => {
                        let output = scratch_output_path(ctx, self, mode)?;
                        CommandArgv::new("diff")
                            .arg("--format")
                            .arg("osc")
                            .input(from)?
                            .input(to)?
                            .output(&output)?
                            .scratch(&ctx.scratch_dir)
                    }
                };
                Ok(jobs(argv))
            }

            Self::Repack {
                elements_per_blob,
                force,
            } => {
                let output = scratch_output_path(ctx, self, mode)?;
                let mut argv = CommandArgv::new("repack")
                    .input(ctx.pbf_str()?)?
                    .output(&output)?;
                if let Some(n) = elements_per_blob {
                    argv = argv.arg("--elements-per-blob").arg(n.to_string());
                }
                Ok(if *force { argv.arg("--force") } else { argv })
            }
            Self::Degrade {
                unsort,
                strip_locations,
                strip_indexdata,
            } => {
                let output = scratch_output_path(ctx, self, mode)?;
                let mut argv = CommandArgv::new("degrade")
                    .input(ctx.pbf_str()?)?
                    .output(&output)?;
                if *unsort {
                    argv = argv.arg("--unsort");
                }
                if *strip_locations {
                    argv = argv.arg("--strip-locations");
                }
                if *strip_indexdata {
                    argv = argv.arg("--strip-indexdata");
                }
                Ok(argv)
            }

            // -----------------------------------------------------------------
            // Standalone commands
            // -----------------------------------------------------------------
            Self::BuildGeocodeIndex => {
                let output_dir = scratch_output_path(ctx, self, mode)?;
                // pbfhogg's `--force` here means overwrite plus allow-unindexed,
                // in every version brokkr builds, so it replaces the output on
                // its own and the negotiated flag is not needed.
                Ok(CommandArgv::new("build-geocode-index")
                    .input(ctx.pbf_str()?)?
                    .output_dir("--output-dir", &output_dir)?
                    .arg("--force")
                    .forces_overwrite()
                    .scratch(&ctx.scratch_dir))
            }

            // -----------------------------------------------------------------
            // Multi-extract: single-pass N-region extract benchmark
            // -----------------------------------------------------------------
            Self::MultiExtract { regions, strategy } => {
                // Validates regions/bbox and names the config file, but does
                // NOT write it: build_args is also dry-run's validator, and
                // dry-run must not touch the filesystem. The real run paths
                // call `prepare_scratch` to materialise it.
                let output_dir = scratch_output_path(ctx, self, mode)?;
                let bbox = ctx
                    .inputs
                    .bbox()
                    .ok_or_else(|| DevError::Config("multi-extract requires a bbox".into()))?;
                let plan = multi_extract_plan(ctx, output_dir, bbox, *regions)?;
                // The outputs are named in the config file; one argv
                // `--overwrite` covers every one of them.
                let argv = CommandArgv::new("extract")
                    .input(ctx.pbf_str()?)?
                    .extract_config(&plan.config_path, plan.outputs)?;
                Ok(match strategy.flag() {
                    Some(flag) => argv.arg(flag),
                    None => argv,
                })
            }

            // -----------------------------------------------------------------
            // Multi-variant: extract (with resolved bbox)
            // -----------------------------------------------------------------
            Self::Extract { .. } => {
                let bbox = ctx
                    .inputs
                    .bbox()
                    .ok_or_else(|| DevError::Config("extract requires a bbox".into()))?;
                let output = scratch_output_path(ctx, self, mode)?;
                self.extract_args_for(Path::new(ctx.pbf_str()?), bbox, &output)
            }
        }
    }

    /// Construct the shared apply-changes core argv. `ensure_merged_pbf`
    /// uses default settings to build a cache input; measured apply-changes
    /// commands pass their resolved parameters here.
    pub(crate) fn apply_changes_args(
        pbf: &Path,
        osc: &Path,
        output: &Path,
        params: &CommandParams,
    ) -> Result<CommandArgv, DevError> {
        let mut argv = CommandArgv::new("apply-changes").input(pbf)?.input(osc)?;
        if params.locations_on_ways {
            argv = argv.arg("--locations-on-ways");
        }
        if let Some(jobs) = params.jobs {
            argv = argv.arg("--jobs").arg(jobs.to_string());
        }
        argv.output(output)
    }

    /// Materialise the on-disk inputs `build_args` only names.
    ///
    /// `build_args` is pure so that `--dry-run` can call it as a validator
    /// without writing anything. Commands whose argv refers to a generated
    /// file (multi-extract's `--config` JSON and its output directory) get
    /// that file written here, by the paths that actually run the binary.
    /// A no-op for every other command.
    pub fn prepare_scratch(&self, ctx: &CommandContext) -> Result<(), DevError> {
        let Self::MultiExtract { regions, .. } = self else {
            return Ok(());
        };
        let output_dir = scratch_output_path(ctx, self, ArgMode::Bench)?;
        let bbox = ctx
            .inputs
            .bbox()
            .ok_or_else(|| DevError::Config("multi-extract requires a bbox".into()))?;
        let plan = multi_extract_plan(ctx, output_dir, bbox, *regions)?;
        std::fs::create_dir_all(&plan.output_dir)?;
        // A launch reading the config is reserved; the gate refuses to
        // rewrite it under one.
        crate::reservations::write(&plan.config_path, plan.config_json.as_bytes())?;
        Ok(())
    }
}

/// Multi-extract's generated config: where it goes and what it says.
struct MultiExtractPlan {
    output_dir: PathBuf,
    config_path: PathBuf,
    config_json: String,
    outputs: Vec<PathBuf>,
}

/// Compute multi-extract's config without touching the filesystem.
///
/// Splits the bbox into `regions` equal-width longitude strips. The JSON is
/// built with `serde_json` so the output directory path is escaped properly
/// (it used to be spliced in with `display()`, which broke on a `"` or `\`).
fn multi_extract_plan(
    ctx: &CommandContext,
    output_dir: PathBuf,
    bbox: &Bbox,
    regions: usize,
) -> Result<MultiExtractPlan, DevError> {
    let strips = bbox.strips(regions)?;
    let config = multi_extract_config(&output_dir, &strips)?;
    Ok(MultiExtractPlan {
        output_dir,
        config_path: multi_extract_config_path(&ctx.scratch_dir),
        config_json: config.json,
        outputs: config.outputs,
    })
}

/// A multi-extract config and the outputs it names, from one construction.
pub(crate) struct MultiExtractConfig {
    pub(crate) json: String,
    pub(crate) outputs: Vec<PathBuf>,
}

/// Encode shared multi-extract strips for a resolved output path. Both the
/// measured command and `verify multi-extract` use serde_json for escaping.
pub(crate) fn multi_extract_config(
    output_dir: &Path,
    strips: &[Bbox],
) -> Result<MultiExtractConfig, DevError> {
    if strips.is_empty() {
        return Err(DevError::Config(
            "multi-extract requires at least 1 region".into(),
        ));
    }
    let output_dir_str = crate::utf8_path::path_string(output_dir)?;
    let names: Vec<String> = (0..strips.len())
        .map(|i| format!("strip-{i}.osm.pbf"))
        .collect();
    let extracts: Vec<serde_json::Value> = strips
        .iter()
        .zip(&names)
        .map(|(bbox, name)| {
            serde_json::json!({
                "output": name,
                "bbox": bbox.coordinates(),
            })
        })
        .collect();
    let config = serde_json::json!({
        "directory": output_dir_str,
        "extracts": extracts,
    });
    let config_json = serde_json::to_string_pretty(&config)
        .map_err(|e| DevError::Config(format!("failed to encode multi-extract config: {e}")))?;

    Ok(MultiExtractConfig {
        json: config_json,
        outputs: names.iter().map(|name| output_dir.join(name)).collect(),
    })
}

/// Stable config filename shared by measured multi-extract and verification.
pub(crate) fn multi_extract_config_path(scratch_dir: &Path) -> PathBuf {
    PbfhoggLayout::fixed_scratch_file(scratch_dir, "multi-extract-config.json")
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Compute the scratch output path for a command based on its output kind.
///
/// Bench and hotpath runs land in distinct filenames so back-to-back runs
/// in the same scratch dir don't clobber each other between invocations.
pub(crate) fn scratch_output_path(
    ctx: &CommandContext,
    cmd: &PbfhoggCommand,
    mode: ArgMode,
) -> Result<PathBuf, DevError> {
    let name = cmd.id();
    let prefix = mode.scratch_prefix();
    Ok(match cmd.output_kind() {
        OutputKind::ScratchPbf => {
            PbfhoggLayout::command_output_file(&ctx.scratch_dir, prefix, name, PbfhoggFileKind::Pbf)
        }
        OutputKind::ScratchOsc => {
            PbfhoggLayout::command_output_file(&ctx.scratch_dir, prefix, name, PbfhoggFileKind::Osc)
        }
        OutputKind::ScratchText => PbfhoggLayout::command_output_file(
            &ctx.scratch_dir,
            prefix,
            name,
            PbfhoggFileKind::Text,
        ),
        OutputKind::ScratchDir {
            name,
            dataset_suffix: true,
        } => PbfhoggLayout::scratch_dir_with_dataset(&ctx.scratch_dir, name, &ctx.dataset),
        OutputKind::ScratchDir {
            name,
            dataset_suffix: false,
        } => PbfhoggLayout::fixed_scratch_dir(&ctx.scratch_dir, name),
        OutputKind::None => {
            return Err(DevError::Config(format!(
                "command '{}' does not produce a scratch output",
                cmd.id()
            )));
        }
    })
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
    use crate::pbfhogg::context::{ResolvedInputs, TwoPbfOrigin};

    fn test_ctx() -> CommandContext {
        CommandContext {
            inputs: ResolvedInputs::PbfAndBbox {
                pbf: PathBuf::from("/data/denmark.osm.pbf"),
                bbox: Bbox::parse("12.4,55.6,12.7,55.8").unwrap(),
            },
            scratch_dir: PathBuf::from("/data/scratch"),
            dataset: "denmark".into(),
            params: CommandParams::default(),
            observations: CommandObservations::default(),
        }
    }

    fn getid() -> PbfhoggCommand {
        PbfhoggCommand::Getid {
            add_referenced: false,
            invert: false,
        }
    }

    fn id_args(cmd: &PbfhoggCommand, ctx: &CommandContext) -> Vec<String> {
        cmd.build_args(ctx, ArgMode::Bench)
            .unwrap()
            .into_args()
            .into_iter()
            .filter(|a| a.len() > 1 && a[1..].bytes().all(|b| b.is_ascii_digit()))
            .collect()
    }

    #[test]
    fn unconfigured_bench_ids_are_the_builtin_lists_and_unqualified() {
        let ctx = test_ctx();
        assert_eq!(id_args(&getid(), &ctx), GETID_BENCH_IDS);
        assert_eq!(
            id_args(&PbfhoggCommand::Getparents, &ctx),
            GETPARENTS_BENCH_IDS
        );
        assert_eq!(getid().arm_qualifier(&ctx), None);
        assert_eq!(PbfhoggCommand::Getparents.arm_qualifier(&ctx), None);
    }

    #[test]
    fn configured_bench_ids_reach_the_argv_and_qualify_the_row() {
        let mut ctx = test_ctx();
        ctx.params.selection.getid = Some(ElementIdList::parse(&["n5", "w06"]).unwrap());
        assert_eq!(id_args(&getid(), &ctx), ["n5", "w6"]);
        assert_eq!(getid().arm_qualifier(&ctx).as_deref(), Some("ids=n5,w6"));
        // getid_ids does not move getparents, and no other command qualifies.
        assert_eq!(PbfhoggCommand::Getparents.arm_qualifier(&ctx), None);
        assert_eq!(PbfhoggCommand::Sort.arm_qualifier(&ctx), None);
    }

    #[test]
    fn extract_qualifies_by_what_the_brokkr_argv_does_not_show() {
        let smart = ExtractStrategy::parse("smart").unwrap();
        let extract = PbfhoggCommand::Extract { strategy: smart };
        let multi = PbfhoggCommand::MultiExtract {
            regions: 2,
            strategy: smart,
        };
        // Dataset bbox, single strategy: the bbox alone.
        let mut ctx = test_ctx();
        for cmd in [&extract, &multi] {
            assert_eq!(
                cmd.arm_qualifier(&ctx).as_deref(),
                Some("bbox=12.4,55.6,12.7,55.8")
            );
        }
        // Dataset bbox under a fan-out: bbox then strategy.
        ctx.params.strategy_fanout = true;
        assert_eq!(
            extract.arm_qualifier(&ctx).as_deref(),
            Some("bbox=12.4,55.6,12.7,55.8;strategy=smart")
        );
        // Explicit --bbox under a fan-out: the strategy alone.
        ctx.params.bbox = ctx.inputs.bbox().cloned();
        assert_eq!(multi.arm_qualifier(&ctx).as_deref(), Some("strategy=smart"));
        // Explicit --bbox, single strategy: the argv key alone, as before.
        ctx.params.strategy_fanout = false;
        assert_eq!(extract.arm_qualifier(&ctx), None);
        assert_eq!(multi.arm_qualifier(&ctx), None);
    }

    #[test]
    fn bbox_qualifier_is_the_parsed_box_not_its_spelling() {
        let a = Bbox::parse("12,55.60,13,56").unwrap();
        let b = Bbox::parse("12.0,55.6,13.0,56.0").unwrap();
        assert_eq!(canonical_bbox(&a), canonical_bbox(&b));
        assert_eq!(canonical_bbox(&a), "12,55.6,13,56");
        let neg = Bbox::parse("-0.0,0,1,1").unwrap();
        assert_eq!(canonical_bbox(&neg), "0,0,1,1");
    }

    #[test]
    fn configuring_the_builtin_list_keeps_the_legacy_identity() {
        let mut ctx = test_ctx();
        ctx.params.selection.getparents = Some(ElementIdList::parse(GETPARENTS_BENCH_IDS).unwrap());
        assert_eq!(PbfhoggCommand::Getparents.arm_qualifier(&ctx), None);
    }

    fn use_pbf_and_osc(ctx: &mut CommandContext) {
        ctx.inputs = ResolvedInputs::PbfAndOsc {
            pbf: PathBuf::from("/data/denmark.osm.pbf"),
            osc: PathBuf::from("/data/denmark-4705.osc.gz"),
        };
    }

    fn use_two_pbfs(ctx: &mut CommandContext) {
        ctx.inputs = ResolvedInputs::TwoPbfs {
            a: PathBuf::from("/data/denmark.osm.pbf"),
            b: PathBuf::from("/data/scratch/merged.osm.pbf"),
            origin: TwoPbfOrigin::Merged,
        };
    }

    fn use_osc_only(ctx: &mut CommandContext) {
        ctx.inputs = ResolvedInputs::OscOnly {
            oscs: vec![PathBuf::from("/data/denmark-4705.osc.gz")],
        };
    }

    /// Every command state the CLI and the suite presets can produce.
    fn every_command_state() -> Vec<PbfhoggCommand> {
        let mut states = vec![
            PbfhoggCommand::Inspect {
                mode: InspectMode::Default { extended: false },
            },
            PbfhoggCommand::Inspect {
                mode: InspectMode::Default { extended: true },
            },
            PbfhoggCommand::Inspect {
                mode: InspectMode::Nodes,
            },
            PbfhoggCommand::Inspect {
                mode: InspectMode::Tags {
                    type_filter: Some(InspectTypeFilter::Way),
                },
            },
            PbfhoggCommand::CheckRefs,
            PbfhoggCommand::CheckIds { full: false },
            PbfhoggCommand::CheckIds { full: true },
            PbfhoggCommand::CheckIndex,
            PbfhoggCommand::Digest { form: None },
            PbfhoggCommand::Digest {
                form: Some(DigestForm::Full),
            },
            PbfhoggCommand::Digest {
                form: Some(DigestForm::Content),
            },
            PbfhoggCommand::Sort,
            PbfhoggCommand::Cat(CatMode::Plain {
                type_filter: Some(CatTypeFilter::Way),
                clean: true,
            }),
            PbfhoggCommand::Cat(CatMode::Plain {
                type_filter: None,
                clean: false,
            }),
            PbfhoggCommand::Cat(CatMode::Dedupe),
            PbfhoggCommand::TagsFilter {
                filter: "w/highway=primary".into(),
                input: TagsFilterInput::Pbf {
                    omit_referenced: true,
                    invert: true,
                    remove_tags: true,
                },
            },
            PbfhoggCommand::TagsFilter {
                filter: "highway=primary".into(),
                input: TagsFilterInput::Osc,
            },
            PbfhoggCommand::Getid {
                add_referenced: true,
                invert: true,
            },
            PbfhoggCommand::Getparents,
            PbfhoggCommand::Renumber,
            PbfhoggCommand::MergeChanges { simplify: true },
            PbfhoggCommand::MergeChanges { simplify: false },
            PbfhoggCommand::ApplyChanges,
            PbfhoggCommand::AddLocationsToWays,
            PbfhoggCommand::TimeFilter,
            PbfhoggCommand::Repack {
                elements_per_blob: Some(100),
                force: true,
            },
            PbfhoggCommand::Degrade {
                unsort: true,
                strip_locations: true,
                strip_indexdata: true,
            },
            PbfhoggCommand::Degrade {
                unsort: true,
                strip_locations: false,
                strip_indexdata: false,
            },
            PbfhoggCommand::BuildGeocodeIndex,
        ];
        for format in [DiffFormat::Default, DiffFormat::Osc] {
            states.push(PbfhoggCommand::Diff { format });
            states.push(PbfhoggCommand::DiffSnapshots { format });
        }
        for &strategy in ExtractStrategy::all() {
            states.push(PbfhoggCommand::Extract { strategy });
            states.push(PbfhoggCommand::MultiExtract {
                regions: 2,
                strategy,
            });
        }
        for name in crate::pbfhogg::bench_commands::ALL_COMMANDS {
            states.push(crate::pbfhogg::bench_commands::preset_to_command(name).unwrap());
        }
        states
    }

    /// A context whose inputs fit `cmd`.
    fn ctx_for(cmd: &PbfhoggCommand) -> CommandContext {
        let mut ctx = test_ctx();
        if matches!(
            cmd,
            PbfhoggCommand::Diff { .. } | PbfhoggCommand::DiffSnapshots { .. }
        ) {
            use_two_pbfs(&mut ctx);
        } else if !cmd.needs_pbf() {
            use_osc_only(&mut ctx);
        } else if cmd.needs_osc_input() {
            use_pbf_and_osc(&mut ctx);
        }
        ctx
    }

    #[test]
    fn the_arm_building_the_argv_decides_overwrite_consistently_with_the_descriptor() {
        use crate::pbfhogg::argv::Overwrite;
        // An output-producing command whose arm forgot to negotiate would
        // fail on its second iteration once pbfhogg refuses existing outputs.
        for cmd in every_command_state() {
            let ctx = ctx_for(&cmd);
            for mode in [ArgMode::Bench, ArgMode::Hotpath] {
                let argv = cmd.build_args(&ctx, mode).unwrap();
                let expected = match cmd.output_kind() {
                    OutputKind::None => Overwrite::NoOutput,
                    _ if cmd == PbfhoggCommand::BuildGeocodeIndex => Overwrite::ImpliedByForce,
                    _ => Overwrite::Negotiate,
                };
                assert_eq!(argv.overwrite(), expected, "{cmd:?} in {mode:?}");
                if expected == Overwrite::ImpliedByForce {
                    assert!(argv.args().iter().any(|a| a == "--force"), "{cmd:?}");
                }
                // The flag is the binary handle's alone to add.
                assert!(
                    !argv
                        .args()
                        .iter()
                        .any(|a| a == crate::pbfhogg::binary::OVERWRITE_FLAG || a == "-O"),
                    "{cmd:?}"
                );
            }
        }
    }

    /// Run scratch is declared by exactly the commands pbfhogg makes it
    /// for: add-locations-to-ways, build-geocode-index, an OSC diff on every
    /// path, and a text diff unless it runs with `-j 1`.
    #[test]
    fn scratch_is_declared_by_the_commands_that_write_it() {
        for cmd in every_command_state() {
            let mut ctx = ctx_for(&cmd);
            for jobs in [None, Some(1), Some(4)] {
                ctx.params.jobs = jobs;
                let argv = cmd.build_args(&ctx, ArgMode::Bench).unwrap();
                let expected = match &cmd {
                    PbfhoggCommand::AddLocationsToWays | PbfhoggCommand::BuildGeocodeIndex => true,
                    PbfhoggCommand::Diff { format } | PbfhoggCommand::DiffSnapshots { format } => {
                        *format == DiffFormat::Osc || jobs != Some(1)
                    }
                    _ => false,
                };
                assert_eq!(argv.writes_scratch(), expected, "{cmd:?} with -j {jobs:?}");
            }
        }
    }

    #[test]
    fn cat_plain_forwards_type_and_clean() {
        let cmd = PbfhoggCommand::Cat(CatMode::Plain {
            type_filter: Some(CatTypeFilter::Way),
            clean: true,
        });
        let args = cmd
            .build_args(&ctx_for(&cmd), ArgMode::Bench)
            .unwrap()
            .into_args();
        assert_eq!(
            args.get(..6),
            Some(
                &[
                    "cat",
                    "--clean",
                    "version",
                    "/data/denmark.osm.pbf",
                    "--type",
                    "way"
                ]
                .map(String::from)[..]
            )
        );
        assert!(!args.iter().any(|a| a == "--dedupe"));
        assert!(!cmd.supports_io_uring());
    }

    #[test]
    fn check_index_passes_only_the_index_selector_and_accepts_a_negative_answer() {
        let cmd = PbfhoggCommand::CheckIndex;
        let args = cmd
            .build_args(&ctx_for(&cmd), ArgMode::Bench)
            .unwrap()
            .into_args();
        assert_eq!(
            args.get(..3),
            Some(&["check", "--index", "/data/denmark.osm.pbf"].map(String::from)[..])
        );
        // Any other selector would change what pbfhogg checks: `--index`
        // alone suppresses the default `--ids --refs` pair.
        assert!(!args.iter().any(|a| a == "--ids" || a == "--refs"));
        assert_eq!(cmd.ok_exit_codes(), &[1]);
        assert!(!cmd.supports_io_uring());
    }

    #[test]
    fn cat_dedupe_takes_the_input_twice_and_nothing_else() {
        let cmd = PbfhoggCommand::Cat(CatMode::Dedupe);
        let args = cmd
            .build_args(&ctx_for(&cmd), ArgMode::Bench)
            .unwrap()
            .into_args();
        assert_eq!(
            args.get(..4),
            Some(
                &[
                    "cat",
                    "--dedupe",
                    "/data/denmark.osm.pbf",
                    "/data/denmark.osm.pbf"
                ]
                .map(String::from)[..]
            )
        );
        assert!(!args.iter().any(|a| a == "--clean" || a == "--type"));
        assert!(cmd.supports_io_uring());
    }

    #[test]
    fn inspect_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Inspect {
            mode: InspectMode::Default { extended: false },
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert_eq!(args, vec!["inspect", "/data/denmark.osm.pbf"]);
    }

    #[test]
    fn inspect_extended_appends_flag() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Inspect {
            mode: InspectMode::Default { extended: true },
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert_eq!(args, vec!["inspect", "/data/denmark.osm.pbf", "--extended"]);
    }

    #[test]
    fn inspect_extended_ignored_in_nodes_mode() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Inspect {
            mode: InspectMode::Nodes,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(!args.iter().any(|a| a == "--extended"));
    }

    #[test]
    fn inspect_tags_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Inspect {
            mode: InspectMode::Tags { type_filter: None },
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
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
    fn inspect_tags_uses_the_same_filter_in_bench_and_hotpath() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::Inspect {
            mode: InspectMode::Tags {
                type_filter: Some(InspectTypeFilter::Way),
            },
        };
        let bench = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        let hotpath = cmd.build_args(&ctx, ArgMode::Hotpath).unwrap().into_args();
        assert_eq!(bench, hotpath);
        assert!(bench.windows(2).any(|pair| pair == ["--type", "way"]));
        assert!(
            bench
                .windows(2)
                .any(|pair| pair == ["--min-count", INSPECT_ALL_TAGS_MIN_COUNT])
        );
    }

    #[test]
    fn apply_changes_hotpath_does_not_override_the_compression_default() {
        let mut ctx = test_ctx();
        use_pbf_and_osc(&mut ctx);
        let cmd = PbfhoggCommand::ApplyChanges;
        let bench = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        let hotpath = cmd.build_args(&ctx, ArgMode::Hotpath).unwrap().into_args();
        assert!(!bench.iter().any(|arg| arg == "--compression"));
        assert!(!hotpath.iter().any(|arg| arg == "--compression"));

        ctx.params.compression = Some("zstd:5".parse().unwrap());
        for mode in [ArgMode::Bench, ArgMode::Hotpath] {
            let args = cmd.build_args(&ctx, mode).unwrap().into_args();
            let option = args.iter().position(|arg| arg == "--compression").unwrap();
            assert_eq!(args[option + 1], "zstd:5");
        }
    }

    #[test]
    fn apply_changes_builds_correct_args() {
        let mut ctx = test_ctx();
        use_pbf_and_osc(&mut ctx);
        let cmd = PbfhoggCommand::ApplyChanges;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert_eq!(args[0], "apply-changes");
        assert_eq!(args[1], "/data/denmark.osm.pbf");
        assert_eq!(args[2], "/data/denmark-4705.osc.gz");
        assert_eq!(args[3], "--output");
        assert!(!args.iter().any(|a| a == "--locations-on-ways"));
    }

    #[test]
    fn apply_changes_forwards_locations_on_ways() {
        let mut ctx = test_ctx();
        use_pbf_and_osc(&mut ctx);
        ctx.params.locations_on_ways = true;
        let cmd = PbfhoggCommand::ApplyChanges;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert_eq!(args[0], "apply-changes");
        assert!(args.iter().any(|a| a == "--locations-on-ways"));
    }

    #[test]
    fn apply_changes_forwards_jobs_flag() {
        let mut ctx = test_ctx();
        use_pbf_and_osc(&mut ctx);
        ctx.params.jobs = Some(8);
        let cmd = PbfhoggCommand::ApplyChanges;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        let pos = args
            .iter()
            .position(|a| a == "--jobs")
            .expect("--jobs present");
        assert_eq!(args[pos + 1], "8");
    }

    #[test]
    fn apply_changes_omits_jobs_when_unset() {
        let mut ctx = test_ctx();
        use_pbf_and_osc(&mut ctx);
        let cmd = PbfhoggCommand::ApplyChanges;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(!args.iter().any(|a| a == "--jobs"));
    }

    #[test]
    fn diff_builds_correct_args() {
        let mut ctx = test_ctx();
        use_two_pbfs(&mut ctx);
        let cmd = PbfhoggCommand::Diff {
            format: DiffFormat::Default,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert_eq!(
            args,
            vec![
                "diff",
                "/data/denmark.osm.pbf",
                "/data/scratch/merged.osm.pbf",
                "--suppress-common",
                "--output",
                "/data/scratch/bench-diff-output.txt",
            ]
        );
    }

    #[test]
    fn a_text_diff_writes_its_text_to_a_scratch_file_per_command_and_mode() {
        let mut ctx = test_ctx();
        use_two_pbfs(&mut ctx);
        for (cmd, mode, want) in [
            (
                PbfhoggCommand::Diff {
                    format: DiffFormat::Default,
                },
                ArgMode::Hotpath,
                "/data/scratch/hotpath-diff-output.txt",
            ),
            (
                PbfhoggCommand::DiffSnapshots {
                    format: DiffFormat::Default,
                },
                ArgMode::Bench,
                "/data/scratch/bench-diff-snapshots-output.txt",
            ),
            (
                PbfhoggCommand::Diff {
                    format: DiffFormat::Osc,
                },
                ArgMode::Bench,
                "/data/scratch/bench-diff-output.osc.gz",
            ),
        ] {
            let args = cmd.build_args(&ctx, mode).unwrap().into_args();
            let pos = args
                .iter()
                .position(|a| a == "--output")
                .expect("--output present");
            assert_eq!(args[pos + 1], want, "{cmd:?}");
        }
    }

    #[test]
    fn inspect_nodes_forwards_jobs_flag() {
        let mut ctx = test_ctx();
        ctx.params.jobs = Some(4);
        let cmd = PbfhoggCommand::Inspect {
            mode: InspectMode::Nodes,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        let pos = args
            .iter()
            .position(|a| a == "--jobs")
            .expect("--jobs present");
        assert_eq!(args[pos + 1], "4");
    }

    #[test]
    fn inspect_tags_forwards_jobs_flag() {
        let mut ctx = test_ctx();
        ctx.params.jobs = Some(0);
        let cmd = PbfhoggCommand::Inspect {
            mode: InspectMode::Tags { type_filter: None },
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        let pos = args
            .iter()
            .position(|a| a == "--jobs")
            .expect("--jobs present");
        assert_eq!(args[pos + 1], "0");
    }

    #[test]
    fn inspect_default_drops_jobs_flag() {
        let mut ctx = test_ctx();
        ctx.params.jobs = Some(4);
        let cmd = PbfhoggCommand::Inspect {
            mode: InspectMode::Default { extended: false },
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(!args.iter().any(|a| a == "--jobs"));
    }

    #[test]
    fn diff_forwards_jobs_flag() {
        let mut ctx = test_ctx();
        use_two_pbfs(&mut ctx);
        ctx.params.jobs = Some(0);
        let cmd = PbfhoggCommand::Diff {
            format: DiffFormat::Default,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        let pos = args
            .iter()
            .position(|a| a == "--jobs")
            .expect("--jobs present");
        assert_eq!(args[pos + 1], "0");
    }

    #[test]
    fn diff_snapshots_forwards_jobs_flag() {
        let mut ctx = test_ctx();
        ctx.inputs = ResolvedInputs::TwoPbfs {
            a: PathBuf::from("/data/denmark-from.osm.pbf"),
            b: PathBuf::from("/data/denmark-to.osm.pbf"),
            origin: TwoPbfOrigin::Snapshot,
        };
        ctx.params.jobs = Some(8);
        let cmd = PbfhoggCommand::DiffSnapshots {
            format: DiffFormat::Osc,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        let pos = args
            .iter()
            .position(|a| a == "--jobs")
            .expect("--jobs present");
        assert_eq!(args[pos + 1], "8");
    }

    #[test]
    fn add_locations_to_ways_with_index_type() {
        let mut ctx = test_ctx();
        ctx.params.index_type = Some("external".into());
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(args.contains(&String::from("--index-type")));
        assert!(args.contains(&String::from("external")));
    }

    #[test]
    fn add_locations_to_ways_forwards_inject_prepass() {
        let mut ctx = test_ctx();
        ctx.params.inject_prepass = true;
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(args.contains(&String::from("--inject-prepass")));
    }

    #[test]
    fn add_locations_to_ways_omits_inject_prepass_by_default() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(!args.contains(&String::from("--inject-prepass")));
    }

    #[test]
    fn add_locations_to_ways_inject_prepass_composes_with_index_type() {
        let mut ctx = test_ctx();
        ctx.params.index_type = Some("external".into());
        ctx.params.inject_prepass = true;
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(args.contains(&String::from("--index-type")));
        assert!(args.contains(&String::from("external")));
        assert!(args.contains(&String::from("--inject-prepass")));
    }

    #[test]
    fn add_locations_to_ways_forwards_force_altw() {
        let mut ctx = test_ctx();
        ctx.params.force_altw = true;
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(args.contains(&String::from("--force")));
    }

    #[test]
    fn add_locations_to_ways_omits_force_altw_by_default() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(!args.contains(&String::from("--force")));
    }

    #[test]
    fn add_locations_to_ways_force_altw_composes_with_inject_prepass() {
        // The intended verdict cell: raw input reaches the decode-all
        // fallback (--force) while emitting the injected-prepass extensions.
        let mut ctx = test_ctx();
        ctx.params.inject_prepass = true;
        ctx.params.force_altw = true;
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(args.contains(&String::from("--inject-prepass")));
        assert!(args.contains(&String::from("--force")));
    }

    #[test]
    fn altw_hotpath_no_index_type_default() {
        // Hotpath should NOT default to --index-type external when omitted.
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::AddLocationsToWays;
        let args = cmd.build_args(&ctx, ArgMode::Hotpath).unwrap().into_args();
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
        assert!(cmd.metadata(&ctx.observations).is_empty());
    }

    #[test]
    fn build_geocode_index_builds_correct_args() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::BuildGeocodeIndex;
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert_eq!(args[0], "build-geocode-index");
        assert_eq!(args[1], "/data/denmark.osm.pbf");
        assert_eq!(args[2], "--output-dir");
        assert!(args[3].contains("geocode-denmark"));
        assert_eq!(args[4], "--force");
    }

    #[test]
    fn tags_filter_osc_requires_osc() {
        let cmd = PbfhoggCommand::TagsFilter {
            filter: "highway=primary".into(),
            input: TagsFilterInput::Osc,
        };
        assert!(cmd.needs_osc_input());
        assert!(cmd.needs_osc_for_setup());
        assert!(!cmd.needs_pbf());
    }

    #[test]
    fn tags_filter_pbf_does_not_require_osc() {
        let cmd = PbfhoggCommand::TagsFilter {
            filter: "w/highway=primary".into(),
            input: TagsFilterInput::Pbf {
                omit_referenced: true,
                invert: false,
                remove_tags: false,
            },
        };
        assert!(!cmd.needs_osc_input());
        assert!(!cmd.needs_osc_for_setup());
        assert!(cmd.needs_pbf());
    }

    #[test]
    fn tags_filter_forwards_invert_and_remove_tags() {
        let ctx = test_ctx();
        let cmd = PbfhoggCommand::TagsFilter {
            filter: "highway=primary".into(),
            input: TagsFilterInput::Pbf {
                omit_referenced: false,
                invert: true,
                remove_tags: true,
            },
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(args.iter().any(|a| a == "--invert-match"));
        assert!(args.iter().any(|a| a == "--remove-tags"));
        assert!(!args.iter().any(|a| a == "--omit-referenced"));
    }

    #[test]
    fn tags_filter_osc_mode_drops_pbf_only_flags() {
        let mut ctx = test_ctx();
        use_osc_only(&mut ctx);
        let cmd = PbfhoggCommand::TagsFilter {
            filter: "highway=primary".into(),
            input: TagsFilterInput::Osc,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(!args.iter().any(|a| a == "--omit-referenced"));
        assert!(!args.iter().any(|a| a == "--invert-match"));
        assert!(!args.iter().any(|a| a == "--remove-tags"));
    }

    #[test]
    fn tags_filter_forwards_jobs_flag() {
        let mut ctx = test_ctx();
        ctx.params.jobs = Some(4);
        let cmd = PbfhoggCommand::TagsFilter {
            filter: "w/highway=primary".into(),
            input: TagsFilterInput::Pbf {
                omit_referenced: true,
                invert: false,
                remove_tags: false,
            },
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        let pos = args
            .iter()
            .position(|a| a == "--jobs")
            .expect("--jobs present");
        assert_eq!(args[pos + 1], "4");
    }

    #[test]
    fn merge_changes_uses_osc_only() {
        let cmd = PbfhoggCommand::MergeChanges { simplify: false };
        assert!(!cmd.needs_pbf());
        assert!(cmd.needs_osc_input());
        assert!(cmd.needs_osc_for_setup());
    }

    #[test]
    fn merge_changes_emits_simplify_when_set() {
        let mut ctx = test_ctx();
        use_osc_only(&mut ctx);
        let cmd = PbfhoggCommand::MergeChanges { simplify: true };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(args.iter().any(|a| a == "--simplify"));
    }

    #[test]
    fn merge_changes_omits_simplify_by_default() {
        let mut ctx = test_ctx();
        use_osc_only(&mut ctx);
        let cmd = PbfhoggCommand::MergeChanges { simplify: false };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        assert!(!args.iter().any(|a| a == "--simplify"));
    }

    // build_args is dry-run's validator, so it must not write the config it
    // names; prepare_scratch is what materialises it.
    #[test]
    fn multi_extract_build_args_writes_nothing() {
        let mut ctx = test_ctx();
        ctx.scratch_dir = crate::test_scratch::scratch("pbfhogg-commands", "multi_extract_pure");
        let cmd = PbfhoggCommand::MultiExtract {
            regions: 3,
            strategy: ExtractStrategy::Simple,
        };
        let args = cmd.build_args(&ctx, ArgMode::Bench).unwrap().into_args();
        let config_path = multi_extract_config_path(&ctx.scratch_dir);
        assert!(args.iter().any(|a| a == config_path.to_str().unwrap()));
        assert_eq!(std::fs::read_dir(&ctx.scratch_dir).unwrap().count(), 0);

        cmd.prepare_scratch(&ctx).unwrap();
        assert!(PbfhoggLayout::multi_extract_dir(&ctx.scratch_dir).is_dir());
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(json["extracts"].as_array().unwrap().len(), 3);
        assert_eq!(json["extracts"][2]["bbox"][2].as_f64().unwrap(), 12.7);
    }

    #[test]
    fn multi_extract_config_escapes_the_directory() {
        let mut ctx = test_ctx();
        ctx.scratch_dir = PathBuf::from("/data/odd \"quoted\\\" dir");
        let output_dir = scratch_output_path(
            &ctx,
            &PbfhoggCommand::MultiExtract {
                regions: 1,
                strategy: ExtractStrategy::Simple,
            },
            ArgMode::Bench,
        )
        .unwrap();
        let bbox = ctx.inputs.bbox().unwrap();
        let plan = multi_extract_plan(&ctx, output_dir, bbox, 1).unwrap();
        let json: serde_json::Value = serde_json::from_str(&plan.config_json).unwrap();
        assert_eq!(
            json["directory"].as_str().unwrap(),
            "/data/odd \"quoted\\\" dir/multi-extract"
        );
    }
}
