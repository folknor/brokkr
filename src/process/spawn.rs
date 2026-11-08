//! The subprocess construction policy shared by brokkr's spawn choke points.
//!
//! Callers choose a policy by purpose instead of repeating cargo environment,
//! OOM, parent-death, orphan-token, and hold-capability setup. The environment
//! renderer is also used by nextest's generated Cargo config, whose engine
//! builds process commands without exposing them to brokkr.

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
    Service,
}

/// The named process policy: `Plain`, `Cargo`, `TestProcess`, `Service`, or a
/// cargo-launched test process (`cargo_test`).
#[derive(Clone, Copy)]
pub(crate) struct ChildPolicy {
    cargo: CargoProfile,
    kind: ChildKind,
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

    pub(crate) const fn service() -> Self {
        Self {
            cargo: CargoProfile::Plain,
            kind: ChildKind::Service,
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
        program: &str,
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
        matches!(self.kind, ChildKind::TestProcess | ChildKind::Service)
    }

    fn carries_orphan_token(self) -> bool {
        matches!(self.kind, ChildKind::TestProcess | ChildKind::Service)
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
pub(crate) fn spawn(
    mut command: Command,
    on_spawn: Option<&dyn Fn(u32)>,
) -> Result<std::process::Child, DevError> {
    let program = command.get_program().to_string_lossy().into_owned();
    stamp(&mut command);
    let child = command
        .spawn()
        .map_err(|error| DevError::Spawn { program, error })?;
    if let Some(on_spawn) = on_spawn {
        on_spawn(child.id());
    }
    Ok(child)
}

/// What the spawn choke point adds to every command at the last moment.
fn stamp(command: &mut Command) {
    // A reserved launch (`crate::reservations`): this child, and only this
    // one, inherits the lease, dies with its parent where the kernel allows,
    // and carries the orphan token, so a brokkr SIGKILLed under it leaves an
    // orphan the next hold reaps rather than one that runs on.
    if let Some(fd) = crate::reservations::take_armed(command.get_program()) {
        crate::reservations::inherit_in_child(command, fd);
        crate::shutdown::die_with_parent(command);
        crate::test_orphans::stamp(command);
    }
    if let Some(launch) = crate::lockfile::activity::launch_context(&launch_label(command)) {
        command.env(EXEC_ENV, launch.exec);
        match launch.capability {
            Some(nonce) => command.env(crate::hold::CAPABILITY_ENV, nonce.expose()),
            None => command.env_remove(crate::hold::CAPABILITY_ENV),
        };
    }
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
/// as it reaches the exec: a byte means the exec itself refused.
pub(crate) fn spawn_exact(command: ExactCommand) -> Result<std::process::Child, ExactSpawnError> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
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
    let mut fds = [0; 2];
    // Non-blocking: a child another thread forks meanwhile may inherit the
    // write end, so a closed write end in this process is no promise of EOF.
    // SAFETY: a valid two-element buffer for the call's duration.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(setup(std::io::Error::last_os_error()));
    }
    // SAFETY: pipe2 just returned these two descriptors, owned by nobody else.
    let (reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let signal = writer.as_raw_fd();
    // SAFETY: the closure runs between fork and exec and calls only
    // async-signal-safe functions (write, execve) on memory prepared before
    // the fork; it is registered last, so every other setup has run. A
    // marker that cannot be written fails the spawn unmarked, as a setup
    // failure: brokkr's, never a refusal of the program.
    unsafe {
        command.pre_exec(move || {
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
        Err(error) => {
            let mut byte = [0u8; 1];
            // SAFETY: a valid one-byte buffer. Once spawn has returned, the
            // failed child's marker is in the pipe or never will be, and the
            // read does not block.
            let read = unsafe { libc::read(reader.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
            if read == 1 {
                Err(ExactSpawnError::Exec(error))
            } else {
                Err(setup(error))
            }
        }
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
