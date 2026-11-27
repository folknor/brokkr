//! `brokkr serve`: long-lived dev servers that do not hold the global lock.
//!
//! A start runs under the dispatch hold: it builds the program (and a `--with`
//! target) as snapshots, replaces a live generation of the same identity
//! (state root plus name), publishes a new generation in the per-user
//! registry (`registry`), spawns a detached brokkr supervisor that owns the
//! server (`supervisor`), and returns, releasing the hold. From then on the
//! generation is a bench-lock: every measurement refuses while one is
//! outstanding ([`refuse_measurement`]), every other locked command only
//! warns ([`warn_if_live`]), and listing and stopping take no lock at all.
//! The snapshots are linked into the generation directory
//! `.brokkr/serve/NAME.TOKEN/` under the state root.
//!
//! Every reader that acts on the registry reconciles it first: a free entry
//! is SIGKILLed down by carrier scan and retired once none is left, an entry
//! from a positively earlier boot retires unscanned, and another namespace's
//! entry, or one whose scope can't be read, is unknown and blocks
//! (`registry::scope_of`). [`lock_report`] and [`kill_report`] are the
//! `brokkr lock` and `brokkr kill` lines.
//!
//! # What is guaranteed
//!
//! While a generation's supervisor executes its protocol, the generation's
//! flock is held and every measurement refuses; the supervisor gives the
//! flock up only after its server has been reaped, its process group
//! SIGKILLed and its carriers swept. That part is strict: measurement checks
//! run under the global hold, and generations are published only under it.
//!
//! After the supervisor is gone - it exited, or something SIGKILLed it - the
//! generation stays outstanding until a carrier scan in its namespace finds
//! none of its processes (`carriers`), and every process the scan finds is
//! SIGKILLed. That part is best effort, conservative on every uncertainty it
//! can see: a process that drops `BROKKR_SERVE`, turns undumpable, leaves the
//! namespace, or is forked unobserved while a scan passes escapes it. A
//! kernel-contained generation (its own delegated cgroup) is what would make
//! it strict.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::DevError;
use crate::launch::probed::{self, Program};
use crate::lockfile::RunToken;
use crate::output;

mod carriers;
mod registry;
mod supervisor;

pub(crate) use carriers::SERVE_ENV;
pub(crate) use supervisor::maybe_run as maybe_run_supervisor;

use carriers::{Carrier, Scan};
use registry::{Entry, Intent, Phase, State};

/// The generation directory's slot for the served program.
const PROGRAM_SLOT: &str = "program";
/// The generation directory's slot for the `--with` executable.
const WITH_SLOT: &str = "with";
/// The generation directory's log: the server's and the supervisor's output.
const LOG_FILE: &str = "log";
/// A private file with the complete environment for this generation.
const LAUNCH_ENV_FILE: &str = "launch.env";

/// The default SIGTERM grace a server gets before its group is SIGKILLed.
pub(crate) const DEFAULT_GRACE: Duration = Duration::from_secs(5);
/// The default `--ready` wait.
pub(crate) const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a start waits for the supervisor to report its server spawned.
const START_BUDGET: Duration = Duration::from_secs(15);
/// How long a stop waits for a starting supervisor to publish its identity.
const STATE_WAIT: Duration = Duration::from_secs(5);
/// What a stop allows the supervisor beyond the server's grace: the group
/// SIGKILL, the reap and its own carrier sweep.
const STOP_MARGIN: Duration = Duration::from_secs(10);
/// How long a stop waits after SIGKILLing a supervisor that would not exit.
const KILL_WAIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(50);

/// Where one outstanding generation stands.
#[derive(Debug)]
pub(crate) enum Standing {
    /// Its flock is held but its supervisor has published nothing yet.
    Starting,
    /// Its supervisor executes the protocol.
    Supervised(State),
    /// Its supervisor is gone and these carriers are still visible.
    Abandoned(Vec<Carrier>),
    /// Nothing can be established about it from here.
    Unknown(String),
}

/// An outstanding generation: one whose responsibility is not retired.
#[derive(Debug)]
pub(crate) struct Outstanding {
    token: RunToken,
    intent: Option<Intent>,
    standing: Standing,
}

impl Outstanding {
    fn name(&self) -> String {
        match &self.intent {
            Some(i) => format!("{}/{}", i.project, i.name),
            None => format!("(unreadable entry {})", registry::short_id(&self.token)),
        }
    }

    fn short_id(&self) -> &str {
        registry::short_id(&self.token)
    }

