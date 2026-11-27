//! The subprocess construction policy shared by brokkr's spawn choke points.
//!
//! Callers choose a policy by purpose instead of repeating cargo environment,
//! OOM, parent-death, orphan-token, and hold-capability setup. The environment
//! renderer is also used by nextest's generated Cargo config, whose engine
//! builds process commands without exposing them to brokkr: nextest gets one
//! `BROKKR_EXEC` cohort per lane.
//!
//! The hold capability is stamped last, after caller env, so nothing
//! displaces it. Probes and downloads with their own command builders go
//! through `output::child::command_output` and `command_status`, which apply
//! the policy after caller configuration. Process-group isolation
//! (`ChildIsolation::ProcessGroup`) can be chosen only while a `SigtermGuard`
//! is borrowed.
//!
//! Every launch through [`spawn`], [`spawn_exact`] and [`spawn_detached`] is
//! admitted against a committed stop (`brokkr kill`), in the parent before
//! the fork and in the child just before its exec ([`LaunchGuard`]). Nothing
//! is exempt: every supervisor already ends its child at its first poll of a
//! committed stop, so a launch after one could never do its work, and the
//! unwind restores state through filesystem operations, not subprocesses.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};

pub(crate) use crate::lockfile::observe::EXEC_ENV;

use crate::cargo_env::{self, Phase, SpawnProfile as CargoProfile};
use crate::error::DevError;
use crate::shutdown::ChildIsolation;

#[derive(Clone, Copy)]
enum ChildKind {
    Plain,
    TestProcess,
}

/// The named process policy: `Plain`, `Cargo`, `TestProcess`, or a
/// cargo-launched test process (`cargo_test`).
#[derive(Clone, Copy)]
pub(crate) struct ChildPolicy {
    cargo: CargoProfile,
    kind: ChildKind,
}

pub(crate) type FullEnv = BTreeMap<OsString, OsString>;

/// A command whose environment is supplied in full, including any brokkr
/// variables its caller explicitly adds.
pub(crate) struct FullEnvCommand {
    pub(crate) inner: Command,
}

impl ChildPolicy {
    pub(crate) const fn plain() -> Self {
        Self {
            cargo: CargoProfile::Plain,
            kind: ChildKind::Plain,
        }
    }

    pub(crate) const fn cargo(phase: Option<Phase>) -> Self {
        Self {
            cargo: CargoProfile::Cargo { phase },
            kind: ChildKind::Plain,
        }
    }

    pub(crate) const fn test_process() -> Self {
        Self {
            cargo: CargoProfile::Plain,
            kind: ChildKind::TestProcess,
        }
    }

    pub(crate) const fn cargo_test(phase: Option<Phase>) -> Self {
        Self {
            cargo: CargoProfile::Cargo { phase },
            kind: ChildKind::TestProcess,
        }
    }

    pub(crate) const fn from_cargo_profile(profile: CargoProfile) -> Self {
        match profile {
            CargoProfile::Plain => Self::plain(),
            CargoProfile::Cargo { phase } => Self::cargo(phase),
        }
    }

    /// The brokkr-owned environment carried by this policy. Used when a
    /// subprocess engine, such as nextest, constructs the child itself.
    ///
    /// The engine starts every process from this one rendering, so they all
    /// share one launch cohort (`BROKKR_EXEC`), labelled `label`: `brokkr
    /// lock` attributes them to the lane, not to each process.
    pub(crate) fn render_env(self, label: &str) -> Vec<(String, String)> {
        let launch = crate::lockfile::activity::launch_context(label);
        let mut env = crate::hold::child_env();
        if let Some(launch) = launch {
            // The capability of the same hold the cohort names.
            env.retain(|(name, _)| name != crate::hold::CAPABILITY_ENV);
            if let Some(nonce) = launch.capability {
                env.push((
                    crate::hold::CAPABILITY_ENV.to_owned(),
                    nonce.expose().to_owned(),
                ));
            }
            env.push((EXEC_ENV.to_owned(), launch.exec));
        }
        if self.carries_orphan_token()
            && let Some((name, value)) = crate::test_orphans::child_env()
        {
            env.push((name.to_owned(), value.to_owned()));
        }
        env
    }

