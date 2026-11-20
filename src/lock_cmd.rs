//! Lock status and termination commands.

use crate::error::{DevError, LockError};
use crate::lockfile;
use crate::output;

pub(crate) fn cmd_lock() -> Result<(), DevError> {
    let holder = holder_status();
    queue_status();
    // The serves hold no lock but are the other thing a measurement waits
    // on: they make it refuse (`crate::serve`).
    for line in crate::serve::lock_report() {
        output::lock_msg(&format!("serve: {line}"));
    }
    holder
}

/// What a holder or a queued command was asked to do, as `brokkr lock` and
/// `brokkr kill` name it: the project key and the invocation, or the
/// invocation marked as having no `brokkr.toml`.
fn invocation_label(configured: bool, project: &str, command: &str, args: &str) -> String {
    let what = if args.is_empty() { command } else { args };
    if configured {
        format!("{project} {what}")
    } else {
        format!("(no brokkr.toml) {what}")
    }
}

/// One queued command, as `brokkr lock` and `brokkr kill` name it. Its PID
/// only where it means the same thing here as to the waiter.
fn waiter_label(record: &lockfile::queue::WaiterRecord) -> String {
    let mut label = invocation_label(
        record.configured,
        &record.project,
        &record.command,
        &record.args,
    );
    let waited = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(record.joined);
    let pid = if crate::process::shares_namespace_tokens(&record.pid_ns, &record.time_ns) {
        format!("PID {}, ", record.pid)
    } else {
        String::new()
    };
    label.push_str(&format!(
        " ({pid}waiting {}, root {})",
        output::format_duration(std::time::Duration::from_secs(waited)),
        record.root
    ));
    label
}

/// `brokkr lock`'s queue lines, oldest first. Reads only: a dead waiter's
/// leftover is not listed, and is swept by the next waiter or kill.
fn queue_status() {
    let Some(dir) = lockfile::queue_dir() else {
        return;
    };
    match lockfile::queue::list(&dir) {
        Ok(entries) => {
            for entry in entries {
                let state = match entry.standing {
                    lockfile::queue::Standing::Waiting => "queued",
                    lockfile::queue::Standing::Cancelling => "queued, cancelled",
                };
                match &entry.record {
                    Ok(record) => output::lock_msg(&format!("{state}: {}", waiter_label(record))),
                    Err(why) => output::lock_msg(&format!(
                        "{state}: a waiter whose record cannot be read ({why})"
                    )),
                }
            }
        }
        Err(e) => output::lock_msg(&format!("the lock queue cannot be read: {e}")),
    }
}

fn holder_status() -> Result<(), DevError> {
    let Some(info) = lockfile::status()? else {
        output::lock_msg("no active lock");
        return Ok(());
    };

    let invocation = invocation_label(info.configured, &info.project, &info.command, &info.args);

    // First ask the holder itself, over its control socket. It answers about
    // its own process, so no PID crosses a namespace boundary - this works
    // from the host, from a sibling sandbox, from anywhere the socket is.
    let mut unanswered = "it records no control socket (an older brokkr)".to_owned();
    if !info.acq_id.is_empty() {
        match crate::lock_service::request(&info.acq_id, "status") {
            crate::lock_service::Reply::Status(s) => {
                let s = fresher_status(&info.acq_id, s);
                print_holder_status(&info, &s);
                return Ok(());
            }
            crate::lock_service::Reply::Busy => {
                output::lock_msg(&invocation);
                output::lock_msg("the holder was mid-update; run `brokkr lock` again for details");
                output::lock_msg(&format!("root: {}", info.project_root));
                return Ok(());
            }
            crate::lock_service::Reply::InvalidStatus(reason) => {
                output::lock_msg(&format!(
                    "the holder's status reply could not be decoded ({reason}); showing lock-file details"
                ));
                unanswered = format!("its status reply could not be decoded ({reason})");
            }
            // Released between the flock probe and the request, or stale
            // metadata: say what the file says, then try the local view.
            crate::lock_service::Reply::Gone => {
                unanswered = "it said this acquisition is over".into();
            }
            crate::lock_service::Reply::Accepted { .. } => {
                unanswered = "it gave an unexpected answer".into();
            }
            crate::lock_service::Reply::NoAnswer(why)
            | crate::lock_service::Reply::Unconfirmed(why) => unanswered = why,
        }
    }
    if let Some(hint) = lockfile::same_agent_hint(&info) {
        output::lock_msg(&hint);
    }

    // The holder did not answer (an older brokkr, or one that is stopped).
    // Every PID in the lock file was written in the HOLDER's PID namespace
    // (a sandboxed holder writes pid=2, which is kthreadd here), so each
    // one is shown only when the namespaces match and its identity tokens
    // verify against this namespace's /proc - an unverified PID's stats
    // belong to some other process and are suppressed wholesale.
    if !crate::process::shares_namespace_tokens(&info.pid_ns, &info.time_ns) {
        output::lock_msg(&invocation);
        output::lock_msg(&format!(
            "the holder did not answer its control socket, and it runs in {} - its process cannot \
             be inspected from here",
            if crate::process::foreign_pid_namespace(&info.pid_ns).is_some() {
                "another PID namespace than this command (one of them is sandboxed)"
            } else {
                "a PID or time namespace this process cannot verify"
            }
        ));
        output::lock_msg(&format!("root: {}", info.project_root));
        return Ok(());
    }
    let holder_identity = info.process_identity();
    let holder_boot = crate::process::BootId::new(&info.boot_id);
    let verified_holder = holder_identity
        .zip(holder_boot.as_ref())
        .filter(|&(identity, boot)| crate::process::verify_identity(identity, boot));
    let holder_verified = verified_holder.is_some();
    let uptime_suffix = verified_holder
        .and_then(|(identity, boot)| lockfile::verified_uptime_for(identity, boot))
        .map(|u| format!(" running {u}"))
        .unwrap_or_default();
    if holder_verified {
        output::lock_msg(&format!(
            "brokkr PID {}{}: {}",
            info.pid, uptime_suffix, invocation,
        ));
        let stopped = verified_holder
            .and_then(|(identity, _)| crate::process::ProcStat::read(identity.pid()).ok())
            .filter(|stat| matches!(stat.state(), 'T' | 't'))
            .map_or("", |_| " - it is stopped (SIGSTOP or a debugger)");
        output::lock_msg(&format!(
            "no status from the holder's control socket ({unanswered}){stopped}; what follows is \
             observed from here, without its activities or last marker"
        ));
    } else {
        output::lock_msg(&invocation);
        output::lock_msg(
            "process details unavailable - holder identity could not be verified from this namespace",
        );
    }
    output::lock_msg(&format!("root: {}", info.project_root));

    if let Some((r, t)) = info.progress {
        output::lock_msg(&format!("run {r}/{t}"));
    }

    if let Some((identity, _)) = verified_holder {
        print_observed_processes(&info, identity);
    }
    Ok(())
}

