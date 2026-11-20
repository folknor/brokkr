//! The `brokkr serve` supervisor: the detached brokkr process that owns one
//! generation's server for its whole life.
//!
//! The launcher spawns the brokkr executable itself with [`SUPERVISOR_ARG`],
//! the entry's path and the inherited descriptor holding the entry's flock
//! (`process::spawn::spawn_detached`, which has already put it in its own
//! session and out of the hold's cgroup). `main` routes that invocation here
//! before detection, parsing, history or any lock: the supervisor is no
//! command and takes no hold.
//!
//! Its order is the protocol `super::registry` relies on:
//!
//! 1. Re-mark the flock descriptor close-on-exec, so the server never
//!    inherits it; a failure aborts before anything runs.
//! 2. Install the stop handling, in the mode where a repeated signal stays a
//!    request (`shutdown::never_escalate`), and only then publish its own
//!    identity: a stopper that read the identity always finds the handler.
//! 3. Spawn the server as its own process-group leader, dying with the
//!    supervisor (parent-death SIGKILL), carrying `BROKKR_SERVE=<token>` and
//!    none of brokkr's hold variables, its streams on the generation log.
//! 4. Supervise it until it exits or a stop arrives; either way its group is
//!    SIGKILLed while the leader is still an unreaped child
//!    (`output::child::supervise_group_leader`).
//! 5. Sweep the generation's remaining carriers, append the ending to the
//!    log, and exit - which is what frees the flock.

use std::io::Write as _;
use std::os::fd::RawFd;
use std::path::Path;
use std::process::Stdio;

use super::registry::{self, Phase, State};

/// `argv[1]` of a supervisor invocation. Not a word any command takes.
pub(super) const SUPERVISOR_ARG: &str = "--brokkr-serve-supervisor";

/// Run as the supervisor when invoked as one; `None` for any other
/// invocation. Returns the process exit code.
pub(crate) fn maybe_run() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()?.to_str()? != SUPERVISOR_ARG {
        return None;
    }
    let entry = std::path::PathBuf::from(args.next()?);
    let fd: RawFd = args.next()?.to_str()?.parse().ok()?;
    Some(match run(&entry, fd) {
        Ok(()) => 0,
        Err(message) => {
            // The supervisor's streams are the generation log.
            log_line_stderr(&message);
            1
        }
    })
}

#[allow(clippy::print_stderr)] // The supervisor's stderr is the generation log, not a console.
fn log_line_stderr(message: &str) {
    eprintln!("brokkr serve: {message}");
}

fn run(entry: &Path, fd: RawFd) -> Result<(), String> {
    // 1. The flock stays with this process alone.
    // SAFETY: fcntl on a descriptor number this process was handed; it
    // touches only descriptor flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(format!(
            "cannot keep the serve flock from the server: {}",
            std::io::Error::last_os_error()
        ));
    }
    let dir = entry
        .parent()
        .ok_or_else(|| "the serve entry has no directory".to_owned())?;
    let token = entry
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(crate::lockfile::RunToken::parse)
        .ok_or_else(|| "the serve entry is not named by a token".to_owned())?;
    let found = registry::read_entry(dir, &token)
        .map_err(|e| format!("cannot read the serve entry: {e}"))?
        .ok_or_else(|| "the serve entry is gone".to_owned())?;
    let intent = found
        .intent
        .ok_or_else(|| "the serve entry holds no complete record".to_owned())?;

    // 2. Stop handling first, then the identity stoppers act on.
    crate::shutdown::never_escalate();
    let guard = crate::shutdown::SigtermGuard::install();
    let me = crate::process::ProcStat::read_self()
        .map_err(|e| format!("cannot read this process's identity: {e}"))?
        .identity();
    let mut state = State {
        supervisor: me,
        server: None,
        phase: Phase::Supervising,
    };
    registry::write_state(dir, &token, &state)
        .map_err(|e| format!("cannot publish the supervisor's state: {e}"))?;
    if crate::shutdown::is_shutdown_requested() {
        return Ok(());
    }

    // 3. The server.
    let shown = crate::output::path(&intent.log);
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&intent.log)
        .map_err(|e| format!("cannot open the log {shown}: {e}"))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot share the log: {e}"))?;
    let mut cmd = crate::process::spawn::ChildPolicy::plain_command(&intent.program);
    cmd.args(&intent.args)
        .current_dir(&intent.cwd)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    for name in crate::process::spawn::HOLD_BOUND_ENV {
        cmd.env_remove(name);
    }
    cmd.env(super::carriers::SERVE_ENV, token.as_str());
    match &intent.with {
        Some(with) => cmd.env(super::WITH_EXE_ENV, with),
        None => cmd.env_remove(super::WITH_EXE_ENV),
    };
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    crate::process::spawn::ChildPolicy::plain().apply_runtime_policy(&mut cmd);
    crate::shutdown::die_with_parent(&mut cmd);
    let child = crate::process::spawn::spawn(cmd, None).map_err(|e| e.to_string())?;
    let server_pid = crate::process::Pid::new(child.id());
    state.server = server_pid
        .and_then(|pid| crate::process::ProcStat::read(pid).ok())
        .map(|stat| stat.identity());
    state.phase = Phase::Serving;
    let published = registry::write_state(dir, &token, &state);

    // 4. Supervise. A state that could not be published still leaves the
    // server supervised; a stop then finds the supervisor by its first state.
    let ending = crate::output::child::supervise_group_leader(
        child,
        crate::shutdown::ChildIsolation::ProcessGroup(&guard),
        intent.grace,
    );

    // 5. What is left, then the record of how it ended. Nothing of the
    // supervisor's reaches the log once a server has served unless `Ended`
    // was published first: the launcher's `--ready` wait reads the log and
    // trusts a match only while the generation has not ended, so a line of
    // the supervisor's own must never pass for the server's.
    state.phase = Phase::Ended;
    let ended = registry::write_state(dir, &token, &state);
    let left = match super::carriers::sweep(token.as_str(), super::carriers::Exempt::Nothing) {
        super::carriers::Scan::Known(left) => left.len().to_string(),
        super::carriers::Scan::Unknown(why) => format!("unknown ({why})"),
    };
    drop(guard);
    if ended.is_err() {
        return Ok(());
    }
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(&intent.log)
        .map_err(|e| e.to_string())?;
    writeln!(
        log,
        "brokkr serve: server ended ({}); processes still carrying its token: {left}",
        describe(&ending)
    )
    .ok();
    published.map_err(|e| format!("cannot publish the server's state: {e}"))
}

fn describe(ending: &crate::output::child::Ending) -> String {
    use crate::output::child::Ending;
    match ending {
        Ending::Exited(code) => format!("exit code {code}"),
        Ending::Signalled(signal) => format!("signal {signal}"),
        Ending::Interrupted => "stopped".to_owned(),
        Ending::WaitFailed(why) => format!("wait failed: {why}"),
        Ending::StopMarker(_) | Ending::Deadline(_) => format!("{ending:?}"),
    }
}