    /// Build and configure one child command. Caller-provided environment is
    /// applied first; brokkr's capability policy is applied last so it cannot
    /// be displaced by a caller pair.
    pub(crate) fn command(
        self,
        program: impl AsRef<std::ffi::OsStr>,
        args: &[&str],
        cwd: Option<&Path>,
        env: &[(&str, &str)],
        isolation: ChildIsolation<'_>,
    ) -> Result<Command, DevError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        for &(name, value) in env {
            cmd.env(name, value);
        }

        cargo_env::apply(&mut cmd, self.cargo, args, cwd, env)?;
        self.apply_runtime_policy(&mut cmd);

        if isolation.is_process_group() {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        Ok(cmd)
    }

    /// [`ChildPolicy::command`] for [`spawn_exact`]: `program` an absolute
    /// path to execute as it is. What the caller may still set is only the
    /// three streams, so nothing `spawn_exact` cannot reproduce in its own
    /// `execve` (a cleared environment, an `arg0`) can reach it.
    pub(crate) fn exact(
        self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        env: &[(&str, &str)],
        isolation: ChildIsolation<'_>,
    ) -> Result<ExactCommand, DevError> {
        self.command(program, args, cwd, env, isolation)
            .map(|inner| ExactCommand { inner })
    }

    /// Clear inheritance and keep the plain child's non-environment policy.
    pub(crate) fn full_env(
        program: impl AsRef<OsStr>,
        args: &[&str],
        cwd: Option<&Path>,
        env: &FullEnv,
        isolation: ChildIsolation<'_>,
    ) -> FullEnvCommand {
        use std::os::unix::process::CommandExt as _;

        let mut inner = Command::new(program);
        inner.args(args).env_clear().envs(env);
        if let Some(cwd) = cwd {
            inner.current_dir(cwd);
        }
        if isolation.is_process_group() {
            inner.process_group(0);
        }
        crate::oom::protect_child(&mut inner);
        FullEnvCommand { inner }
    }

    /// Apply capability and lifecycle policy after the caller finishes its
    /// command environment. Cargo evaluation remains in `command`.
    pub(crate) fn apply_runtime_policy(self, cmd: &mut Command) {
        crate::oom::protect_child(cmd);
        if self.needs_parent_death() {
            crate::shutdown::die_with_parent(cmd);
        }
        if self.carries_orphan_token() {
            crate::test_orphans::stamp(cmd);
        }
        crate::hold::stamp(cmd);
    }

    /// Construct a plain command for callers needing non-text arguments or
    /// custom streams. `output::child::command_output` / `command_status`
    /// apply the policy after all caller configuration.
    pub(crate) fn plain_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
        Command::new(program)
    }

    fn needs_parent_death(self) -> bool {
        matches!(self.kind, ChildKind::TestProcess)
    }

    fn carries_orphan_token(self) -> bool {
        matches!(self.kind, ChildKind::TestProcess)
    }
}

/// Spawn a configured child and publish its PID to the caller immediately.
/// The caller's callback owns the lifetime of that publication and clears it
/// after the child has been reaped.
///
/// Under a hold, this is where the child's launch cohort is allocated and
/// stamped (`BROKKR_EXEC`), together with the capability of the same hold -
/// at the final boundary, so a command built earlier and spawned later is
/// attributed to the activity and acquisition it actually ran under. Without
/// a hold, both are left as the command already carries them (an inherited
/// cohort stays: the child still belongs to it).
///
/// Every launch through here is admitted against a committed stop
/// ([`LaunchGuard`]): refused before the fork, and again by the child as the
/// last step before its exec, as [`DevError::Interrupted`].
pub(crate) fn spawn(
    mut command: Command,
    on_spawn: Option<&dyn Fn(u32)>,
) -> Result<std::process::Child, DevError> {
    stamp(&mut command);
    spawn_command(command, on_spawn)
}

pub(crate) fn spawn_full_env(
    mut command: FullEnvCommand,
    on_spawn: Option<&dyn Fn(u32)>,
) -> Result<std::process::Child, DevError> {
    inherit_reservation(&mut command.inner);
    spawn_command(command.inner, on_spawn)
}

fn spawn_command(
    mut command: Command,
    on_spawn: Option<&dyn Fn(u32)>,
) -> Result<std::process::Child, DevError> {
    let program = command.get_program().to_string_lossy().into_owned();
    let gate = LaunchGuard::arm(&mut command, &program)?;
    let child = command
        .spawn()
        .map_err(|error| gate.classify(DevError::Spawn { program, error }))?;
    if let Some(on_spawn) = on_spawn {
        on_spawn(child.id());
    }
    Ok(child)
}

