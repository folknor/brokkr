//! Exhaustive command policy shared by dispatch, help and the manual.

use crate::project::Project;

#[derive(Clone, Copy)]
pub(crate) enum Visibility {
    Any,
    Only(&'static [Project]),
    Except(&'static [Project]),
}
pub(crate) const MEASURED_DB_ABSENT: &[Project] = &[Project::Litehtml];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigNeed {
    Required,
    Optional,
    None,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockPolicy {
    None,
    Blocking,
    NonBlocking,
}

pub(crate) struct CommandSpec {
    pub name: &'static str,
    pub applies: Visibility,
    pub config: ConfigNeed,
    pub lock: LockPolicy,
    pub disk_gate: bool,
    pub default_mode: crate::measure::MeasureMode,
    pub run_log: Option<&'static str>,
    pub tools: &'static [&'static str],
}

macro_rules! commands {
    ($($variant:ident => ($name:literal, $applies:expr, $config:ident, $lock:ident, $gate:literal, $mode:expr, $log:expr, $tools:expr)),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum CommandId { $($variant),* }
        impl CommandId {
            pub(crate) fn spec(self) -> &'static CommandSpec {
                match self { $(Self::$variant => &CommandSpec {
                    name: $name, applies: $applies, config: ConfigNeed::$config,
                    lock: LockPolicy::$lock, disk_gate: $gate,
                    default_mode: $mode, run_log: $log, tools: $tools,
                }),* }
            }
            pub(crate) fn from_name(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant),)* _ => None }
            }
        }
        pub(crate) const COMMANDS: &[CommandId] = &[$(CommandId::$variant),*];
    }
}

