//! Exhaustive command policy shared by dispatch, help and the manual.
//!
//! Each `CommandSpec` records a command's applicability, config need, lock
//! policy, worktree selection, disk gate, default mode, run log and tool
//! probes; `COMMANDS` lists every `CommandId` so registry checks can prove
//! the catalogue complete. Dispatch decides `reads_only` once per invocation
//! and feeds it to `lock_for`, `run_log_for` and tree selection, so a command
//! that only reads (a `brokkr bench --dry-run`, or one that only lists,
//! describes or refuses) takes no lock, writes no log and gets no tree.
//!
//! Applicability hides a command from `--help` but never from the parser: a
//! hidden subcommand still parses, and dispatch refuses it from the same
//! spec before its handler runs. Project prefixes and man topic visibility
//! read the same specs.

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
Serve => ("serve", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Direct),
Build => ("build", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Direct),
Install => ("install", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Wc => ("wc", Visibility::Any, Optional, None, true, crate::measure::MeasureMode::Run, None, &[]),
Man => ("man", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
Crit => ("crit", Visibility::Any, Optional, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::CRIT), &["cargo"], Direct),
Bench => ("bench", Visibility::Any, Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::SCRIPT_BENCH), &["cargo"], Direct),
Deps => ("deps", Visibility::Any, Optional, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Env => ("env", Visibility::Any, Required, None, false, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Passthrough => ("passthrough", Visibility::Any, Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Results => ("results", Visibility::Any, Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
Sidecar => ("sidecar", Visibility::Any, Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
Clean => ("clean", Visibility::Any, Required, Blocking, false, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Lock => ("lock", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
Strays => ("strays", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
Guard => ("guard", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
Fstrim => ("fstrim", Visibility::Any, Optional, Blocking, false, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &[]),
Kill => ("kill", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
History => ("history", Visibility::Any, None, None, true, crate::measure::MeasureMode::Run, None, &[]),
Invalidate => ("invalidate", Visibility::Except(MEASURED_DB_ABSENT), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
Visual => ("visual", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "node"]),
Test => ("test", Visibility::Any, Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::TEST), &["cargo"]),
List => ("list", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
Approve => ("approve", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"]),
Report => ("report", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
VisualStatus => ("visual-status", Visibility::Only(&[Project::Litehtml, Project::Sluggrs]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
Prepare => ("prepare", Visibility::Only(&[Project::Litehtml]), Required, Blocking, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo", "node"]),
HtmlExtract => ("html-extract", Visibility::Only(&[Project::Litehtml]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
Outline => ("outline", Visibility::Only(&[Project::Litehtml]), Required, None, true, crate::measure::MeasureMode::Run, None, &[]),
Mogwai => ("mogwai", Visibility::Only(&[Project::Mogwai]), Required, None, true, crate::measure::MeasureMode::Run, Some(crate::run_log::run_log_kind::MEASURE), &["cargo"], Mode),}

static INVOCATION: std::sync::OnceLock<CommandId> = std::sync::OnceLock::new();
pub(crate) fn set_invocation(command: CommandId) {
    // The first installed command wins; a second install is a no-op.
    INVOCATION.set(command).ok();
}
pub(crate) fn invocation() -> Result<CommandId, crate::error::DevError> {
    INVOCATION.get().copied().ok_or_else(|| {
        crate::error::DevError::Internal("command policy is installed before dispatch".into())
    })
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
    /// The lock this invocation takes: the command's, except where the
    /// invocation only reads config and inputs, which no lock protects
    /// ([`Self::reads_only`] decides, once per invocation). A
    /// `brokkr bench --dry-run` builds and stores nothing, so a queue of them
    /// can be checked while a bench holds the lock, and listing or describing
    /// the benches never waits behind a running check.
    pub(crate) fn lock_for(&self, reads_only: bool) -> LockPolicy {
        if reads_only {
            return LockPolicy::None;
        }
        self.lock
    }

    /// The run log this invocation writes: none where it takes no lock for
    /// [`Self::lock_for`]'s reason, since a log is activated and pruned under
    /// the lock.
    pub(crate) fn run_log_for(&self, reads_only: bool) -> Option<&'static str> {
        if reads_only {
            return None;
        }
        self.run_log
    }

    /// Whether the invocation reads config and inputs alone, never building,
    /// launching or storing: `brokkr bench --dry-run`, `brokkr bench --test`
    /// (whose hosts are virtual), and any `brokkr bench` that only lists,
    /// describes or refuses, decided against the declared
    /// `benches` (`None` before a config is detected, when a bench is
    /// refused anyway). Bare `brokkr serve` and `serve --stop` count too:
    /// they list or signal a serve generation, which the serve registry's
    /// flocks govern and no hold protects, and they build nothing.
    pub(crate) fn reads_only(
        command: &super::Command,
        benches: Option<&std::collections::BTreeMap<String, crate::config::bench::Bench>>,
    ) -> bool {
        match command {
            super::Command::Bench(super::BenchCmd { dry_run: true, .. })
            | super::Command::Bench(super::BenchCmd { test: true, .. }) => true,
            super::Command::Bench(super::BenchCmd { bench, rounds, .. }) => {
                benches.is_none_or(|benches| {
                    !crate::bench::runner::runs(benches, bench, rounds.is_some())
                })
            }
            // Listing and stopping serves: the registry's own flocks govern
            // them, never the global lock (`crate::serve`).
            super::Command::Serve(super::ServeCmd { target, stop, .. }) => {
                target.is_none() || stop.is_some()
            }
            // Listing what install would install reads metadata only.
            super::Command::Install(super::InstallCmd { names: true, .. }) => true,
            _ => false,
        }
    }

    pub(crate) fn uses_worktree(
        &self,
        command: &super::Command,
    ) -> Result<bool, crate::error::DevError> {
        match self.worktree {
            WorktreePolicy::None => Ok(false),
            WorktreePolicy::Mode => {
                let mode = match command {
                    super::Command::Mogwai(super::MogwaiCmd { mode, .. }) => Some(mode),
                    _ => None,
                };
                mode.map(|mode| mode.commit.is_some()).ok_or_else(|| {
                    crate::error::DevError::Internal(
                        "command has mode-based worktree policy but does not carry ModeArgs".into(),
                    )
                })
            }
            WorktreePolicy::Direct => match command {
                // Historical only when no stage builds the live tree: a
                // `--with` target built from the live tree needs the live
                // tree's policy even when the program itself is `--commit`.
                super::Command::Run(super::RunCmd { program, .. })
                | super::Command::Serve(super::ServeCmd { program, .. }) => {
                    Ok(program.commit.is_some()
                        && (program.with.is_none() || program.with_commit.is_some()))
                }
                super::Command::Build(super::BuildCmd { commit, .. })
                | super::Command::Crit(super::CritCmd { commit, .. }) => Ok(commit.is_some()),
                // Every build is a worktree's once one commit is given, the
                // `--rounds` competitors included.
                super::Command::Bench(super::BenchCmd { commit, .. }) => Ok(!commit.is_empty()),
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