/// The byte a child writes to its gate pipe when the launch gate refuses it.
const GATE_REFUSED: u8 = b's';

/// The admission of one launch against a committed stop (`brokkr kill`).
///
/// A stop commits by closing a byte of memory every child shares
/// (`crate::shutdown::launch_gate`) and only then its in-process state. The
/// parent refuses a launch whose stop is already committed before it forks;
/// the child loads the shared byte as its last step before the exec and, if
/// closed, writes [`GATE_REFUSED`] to a close-on-exec pipe and fails the
/// spawn, so its program never starts. A child whose load precedes the commit
/// was admitted before the stop and is an ordinary tracked child: every
/// supervisor ends its child at its first poll of the stop. The stop service
/// never waits on a launch: committing is one store, so a wedged fork or exec
/// cannot hang it.
///
/// Only the marker tells a refusal from any other spawn failure, so a real
/// setup or exec error that races a stop keeps its meaning.
struct LaunchGuard {
    reader: std::os::fd::OwnedFd,
}

impl LaunchGuard {
    /// Refuse a launch under a committed stop, else register the child's gate
    /// as the command's last `pre_exec` step.
    fn arm(command: &mut Command, program: &str) -> Result<Self, DevError> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;
        let (gate, reader, writer) = gate_parts(program)?;
        let signal = writer.as_raw_fd();
        // SAFETY: the closure runs between fork and exec and calls only an
        // atomic load and write(2) on memory and a descriptor prepared before
        // the fork. The writer is close-on-exec and dropped by the parent
        // with the command, after the spawn.
        unsafe {
            command.pre_exec(move || {
                let _keep = &writer;
                if crate::shutdown::gate_open(gate) {
                    return Ok(());
                }
                libc::write(signal, [GATE_REFUSED].as_ptr().cast(), 1);
                Err(std::io::Error::from_raw_os_error(libc::ECANCELED))
            });
        }
        Ok(Self { reader })
    }

    /// A failed spawn's error: [`DevError::Interrupted`] when the child's
    /// gate refused it, else `error` as it is.
    fn classify(&self, error: DevError) -> DevError {
        if read_marker(&self.reader) == Some(GATE_REFUSED) {
            DevError::Interrupted
        } else {
            error
        }
    }
}

/// The gate a launch checks and a non-blocking close-on-exec pipe for the
/// child's marker, after refusing a launch whose stop is already committed.
/// Non-blocking: a child another thread forks meanwhile may inherit the write
/// end, so a closed write end in this process is no promise of EOF.
fn gate_parts(
    program: &str,
) -> Result<
    (
        &'static std::sync::atomic::AtomicU8,
        std::os::fd::OwnedFd,
        std::os::fd::OwnedFd,
    ),
    DevError,
> {
    use std::os::fd::{FromRawFd as _, OwnedFd};
    let gate = crate::shutdown::launch_gate().ok_or_else(|| DevError::Spawn {
        program: program.to_owned(),
        error: std::io::Error::other("cannot map the launch gate"),
    })?;
    if parent_admission() && (!crate::shutdown::gate_open(gate) || !crate::shutdown::admits_new_work())
    {
        return Err(DevError::Interrupted);
    }
    let mut fds = [0; 2];
    // SAFETY: a valid two-element buffer for the call's duration.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(DevError::Spawn {
            program: program.to_owned(),
            error: std::io::Error::last_os_error(),
        });
    }
    // SAFETY: pipe2 just returned these two descriptors, owned by nobody else.
    let (reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    Ok((gate, reader, writer))
}

/// Set by the launch-gate test to skip the parent's check, standing for a
/// stop committed after the parent admitted the launch and before its child
/// reaches the gate.
#[cfg(test)]
static SKIP_PARENT_ADMISSION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether the parent checks admission before the fork: always, except in
/// the test that reaches the child's gate past it.
fn parent_admission() -> bool {
    #[cfg(test)]
    {
        !SKIP_PARENT_ADMISSION.load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(not(test))]
    {
        true
    }
}

