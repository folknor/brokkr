//! Lock status and termination commands.

use crate::error::{DevError, LockError};
use crate::lockfile;
use crate::output;

pub(crate) fn cmd_lock() -> Result<(), DevError> {
    let Some(info) = lockfile::status()? else {
        output::lock_msg("no active lock");
        return Ok(());
    };

    let invocation = if info.args.is_empty() {
        format!("{} {}", info.project, info.command)
    } else {
        format!("{} {}", info.project, info.args)
    };

    // First ask the holder itself, over its control socket. It answers about
    // its own process, so no PID crosses a namespace boundary - this works
    // from the host, from a sibling sandbox, from anywhere the socket is.
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
            }
            // Released between the flock probe and the request, or stale
            // metadata: say what the file says, then try the local view.
            crate::lock_service::Reply::Gone
            | crate::lock_service::Reply::Accepted { .. }
            | crate::lock_service::Reply::NoAnswer(_)
            | crate::lock_service::Reply::Unconfirmed(_) => {}
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
                "another PID namespace (a sandboxed command)"
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
    let sample = lockfile::collector::sample(&lockfile::collector::SampleSpec {
        acq,
        root: holder.pid().get(),
        pids_meaningful: true,
        observed: None,
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
    std::thread::sleep(
        lockfile::collector::SAMPLE_GAP + std::time::Duration::from_millis(250),
    );
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
    let invocation = if s.identity.args.is_empty() {
        format!("{} {}", s.identity.project, s.identity.command)
    } else {
        format!("{} {}", s.identity.project, s.identity.args)
    };
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
            "the holder runs in another PID namespace (a sandboxed command); `brokkr kill` asks it \
             to stop over its control socket, `brokkr kill --hard` cannot reach it from here",
        );
    }
    if let Some(hint) = lockfile::same_agent_hint(info) {
        output::lock_msg(&hint);
    }
}

/// Ask the brokkr process holding the lock to shut down. Default sends
/// SIGTERM (cooperative - brokkr handles cleanup itself). `--hard`
/// SIGSTOPs brokkr, then SIGKILLs the recorded child PID, every mock, every
/// other process beneath brokkr (each starttime-checked, see
/// `shutdown::kill_descendants`), and brokkr.
///
/// Every recorded PID was written in the HOLDER's PID namespace, which may
/// not be ours (a sandboxed holder writes pid=2 - kthreadd here), and any
/// PID can be recycled. So each signal target is authenticated first: its
/// recorded starttime token must match `/proc/{pid}/stat` in this namespace
/// under the same boot id, and a pidfd is opened and re-verified before
/// signalling (`pidfd_send_signal` then cannot be redirected by recycling).
/// Verification is all-or-nothing BEFORE the first signal: killing one
/// verified target and then discovering another is unverifiable would leave
/// a partially destroyed workload. Delivery itself is not transactional - a
/// target can exit naturally after preflight.
pub(crate) fn cmd_kill(hard: bool) -> Result<(), DevError> {
    let Some(info) = lockfile::status()? else {
        output::lock_msg("no active lock");
        return Ok(());
    };
    if info.pid == 0 {
        output::lock_msg("lock held by unknown process; nothing to kill");
        return Ok(());
    }

    if !hard {
        return kill_graceful(&info);
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
    hard_kill(&info)
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
    // with no signal sent.
    let mut targets = Vec::new();
    for (child_pid, child_starttime) in &info.children {
        targets.push(
            preflight_target("child", *child_pid, child_starttime, &info.boot_id)
                .map_err(|why| refuse_kill(&why))?,
        );
    }
    for (mock_pid, mock_starttime) in &info.mocks {
        targets.push(
            preflight_target("mock", *mock_pid, mock_starttime, &info.boot_id)
                .map_err(|why| refuse_kill(&why))?,
        );
    }
    let holder = preflight_holder(info).map_err(|why| refuse_kill(&why))?;

    // Freeze brokkr first, so it cannot start anything new (the next sweep,
    // a respawned mock) while its tree is being taken down.
    if let Err(e) = signal_target(&holder, libc::SIGSTOP) {
        output::lock_msg(&format!("SIGSTOP brokkr PID {}: failed ({e})", info.pid));
    }

    // Kill children first, then brokkr - otherwise there's a brief window
    // where brokkr is dead but the tool it was measuring is still alive
    // (and anyone peeking at `brokkr lock` sees stale state pointing at a
    // live child with no owner).
    for target in &targets {
        output::lock_msg(&format!(
            "SIGKILL {} {} {}: {}",
            target.role,
            if target.pg_leader { "PG" } else { "PID" },
            target.pid,
            describe_delivery(&signal_target(target, libc::SIGKILL)),
        ));
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
    output::lock_msg(&format!(
        "SIGKILL brokkr PID {}: {}",
        info.pid,
        describe_delivery(&signal_target(&holder, libc::SIGKILL)),
    ));
    output::lock_msg("follow up with `brokkr clean` to wipe scratch");
    Ok(())
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