    fn describe(&self) -> String {
        let standing = match &self.standing {
            Standing::Starting => "starting".to_owned(),
            Standing::Supervised(state) => match (state.phase, state.server) {
                (Phase::Serving, Some(server)) => format!("serving, PID {}", server.pid().get()),
                (Phase::Ended, _) => "stopping".to_owned(),
                _ => "starting".to_owned(),
            },
            Standing::Abandoned(left) => format!(
                "supervisor gone, {} left ({})",
                output::count(left.len(), "process"),
                left.iter()
                    .map(|c| format!("{} {}", c.comm, c.identity.pid().get()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Standing::Unknown(why) => format!("unknown: {why}"),
        };
        format!("{} [{}] {standing}", self.name(), self.short_id())
    }
}

fn registry_dir() -> Result<PathBuf, DevError> {
    registry::dir().ok_or_else(|| {
        DevError::Config("$HOME is not set or is empty - cannot locate the serve registry".into())
    })
}

fn io(context: &str) -> impl FnOnce(std::io::Error) -> DevError + '_ {
    move |e| DevError::Config(format!("{context}: {e}"))
}

/// Every generation whose responsibility is not retired, reconciling the
/// abandoned ones on the way: a generation whose supervisor is gone has its
/// visible carriers SIGKILLed and is retired once a scan in its namespace
/// finds none. A generation from an earlier boot is retired outright.
fn outstanding(dir: &Path) -> Result<Vec<Outstanding>, DevError> {
    collect(dir, Reconcile::Act)
}

/// Whether reading the registry also acts on it: SIGKILLing an abandoned
/// generation's carriers and retiring what is quiescent. `brokkr lock`
/// only looks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reconcile {
    Act,
    Look,
}

fn collect(dir: &Path, mode: Reconcile) -> Result<Vec<Outstanding>, DevError> {
    let mut out = Vec::new();
    for entry in registry::entries(dir).map_err(io("cannot read the serve registry"))? {
        if let Some(found) = reconcile(dir, entry, mode)? {
            out.push(found);
        }
    }
    Ok(out)
}

fn reconcile(dir: &Path, entry: Entry, mode: Reconcile) -> Result<Option<Outstanding>, DevError> {
    let Entry {
        token,
        held,
        intent,
        state,
    } = entry;
    if held {
        let standing = state.map_or(Standing::Starting, Standing::Supervised);
        return Ok(Some(Outstanding {
            token,
            intent,
            standing,
        }));
    }
    // A free entry whose record is incomplete never had a supervisor: the
    // record is written in full before the supervisor is spawned.
    let Some(record) = intent.as_ref() else {
        if mode == Reconcile::Act {
            retire(dir, &token, None)?;
        }
        return Ok(None);
    };
    match registry::scope_of(record) {
        registry::Scope::Here => {}
        registry::Scope::EarlierBoot => {
            if mode == Reconcile::Act {
                retire(dir, &token, Some(record))?;
            }
            return Ok(None);
        }
        registry::Scope::Unknown => {
            return Ok(Some(Outstanding {
                token,
                intent,
                standing: Standing::Unknown(
                    "started in another PID namespace, or its scope cannot be read here; its \
                     processes cannot be checked from here"
                        .into(),
                ),
            }));
        }
    }
    let scanned = match mode {
        Reconcile::Act => carriers::sweep(token.as_str(), carriers::Exempt::Family),
        Reconcile::Look => carriers::scan(token.as_str(), carriers::Exempt::Family),
    };
    let standing = match scanned {
        Scan::Known(left) if left.is_empty() && mode == Reconcile::Look => return Ok(None),
        Scan::Known(left) if left.is_empty() => {
            if retire(dir, &token, Some(record))? {
                return Ok(None);
            }
            Standing::Starting
        }
        Scan::Known(left) => Standing::Abandoned(left),
        Scan::Unknown(why) => Standing::Unknown(why),
    };
    Ok(Some(Outstanding {
        token,
        intent,
        standing,
    }))
}

fn retire(dir: &Path, token: &RunToken, intent: Option<&Intent>) -> Result<bool, DevError> {
    registry::retire(dir, token, intent).map_err(io("cannot retire a serve entry"))
}

/// Refuse a measurement while any generation is outstanding. Called at every
/// place a measurement begins - a recorded measured session, a ratatoskr sync
/// bench, a script bench before preflight, a criterion sampling run - always
/// under the global hold, which no serve can start inside, so the answer
/// holds for the measurement's whole hold. A process that itself carries a
/// serve token belongs to a served program and never measures.
pub(crate) fn refuse_measurement(what: &str) -> Result<(), DevError> {
    if std::env::var_os(SERVE_ENV).is_some() {
        return Err(DevError::Refused(format!(
            "{what} refuses inside a served program (this process carries {SERVE_ENV})"
        )));
    }
    crate::lockfile::require_hold(|| {
        DevError::Internal(format!(
            "{what} checked for live serves without holding the brokkr lock"
        ))
    })?;
    let dir = registry_dir()?;
    let live = outstanding(&dir)?;
    if live.is_empty() {
        return Ok(());
    }
    let mut message = format!(
        "{what} refuses while {} {} outstanding - a serving process perturbs measurements:",
        output::count(live.len(), "serve"),
        if live.len() == 1 { "is" } else { "are" }
    );
    for one in &live {
        message.push_str("\n  ");
        message.push_str(&one.describe());
    }
    message.push_str("\nstop it with `brokkr serve --stop NAME`; bare `brokkr serve` lists them");
    Err(DevError::Refused(message))
}

/// Warn, once per process, when a fresh hold finds serves registered: the
/// command proceeds beside them, but a measurement would refuse. Reads the
/// registry only; reconciling is for the commands that act on it.
pub(crate) fn warn_if_live() {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if cfg!(test) || matches!(crate::cli::invocation(), Ok(crate::cli::CommandId::Serve)) {
        return;
    }
    let Some(dir) = registry::dir() else { return };
    let Ok(entries) = registry::entries(&dir) else {
        return;
    };
    if entries.is_empty() || WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let names: Vec<String> = entries
        .iter()
        .map(|e| match &e.intent {
            Some(i) => format!("{}/{}", i.project, i.name),
            None => registry::short_id(&e.token).to_owned(),
        })
        .collect();
    output::warn(&format!(
        "{} registered ({}): this command runs beside them, a measurement would refuse; \
         `brokkr serve` lists them",
        output::count(entries.len(), "serve"),
        names.join(", ")
    ));
}

/// The outstanding generations as `brokkr lock` reports them, one line each.
pub(crate) fn lock_report() -> Vec<String> {
    let Some(dir) = registry::dir() else {
        return Vec::new();
    };
    match collect(&dir, Reconcile::Look) {
        Ok(live) => live.iter().map(Outstanding::describe).collect(),
        Err(e) => vec![format!("serves: {e}")],
    }
}

/// The outstanding generations `brokkr kill` reports as left running: those
/// whose intent `in_scope` accepts (configured, project, state root), one
/// line each naming the full token, since a stop by a listed short id refuses
/// when two tokens share it. Looks only, like [`lock_report`]. A kill never
/// stops an established serve: it cancels commands, and a serve whose start
/// returned is no command in flight.
pub(crate) fn kill_report(in_scope: &dyn Fn(bool, &str, &Path) -> bool) -> Vec<String> {
    let Some(dir) = registry::dir() else {
        return Vec::new();
    };
    match collect(&dir, Reconcile::Look) {
        Ok(live) => live
            .iter()
            .filter(|o| {
                o.intent
                    .as_ref()
                    .is_some_and(|i| in_scope(i.configured, &i.project, &i.root))
            })
            .map(|o| {
                format!(
                    "{} left running; `brokkr serve --stop {}` stops it",
                    o.describe(),
                    o.token.as_str()
                )
            })
            .collect(),
        Err(e) => vec![format!("serves: {e}")],
    }
}

/// Bare `brokkr serve`: every outstanding generation, all projects.
pub(crate) fn cmd_list() -> Result<(), DevError> {
    let dir = registry_dir()?;
    let live = outstanding(&dir)?;
    if live.is_empty() {
        output::serve_msg("no live serves");
        return Ok(());
    }
    output::serve_msg(&format!("{}:", output::count(live.len(), "serve")));
    let now = unix_now();
    for one in &live {
        output::serve_msg(&format!("  {}", one.describe()));
        if let Some(i) = &one.intent {
            output::serve_msg(&format!(
                "    target {}, up {}, root {}",
                i.target,
                output::format_duration(Duration::from_secs(now.saturating_sub(i.launched))),
                crate::output::path(&i.root)
            ));
            output::serve_msg(&format!("    log {}", crate::output::path(&i.log)));
        }
    }
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `brokkr serve --stop [WHICH]`. Bare: the current project's one
/// outstanding generation. WHICH: a name in the current project, or a listed
/// id (any prefix of a token) in any project; a WHICH matching more than one
/// generation refuses. Takes no lock: the generation is resolved once, to a
/// token, and only that token is acted on.
pub(crate) fn cmd_stop(which: Option<&str>, root: Option<&Path>) -> Result<(), DevError> {
    let dir = registry_dir()?;
    let live = outstanding(&dir)?;
    let in_project =
        |o: &&Outstanding| root.is_some_and(|r| o.intent.as_ref().is_some_and(|i| i.root == r));
    let chosen: Vec<&Outstanding> = match which {
        None => live.iter().filter(in_project).collect(),
        Some(w) => live
            .iter()
            .filter(|o| {
                (in_project(o) && o.intent.as_ref().is_some_and(|i| i.name == w))
                    || (w.len() >= 4 && o.token.as_str().starts_with(w))
            })
            .collect(),
    };
    let one = match chosen.as_slice() {
        [one] => *one,
        [] => {
            let what = which.map_or_else(
                || "no live serve in this project".to_owned(),
                |w| format!("no live serve matches '{w}'"),
            );
            return Err(DevError::Refused(with_listing(what, &live)));
        }
        many => {
            let what = format!(
                "{} match; name one by its id",
                output::count(many.len(), "serve")
            );
            let listed: Vec<&Outstanding> = many.to_vec();
            return Err(DevError::Refused(with_listing_refs(what, &listed)));
        }
    };
    stop(&dir, &one.token)?;
    output::serve_msg(&format!("stopped {} [{}]", one.name(), one.short_id()));
    Ok(())
}

fn with_listing(what: String, live: &[Outstanding]) -> String {
    let refs: Vec<&Outstanding> = live.iter().collect();
    with_listing_refs(what, &refs)
}

fn with_listing_refs(mut what: String, live: &[&Outstanding]) -> String {
    if live.is_empty() {
        what.push_str("; no serves are live");
        return what;
    }
    what.push_str("; live serves:");
    for one in live {
        what.push_str("\n  ");
        what.push_str(&one.describe());
    }
    what
}

/// Stop one generation and retire it. Success means its supervisor is gone
/// and a carrier scan in its namespace found nothing left - the best-effort
/// limit the module doc states. Anything less is an error that leaves the
/// generation outstanding.
fn stop(dir: &Path, token: &RunToken) -> Result<(), DevError> {
    let Some(entry) = registry::read_entry(dir, token).map_err(io("cannot read a serve entry"))?
    else {
        return Ok(());
    };
    let Some(intent) = entry.intent.clone() else {
        if entry.held {
            return Err(DevError::Refused(format!(
                "serve {} is still being published; try again",
                registry::short_id(token)
            )));
        }
        // A free entry that never had a supervisor (see `reconcile`).
        retire(dir, token, None)?;
        return Ok(());
    };
    match registry::scope_of(&intent) {
        registry::Scope::Here => {}
        registry::Scope::EarlierBoot => {
            retire(dir, token, Some(&intent))?;
            return Ok(());
        }
        registry::Scope::Unknown => {
            return Err(DevError::Refused(format!(
                "{}/{} was started in another PID namespace, or its scope cannot be read \
                 here; stop it from where it was started",
                intent.project, intent.name
            )));
        }
    }
    if entry.held {
        stop_supervisor(dir, token, &intent)?;
    }
    match carriers::sweep(token.as_str(), carriers::Exempt::Family) {
        Scan::Known(left) if left.is_empty() => {
            if retire(dir, token, Some(&intent))? {
                Ok(())
            } else {
                Err(DevError::Internal(
                    "a stopped serve's flock was taken again".into(),
                ))
            }
        }
        Scan::Known(left) => {
            let own = left.iter().any(|c| c.exempt);
            Err(DevError::Refused(format!(
                "{}/{}: its supervisor is gone but {} still carry its token ({}){}; it stays \
                 outstanding",
                intent.project,
                intent.name,
                output::count(left.len(), "process"),
                left.iter()
                    .map(|c| format!("{} {}", c.comm, c.identity.pid().get()))
                    .collect::<Vec<_>>()
                    .join(", "),
                if own {
                    ", this command among them or its own ancestors - a served program cannot \
                     wait for its own end"
                } else {
                    ""
                }
            )))
        }
        Scan::Unknown(why) => Err(DevError::Refused(format!(
            "{}/{}: its supervisor is gone but what is left cannot be checked: {why}",
            intent.project, intent.name
        ))),
    }
}

/// Ask the supervisor to stop, then make sure it has: SIGTERM through a
/// verified pidfd, its grace plus the margin for its own cleanup, then
/// SIGKILL (parent-death takes the server with it). Returns once the flock is
/// free; an error if it never frees.
fn stop_supervisor(dir: &Path, token: &RunToken, intent: &Intent) -> Result<(), DevError> {
    let start = Instant::now();
    let state = loop {
        let Some(now) =
            registry::read_entry(dir, token).map_err(io("cannot read a serve entry"))?
        else {
            return Ok(());
        };
        if !now.held {
            return Ok(());
        }
        if let Some(state) = now.state {
            break state;
        }
        if start.elapsed() >= STATE_WAIT {
            return Err(DevError::Refused(format!(
                "{}/{} is still starting and has published no supervisor to signal; try again",
                intent.project, intent.name
            )));
        }
        std::thread::sleep(POLL);
    };
    let boot = crate::process::BootId::new(&intent.boot_id);
    let supervisor = crate::process::VerifiedProcess::open(state.supervisor, boot.as_ref()).ok();
    if let Some(process) = &supervisor {
        process.send_signal(libc::SIGTERM).ok();
    }
    if wait_free(dir, token, intent.grace + STOP_MARGIN)? {
        return Ok(());
    }
    if let Some(process) = &supervisor {
        output::serve_msg(&format!(
            "the supervisor of {}/{} did not finish within {}; sending SIGKILL",
            intent.project,
            intent.name,
            output::format_duration(intent.grace + STOP_MARGIN)
        ));
        process.send_signal(libc::SIGKILL).ok();
    }
    if wait_free(dir, token, KILL_WAIT)? {
        return Ok(());
    }
    Err(DevError::Refused(format!(
        "the supervisor of {}/{} (PID {}) still holds its flock; it stays outstanding",
        intent.project,
        intent.name,
        state.supervisor.pid().get()
    )))
}

/// Wait up to `budget` for the entry's flock to free: `Ok(true)` once it has.
fn wait_free(dir: &Path, token: &RunToken, budget: Duration) -> Result<bool, DevError> {
    let start = Instant::now();
    loop {
        match registry::read_entry(dir, token).map_err(io("cannot read a serve entry"))? {
            None => return Ok(true),
            Some(entry) if !entry.held => return Ok(true),
            Some(_) => {}
        }
        if start.elapsed() >= budget {
            return Ok(false);
        }
        std::thread::sleep(POLL);
    }
}

/// What `brokkr serve TARGET` starts.
pub(crate) struct StartRequest<'a> {
    pub(crate) project: &'a str,
    /// Whether `project` is a `brokkr.toml` key ([`Intent::configured`]).
    pub(crate) configured: bool,
    /// The state root: with the name, the identity a start replaces.
    pub(crate) root: &'a Path,
    /// The program's working directory, as for `run`.
    pub(crate) cwd: &'a Path,
    /// `--name`; the target's name when absent.
    pub(crate) name: Option<&'a str>,
    /// Snapshots in the artifact store, with their validated cargo records.
    pub(crate) program: Program,
    pub(crate) with: Option<Program>,
    pub(crate) args: &'a [String],
    pub(crate) grace: Duration,
    pub(crate) ready: Option<&'a str>,
    pub(crate) ready_timeout: Duration,
}

/// Refuse a start from inside a served program before anything is built: its
/// work would leave the generation that owns it.
pub(crate) fn refuse_nested_start() -> Result<(), DevError> {
    if std::env::var_os(SERVE_ENV).is_some() {
        return Err(DevError::Refused(format!(
            "a served program cannot start another serve (this process carries {SERVE_ENV})"
        )));
    }
    Ok(())
}

/// A served instance's name: a cargo-target-shaped word, since it names a
/// directory and is typed back to `--stop`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Start a generation, replacing one of the same identity. Runs under the
/// dispatch hold, which it needs: a generation is published only under it.
pub(crate) fn start(mut req: StartRequest<'_>) -> Result<(), DevError> {
    refuse_nested_start()?;
    crate::lockfile::require_hold(|| {
        DevError::Internal("a serve started without holding the brokkr lock".into())
    })?;
    let target = req
        .program
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| DevError::Internal("the served snapshot has no file name".into()))?
        .to_owned();
    let name = req.name.unwrap_or(&target).to_owned();
    if !valid_name(&name) {
        return Err(DevError::Usage(format!(
            "serve name '{name}' must be 1 to 64 ASCII letters, digits, '-' or '_'"
        )));
    }
    let dir = registry_dir()?;