/// The one byte a failed child left in its marker pipe, if any. Once spawn
/// has returned, the child's marker is in the pipe or never will be, so the
/// non-blocking read is final.
fn read_marker(reader: &std::os::fd::OwnedFd) -> Option<u8> {
    use std::os::fd::AsRawFd as _;
    let mut byte = [0u8; 1];
    // SAFETY: a valid one-byte buffer.
    let read = unsafe { libc::read(reader.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
    (read == 1).then_some(byte[0])
}

/// The variables that tie a process to a hold, a run or a sidecar. A process
/// spawned to outlive the hold ([`spawn_detached`]) carries none of them: it
/// is no descendant of any hold, its compiles are no hold's, and no reaper or
/// launch cohort owns it.
pub(crate) const HOLD_BOUND_ENV: &[&str] = &[
    crate::hold::CAPABILITY_ENV,
    EXEC_ENV,
    crate::test_orphans::MARKER_ENV,
    crate::hold::LEASE_MARKER_ENV,
    crate::sidecar::MARKER_FIFO_ENV,
];

/// Spawn a process meant to outlive the current hold: the `brokkr serve`
/// supervisor (`crate::serve`). It bypasses [`stamp`] entirely, so no launch
/// cohort, capability or armed reservation reaches it, and the caller must not
/// have applied a runtime policy (no parent-death, no orphan token). Before it
/// execs, the child leaves brokkr's session and process group (`setsid`), so
/// no terminal signal and no group signal of brokkr's reaches it, and, where
/// the hold runs inside its own accounting cgroup, moves into the holder's
/// original cgroup; a move that fails fails the spawn, since a process left
/// in the hold's group would keep it populated after the hold.
///
/// `keep` is the one descriptor the child inherits beyond its streams; every
/// other descriptor brokkr opens is close-on-exec.
pub(crate) fn spawn_detached(
    mut command: Command,
    keep: std::os::fd::RawFd,
) -> Result<std::process::Child, DevError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt as _;

    let program = command.get_program().to_string_lossy().into_owned();
    for name in HOLD_BOUND_ENV {
        command.env_remove(name);
    }
    let cgroup =
        crate::lockfile::current_hold_cgroup_parent().map_err(|error| DevError::Spawn {
            program: program.clone(),
            error,
        })?;
    let cgroup_fd = cgroup.as_ref().map(AsRawFd::as_raw_fd);
    // SAFETY: the closure runs between fork and exec and calls only
    // async-signal-safe functions (setsid, fcntl, openat, write, close) on
    // descriptors and a literal prepared before the fork.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(keep, libc::F_GETFD);
            if flags < 0 || libc::fcntl(keep, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(dir) = cgroup_fd {
                let procs = libc::openat(
                    dir,
                    c"cgroup.procs".as_ptr(),
                    libc::O_WRONLY | libc::O_CLOEXEC,
                );
                if procs < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let written = libc::write(procs, b"0".as_ptr().cast(), 1);
                let error = std::io::Error::last_os_error();
                libc::close(procs);
                if written != 1 {
                    return Err(error);
                }
            }
            Ok(())
        });
    }
    // Detachment decides who owns the process, not whether it may start: a
    // serve launched after a committed stop is refused like any launch.
    let gate = LaunchGuard::arm(&mut command, &program)?;
    let child = command
        .spawn()
        .map_err(|error| gate.classify(DevError::Spawn { program, error }))?;
    drop(cgroup);
    Ok(child)
}

/// Transfer an armed reservation without changing the command environment.
fn inherit_reservation(command: &mut Command) -> bool {
    // A reserved launch (`crate::reservations`): this child, and only this
    // one, inherits the lease, dies with its parent where the kernel allows,
    // and carries the orphan token, so a brokkr SIGKILLed under it leaves an
    // orphan the next hold reaps rather than one that runs on.
    if let Some(fd) = crate::reservations::take_armed(command.get_program()) {
        crate::reservations::inherit_in_child(command, fd);
        crate::shutdown::die_with_parent(command);
        return true;
    }
    false
}

fn stamp(command: &mut Command) {
    if inherit_reservation(command) {
        crate::test_orphans::stamp(command);
    }
    stamp_launch_context(command);
}

fn stamp_launch_context(command: &mut Command) {
    if let Some(launch) = crate::lockfile::activity::launch_context(&launch_label(command)) {
        command.env(EXEC_ENV, launch.exec);
        match launch.capability {
            Some(nonce) => command.env(crate::hold::CAPABILITY_ENV, nonce.expose()),
            None => command.env_remove(crate::hold::CAPABILITY_ENV),
        };
    }
}