commands! {
    Check => ("check", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::CHECK), &["cargo"]),
    Clippy => ("clippy", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Fmt => ("fmt", Visibility::Any, Optional, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Run => ("run", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Install => ("install", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Wc => ("wc", Visibility::Any, Optional, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Man => ("man", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Bench => ("bench", Visibility::Any, Optional, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::BENCH), &["cargo"]),
    Deps => ("deps", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Env => ("env", Visibility::Any, Required, None, false, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Inspect => ("inspect", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    CheckRefs => ("check-refs", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    CheckIds => ("check-ids", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Sort => ("sort", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Cat => ("cat", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    TagsFilter => ("tags-filter", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Getid => ("getid", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Getparents => ("getparents", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Renumber => ("renumber", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    MergeChanges => ("merge-changes", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    ApplyChanges => ("apply-changes", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    AddLocationsToWays => ("add-locations-to-ways", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Repack => ("repack", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Degrade => ("degrade", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    MultiExtract => ("multi-extract", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    TimeFilter => ("time-filter", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Diff => ("diff", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    DiffSnapshots => ("diff-snapshots", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    BuildGeocodeIndex => ("build-geocode-index", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Extract => ("extract", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Read => ("read", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Write => ("write", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    MergeBench => ("merge", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Tilegen => ("tilegen", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    PmtilesWriter => ("pmtiles-writer", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    NodeStore => ("node-store", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    ElivPlanetiler => ("planetiler", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "java"]),
    ElivTilemaker => ("tilemaker", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "cmake", "g++", "make"]),
    RunApi => ("api", Visibility::Only(&[Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    RunNidIngest => ("nid-ingest", Visibility::Only(&[Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    RunTiles => ("tiles", Visibility::Only(&[Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    GenericHotpath => ("generic-hotpath", Visibility::Any, Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Suite => ("suite", Visibility::Only(&[Project::Pbfhogg, Project::Elivagar, Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Passthrough => ("passthrough", Visibility::Any, Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Results => ("results", Visibility::Except(MEASURED_DB_ABSENT), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Sidecar => ("sidecar", Visibility::Except(MEASURED_DB_ABSENT), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Clean => ("clean", Visibility::Any, Required, Blocking, false, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Lock => ("lock", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Strays => ("strays", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Guard => ("guard", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Kill => ("kill", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
    History => ("history", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Invalidate => ("invalidate", Visibility::Except(MEASURED_DB_ABSENT), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Verify => ("verify", Visibility::Only(&[Project::Pbfhogg, Project::Elivagar, Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"]),
    Download => ("download", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium", "curl"]),
    CompareTiles => ("compare-tiles", Visibility::Only(&[Project::Elivagar]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    DownloadOcean => ("download-ocean", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "curl"]),
    OceanBuild => ("ocean-build", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    DownloadNaturalEarth => ("download-natural-earth", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "curl"]),
    PmtilesInspect => ("pmtiles-inspect", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Diag => ("diag", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Svg => ("svg", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Regress => ("regress", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    PmtilesCorpus => ("pmtiles-corpus", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    PmtilesStats => ("pmtiles-stats", Visibility::Only(&[Project::Elivagar, Project::Nidhogg]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Serve => ("serve", Visibility::Only(&[Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Stop => ("stop", Visibility::Only(&[Project::Nidhogg]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Status => ("status", Visibility::Only(&[Project::Nidhogg]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Ingest => ("ingest", Visibility::Only(&[Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Update => ("update", Visibility::Only(&[Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Query => ("query", Visibility::Only(&[Project::Nidhogg]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Geocode => ("geocode", Visibility::Only(&[Project::Nidhogg]), Required, None, true, crate::measure::MeasureMode::Run, None, &["curl"]),
    Visual => ("visual", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "node"]),
    Test => ("test", Visibility::Any, Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::TEST), &["cargo"]),
    List => ("list", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Approve => ("approve", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Report => ("report", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    VisualStatus => ("visual-status", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Prepare => ("prepare", Visibility::Only(&[Project::Litehtml]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "node"]),
    HtmlExtract => ("html-extract", Visibility::Only(&[Project::Litehtml]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Outline => ("outline", Visibility::Only(&[Project::Litehtml]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    Hotpath => ("hotpath", Visibility::Only(&[Project::Sluggrs]), Required, Blocking, true, crate::measure::MeasureMode::Hotpath { runs: std::num::NonZeroUsize::MIN }, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Dellingr => ("dellingr", Visibility::Only(&[Project::Dellingr]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Mogwai => ("mogwai", Visibility::Only(&[Project::Mogwai]), Required, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Service => ("service", Visibility::Only(&[Project::Ratatoskr]), Required, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Sync => ("sync", Visibility::Only(&[Project::Ratatoskr]), Required, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    MockServe => ("mock-serve", Visibility::Only(&[Project::Ratatoskr]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    Corpus => ("corpus", Visibility::Only(&[Project::Piners]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    CorpusResults => ("corpus-results", Visibility::Only(&[Project::Piners]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
    LintCorpus => ("lint-corpus", Visibility::Only(&[Project::Piners]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
    LintResults => ("lint-results", Visibility::Only(&[Project::Piners]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
}

impl super::Command {
    pub(crate) fn id(&self) -> CommandId {
        match self {
            Self::Check { .. } => CommandId::Check,
            Self::Clippy { .. } => CommandId::Clippy,
            Self::Fmt { .. } => CommandId::Fmt,
            Self::Run { .. } => CommandId::Run,
            Self::Install { .. } => CommandId::Install,
            Self::Wc { .. } => CommandId::Wc,
            Self::Man { .. } => CommandId::Man,
            Self::Bench { .. } => CommandId::Bench,
            Self::Deps { .. } => CommandId::Deps,
            Self::Env => CommandId::Env,
            Self::Inspect { .. } => CommandId::Inspect,
            Self::CheckRefs { .. } => CommandId::CheckRefs,
            Self::CheckIds { .. } => CommandId::CheckIds,
            Self::Sort { .. } => CommandId::Sort,
            Self::Cat { .. } => CommandId::Cat,
            Self::TagsFilter { .. } => CommandId::TagsFilter,
            Self::Getid { .. } => CommandId::Getid,
            Self::Getparents { .. } => CommandId::Getparents,
            Self::Renumber { .. } => CommandId::Renumber,
            Self::MergeChanges { .. } => CommandId::MergeChanges,
            Self::ApplyChanges { .. } => CommandId::ApplyChanges,
            Self::AddLocationsToWays { .. } => CommandId::AddLocationsToWays,
            Self::Repack { .. } => CommandId::Repack,
            Self::Degrade { .. } => CommandId::Degrade,
            Self::MultiExtract { .. } => CommandId::MultiExtract,
            Self::TimeFilter { .. } => CommandId::TimeFilter,
            Self::Diff { .. } => CommandId::Diff,
            Self::DiffSnapshots { .. } => CommandId::DiffSnapshots,
            Self::BuildGeocodeIndex { .. } => CommandId::BuildGeocodeIndex,
            Self::Extract { .. } => CommandId::Extract,
            Self::Read { .. } => CommandId::Read,
            Self::Write { .. } => CommandId::Write,
            Self::MergeBench { .. } => CommandId::MergeBench,
            Self::Tilegen { .. } => CommandId::Tilegen,
            Self::PmtilesWriter { .. } => CommandId::PmtilesWriter,
            Self::NodeStore { .. } => CommandId::NodeStore,
            Self::ElivPlanetiler { .. } => CommandId::ElivPlanetiler,
            Self::ElivTilemaker { .. } => CommandId::ElivTilemaker,
            Self::RunApi { .. } => CommandId::RunApi,
            Self::RunNidIngest { .. } => CommandId::RunNidIngest,
            Self::RunTiles { .. } => CommandId::RunTiles,
            Self::GenericHotpath { .. } => CommandId::GenericHotpath,
            Self::Suite { .. } => CommandId::Suite,
            Self::Passthrough { .. } => CommandId::Passthrough,
            Self::Results { .. } => CommandId::Results,
            Self::Sidecar { .. } => CommandId::Sidecar,
            Self::Clean { .. } => CommandId::Clean,
            Self::Lock => CommandId::Lock,
            Self::Strays { .. } => CommandId::Strays,
            Self::Guard { .. } => CommandId::Guard,
            Self::Kill { .. } => CommandId::Kill,
            Self::History { .. } => CommandId::History,
            Self::Invalidate { .. } => CommandId::Invalidate,
            Self::Verify { .. } => CommandId::Verify,
            Self::Download { .. } => CommandId::Download,
            Self::CompareTiles { .. } => CommandId::CompareTiles,
            Self::DownloadOcean => CommandId::DownloadOcean,
            Self::OceanBuild { .. } => CommandId::OceanBuild,
            Self::DownloadNaturalEarth => CommandId::DownloadNaturalEarth,
            Self::PmtilesInspect { .. } => CommandId::PmtilesInspect,
            Self::Diag { .. } => CommandId::Diag,
            Self::Svg { .. } => CommandId::Svg,
            Self::Regress { .. } => CommandId::Regress,
            Self::PmtilesCorpus { .. } => CommandId::PmtilesCorpus,
            Self::PmtilesStats { .. } => CommandId::PmtilesStats,
            Self::Serve { .. } => CommandId::Serve,
            Self::Stop => CommandId::Stop,
            Self::Status => CommandId::Status,
            Self::Ingest { .. } => CommandId::Ingest,
            Self::Update { .. } => CommandId::Update,
            Self::Query { .. } => CommandId::Query,
            Self::Geocode { .. } => CommandId::Geocode,
            Self::Visual { .. } => CommandId::Visual,
            Self::Test { .. } => CommandId::Test,
            Self::List => CommandId::List,
            Self::Approve { .. } => CommandId::Approve,
            Self::Report { .. } => CommandId::Report,
            Self::VisualStatus => CommandId::VisualStatus,
            Self::Prepare { .. } => CommandId::Prepare,
            Self::HtmlExtract { .. } => CommandId::HtmlExtract,
            Self::Outline { .. } => CommandId::Outline,
            Self::Hotpath { .. } => CommandId::Hotpath,
            Self::Dellingr { .. } => CommandId::Dellingr,
            Self::Mogwai { .. } => CommandId::Mogwai,
            Self::Service { .. } => CommandId::Service,
            Self::Sync { .. } => CommandId::Sync,
            Self::MockServe { .. } => CommandId::MockServe,
            Self::Corpus { .. } => CommandId::Corpus,
            Self::CorpusResults { .. } => CommandId::CorpusResults,
            Self::LintCorpus { .. } => CommandId::LintCorpus,
            Self::LintResults { .. } => CommandId::LintResults,
        }
    }
    pub(crate) fn uses_worktree(&self) -> bool {
        match self {
            Self::Check { .. } => false,
            Self::Clippy { .. } => false,
            Self::Fmt { .. } => false,
            Self::Run { commit, .. } => commit.is_some(),
            Self::Install { .. } => false,
            Self::Wc { .. } => false,
            Self::Man { .. } => false,
            Self::Bench { commit, .. } => commit.is_some(),
            Self::Deps { .. } => false,
            Self::Env => false,
            Self::Inspect { mode, .. } => mode.commit.is_some(),
            Self::CheckRefs { mode, .. } => mode.commit.is_some(),
            Self::CheckIds { mode, .. } => mode.commit.is_some(),
            Self::Sort { mode, .. } => mode.commit.is_some(),
            Self::Cat { mode, .. } => mode.commit.is_some(),
            Self::TagsFilter { mode, .. } => mode.commit.is_some(),
            Self::Getid { mode, .. } => mode.commit.is_some(),
            Self::Getparents { mode, .. } => mode.commit.is_some(),
            Self::Renumber { mode, .. } => mode.commit.is_some(),
            Self::MergeChanges { mode, .. } => mode.commit.is_some(),
            Self::ApplyChanges { mode, .. } => mode.commit.is_some(),
            Self::AddLocationsToWays { mode, .. } => mode.commit.is_some(),
            Self::Repack { mode, .. } => mode.commit.is_some(),
            Self::Degrade { mode, .. } => mode.commit.is_some(),
            Self::MultiExtract { mode, .. } => mode.commit.is_some(),
            Self::TimeFilter { mode, .. } => mode.commit.is_some(),
            Self::Diff { mode, .. } => mode.commit.is_some(),
            Self::DiffSnapshots { mode, .. } => mode.commit.is_some(),
            Self::BuildGeocodeIndex { mode, .. } => mode.commit.is_some(),
            Self::Extract { mode, .. } => mode.commit.is_some(),
            Self::Read { mode, .. } => mode.commit.is_some(),
            Self::Write { mode, .. } => mode.commit.is_some(),
            Self::MergeBench { mode, .. } => mode.commit.is_some(),
            Self::Tilegen { mode, .. } => mode.commit.is_some(),
            Self::PmtilesWriter { mode, .. } => mode.commit.is_some(),
            Self::NodeStore { mode, .. } => mode.commit.is_some(),
            Self::ElivPlanetiler { mode, .. } => mode.commit.is_some(),
            Self::ElivTilemaker { mode, .. } => mode.commit.is_some(),
            Self::RunApi { mode, .. } => mode.commit.is_some(),
            Self::RunNidIngest { mode, .. } => mode.commit.is_some(),
            Self::RunTiles { mode, .. } => mode.commit.is_some(),
            Self::GenericHotpath { mode, .. } => mode.commit.is_some(),
            Self::Suite { mode, .. } => mode.commit.is_some(),
            Self::Passthrough { .. } => false,
            Self::Results { .. } => false,
            Self::Sidecar { .. } => false,
            Self::Clean { .. } => false,
            Self::Lock => false,
            Self::Strays { .. } => false,
            Self::Guard { .. } => false,
            Self::Kill { .. } => false,
            Self::History { .. } => false,
            Self::Invalidate { .. } => false,
            Self::Verify { commit, .. } => commit.is_some(),
            Self::Download { .. } => false,
            Self::CompareTiles { .. } => false,
            Self::DownloadOcean => false,
            Self::OceanBuild { .. } => false,
            Self::DownloadNaturalEarth => false,
            Self::PmtilesInspect { .. } => false,
            Self::Diag { .. } => false,
            Self::Svg { .. } => false,
            Self::Regress { .. } => false,
            Self::PmtilesCorpus { .. } => false,
            Self::PmtilesStats { .. } => false,
            Self::Serve { .. } => false,
            Self::Stop => false,
            Self::Status => false,
            Self::Ingest { .. } => false,
            Self::Update { .. } => false,
            Self::Query { .. } => false,
            Self::Geocode { .. } => false,
            Self::Visual { .. } => false,
            Self::Test { .. } => false,
            Self::List => false,
            Self::Approve { .. } => false,
            Self::Report { .. } => false,
            Self::VisualStatus => false,
            Self::Prepare { .. } => false,
            Self::HtmlExtract { .. } => false,
            Self::Outline { .. } => false,
            Self::Hotpath { mode, .. } => mode.commit.is_some(),
            Self::Dellingr { mode, .. } => mode.commit.is_some(),
            Self::Mogwai { mode, .. } => mode.commit.is_some(),
            Self::Service { .. } => false,
            Self::Sync { commit, .. } => commit.is_some(),
            Self::MockServe { .. } => false,
            Self::Corpus { mode, .. } => mode.commit.is_some(),
            Self::CorpusResults { .. } => false,
            Self::LintCorpus { .. } => false,
            Self::LintResults { .. } => false,
        }
    }
    pub(crate) fn spec(&self) -> &'static CommandSpec {
        self.id().spec()
    }
}

static INVOCATION: std::sync::OnceLock<CommandId> = std::sync::OnceLock::new();
pub(crate) fn set_invocation(command: CommandId) {
    // The first installed command wins; a second install is a no-op.
    INVOCATION.set(command).ok();
}
pub(crate) fn invocation() -> CommandId {
    *INVOCATION
        .get()
        .expect("command policy is installed before dispatch")
}

pub(crate) fn visible_to(vis: &Visibility, project: Project) -> bool {
    let member = |projects: &[Project]| {
        projects
            .iter()
            .any(|p| std::mem::discriminant(p) == std::mem::discriminant(&project))
    };
    match vis {
        Visibility::Any => true,
        Visibility::Only(projects) => member(projects),
        Visibility::Except(projects) => !member(projects),
    }
}
impl CommandSpec {
    pub(crate) fn require(&self, project: Project) -> Result<(), crate::error::DevError> {
        if visible_to(&self.applies, project) {
            return Ok(());
        }
        let expected = match self.applies {
            Visibility::Only(projects) => projects
                .iter()
                .map(|p| p.name())
                .collect::<Vec<_>>()
                .join(" and "),
            // Rendered as "only available in <expected> projects".
            Visibility::Except(projects) => format!(
                "non-{}",
                projects
                    .iter()
                    .map(|p| p.name())
                    .collect::<Vec<_>>()
                    .join("/non-")
            ),
            Visibility::Any => return Ok(()),
        };
        Err(crate::error::DevError::WrongProject {
            command: self.name.to_owned(),
            expected,
            current: project.to_string(),
        })
    }
    pub(crate) fn help_prefix(&self) -> String {
        match self.applies {
            Visibility::Only(projects) => format!(
                "[{}] ",
                projects
                    .iter()
                    .map(|p| p.name())
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            Visibility::Any | Visibility::Except(_) => String::new(),
        }
    }
}

/// Tool probes for an environment report follow the applicable commands.
pub(crate) fn tools_in(project: Project) -> Vec<&'static str> {
    let mut tools: Vec<_> = COMMANDS
        .iter()
        .filter(|id| visible_to(&id.spec().applies, project))
        .flat_map(|id| id.spec().tools.iter().copied())
        .collect();
    tools.sort_unstable();
    tools.dedup();
    tools
}