    // The generation this one replaces, stopped through the same verified
    // path as `--stop`; a stop that cannot finish refuses the start.
    for old in outstanding(&dir)? {
        if old
            .intent
            .as_ref()
            .is_some_and(|i| i.root == req.root && i.name == name)
        {
            output::serve_msg(&format!("replacing {}", old.describe()));
            stop(&dir, &old.token)?;
        }
    }
    let state = crate::clean::layout::ProjectState::new(req.root);
    sweep_retired_generations(&state.serve_dir(), Some(&name));

    let token = RunToken::mint()
        .ok_or_else(|| DevError::Internal("no entropy for a serve token".into()))?;
    let generation = state.serve_dir().join(format!("{name}.{}", token.as_str()));
    let (boot_id, pid_ns, time_ns) = registry::local_scope().ok_or_else(|| {
        DevError::Refused(
            "this host's boot id or this process's PID or time namespace cannot be read, so a \
             serve started here could never be judged gone; refusing to start one"
                .into(),
        )
    })?;
    let mut pending = UnpublishedGeneration::new(&generation)?;
    let (program, with) = prepare_generation(&generation, &token, &mut req)?;
    let log = generation.join(LOG_FILE);
    std::fs::write(&log, b"").map_err(io("cannot create the serve log"))?;
    let intent = Intent {
        token: token.clone(),
        project: req.project.to_owned(),
        configured: req.configured,
        root: req.root.to_owned(),
        name: name.clone(),
        target,
        generation: generation.clone(),
        program,
        with,
        args: req.args.to_vec(),
        cwd: req.cwd.to_owned(),
        log: log.clone(),
        grace: req.grace,
        boot_id,
        pid_ns,
        time_ns,
        launched: unix_now(),
    };
    // Before the supervisor exists: a stop from here on is a request the
    // waits below poll, so it takes the started server down through `stop`
    // rather than killing this process and leaving the supervisor running.
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let flock = registry::publish(&dir, &intent).map_err(io("cannot publish the serve"))?;
    pending.published = true;
    let spawned = spawn_supervisor(&dir, &token, &flock, &log);
    // From here the supervisor, if it was spawned, holds the flock alone.
    drop(flock);
    if let Err(e) = spawned {
        retire(&dir, &token, Some(&intent)).ok();
        return Err(e);
    }