/// Retain all inputs of a prepared Cargo command, including policy stamps
/// and explicit unsets, so its build and probe spawn from the same value.
pub(crate) fn freeze_env(mut command: Command) -> FullEnv {
    crate::hold::stamp(&mut command);
    stamp_launch_context(&mut command);
    let mut env: FullEnv = std::env::vars_os().collect();
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            env.insert(name.to_owned(), value.to_owned());
        } else {
            env.remove(name);
        }
    }
    env
}

/// Why [`spawn_exact`] started nothing.
#[derive(Debug)]
pub(crate) enum ExactSpawnError {
    /// Everything before the exec failed: the working directory, brokkr's
    /// own setup in the child, the fork. brokkr's failure, not the
    /// program's.
    Setup(DevError),
    /// `execve` of the program itself refused it: not there, not
    /// permitted, no format the kernel runs, an interpreter its `#!` names
    /// that is missing. The program's failure.
    Exec(std::io::Error),
}

/// [`spawn`] for a command whose program is an absolute path to run exactly:
/// the child calls `execve` itself as the last step of its setup, with the
/// environment the command carries, so libc's lookup and its `/bin/sh`
/// fallback for a file in no executable format never apply. Which stage
/// failed is told apart by a close-on-exec pipe the child writes one byte to
/// as it reaches the exec: an `x` means the exec itself refused. The launch
/// gate ([`LaunchGuard`]) runs just before that write and marks its refusal
/// with a byte of its own, which is [`DevError::Interrupted`].
pub(crate) fn spawn_exact(command: ExactCommand) -> Result<std::process::Child, ExactSpawnError> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;

    let mut command = command.inner;
    let program = command.get_program().to_string_lossy().into_owned();
    let setup = |error: std::io::Error| {
        ExactSpawnError::Setup(DevError::Spawn {
            program: program.clone(),
            error,
        })
    };
    stamp(&mut command);
    let exact = Exact::of(&command).map_err(setup)?;
    // The launch gate ([`LaunchGuard`]) shares this pipe: a refusal writes
    // its own byte where the exec stage writes `x`.
    let (gate, reader, writer) = gate_parts(&program).map_err(ExactSpawnError::Setup)?;
    let signal = writer.as_raw_fd();
    // SAFETY: the closure runs between fork and exec and calls only
    // async-signal-safe functions (an atomic load, write, execve) on memory
    // prepared before the fork; it is registered last, so every other setup
    // has run. A marker that cannot be written fails the spawn unmarked, as
    // a setup failure: brokkr's, never a refusal of the program.
    unsafe {
        command.pre_exec(move || {
            if !crate::shutdown::gate_open(gate) {
                libc::write(signal, [GATE_REFUSED].as_ptr().cast(), 1);
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            if libc::write(signal, b"x".as_ptr().cast(), 1) != 1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::execve(exact.path(), exact.argv(), exact.envp());
            Err(std::io::Error::last_os_error())
        });
    }
    let spawned = command.spawn();
    drop(writer);
    match spawned {
        Ok(child) => Ok(child),
        Err(error) => match read_marker(&reader) {
            Some(GATE_REFUSED) => Err(ExactSpawnError::Setup(DevError::Interrupted)),
            Some(_) => Err(ExactSpawnError::Exec(error)),
            None => Err(setup(error)),
        },
    }
}

/// A command [`spawn_exact`] runs, made by [`ChildPolicy::exact`]: only its
/// streams can be set, so its environment is the inherited one with the
/// command's own changes and its `argv[0]` its program, which is what
/// `spawn_exact` passes to `execve`.
pub(crate) struct ExactCommand {
    inner: Command,
}

impl ExactCommand {
    pub(crate) fn stdin(&mut self, stdio: Stdio) -> &mut Self {
        self.inner.stdin(stdio);
        self
    }

    pub(crate) fn stdout(&mut self, stdio: Stdio) -> &mut Self {
        self.inner.stdout(stdio);
        self
    }

    pub(crate) fn stderr(&mut self, stdio: Stdio) -> &mut Self {
        self.inner.stderr(stdio);
        self
    }
}

/// The program, argv and environment of a command as C strings, with the
/// pointer arrays `execve` takes, made before the fork so the child
/// allocates nothing.
struct Exact {
    _strings: Vec<std::ffi::CString>,
    path: *const libc::c_char,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
}