/// The holder's processes, observed from here with the same sampler and
/// renderer the holder uses: its descendants, plus everything carrying its
/// acquisition's launch token. Only for a verified holder, whose PID means the
/// same thing here. Activities and the sidecar marker live in the holder's
/// memory, so they are not available on this path.
fn print_observed_processes(info: &lockfile::LockInfo, holder: crate::process::ProcIdentity) {
    let acq = (!info.acq_id.is_empty()).then_some(info.acq_id.as_str());
    // The holder's start: nothing carrying its token is older, so an
    // uninspectable process from before it is not counted as possibly its.
    let began_ms = crate::process::clock_ticks_per_second()
        .map(|clk| holder.start().get().saturating_mul(1000) / clk);
    let sample = lockfile::collector::sample(&lockfile::collector::SampleSpec {
        acq,
        root: holder.pid().get(),
        pids_meaningful: true,
        observed: None,
        began_ms,
    });
    // The sample took a quarter second: the hold it describes must still be
    // the one that holds the lock, or the rows are another's.
    let still = lockfile::status()
        .ok()
        .flatten()
        .is_some_and(|now| now.acq_id == info.acq_id && now.pid == info.pid);
    if !still {
        output::lock_msg("the hold ended while its processes were being sampled");
        return;
    }
    let ext = lockfile::report::Extension {
        activities: Some(Vec::new()),
        sample: Some(sample),
        ..Default::default()
    };
    for line in lockfile::report::render(&ext, true) {
        output::lock_msg(&line);
    }
}

/// Ask once more when the first reply carries no process sample yet, or the
/// holder said a fresh one was being collected: the first request starts a
/// collection the holder serves from cache, so a second request a moment
/// later gets a current sample. Keeps the first reply when the second fails.
fn fresher_status(
    acq_id: &str,
    first: Box<crate::lock_service::HolderStatus>,
) -> Box<crate::lock_service::HolderStatus> {
    let Some(ext) = &first.ext else {
        return first;
    };
    let stale = ext.collecting || ext.sample.as_ref().is_none_or(|s| s.age_ms > 2_000);
    if !stale {
        return first;
    }
    std::thread::sleep(lockfile::collector::SAMPLE_GAP + std::time::Duration::from_millis(250));
    match crate::lock_service::request(acq_id, "status") {
        crate::lock_service::Reply::Status(second)
            if second.identity.args == first.identity.args =>
        {
            second
        }
        _ => first,
    }
}

/// Render the holder's own answer. Everything here is the holder speaking
/// about itself; process PIDs are printed only when they mean the same thing
/// here as they did to the holder.
fn print_holder_status(info: &lockfile::LockInfo, s: &crate::lock_service::HolderStatus) {
    let invocation = invocation_label(
        s.identity.configured,
        &s.identity.project,
        &s.identity.command,
        &s.identity.args,
    );
    let mut facts = Vec::new();
    facts.push(format!("holding {}", output::format_duration(s.held)));
    // The holder's own figures: brokkr itself, which mostly waits on its
    // children - their work is in the process lines below.
    match (s.own_cpu, s.cpu) {
        (Some(cpu), _) | (None, Some(cpu)) => {
            facts.push(format!("own CPU {}", output::format_duration(cpu)));
        }
        (None, None) => {}
    }
    if let Some(kb) = s.rss_kb {
        facts.push(format!("own RSS {} MB", kb / 1024));
    }
    if s.snapshot.draining {
        facts.push("draining earlier compilers".into());
    }
    let facts = if facts.is_empty() {
        String::new()
    } else {
        format!(" ({})", facts.join(", "))
    };
    output::lock_msg(&format!("brokkr{facts}: {invocation}"));
    output::lock_msg(&format!("root: {}", s.identity.root));

    let mut work = Vec::new();
    if let Some((run, total)) = s.snapshot.progress {
        work.push(format!("run {run}/{total}"));
    }
    // An older holder has no extension: its coarse flags are all there is.
    if s.ext.is_none() && s.snapshot.has_child {
        work.push("a child process is running".into());
    }
    if s.snapshot.mocks > 0 {
        work.push(output::count(s.snapshot.mocks, "mock server"));
    }
    if !work.is_empty() {
        output::lock_msg(&work.join(", "));
    }
    if let Some(ext) = &s.ext {
        let same_ns = crate::process::shares_namespace_tokens(&info.pid_ns, &info.time_ns);
        for line in lockfile::report::render(ext, same_ns) {
            output::lock_msg(&line);
        }
    }

    if crate::process::foreign_pid_namespace(&info.pid_ns).is_some() {
        output::lock_msg(
            "the holder runs in another PID namespace than this command (one of them is \
             sandboxed), so its PIDs are not shown; `brokkr kill` in its scope asks \
             it to stop over its control socket, `brokkr kill --hard` cannot reach it from here",
        );
    }
    if let Some(hint) = lockfile::same_agent_hint(info) {
        output::lock_msg(&hint);
    }
}