    match wait_started(&dir, &intent)
        .and_then(|server| wait_ready(&dir, &intent, &req).map(|()| server))
    {
        Ok(server) => {
            output::serve_msg(&format!(
                "serving {}/{name} [{}], PID {server}; log {}",
                intent.project,
                intent.short_id(),
                crate::output::path(&log)
            ));
            output::serve_msg(&format!(
                "stop it with `brokkr serve --stop {name}`; measurements refuse until then"
            ));
            Ok(())
        }
        Err(e) => {
            if let Err(stop_error) = stop(&dir, &token) {
                output::warn(&format!(
                    "could not take the failed serve down: {stop_error}"
                ));
            }
            Err(e)
        }
    }
}

/// Until the intent is published, the launcher owns setup and cleanup.
struct UnpublishedGeneration<'a> {
    path: &'a Path,
    published: bool,
}

impl<'a> UnpublishedGeneration<'a> {
    fn new(path: &'a Path) -> Result<Self, DevError> {
        let parent = path.parent().ok_or_else(|| {
            DevError::Internal("the serve generation has no parent directory".into())
        })?;
        std::fs::create_dir_all(parent)?;
        std::fs::create_dir(path)?;
        Ok(Self {
            path,
            published: false,
        })
    }
}

impl Drop for UnpublishedGeneration<'_> {
    fn drop(&mut self) {
        if !self.published
            && let Err(error) = std::fs::remove_dir_all(self.path)
        {
            output::warn(&format!(
                "could not remove unpublished serve generation: {error}"
            ));
        }
    }
}