// SAFETY: the pointers point into `_strings`, owned by the same value and
// never mutated; the value is only read, by the child after the fork.
unsafe impl Send for Exact {}
// SAFETY: as for Send: shared reads of immutable memory.
unsafe impl Sync for Exact {}

impl Exact {
    fn of(command: &Command) -> std::io::Result<Self> {
        use std::os::unix::ffi::OsStrExt as _;
        let c = |bytes: &[u8]| {
            std::ffi::CString::new(bytes)
                .map_err(|_| std::io::Error::other("an argument or variable holds a NUL byte"))
        };
        let mut env: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        for (name, value) in command.get_envs() {
            match value {
                Some(value) => env.insert(name.to_owned(), value.to_owned()),
                None => env.remove(name),
            };
        }
        let mut strings = vec![c(command.get_program().as_bytes())?];
        strings.push(c(command.get_program().as_bytes())?);
        for arg in command.get_args() {
            strings.push(c(arg.as_bytes())?);
        }
        let argc = strings.len() - 1;
        for (name, value) in &env {
            let mut pair = name.as_bytes().to_vec();
            pair.push(b'=');
            pair.extend_from_slice(value.as_bytes());
            strings.push(c(&pair)?);
        }
        let path = strings[0].as_ptr();
        let mut argv: Vec<*const libc::c_char> =
            strings[1..=argc].iter().map(|s| s.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<*const libc::c_char> =
            strings[argc + 1..].iter().map(|s| s.as_ptr()).collect();
        envp.push(std::ptr::null());
        Ok(Self {
            _strings: strings,
            path,
            argv,
            envp,
        })
    }

    fn path(&self) -> *const libc::c_char {
        self.path
    }

    fn argv(&self) -> *const *const libc::c_char {
        self.argv.as_ptr()
    }

    fn envp(&self) -> *const *const libc::c_char {
        self.envp.as_ptr()
    }
}

/// The launch label `brokkr lock` shows for a child: the program's file name
/// and its first few arguments.
fn launch_label(command: &Command) -> String {
    let program = command.get_program().to_string_lossy();
    let name = program.rsplit('/').next().unwrap_or(&program).to_owned();
    let args: Vec<String> = command
        .get_args()
        .take(3)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if args.is_empty() {
        name
    } else {
        format!("{name} {}", args.join(" "))
    }
}

/// Configure a captured command's streams after the common process policy.
pub(crate) fn capture_stdio(command: &mut Command) {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
}

#[cfg(test)]
mod exact_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    fn exact(program: &str, args: &[&str], cwd: &Path) -> ExactCommand {
        let mut command = ChildPolicy::plain()
            .exact(
                program,
                args,
                Some(cwd),
                &[("BROKKR_EXACT_TEST", "set")],
                ChildIsolation::Inherit,
            )
            .unwrap();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    /// The child gets the command's environment over the inherited one and
    /// runs in its directory, its argv as given.
    #[test]
    fn the_exec_carries_the_commands_environment_and_argv() {
        let dir = crate::test_scratch::scratch("spawn_exact", "env");
        let command = exact(
            "/bin/sh",
            &[
                "-c",
                "printf '%s|%s|%s' \"$BROKKR_EXACT_TEST\" \"$1\" \"$PWD\"",
                "sh",
                "arg",
            ],
            &dir,
        );
        let output = spawn_exact(command).unwrap().wait_with_output().unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        // $PWD is the directory as the kernel resolved it, through any link.
        let canonical = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(text, format!("set|arg|{}", canonical.display()));
    }

    /// A blocking pipe, read end first, as raw descriptors the test leaks.
    fn blocking_pipe() -> (i32, i32) {
        let mut fds = [0; 2];
        // SAFETY: a valid two-element buffer.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        (fds[0], fds[1])
    }

    /// Set in the child process of the launch-gate test: closing the gate is
    /// sticky and process-wide, so it happens only in a process of its own.
    const GATE_CHILD: &str = "BROKKR_TEST_LAUNCH_GATE_CHILD";