/// Whose brokkr commands a `brokkr kill` stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KillScope {
    /// The commands of one `brokkr.toml` project key, whatever checkout or
    /// worktree they run in.
    Project(String),
    /// The commands run with no `brokkr.toml` under one state root: such a
    /// command has no key, and its root is the only identity it records.
    Unconfigured(String),
    /// Every project's.
    All,
}

impl KillScope {
    /// The scope a kill runs in: `--all`, `--project NAME`, or the caller's
    /// own. A `brokkr.toml` that is present but did not load names no key, so
    /// the bare form refuses there rather than guess one.
    pub(crate) fn resolve(
        named: Option<&str>,
        all: bool,
        detection: Option<&crate::project::Detection>,
    ) -> Result<Self, DevError> {
        if all {
            return Ok(Self::All);
        }
        if let Some(name) = named {
            return Ok(Self::Project(name.to_owned()));
        }
        if let Some(d) = detection {
            return Ok(Self::Project(d.project.name().to_owned()));
        }
        let cwd = std::env::current_dir()?;
        if let Some(dir) = crate::project::find_config_dir(&cwd) {
            return Err(DevError::Refused(format!(
                "{}/brokkr.toml does not load, so a bare `brokkr kill` cannot know this project's \
                 key; name it with `brokkr kill --project NAME`, or stop every project's commands \
                 with `brokkr kill --all`",
                crate::output::path(&dir)
            )));
        }
        let (root, _) = crate::project::detect_roots()?;
        Ok(Self::Unconfigured(root.as_path().display().to_string()))
    }

    /// Whether a holder or waiter recording this identity is in the scope.
    fn covers(&self, configured: bool, project: &str, root: &str) -> bool {
        match self {
            Self::Project(key) => configured && project == key,
            Self::Unconfigured(here) => !configured && root == here,
            Self::All => true,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Project(key) => format!("project {key}"),
            Self::Unconfigured(root) => format!("the commands with no brokkr.toml under {root}"),
            Self::All => "any project".into(),
        }
    }
}

/// The explicit form that reaches a command recorded with this identity.
fn reaching_form(configured: bool, project: &str) -> String {
    if configured {
        format!("`brokkr kill --project {project}` or `brokkr kill --all`")
    } else {
        "`brokkr kill --all`, or a bare `brokkr kill` from its root".into()
    }
}

/// How long a kill waits, once, for its cancelled waiters to let go.
const CANCEL_CONFIRM: std::time::Duration = std::time::Duration::from_secs(2);

/// How often, and how far apart, a kill re-reads a holder record it could not
/// parse: a holder rewrites or releases it in a moment.
const UNREADABLE_RETRIES: usize = 3;
const UNREADABLE_RETRY_GAP: std::time::Duration = std::time::Duration::from_millis(100);