fn prepare_generation(
    generation: &Path,
    token: &RunToken,
    req: &mut StartRequest<'_>,
) -> Result<(PathBuf, Option<PathBuf>), DevError> {
    let program = place(&req.program.path, &generation.join(PROGRAM_SLOT))?;
    // Both outlive the hold the probes ran under.
    req.program.env.detach();
    let mut additions = vec![(SERVE_ENV.into(), token.as_str().into())];
    let peer = if let Some(mut peer) = req.with.take() {
        peer.path = place(&peer.path, &generation.join(WITH_SLOT))?;
        peer.env.detach();
        peer.env.set(SERVE_ENV, token.as_str());
        let dir = probed::peer_directory(&peer.path)?;
        additions.extend(probed::peer_additions(
            &probed::peer_launcher_path(&dir),
            &peer.path,
        ));
        Some((peer, dir))
    } else {
        None
    };
    let env = req.program.env.with_additions(additions);
    // The saved environment, which every start of this generation's server
    // uses, is the peer plan's base.
    if let Some((peer, dir)) = &peer {
        probed::place_peer(peer, dir, &env)?;
    }
    probed::save_env(&generation.join(LAUNCH_ENV_FILE), &env)?;
    Ok((program, peer.map(|(peer, _)| peer.path)))
}