    /// A launch the parent admitted is refused by its child's gate once the
    /// gate closes, through every spawn entry, as `Interrupted` and with the
    /// program never run; with the parent's check back, a committed stop
    /// refuses before the fork.
    #[test]
    fn a_closed_launch_gate_refuses_the_child_before_its_exec() {
        if std::env::var_os(GATE_CHILD).is_some() {
            let dir = crate::test_scratch::scratch("spawn_gate", "child");
            let ran = dir.join("ran");
            let script = format!("touch {}", ran.display());
            let mut open = ChildPolicy::plain_command("/bin/sh");
            open.args(["-c", "true"]);
            let open = spawn(open, None).unwrap().wait().unwrap();
            assert!(open.success(), "an open gate admits");
            // The gate closes after the fork: the child, parked in an earlier
            // hook, sees the parent's store only through the shared page.
            let (forked_r, forked_w) = blocking_pipe();
            let (release_r, release_w) = blocking_pipe();
            let mut parked = ChildPolicy::plain_command("/bin/sh");
            parked.args(["-c", &script]);
            // SAFETY: write and read on descriptors made before the fork.
            unsafe {
                use std::os::unix::process::CommandExt as _;
                parked.pre_exec(move || {
                    let mut byte = [0u8; 1];
                    libc::write(forked_w, b"f".as_ptr().cast(), 1);
                    libc::read(release_r, byte.as_mut_ptr().cast(), 1);
                    Ok(())
                });
            }
            let closer = std::thread::spawn(move || {
                let mut byte = [0u8; 1];
                // SAFETY: a valid one-byte buffer and descriptors this test owns.
                unsafe {
                    libc::read(forked_r, byte.as_mut_ptr().cast(), 1);
                    crate::shutdown::close_launch_gate_only();
                    libc::write(release_w, b"r".as_ptr().cast(), 1);
                }
            });
            assert!(matches!(spawn(parked, None), Err(DevError::Interrupted)));
            closer.join().unwrap();
            assert!(!ran.exists(), "a child forked before the close never runs");
            // Past the parent's check: only the child's gate can refuse.
            SKIP_PARENT_ADMISSION.store(true, std::sync::atomic::Ordering::SeqCst);
            let mut plain = ChildPolicy::plain_command("/bin/sh");
            plain.args(["-c", &script]);
            assert!(matches!(spawn(plain, None), Err(DevError::Interrupted)));
            assert!(matches!(
                spawn_exact(exact("/bin/sh", &["-c", &script], &dir)),
                Err(ExactSpawnError::Setup(DevError::Interrupted))
            ));
            let mut detached = ChildPolicy::plain_command("/bin/sh");
            detached.args(["-c", &script]);
            assert!(matches!(
                spawn_detached(detached, 2),
                Err(DevError::Interrupted)
            ));
            assert!(!ran.exists(), "a refused child never runs its program");
            SKIP_PARENT_ADMISSION.store(false, std::sync::atomic::Ordering::SeqCst);
            assert!(crate::shutdown::admits_new_work(), "only the gate is closed");
            crate::shutdown::commit_stop();
            let mut late = ChildPolicy::plain_command("/bin/sh");
            late.args(["-c", &script]);
            assert!(matches!(spawn(late, None), Err(DevError::Interrupted)));
            assert!(!ran.exists());
            return;
        }
        let ran = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::spawn::exact_tests::a_closed_launch_gate_refuses_the_child_before_its_exec",
                "--nocapture",
            ])
            .env(GATE_CHILD, "1")
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{}{}",
            String::from_utf8_lossy(&ran.stdout),
            String::from_utf8_lossy(&ran.stderr)
        );
    }

    /// A file exec refuses is the exec's refusal, never run through a shell;
    /// a working directory that is gone fails the setup before it.
    #[test]
    fn the_exec_stage_is_told_from_the_setup_before_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_scratch::scratch("spawn_exact", "stages");
        let junk = dir.join("junk");
        std::fs::write(&junk, b"echo through a shell\n").unwrap();
        std::fs::set_permissions(&junk, std::fs::Permissions::from_mode(0o755)).unwrap();
        match spawn_exact(exact(&junk.display().to_string(), &[], &dir)) {
            Err(ExactSpawnError::Exec(error)) => {
                assert_eq!(error.raw_os_error(), Some(libc::ENOEXEC));
            }
            other => panic!("expected the exec's refusal: {other:?}"),
        }
        match spawn_exact(exact("/bin/sh", &["-c", "true"], &dir.join("gone"))) {
            Err(ExactSpawnError::Setup(_)) => {}
            other => panic!("expected a setup failure: {other:?}"),
        }
    }
}