/// `brokkr kill`: cancel the scope's queued commands, then stop the lock
/// holder when it is in the scope, and report what was left alone.
///
/// Waiters go first, so none of the scope's takes the lock the holder is
/// about to release. A waiter that took the lock between the queue read and
/// its cancel is the holder by the time the holder is read, and is stopped as
/// one. The holder is read fresh after the cancels and stopped by the holder
/// policy whatever it is, a just-cancelled acquisition included: the cancel
/// and the release of the lock are separate outcomes. Serves are never
/// stopped (`crate::serve::kill_report`).
///
/// Anything in the scope that fails, and anything live that cannot be placed
/// in or out of it (an unreadable record), fails the command after every
/// other action was taken.
pub(crate) fn cmd_kill(scope: &KillScope, hard: bool) -> Result<(), DevError> {
    let mut report = KillReport::default();
    cancel_queued(scope, &mut report)?;
    stop_scoped_holder(scope, hard, &mut report)?;
    let KillReport {
        acted,
        failures,
        elsewhere,
    } = report;

    if !elsewhere.is_empty() {
        output::lock_msg(&format!(
            "left {} outside the scope waiting:",
            output::count(elsewhere.len(), "queued command")
        ));
        for label in &elsewhere {
            output::lock_msg(&format!("  {label}"));
        }
    }
    for line in crate::serve::kill_report(&|configured, project, root| {
        scope.covers(configured, project, &root.display().to_string())
    }) {
        output::lock_msg(&format!("serve: {line}"));
    }
    if !acted {
        output::lock_msg(&format!(
            "nothing of {} to stop: no queued command and not the lock holder",
            scope.describe()
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(DevError::Lock(LockError::from(failures.join("; "))))
    }
}

/// What a kill did so far, for its closing lines and its exit.
#[derive(Default)]
struct KillReport {
    /// Something in the scope was found (stopped, cancelled or failed).
    acted: bool,
    failures: Vec<String>,
    /// Queued commands outside the scope, left waiting.
    elsewhere: Vec<String>,
}

/// Cancel every in-scope queued command, then confirm them together under
/// one deadline.
fn cancel_queued(scope: &KillScope, report: &mut KillReport) -> Result<(), DevError> {
    let dir = lockfile::queue_dir().ok_or_else(|| {
        DevError::Lock(LockError::from(
            "$HOME is not set or is empty - cannot locate the lock queue",
        ))
    })?;
    let entries = lockfile::queue::list(&dir).map_err(|e| {
        DevError::Lock(LockError::from(format!(
            "cannot read the lock queue {}: {e}",
            crate::output::path(&dir)
        )))
    })?;
    let mut committed = Vec::new();
    for entry in &entries {
        let (label, in_scope) = match &entry.record {
            Ok(record) => (
                waiter_label(record),
                scope.covers(record.configured, &record.project, &record.root),
            ),
            Err(why) => (
                format!("a queued command whose record cannot be read ({why})"),
                *scope == KillScope::All,
            ),
        };
        if !in_scope {
            match &entry.record {
                Ok(_) if entry.standing == lockfile::queue::Standing::Waiting => {
                    report.elsewhere.push(label);
                }
                Ok(_) => {}
                Err(_) => report.failures.push(format!(
                    "{label} could not be placed in or out of {}",
                    scope.describe()
                )),
            }
            continue;
        }
        report.acted = true;
        if entry.standing == lockfile::queue::Standing::Cancelling {
            output::lock_msg(&format!("already cancelled, still letting go: {label}"));
            continue;
        }
        match lockfile::queue::cancel(&dir, &entry.token) {
            Ok(lockfile::queue::Cancel::Committed(handle)) => committed.push((label, handle)),
            Ok(lockfile::queue::Cancel::AlreadyCancelled) => {
                output::lock_msg(&format!("already cancelled by another kill: {label}"));
            }
            Ok(lockfile::queue::Cancel::Left) => output::lock_msg(&format!(
                "left the queue before the cancel landed (it took the lock or exited): {label}"
            )),
            Err(e) => report
                .failures
                .push(format!("could not cancel {label}: {e}")),
        }
    }
    let handles: Vec<_> = committed.iter().map(|(_, h)| h).collect();
    let released = lockfile::queue::confirm(&handles, CANCEL_CONFIRM);
    // A committed cancel is the outcome; the confirmation only says whether
    // the waiter has let go yet.
    for ((label, _), done) in committed.iter().zip(released) {
        match done {
            Ok(true) => output::lock_msg(&format!("cancelled, will not run: {label}")),
            Ok(false) => output::lock_msg(&format!(
                "cancelled, will not run, but it has not let go yet (it may be stopped): {label}"
            )),
            Err(e) => output::lock_msg(&format!(
                "cancelled, will not run; whether it has let go could not be checked ({e}): \
                 {label}"
            )),
        }
    }
    Ok(())
}

/// Stop the lock holder when it is in the scope; name it when it is not.
fn stop_scoped_holder(
    scope: &KillScope,
    hard: bool,
    report: &mut KillReport,
) -> Result<(), DevError> {
    match read_holder()? {
        None => {}
        Some(info) if info.pid == 0 => {
            report.acted = true;
            report.failures.push(format!(
                "the lock is held, but its holder's record cannot be read, so it {}",
                if *scope == KillScope::All {
                    "cannot be addressed; nothing was sent to it"
                } else {
                    "cannot be placed in or out of the scope; it was left alone"
                }
            ));
        }
        Some(info) if scope.covers(info.configured, &info.project, &info.project_root) => {
            report.acted = true;
            if let Err(e) = stop_holder(&info, hard) {
                report.failures.push(e.to_string());
            }
        }
        Some(info) => output::lock_msg(&format!(
            "left the lock holder alone: {} is not {}; {} stops it",
            invocation_label(info.configured, &info.project, &info.command, &info.args),
            scope.describe(),
            reaching_form(info.configured, &info.project),
        )),
    }
    Ok(())
}

/// The holder, re-read a few times while its record does not parse (it is
/// rewritten or released in a moment), so a transition is not mistaken for
/// an unreadable holder.
fn read_holder() -> Result<Option<lockfile::LockInfo>, DevError> {
    let mut info = lockfile::status()?;
    for _ in 0..UNREADABLE_RETRIES {
        if !info.as_ref().is_some_and(|i| i.pid == 0) {
            break;
        }
        std::thread::sleep(UNREADABLE_RETRY_GAP);
        info = lockfile::status()?;
    }
    Ok(info)
}

/// Stop an in-scope holder: the cooperative stop, or with `hard` the
/// all-or-nothing SIGKILL.
///
/// Every recorded PID was written in the HOLDER's PID namespace, which may
/// not be ours (a sandboxed holder writes pid=2 - kthreadd here), and any
/// PID can be recycled. So each signal target is authenticated first: its
/// recorded starttime token must match `/proc/{pid}/stat` in this namespace
/// under the same boot id, and a pidfd is opened and re-verified before
/// signalling (`pidfd_send_signal` then cannot be redirected by recycling).
/// Verification is all-or-nothing BEFORE the first signal: killing one
/// verified target and then discovering another is unverifiable would leave
/// a partially destroyed workload. A recorded child or mock with positive
/// evidence that its generation is gone is skipped rather than refused (see
/// `provably_gone`); a merely unverifiable one still refuses. Delivery itself is not transactional - a
/// target can exit naturally after preflight.
fn stop_holder(info: &lockfile::LockInfo, hard: bool) -> Result<(), DevError> {
    if !hard {
        return kill_graceful(info);
    }

    // Hard kill signals by number, so every recorded PID must mean the same
    // thing here as it did to the holder.
    if !crate::process::shares_namespace_tokens(&info.pid_ns, &info.time_ns) {
        return Err(refuse_kill(
            "the holder runs in another PID or time namespace (a sandboxed command); `brokkr kill` \
             without --hard asks it to stop over its control socket, or stop the session that owns \
             it",
        ));
    }
    hard_kill(info)
}

/// The cooperative stop. The control socket first: the holder validates the
/// request against its live acquisition and stops itself, so this works
/// across PID namespaces and cannot reach a later holder.
fn kill_graceful(info: &lockfile::LockInfo) -> Result<(), DevError> {
    use crate::lock_service::Reply;
    let unanswered = if info.acq_id.is_empty() {
        "the holder records no control socket (an older brokkr)".to_owned()
    } else {
        match crate::lock_service::request(&info.acq_id, "stop") {
            Reply::Accepted { signal_error: None } => {
                output::lock_msg("stop accepted by the holder - cleanup in progress");
                return Ok(());
            }
            Reply::Accepted {
                signal_error: Some(e),
            } => {
                output::lock_msg(&format!(
                    "stop committed by the holder, but its SIGTERM to itself failed ({e}); it \
                     stops at its next check"
                ));
                return Ok(());
            }
            Reply::Gone => {
                output::lock_msg("the holder had already released the lock; nothing to kill");
                return Ok(());
            }
            Reply::Unconfirmed(why) => return stop_unconfirmed(info, &why),
            Reply::Busy | Reply::Status(_) => "the holder gave an unexpected answer".to_owned(),
            Reply::InvalidStatus(why) => why,
            Reply::NoAnswer(why) => why,
        }
    };
    // No answer (an older brokkr, or a stopped holder): the numeric path,
    // only where the recorded PID means the same thing here.
    if !crate::process::shares_namespace_tokens(&info.pid_ns, &info.time_ns) {
        return Err(refuse_kill(&format!(
            "{unanswered}, and the holder runs in a PID or time namespace this process cannot \
             verify"
        )));
    }
    let holder = preflight_holder(info).map_err(|why| refuse_kill(&why))?;
    match signal_target(&holder, libc::SIGTERM) {
        Ok(true) => output::lock_msg(&format!(
            "SIGTERM sent to brokkr PID {} - cleanup in progress",
            info.pid,
        )),
        Ok(false) => output::lock_msg(&format!(
            "brokkr PID {} exited before the signal landed; nothing to kill",
            info.pid,
        )),
        Err(e) => {
            return Err(DevError::Lock(LockError::from(format!(
                "SIGTERM to brokkr PID {} failed: {e}",
                info.pid
            ))));
        }
    }
    Ok(())
}

/// How long [`stop_unconfirmed`] watches for the holder to let go.
const RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// A `stop` reached the holder but no answer came back, so it may well have
/// been committed - a holder killed by its own `SIGTERM` before it could
/// answer looks exactly like this. No numeric `SIGTERM` follows: against a
/// holder already handling the first one, a second escalates to the default
/// action. Watch the lock instead and report what it shows.
fn stop_unconfirmed(info: &lockfile::LockInfo, why: &str) -> Result<(), DevError> {
    let deadline = std::time::Instant::now() + RELEASE_WAIT;
    loop {
        let released = match lockfile::status() {
            Ok(None) => true,
            Ok(Some(now)) => now.acq_id != info.acq_id,
            Err(_) => false,
        };
        if released {
            output::lock_msg(&format!(
                "stop delivered; the holder released the lock without confirming ({why})"
            ));
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(DevError::Lock(LockError::from(format!(
                "stop delivered but not confirmed ({why}), and the holder still holds the lock \
                 after {} - it may be stopping; check `brokkr lock`. No signal was sent from here",
                output::format_duration(RELEASE_WAIT)
            ))));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// SIGSTOP the holder, SIGKILL its recorded children, everything beneath it,
/// then the holder. Callers have checked `shares_namespace_tokens`.
fn hard_kill(info: &lockfile::LockInfo) -> Result<(), DevError> {
    // Preflight every advertised target: verify identity, open and retain a
    // pidfd (pinning the process generation for the whole operation, PG
    // leaders included), re-verify. Any failure refuses the whole hard kill
    // with no signal sent - except a recorded child or mock that is provably
    // gone (see `provably_gone`), which is skipped: a wedged holder keeps a
    // slot whose process has exited, and refusing on it would leave exactly
    // the holder `--hard` exists for unkillable.
    let holder = preflight_holder(info).map_err(|why| refuse_kill(&why))?;
    let recorded = info
        .children
        .iter()
        .map(|(pid, start)| ("child", *pid, start))
        .chain(info.mocks.iter().map(|(pid, start)| ("mock", *pid, start)));
    let mut targets = Vec::new();
    let mut skipped = Vec::new();
    // Recorded processes whose own generation is gone (or goes before its
    // signal lands): a group they led may still have live members, which
    // only `kill_leaderless_group` reaches once they leave brokkr's tree.
    let mut leaderless = Vec::new();
    for (role, pid, starttime) in recorded {
        if provably_gone(info, pid, starttime) {
            skipped.push((role, pid));
            if let Some(start) = crate::process::StartTicks::parse(starttime) {
                leaderless.push((role, pid, start));
            }
            continue;
        }
        targets.push(
            preflight_target(role, pid, starttime, &info.boot_id)
                .map_err(|why| refuse_kill(&why))?,
        );
    }
    for (role, pid) in skipped {
        output::lock_msg(&format!(
            "{role} PID {pid}: recorded process is gone (exited or PID reused); skipped"
        ));
    }

    // Freeze brokkr first, so it cannot start anything new (the next sweep,
    // a respawned mock) while its tree is being taken down.
    // A failed delivery is reported as it happens and fails the command once
    // every other target has had its signal.
    let mut failed = Vec::new();
    if let Err(e) = signal_target(&holder, libc::SIGSTOP) {
        failed.push(format!("SIGSTOP brokkr PID {}: {e}", info.pid));
        output::lock_msg(&format!("SIGSTOP brokkr PID {}: failed ({e})", info.pid));
    }

    // Kill children first, then brokkr - otherwise there's a brief window
    // where brokkr is dead but the tool it was measuring is still alive
    // (and anyone peeking at `brokkr lock` sees stale state pointing at a
    // live child with no owner).
    for target in &targets {
        let delivery = signal_target(target, libc::SIGKILL);
        let kind = if target.pg_leader { "PG" } else { "PID" };
        if let Err(e) = &delivery {
            failed.push(format!(
                "SIGKILL {} {kind} {}: {e}",
                target.role, target.pid
            ));
        }
        // Undelivered, or delivered to the leader alone (its pgid lookup was
        // lost, or it leads no group): either way no group signal went out,
        // so the member sweep covers its group once it is gone. A process
        // that led no group leaves no members carrying its pid as pgid.
        if matches!(delivery, Ok(false)) || (matches!(delivery, Ok(true)) && !target.pg_leader) {
            leaderless.push((target.role, target.pid, target.process.identity().start()));
        }
        output::lock_msg(&format!(
            "SIGKILL {} {kind} {}: {}",
            target.role,
            target.pid,
            describe_delivery(&delivery),
        ));
    }
    for &(role, pgid, leader_start) in &leaderless {
        kill_leaderless_group(info, role, pgid, leader_start, &mut failed);
    }
    // Then everything else beneath brokkr. The recorded child is only the
    // process brokkr published: a test binary under cargo, a rustc under a
    // build, or a parallel lane's binaries are recorded nowhere, and a parent-
    // death signal reaches only a direct child - so without this walk a hard
    // kill orphaned them. Two sweeps for a child forked mid-walk; with brokkr
    // stopped, nothing restarts them. The holder's identity was verified from
    // this namespace above, so its pid is meaningful to our `/proc`.
    let swept =
        crate::shutdown::kill_descendants(info.pid) + crate::shutdown::kill_descendants(info.pid);
    if swept > 0 {
        output::lock_msg(&format!(
            "SIGKILL sent to {} beneath brokkr",
            output::count(swept, "process")
        ));
    }
    let delivery = signal_target(&holder, libc::SIGKILL);
    if let Err(e) = &delivery {
        failed.push(format!("SIGKILL brokkr PID {}: {e}", info.pid));
    }
    output::lock_msg(&format!(
        "SIGKILL brokkr PID {}: {}",
        info.pid,
        describe_delivery(&delivery),
    ));
    output::lock_msg("follow up with `brokkr clean` to wipe scratch");
    if failed.is_empty() {
        Ok(())
    } else {
        Err(DevError::Lock(LockError::from(format!(
            "the hard kill could not deliver every signal: {}",
            failed.join("; ")
        ))))
    }
}

/// The all-or-nothing refusal: name the unverifiable target and state that
/// nothing was signalled.
fn refuse_kill(why: &str) -> DevError {
    DevError::Lock(LockError::from(format!(
        "{why} - lock holder and child process identities must all verify from this PID/time namespace before killing; no signals were sent"
    )))
}

/// A signal target that passed preflight: identity verified, pidfd held
/// (pinning the process generation until we signal), PG-leadership known.
struct KillTarget {
    role: &'static str,
    pid: u32,
    process: crate::process::VerifiedProcess,
    /// `getpgid(pid) == pid` for a tracked child: spawns that called
    /// `process_group(0)` become their own PG leader, and `kill(-pid, ...)`
    /// sweeps the whole group (rustc, sæhrimnir's helpers, etc.).
    /// Non-leaders inherit brokkr's PG, where a group kill would target
    /// brokkr's own PG (catastrophic), so they get a single-process pidfd
    /// signal. Always false for the brokkr holder itself: brokkr *is*
    /// normally its own PG leader (`shutdown::isolate_process_group`), and a
    /// group signal at it would sweep its children too - defeating the
    /// cooperative-SIGTERM path, whose whole point is that brokkr's handler
    /// shuts the workload down itself.
    pg_leader: bool,
}

/// Preflight the brokkr holder. Same authentication as any target, but the
/// holder is always signalled through its pidfd alone, never as a process
/// group (see `pg_leader`).
fn preflight_holder(info: &lockfile::LockInfo) -> Result<KillTarget, String> {
    let identity = info.process_identity().ok_or_else(|| {
        format!(
            "brokkr PID {}: identity could not be verified from this namespace",
            info.pid
        )
    })?;
    let boot = info
        .host_scope()
        .map(|scope| scope.boot().clone())
        .or_else(|| crate::process::BootId::new(&info.boot_id))
        .ok_or_else(|| {
            format!(
                "brokkr PID {}: identity could not be verified from this namespace",
                info.pid
            )
        })?;
    let mut holder = preflight_identity("brokkr", info.pid, identity, &boot)?;
    holder.pg_leader = false;
    Ok(holder)
}

/// Authenticate one recorded PID and pin it to a pidfd before any signal.
fn preflight_target(
    role: &'static str,
    pid: u32,
    starttime: &str,
    boot_id: &str,
) -> Result<KillTarget, String> {
    let identity = typed_process_identity(pid, starttime).ok_or_else(|| {
        format!("{role} PID {pid}: identity could not be verified from this namespace")
    })?;
    let boot = crate::process::BootId::new(boot_id).ok_or_else(|| {
        format!("{role} PID {pid}: identity could not be verified from this namespace")
    })?;
    preflight_identity(role, pid, identity, &boot)
}

/// Whether a recorded child or mock is provably no longer the process brokkr
/// recorded, so `--hard` may skip it instead of refusing. Needs the holder's
/// namespace tokens (a legacy record without them keeps pure all-or-nothing:
/// a matching holder pid and starttime there is no namespace proof), the
/// recorded boot to be this kernel's, a parseable starttime, and positive
/// evidence from `process::generation` - never a bare missing `/proc` entry,
/// which `hidepid` produces for live processes. Callers have verified the
/// holder and checked `shares_namespace_tokens`.
fn provably_gone(info: &lockfile::LockInfo, pid: u32, starttime: &str) -> bool {
    if info.pid_ns.is_empty() || info.time_ns.is_empty() {
        return false;
    }
    let Some(boot) = crate::process::BootId::new(&info.boot_id) else {
        return false;
    };
    if crate::process::local_boot_id().as_ref() != Some(&boot) {
        return false;
    }
    typed_process_identity(pid, starttime).is_some_and(|identity| {
        crate::process::generation(identity) == crate::process::Generation::Gone
    })
}

/// SIGKILL the surviving members of a group a recorded child or mock led,
/// once that leader is gone: members reparented away from brokkr are beyond
/// the descendant sweep, and with the leader's generation gone there is no
/// identity to re-verify before a `kill(-pgid)`.
///
/// No group signal is sent. The pgid number stays allocated while any member
/// lives, but once every member has exited it can be reused by an unrelated
/// group, and nothing about the group itself tells the two apart. So each
/// member is judged and signalled alone: it must be in group `pgid`, have
/// started no earlier than the recorded leader, and carry this acquisition's
/// launch cohort in `BROKKR_EXEC` - a 128-bit random id every process spawned
/// under the hold inherits, which a stranger's group cannot hold by accident.
/// The member is then pinned to a pidfd under the recorded boot and its group
/// re-read before the signal, so PID reuse during the walk cannot redirect
/// it. A member whose environment cannot be read (another user, `hidepid`)
/// or that exec'd with a cleared one is left alone. Two sweeps, for a member
/// forked mid-walk; failures join `failed`.
fn kill_leaderless_group(
    info: &lockfile::LockInfo,
    role: &'static str,
    pgid: u32,
    leader_start: crate::process::StartTicks,
    failed: &mut Vec<String>,
) {
    if info.acq_id.is_empty() {
        return;
    }
    let Some(boot) = crate::process::BootId::new(&info.boot_id) else {
        return;
    };
    let exclude = [info.pid, std::process::id()];
    let mut killed = 0usize;
    for _ in 0..2 {
        let Ok(table) = crate::process::ProcTable::snapshot() else {
            break;
        };
        for (pid, row) in table.iter() {
            if exclude.contains(&pid.get()) || !in_leaderless_group(row, pgid, leader_start) {
                continue;
            }
            let marker = crate::process::environ_value(pid.get(), crate::process::spawn::EXEC_ENV);
            if !marked_for(marker.as_deref(), &info.acq_id) {
                continue;
            }
            let Ok(member) = crate::process::VerifiedProcess::open(row.identity(), Some(&boot))
            else {
                continue;
            };
            let still_member = crate::process::ProcStat::read(*pid).is_ok_and(|now| {
                now.identity() == row.identity() && in_leaderless_group(&now, pgid, leader_start)
            });
            if !still_member {
                continue;
            }
            match member.send_signal(libc::SIGKILL) {
                Ok(true) => killed += 1,
                Ok(false) => {}
                Err(e) => {
                    failed.push(format!(
                        "SIGKILL {role} PG {pgid} member PID {}: {e}",
                        pid.get()
                    ));
                    output::lock_msg(&format!(
                        "SIGKILL {role} PG {pgid} member PID {}: FAILED ({e})",
                        pid.get()
                    ));
                }
            }
        }
    }
    if killed > 0 {
        output::lock_msg(&format!(
            "SIGKILL sent to {} left in {role} PG {pgid} after its leader exited",
            output::count(killed, "process")
        ));
    }
}

/// Whether a process row could be a member of the group a recorded leader
/// (`pgid`, started at `leader_start`) created: in that group, and started no
/// earlier than the leader. Necessary, never sufficient - a reused pgid passes
/// too, which is what [`marked_for`] rules out.
fn in_leaderless_group(
    row: &crate::process::ProcStat,
    pgid: u32,
    leader_start: crate::process::StartTicks,
) -> bool {
    row.group().map(crate::process::Pid::get) == Some(pgid) && row.start() >= leader_start
}

/// Whether a `BROKKR_EXEC` value names launch cohort of acquisition `acq`.
fn marked_for(marker: Option<&str>, acq: &str) -> bool {
    !acq.is_empty()
        && marker
            .and_then(lockfile::observe::parse_exec)
            .is_some_and(|(owner, _)| owner == acq)
}

fn typed_process_identity(pid: u32, starttime: &str) -> Option<crate::process::ProcIdentity> {
    Some(crate::process::ProcIdentity::new(
        crate::process::Pid::new(pid)?,
        crate::process::StartTicks::parse(starttime)?,
    ))
}

fn preflight_identity(
    role: &'static str,
    pid: u32,
    identity: crate::process::ProcIdentity,
    boot: &crate::process::BootId,
) -> Result<KillTarget, String> {
    let process =
        crate::process::VerifiedProcess::open(identity, Some(boot)).map_err(|r| match r {
            crate::process::PidfdRefusal::Unverified => {
                format!("{role} PID {pid}: identity could not be verified from this namespace")
            }
            crate::process::PidfdRefusal::OpenFailed => {
                format!(
                    "{role} PID {pid}: could not open a pidfd (process gone or namespace-isolated)"
                )
            }
            crate::process::PidfdRefusal::IdentityChanged => {
                format!("{role} PID {pid}: process changed identity during verification")
            }
        })?;
    let pgid = unsafe { libc::getpgid(pid.cast_signed()) };
    // Cast guarded by `pgid > 0`; `cast_unsigned` is the documented
    // i32->u32 conversion that clippy doesn't flag (mirrors the
    // pid.cast_signed() pattern we use for the inverse direction).
    let pg_leader = pgid > 0 && pgid.cast_unsigned() == pid;
    Ok(KillTarget {
        role,
        pid,
        process,
        pg_leader,
    })
}

/// Signal a preflighted target. `Ok(true)` delivered, `Ok(false)` the target
/// was already gone, `Err` a real failure (EPERM, say) - reported, never
/// counted as delivered.
///
/// Non-leaders are signalled through the pidfd, which is race-free against
/// PID recycling. PG leaders need `kill(-pid, ...)` to sweep the group and
/// process-group IDs have no pidfd-equivalent handle, so the leader is
/// re-verified immediately before the group kill (starttime unchanged, i.e.
/// same generation); the sliver between that check and the kill is a
/// documented residual race we accept.
fn signal_target(target: &KillTarget, signal: libc::c_int) -> std::io::Result<bool> {
    if target.pg_leader {
        if !crate::process::identity_matches(target.process.identity()) {
            return Ok(false);
        }
        return crate::process::signal_process_group(target.pid, signal);
    }
    target.process.send_signal(signal)
}

fn describe_delivery(result: &std::io::Result<bool>) -> String {
    match result {
        Ok(true) => "sent".into(),
        Ok(false) => "not running".into(),
        Err(e) => format!("FAILED ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::KillScope;

    /// A project scope is the key alone, across roots; an unconfigured scope
    /// is its root alone; neither takes the other's commands, even where an
    /// unconfigured command and a configured one share a root or the key is
    /// the empty string.
    #[test]
    fn a_scope_takes_its_own_commands_only() {
        let project = KillScope::Project("nidhogg".into());
        assert!(project.covers(true, "nidhogg", "/a"));
        assert!(project.covers(true, "nidhogg", "/worktree"));
        assert!(!project.covers(true, "pbfhogg", "/a"));
        assert!(!project.covers(false, "nidhogg", "/a"));

        let unconfigured = KillScope::Unconfigured("/a".into());
        assert!(unconfigured.covers(false, "", "/a"));
        assert!(!unconfigured.covers(false, "", "/b"));
        assert!(!unconfigured.covers(true, "", "/a"));

        assert!(!KillScope::Project(String::new()).covers(false, "", "/a"));
        assert!(KillScope::All.covers(false, "", "/b"));
    }

    /// A stat row for `pid` in group `pgrp`, started at `start` ticks.
    #[allow(clippy::expect_used)]
    fn row(pid: u32, pgrp: u32, start: u64) -> crate::process::ProcStat {
        crate::process::ProcStat::parse(&format!(
            "{pid} (cmd) S 1 {pgrp} {pgrp} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0 0"
        ))
        .expect("synthetic stat row parses")
    }

    /// A leaderless group's candidate is in the recorded pgid and started
    /// no earlier than the recorded leader; anything else is out.
    #[test]
    fn a_leaderless_member_is_in_the_group_and_not_older_than_its_leader() {
        use super::in_leaderless_group;
        let leader = crate::process::StartTicks::new(1000);
        assert!(in_leaderless_group(&row(50, 40, 1000), 40, leader));
        assert!(in_leaderless_group(&row(50, 40, 5000), 40, leader));
        assert!(!in_leaderless_group(&row(50, 40, 999), 40, leader));
        assert!(!in_leaderless_group(&row(50, 41, 5000), 40, leader));
        assert!(!in_leaderless_group(&row(50, 0, 5000), 40, leader));
    }

    /// Only this acquisition's launch cohort marks a member as ours: a reused
    /// pgid's members carry another cohort or none, and an empty recorded
    /// acquisition id proves nothing.
    #[test]
    fn only_this_acquisitions_cohort_marks_a_member() {
        use super::marked_for;
        let acq = "0123456789abcdef0123456789abcdef";
        assert!(marked_for(Some(&format!("{acq}.7")), acq));
        assert!(!marked_for(Some("fedcba98765432100123456789abcdef.7"), acq));
        assert!(!marked_for(Some(acq), acq));
        assert!(!marked_for(Some(&format!("{acq}.x")), acq));
        assert!(!marked_for(None, acq));
        assert!(!marked_for(Some(".7"), ""));
    }
}