/// Link `exe` into `slot` (a copy where a hard link is refused), so the
/// served file outlives `brokkr clean` removing the snapshot store.
fn place(exe: &Path, slot: &Path) -> Result<PathBuf, DevError> {
    let file = exe
        .file_name()
        .ok_or_else(|| DevError::Internal("a snapshot has no file name".into()))?;
    std::fs::create_dir_all(slot).map_err(io("cannot create a serve generation directory"))?;
    let dest = slot.join(file);
    if std::fs::hard_link(exe, &dest).is_err() {
        std::fs::copy(exe, &dest).map_err(io("cannot place the served executable"))?;
    }
    Ok(dest)
}

fn spawn_supervisor(
    dir: &Path,
    token: &RunToken,
    flock: &std::fs::File,
    log: &Path,
) -> Result<(), DevError> {
    use std::os::fd::AsRawFd as _;
    let exe = std::env::current_exe().map_err(io("cannot locate the brokkr executable"))?;
    let out = std::fs::OpenOptions::new()
        .append(true)
        .open(log)
        .map_err(io("cannot open the serve log"))?;
    let err = out.try_clone().map_err(io("cannot share the serve log"))?;
    let entry = dir.join(format!("{}.lock", token.as_str()));
    let mut cmd = crate::process::spawn::ChildPolicy::plain_command(&exe);
    cmd.arg(supervisor::SUPERVISOR_ARG)
        .arg(&entry)
        .arg(flock.as_raw_fd().to_string())
        .current_dir(dir)
        .env_remove(SERVE_ENV)
        .env_remove("BROKKR_HARNESS_ARTEFACT_DIR")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    crate::process::spawn::spawn_detached(cmd, flock.as_raw_fd()).map(drop)
}

