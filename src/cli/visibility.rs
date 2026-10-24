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

#[derive(Clone, Copy)]
pub(crate) enum WorktreePolicy {
    None,
    Mode,
    Direct,
}

macro_rules! worktree_policy {
    () => {
        WorktreePolicy::None
    };
    (Mode) => {
        WorktreePolicy::Mode
    };
    (Direct) => {
        WorktreePolicy::Direct
    };
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
    pub worktree: WorktreePolicy,
}

macro_rules! commands {
    ($($variant:ident => ($name:literal, $applies:expr, $config:ident, $lock:ident, $gate:literal, $mode:expr, $log:expr, $tools:expr $(, $worktree:ident)?)),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum CommandId { $($variant),* }
        impl CommandId {
            pub(crate) fn spec(self) -> &'static CommandSpec {
                match self { $(Self::$variant => &CommandSpec {
                    name: $name, applies: $applies, config: ConfigNeed::$config,
                    lock: LockPolicy::$lock, disk_gate: $gate,
                    default_mode: $mode, run_log: $log, tools: $tools,
                    worktree: worktree_policy!($($worktree)?),
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
Run => ("run", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Direct),
Build => ("build", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Direct),
Install => ("install", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Wc => ("wc", Visibility::Any, Optional, None, true, crate::measure::MeasureMode::Run, None, &[]),
Man => ("man", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
Bench => ("bench", Visibility::Any, Optional, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::BENCH), &["cargo"], Direct),
Deps => ("deps", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Env => ("env", Visibility::Any, Required, None, false, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Inspect => ("inspect", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
CheckRefs => ("check-refs", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
CheckIds => ("check-ids", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Sort => ("sort", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Cat => ("cat", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
TagsFilter => ("tags-filter", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Getid => ("getid", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Getparents => ("getparents", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Renumber => ("renumber", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
MergeChanges => ("merge-changes", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
ApplyChanges => ("apply-changes", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
AddLocationsToWays => ("add-locations-to-ways", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Repack => ("repack", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Degrade => ("degrade", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
MultiExtract => ("multi-extract", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
TimeFilter => ("time-filter", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Diff => ("diff", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
DiffSnapshots => ("diff-snapshots", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
BuildGeocodeIndex => ("build-geocode-index", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Extract => ("extract", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Read => ("read", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Write => ("write", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
MergeBench => ("merge", Visibility::Only(&[Project::Pbfhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
Tilegen => ("tilegen", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
PmtilesWriter => ("pmtiles-writer", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
NodeStore => ("node-store", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
ElivPlanetiler => ("planetiler", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "java"], Mode),
ElivTilemaker => ("tilemaker", Visibility::Only(&[Project::Elivagar]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "cmake", "g++", "make"], Mode),
RunApi => ("api", Visibility::Only(&[Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
RunNidIngest => ("nid-ingest", Visibility::Only(&[Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
RunTiles => ("tiles", Visibility::Only(&[Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
GenericHotpath => ("generic-hotpath", Visibility::Any, Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
Suite => ("suite", Visibility::Only(&[Project::Pbfhogg, Project::Elivagar, Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Mode),
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
Verify => ("verify", Visibility::Only(&[Project::Pbfhogg, Project::Elivagar, Project::Nidhogg]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "osmium"], Direct),
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
Hotpath => ("hotpath", Visibility::Only(&[Project::Sluggrs]), Required, Blocking, true, crate::measure::MeasureMode::Hotpath { runs: std::num::NonZeroUsize::MIN }, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
Dellingr => ("dellingr", Visibility::Only(&[Project::Dellingr]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
Mogwai => ("mogwai", Visibility::Only(&[Project::Mogwai]), Required, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
Service => ("service", Visibility::Only(&[Project::Ratatoskr]), Required, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Sync => ("sync", Visibility::Only(&[Project::Ratatoskr]), Required, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Direct),
MockServe => ("mock-serve", Visibility::Only(&[Project::Ratatoskr]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Corpus => ("corpus", Visibility::Only(&[Project::Piners]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),
CorpusResults => ("corpus-results", Visibility::Only(&[Project::Piners]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),}

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

pub(crate) fn visible_to(vis: &Visibility, project: &Project) -> bool {
    let member = |projects: &[Project]| {
        projects
            .iter()
            .any(|p| std::mem::discriminant(p) == std::mem::discriminant(project))
    };
    match vis {
        Visibility::Any => true,
        Visibility::Only(projects) => member(projects),
        Visibility::Except(projects) => !member(projects),
    }
}
impl CommandSpec {
    pub(crate) fn uses_worktree(
        &self,
        command: &super::Command,
    ) -> Result<bool, crate::error::DevError> {
        match self.worktree {
            WorktreePolicy::None => Ok(false),
            WorktreePolicy::Mode => {
                let mode = match command {
                    super::Command::Inspect { mode, .. }
                    | super::Command::CheckRefs { mode, .. }
                    | super::Command::CheckIds { mode, .. }
                    | super::Command::Sort { mode, .. }
                    | super::Command::Cat { mode, .. }
                    | super::Command::TagsFilter { mode, .. }
                    | super::Command::Getid { mode, .. }
                    | super::Command::Getparents { mode, .. }
                    | super::Command::Renumber { mode, .. }
                    | super::Command::MergeChanges { mode, .. }
                    | super::Command::ApplyChanges { mode, .. }
                    | super::Command::AddLocationsToWays { mode, .. }
                    | super::Command::Repack { mode, .. }
                    | super::Command::Degrade { mode, .. }
                    | super::Command::MultiExtract { mode, .. }
                    | super::Command::TimeFilter { mode, .. }
                    | super::Command::Diff { mode, .. }
                    | super::Command::DiffSnapshots { mode, .. }
                    | super::Command::BuildGeocodeIndex { mode, .. }
                    | super::Command::Extract { mode, .. }
                    | super::Command::Read { mode, .. }
                    | super::Command::Write { mode, .. }
                    | super::Command::MergeBench { mode, .. }
                    | super::Command::Tilegen { mode, .. }
                    | super::Command::PmtilesWriter { mode, .. }
                    | super::Command::NodeStore { mode, .. }
                    | super::Command::ElivPlanetiler { mode, .. }
                    | super::Command::ElivTilemaker { mode, .. }
                    | super::Command::RunApi { mode, .. }
                    | super::Command::RunNidIngest { mode, .. }
                    | super::Command::RunTiles { mode, .. }
                    | super::Command::GenericHotpath { mode, .. }
                    | super::Command::Suite { mode, .. }
                    | super::Command::Hotpath { mode, .. }
                    | super::Command::Dellingr { mode, .. }
                    | super::Command::Mogwai { mode, .. }
                    | super::Command::Corpus { mode, .. } => Some(mode),
                    _ => None,
                };
                mode.map(|mode| mode.commit.is_some()).ok_or_else(|| {
                    crate::error::DevError::Internal(
                        "command has mode-based worktree policy but does not carry ModeArgs".into(),
                    )
                })
            }
            WorktreePolicy::Direct => match command {
                super::Command::Run { commit, .. }
                | super::Command::Build { commit, .. }
                | super::Command::Bench { commit, .. }
                | super::Command::Verify { commit, .. }
                | super::Command::Sync { commit, .. } => Ok(commit.is_some()),
                _ => Err(crate::error::DevError::Internal(
                    "command has direct worktree policy but carries no direct commit field".into(),
                )),
            },
        }
    }

    pub(crate) fn require(&self, project: &Project) -> Result<(), crate::error::DevError> {
        if visible_to(&self.applies, project) {
            return Ok(());
        }
        let expected = match self.applies {
            Visibility::Only(projects) => projects
                .iter()
                .map(Project::name)
                .collect::<Vec<_>>()
                .join(" and "),
            // Rendered as "only available in <expected> projects".
            Visibility::Except(projects) => format!(
                "non-{}",
                projects
                    .iter()
                    .map(Project::name)
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
                    .map(Project::name)
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            Visibility::Any | Visibility::Except(_) => String::new(),
        }
    }
}

/// Tool probes for an environment report follow the applicable commands.
pub(crate) fn tools_in(project: &Project) -> Vec<&'static str> {
    let mut tools: Vec<_> = COMMANDS
        .iter()
        .filter(|id| visible_to(&id.spec().applies, project))
        .flat_map(|id| id.spec().tools.iter().copied())
        .collect();
    tools.sort_unstable();
    tools.dedup();
    tools
}