/// Wait for the supervisor to report its server spawned; the server's PID.
fn wait_started(dir: &Path, intent: &Intent) -> Result<u32, DevError> {
    let start = Instant::now();
    loop {
        if crate::shutdown::is_shutdown_requested() {
            return Err(DevError::Interrupted);
        }
        let entry =
            registry::read_entry(dir, &intent.token).map_err(io("cannot read the serve entry"))?;
        match entry {
            Some(Entry {
                held: true,
                state: Some(state),
                ..
            }) => match (state.phase, state.server) {
                (Phase::Serving, Some(server)) => return Ok(server.pid().get()),
                (Phase::Ended, _) => return Err(not_running(intent, "the server exited at once")),
                _ => {}
            },
            Some(Entry { held: true, .. }) => {}
            Some(Entry {
                state: Some(state), ..
            }) if state.server.is_some() => {
                return Err(not_running(intent, "the server exited at once"));
            }
            _ => return Err(not_running(intent, "the supervisor exited before serving")),
        }
        if start.elapsed() >= START_BUDGET {
            return Err(not_running(
                intent,
                "the supervisor reported no server in time",
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// `--ready TEXT`: wait for TEXT in the generation's log, read from its first
/// byte. A server that buffers its output may print it late or never.
fn wait_ready(dir: &Path, intent: &Intent, req: &StartRequest<'_>) -> Result<(), DevError> {
    let Some(text) = req.ready else {
        return Ok(());
    };
    let start = Instant::now();
    loop {
        if crate::shutdown::is_shutdown_requested() {
            return Err(DevError::Interrupted);
        }
        // The log first, the generation's standing after: the supervisor
        // publishes `Ended` before it appends its closing line, so a match
        // that came from that line is always seen with the ending.
        let found = std::fs::read(&intent.log).is_ok_and(|log| contains(&log, text.as_bytes()));
        let entry =
            registry::read_entry(dir, &intent.token).map_err(io("cannot read the serve entry"))?;
        let ended = match &entry {
            Some(e) => !e.held || e.state.as_ref().is_some_and(|s| s.phase == Phase::Ended),
            None => true,
        };
        if found && !ended {
            return Ok(());
        }
        if ended {
            return Err(not_running(
                intent,
                "the server ended before it printed the ready text",
            ));
        }
        if start.elapsed() >= req.ready_timeout {
            return Err(not_running(
                intent,
                &format!(
                    "'{text}' did not appear in its output within {}",
                    output::format_duration(req.ready_timeout)
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// A failed start, with the end of the generation's log.
fn not_running(intent: &Intent, why: &str) -> DevError {
    let tail = std::fs::read_to_string(&intent.log)
        .map(|log| {
            let lines: Vec<&str> = log.lines().collect();
            lines[lines.len().saturating_sub(20)..].join("\n")
        })
        .unwrap_or_default();
    let mut message = format!(
        "{}/{}: {why}; log {}",
        intent.project,
        intent.name,
        crate::output::path(&intent.log)
    );
    if !tail.is_empty() {
        message.push('\n');
        message.push_str(&tail);
    }
    DevError::Refused(message)
}

/// Remove the generation directories under `serve_dir` whose generation is
/// retired (no registry entry names its token), optionally only those of one
/// name; returns what it removed. A generation still outstanding is never
/// touched, whatever its standing.
fn sweep_retired_generations(serve_dir: &Path, name: Option<&str>) {
    for dir in retired_generations(serve_dir, name) {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            output::warn(&format!(
                "could not remove the ended serve generation {}: {e}",
                crate::output::path(&dir)
            ));
        }
    }
}

/// For `brokkr clean`: reconcile the registry, so a generation whose server
/// ended is retired if nothing of it is left, then name every retired
/// generation's directory under `serve_dir` - what a server that ended left,
/// its log included. A dry run only looks: it kills and retires nothing, so
/// it names only generations retired already.
pub(crate) fn ended_generations(serve_dir: &Path, dry_run: bool) -> Vec<PathBuf> {
    if let Some(dir) = registry::dir()
        && !dry_run
        && let Err(e) = outstanding(&dir)
    {
        output::warn(&format!(
            "could not reconcile the serve registry ({e}); only generations already retired \
             are removed"
        ));
    }
    retired_generations(serve_dir, None)
}

/// The generation directories under `serve_dir` whose generation is retired,
/// optionally only those of one name.
fn retired_generations(serve_dir: &Path, name: Option<&str>) -> Vec<PathBuf> {
    let Some(registry) = registry::dir() else {
        return Vec::new();
    };
    let Ok(listing) = std::fs::read_dir(serve_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in listing.flatten() {
        let file = item.file_name();
        let Some((stem, token)) = file.to_str().and_then(|n| n.rsplit_once('.')) else {
            continue;
        };
        let Some(token) = RunToken::parse(token) else {
            continue;
        };
        if name.is_some_and(|n| n != stem) {
            continue;
        }
        // An entry that cannot be read is not proved retired.
        if matches!(registry::read_entry(&registry, &token), Ok(None)) {
            out.push(item.path());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn names_are_target_shaped_words() {
        assert!(valid_name("nidhogg-serve"));
        assert!(valid_name("a_b1"));
        assert!(!valid_name(""));
        assert!(!valid_name("../x"));
        assert!(!valid_name("a.b"));
        assert!(!valid_name(&"x".repeat(65)));
    }

    #[test]
    fn ready_text_is_a_byte_substring() {
        assert!(contains(b"x\nListening on 0.0.0.0:3033\n", b"Listening on"));
        assert!(!contains(b"Listen", b"Listening"));
    }

    /// A measurement under a hold refuses while a generation's flock is held
    /// and proceeds once it is retired; without a hold it is an internal
    /// error, never a silent pass.
    #[test]
    fn a_measurement_refuses_exactly_while_a_generation_is_outstanding() {
        // Serialised with the script-bench tests, which measure under a hold
        // against the same scratch registry.
        let _serial = crate::test_scratch::process_global_lock();
        assert!(matches!(
            refuse_measurement("a test"),
            Err(DevError::Internal(_))
        ));
        let roots = crate::project::detect_roots().unwrap();
        let _hold = crate::lockfile::test_hold(&roots.0, &roots.1);
        refuse_measurement("a test").unwrap();
        let dir = registry::dir().unwrap();
        let intent = fixture(&dir);
        let held = registry::publish(&dir, &intent).unwrap();
        let refused = refuse_measurement("a test").unwrap_err().to_string();
        assert!(refused.contains("demo/srv"), "{refused}");
        drop(held);
        refuse_measurement("a test").unwrap();
    }

    /// An intent recorded here, now, with this process's scope.
    fn fixture(dir: &Path) -> Intent {
        let (boot_id, pid_ns, time_ns) = registry::local_scope().unwrap();
        Intent {
            token: RunToken::mint().unwrap(),
            project: "demo".into(),
            configured: true,
            root: "/r".into(),
            name: "srv".into(),
            target: "srv".into(),
            generation: dir.join("gen"),
            program: dir.join("gen/program/srv"),
            with: None,
            args: Vec::new(),
            cwd: "/r".into(),
            log: dir.join("gen/log"),
            grace: DEFAULT_GRACE,
            boot_id,
            pid_ns,
            time_ns,
            launched: 1,
        }
    }

    /// A held entry is starting; a free one from an earlier boot retires
    /// unscanned; a free one here with no carrier left retires; a free one
    /// whose scope has a gap stays outstanding, never judged gone.
    #[test]
    fn reconcile_retires_only_what_it_can_judge_gone() {
        let dir = crate::test_scratch::scratch("serve", "reconcile");
        let earlier = Intent {
            boot_id: "00000000-0000-0000-0000-000000000000".into(),
            ..fixture(&dir)
        };
        let held = registry::publish(&dir, &earlier).unwrap();
        let live = outstanding(&dir).unwrap();
        assert_eq!(live.len(), 1);
        assert!(matches!(live[0].standing, Standing::Starting));
        drop(held);
        assert!(
            outstanding(&dir).unwrap().is_empty(),
            "an earlier boot's entry retires"
        );

        drop(registry::publish(&dir, &fixture(&dir)).unwrap());
        assert!(
            outstanding(&dir).unwrap().is_empty(),
            "no carrier of a fresh token is visible, so it retires"
        );

        let gap = Intent {
            pid_ns: String::new(),
            ..fixture(&dir)
        };
        drop(registry::publish(&dir, &gap).unwrap());
        let live = outstanding(&dir).unwrap();
        assert_eq!(live.len(), 1);
        assert!(matches!(live[0].standing, Standing::Unknown(_)));
        assert!(stop(&dir, &gap.token).is_err(), "nor does a stop retire it");
    }
}
