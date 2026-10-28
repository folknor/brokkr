use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) const EXIT_SUCCESS: i32 = 0;
pub(crate) const EXIT_FAILURE: i32 = 1;
pub(crate) const EXIT_INTERRUPTED: i32 = 130;

/// Resolve the top-level process code from one handler result. The command
/// entry point should use this value for both history and the eventual exit.
pub(crate) fn result_exit_code(result: &Result<(), DevError>) -> i32 {
    match result {
        Ok(()) => EXIT_SUCCESS,
        Err(DevError::ExitCode(code)) => *code,
        Err(DevError::Interrupted) => EXIT_INTERRUPTED,
        Err(_) => EXIT_FAILURE,
    }
}

/// How a diagnostic should be rendered by `DevError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderingPolicy {
    /// The failure site printed the full diagnostic already; keep the stored
    /// label for callers that still need a concise summary.
    AlreadyPrinted,
    /// Render the message without a category prefix.
    Bare,
    /// Prefix the message with this category.
    Prefixed(&'static str),
}

/// A lock error outside acquisition, or a typed reason a fresh acquisition
/// refused to create a hold.
#[derive(Debug)]
pub enum LockError {
    Acquisition(AcquireRefusal),
    Other(String),
}

impl LockError {
    pub fn acquisition(refusal: AcquireRefusal) -> Self {
        Self::Acquisition(refusal)
    }
}

impl From<String> for LockError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl From<&str> for LockError {
    fn from(message: &str) -> Self {
        Self::Other(message.to_owned())
    }
}

/// Machine-readable reasons why a fresh global hold could not be established.
#[derive(Debug)]
pub enum AcquireRefusal {
    NestedDescendant {
        command: String,
    },
    /// A non-blocking acquisition found the lock held. `holder` is the
    /// holder's recorded command, when its record could be read.
    Busy {
        holder: Option<String>,
    },
    InsideCompilation {
        marker: &'static str,
    },
    DiskGate(Vec<String>),
    DrainTimeout {
        waited: Duration,
    },
    LockFileOpen {
        path: PathBuf,
        source: io::Error,
    },
    LockFlock {
        source: io::Error,
    },
    CompileLeaseOpen {
        path: PathBuf,
        source: io::Error,
    },
    CompileLeaseFlock {
        path: PathBuf,
        source: io::Error,
    },
    MetadataPublish {
        stage: MetadataStage,
        source: io::Error,
    },
    Entropy {
        purpose: EntropyPurpose,
    },
    SocketBind {
        path: PathBuf,
        source: io::Error,
    },
    SocketDirectory {
        reason: String,
    },
    ServiceThread {
        source: io::Error,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum MetadataStage {
    Draining,
    Capability,
}

#[derive(Debug, Clone, Copy)]
pub enum EntropyPurpose {
    AcquisitionId,
    Capability,
}

impl fmt::Display for AcquireRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NestedDescendant { command } => write!(
                f,
                "cannot take the brokkr lock independently while the hold you were started under is \
                 active - it is held by your own ancestor (`brokkr {command}`), which waiting here would never \
                 let go of. Run the inner command outside the outer one: run the built binary directly \
                 (for brokkr itself, build it and invoke `target/debug/brokkr ...`, or `brokkr install` \
                 and invoke the installed one), or run it before or after the outer command, not \
                 beneath it."
            ),
            Self::Busy { holder: Some(command) } => write!(
                f,
                "the brokkr lock is held (by `brokkr {command}`) and this command does not wait for it"
            ),
            Self::Busy { holder: None } => {
                f.write_str("the brokkr lock is held and this command does not wait for it")
            }
            Self::InsideCompilation { marker } => write!(
                f,
                "refusing to take the brokkr lock from inside a compilation brokkr admitted \
                 ({marker} is set). Taking it here would deadlock: this compiler holds a share of the \
                 compile lease that a new hold must drain. Run the brokkr command outside the \
                 build - a build script or proc macro cannot drive brokkr."
            ),
            Self::DiskGate(failures) => {
                f.write_str("preflight failed:")?;
                for failure in failures {
                    write!(f, "\n  - {failure}")?;
                }
                Ok(())
            }
            Self::DrainTimeout { waited } => write!(
                f,
                "compilation leases did not drain within {}s. A compiler started before this \
                 hold is still holding its lease, or a process that inherited its \
                 descriptor is. This is not a transient busy signal - an unchanged blocker \
                 will fail every retry. `brokkr strays` shows what is running; \
                 `brokkr lock` shows the holder.",
                waited.as_secs()
            ),
            Self::LockFileOpen { path, source } => {
                write!(f, "failed to open lock file {}: {source}", path.display())
            }
            Self::LockFlock { source } => write!(f, "flock failed: {source}"),
            Self::CompileLeaseOpen { path, source } => write!(
                f,
                "failed to open the compile lease lock {}: {source}",
                path.display()
            ),
            Self::CompileLeaseFlock { path, source } => write!(
                f,
                "could not take the compile lease lock at {}: {source}",
                path.display()
            ),
            Self::MetadataPublish {
                stage: MetadataStage::Draining,
                source,
            } => write!(f, "failed to publish lock metadata: {source}"),
            Self::MetadataPublish {
                stage: MetadataStage::Capability,
                source,
            } => write!(f, "failed to publish the compilation capability: {source}"),
            Self::Entropy { purpose } => match purpose {
                EntropyPurpose::AcquisitionId => {
                    f.write_str("could not read /dev/urandom to mint a lock acquisition id")
                }
                EntropyPurpose::Capability => f.write_str(
                    "could not read /dev/urandom to mint a compilation capability; a hold that cannot \
                     mint one would have every one of its own compiles refused",
                ),
            },
            Self::SocketBind { path, source } => {
                write!(f, "cannot bind lock socket {}: {source}", path.display())
            }
            Self::SocketDirectory { reason } => f.write_str(reason),
            Self::ServiceThread { source } => {
                write!(f, "cannot start the lock service thread: {source}")
            }
        }
    }
}

impl std::error::Error for AcquireRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LockFlock { source }
            | Self::LockFileOpen { source, .. }
            | Self::CompileLeaseOpen { source, .. }
            | Self::CompileLeaseFlock { source, .. }
            | Self::MetadataPublish { source, .. }
            | Self::SocketBind { source, .. }
            | Self::ServiceThread { source } => Some(source),
            Self::NestedDescendant { .. }
            | Self::Busy { .. }
            | Self::InsideCompilation { .. }
            | Self::DiskGate(_)
            | Self::DrainTimeout { .. }
            | Self::Entropy { .. }
            | Self::SocketDirectory { .. } => None,
        }
    }
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Acquisition(refusal) => refusal.fmt(f),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl StdError for LockError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Acquisition(refusal) => Some(refusal),
            Self::Other(_) => None,
        }
    }
}

/// Error type for the dev tool.
///
/// How a subprocess that ran ended: its exit code, or the signal that killed
/// it. On Unix every reaped child has exactly one of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessEnding {
    Code(i32),
    Signal(i32),
}

impl ProcessEnding {
    /// From a reaped child's status. A status with neither (not produced by a
    /// plain `wait`) is recorded by its raw wait value as a code, rather than
    /// invented as a signal.
    pub fn from_status(status: &std::process::ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt;
        match (status.code(), status.signal()) {
            (Some(code), _) => Self::Code(code),
            (None, Some(signal)) => Self::Signal(signal),
            (None, None) => Self::Code(status.into_raw()),
        }
    }
}

impl fmt::Display for ProcessEnding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(code) => write!(f, "exited with code {code}"),
            Self::Signal(libc::SIGKILL) => {
                write!(
                    f,
                    "killed by signal {} (SIGKILL - possible OOM kill)",
                    libc::SIGKILL
                )
            }
            Self::Signal(signal) => write!(f, "killed by signal {signal}"),
        }
    }
}

/// A supervised child produced an outcome brokkr could not accept as a
/// complete run result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildFailureKind {
    Deadline(Duration),
    CaptureLimit { stream: &'static str, limit: usize },
    OutputDrain { grace: Duration },
}

/// No `Box` wrapping - this is a binary, not a library. Keeping it simple.
#[derive(Debug)]
pub enum DevError {
    /// An I/O error.
    Io(io::Error),
    /// Configuration file parse or validation error.
    Config(String),
    /// Invalid command usage that is not rejected by clap itself.
    Usage(String),
    /// A valid command was run for the wrong detected project.
    WrongProject {
        command: String,
        expected: String,
        current: String,
    },
    /// An invariant or dispatch contract inside brokkr failed.
    Internal(String),
    /// Build failure (cargo returned non-zero, or compilation error).
    Build(String),
    /// Data supplied by an external process did not satisfy its documented
    /// output contract.
    ContractViolation(String),
    /// One or more preflight checks failed.
    Preflight(Vec<String>),
    /// A subprocess ran and ended unsuccessfully, with how it ended carried
    /// explicitly - an exit code or the signal that killed it - so rendering
    /// never has to guess from the captured stderr.
    Subprocess {
        program: String,
        ending: ProcessEnding,
        stderr: String,
    },
    /// A child exceeded a runtime deadline, capture limit, or output-drain
    /// grace. These are execution failures, not configuration errors.
    ChildFailure {
        program: String,
        failure: ChildFailureKind,
    },
    /// A subprocess could not be started (or waited on) at all - `program` is
    /// missing, not executable, or the OS refused. Distinct from
    /// [`DevError::Subprocess`] because a spawn failure also has no exit code,
    /// and rendering the two alike made a missing `curl` read as
    /// `curl killed by signal: No such file or directory`.
    Spawn { program: String, error: io::Error },
    /// A failure whose full diagnostic the failing site already printed. The
    /// label is a short name for it (`"gremlins found"`), for summaries that
    /// list what failed - never the detail itself.
    ///
    /// Exists so a summary (`check`'s `check failed in ...` line) can print
    /// every *other* error's message: before it, the summary printed none, on
    /// the convention that "the failing phase printed its own detail" - which
    /// held only where a site remembered, and swallowed the cause of every
    /// failure where it did not (an IO error, a `cargo metadata` failure, a
    /// config refusal discovered at phase time).
    Reported(String),
    /// A request refused before execution: an unknown or ambiguous selection,
    /// a missing baseline, or a modified worktree that cannot safely be reused.
    /// Renders bare; unlike [`DevError::Reported`], this is its first print.
    Refused(String),
    /// Lock file conflict (another dev instance is running).
    Lock(LockError),
    /// Database error (SQLite operations).
    Database(String),
    /// A verification check found a mismatch or unexpected result.
    Verify(String),
    /// A verification could not be completed for a reason outside the code
    /// under test: a reference tool was killed by a signal brokkr did not
    /// send (commonly the kernel OOM killer). Never a verdict on pbfhogg.
    VerifyEnvironment(String),
    /// A subprocess completed and the process should exit with this code.
    /// Used by passthrough commands (run, elivagar) to propagate exit codes
    /// without calling `process::exit` in handlers.
    ExitCode(i32),
    /// A `[cargo_env]` command could not produce its variable for a cargo
    /// invocation, or an earlier one could not and cargo is refused for the
    /// rest of the run. Like [`DevError::Reported`], the label only: the
    /// detail (the variable, command, directory and why) was printed once,
    /// when the failure was recorded. See [`crate::cargo_env`].
    CargoEnv(String),
    /// `brokkr kill` requested a cooperative shutdown (SIGTERM). The bench
    /// aborted mid-run, partial sidecar data was stored under the `dirty`
    /// alias, and `main` should run scratch cleanup and exit 130.
    Interrupted,
}

impl fmt::Display for DevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let policy = self.rendering_policy();
        match self {
            DevError::Io(err) => write_message(f, policy, &err.to_string()),
            DevError::Config(msg) => write_message(f, policy, msg),
            DevError::Usage(msg) => write_message(f, policy, msg),
            DevError::WrongProject {
                command,
                expected,
                current,
            } => write_message(
                f,
                policy,
                &format!(
                    "'brokkr {command}' is only available in {expected} projects (current: {current})"
                ),
            ),
            DevError::Internal(msg) => write_message(f, policy, msg),
            DevError::Build(msg) => write_message(f, policy, msg),
            DevError::ContractViolation(msg) => write_message(f, policy, msg),
            DevError::Preflight(failures) => {
                write!(f, "{}:", prefix(policy, "preflight failed"))?;
                for failure in failures {
                    write!(f, "\n  - {failure}")?;
                }
                Ok(())
            }
            DevError::Subprocess {
                program,
                ending,
                stderr,
            } => {
                write!(f, "{program} {ending}")?;
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            DevError::ChildFailure { program, failure } => match failure {
                ChildFailureKind::Deadline(limit) => write!(
                    f,
                    "{program} exceeded its {} deadline",
                    crate::output::format_duration(*limit)
                ),
                ChildFailureKind::CaptureLimit { stream, limit } => write!(
                    f,
                    "{program} exceeded the {stream} capture limit of {limit} bytes"
                ),
                ChildFailureKind::OutputDrain { grace } => write!(
                    f,
                    "{program} exited but its output stayed open beyond {}; capture may be incomplete",
                    crate::output::format_duration(*grace)
                ),
            },
            DevError::Spawn { program, error } => write!(f, "could not run {program}: {error}"),
            DevError::Reported(label) | DevError::Refused(label) => write!(f, "{label}"),
            DevError::Lock(msg) => write_message(f, policy, &msg.to_string()),
            DevError::Database(msg) => write_message(f, policy, msg),
            DevError::Verify(msg) | DevError::VerifyEnvironment(msg) => {
                write_message(f, policy, msg)
            }
            DevError::CargoEnv(label) => write!(f, "{label}"),
            DevError::ExitCode(_) => Ok(()),
            DevError::Interrupted => write!(f, "interrupted by shutdown request"),
        }
    }
}

impl DevError {
    /// The single source of truth for category prefixes and whether the full
    /// diagnostic was already emitted.
    pub fn rendering_policy(&self) -> RenderingPolicy {
        match self {
            Self::Reported(_) | Self::CargoEnv(_) => RenderingPolicy::AlreadyPrinted,
            Self::Refused(_)
            | Self::Subprocess { .. }
            | Self::ChildFailure { .. }
            | Self::Spawn { .. }
            | Self::ExitCode(_)
            | Self::Interrupted => RenderingPolicy::Bare,
            Self::Io(_) => RenderingPolicy::Prefixed("io"),
            Self::Config(_) => RenderingPolicy::Prefixed("config"),
            Self::Usage(_) => RenderingPolicy::Prefixed("usage"),
            Self::WrongProject { .. } => RenderingPolicy::Prefixed("wrong project"),
            Self::Internal(_) => RenderingPolicy::Prefixed("internal"),
            Self::Build(_) => RenderingPolicy::Prefixed("build"),
            Self::ContractViolation(_) => RenderingPolicy::Prefixed("contract violation"),
            Self::Preflight(_) => RenderingPolicy::Prefixed("preflight failed"),
            Self::Lock(_) => RenderingPolicy::Prefixed("lock"),
            Self::Database(_) => RenderingPolicy::Prefixed("database"),
            Self::Verify(_) => RenderingPolicy::Prefixed("verify"),
            Self::VerifyEnvironment(_) => RenderingPolicy::Prefixed("environment"),
        }
    }
}

fn prefix(policy: RenderingPolicy, fallback: &'static str) -> &'static str {
    match policy {
        RenderingPolicy::Prefixed(category) => category,
        RenderingPolicy::AlreadyPrinted | RenderingPolicy::Bare => fallback,
    }
}

fn write_message(
    f: &mut fmt::Formatter<'_>,
    policy: RenderingPolicy,
    message: &str,
) -> fmt::Result {
    match policy {
        RenderingPolicy::Prefixed(category) => write!(f, "{category}: {message}"),
        RenderingPolicy::AlreadyPrinted | RenderingPolicy::Bare => f.write_str(message),
    }
}

impl StdError for DevError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            DevError::Io(err) | DevError::Spawn { error: err, .. } => Some(err),
            DevError::Lock(err) => Some(err),
            DevError::Config(_)
            | DevError::Usage(_)
            | DevError::WrongProject { .. }
            | DevError::Internal(_)
            | DevError::Build(_)
            | DevError::ContractViolation(_)
            | DevError::Preflight(_)
            | DevError::Subprocess { .. }
            | DevError::ChildFailure { .. }
            | DevError::Database(_)
            | DevError::Verify(_)
            | DevError::VerifyEnvironment(_)
            | DevError::CargoEnv(_)
            | DevError::Reported(_)
            | DevError::Refused(_)
            | DevError::ExitCode(_)
            | DevError::Interrupted => None,
        }
    }
}

impl From<io::Error> for DevError {
    fn from(err: io::Error) -> DevError {
        DevError::Io(err)
    }
}

impl From<toml::de::Error> for DevError {
    fn from(err: toml::de::Error) -> DevError {
        DevError::Config(err.to_string())
    }
}

impl From<serde_json::Error> for DevError {
    fn from(err: serde_json::Error) -> DevError {
        // A bare serde error does not identify who supplied the JSON. Keep
        // the default for brokkr-owned data; external-contract readers must
        // classify failures at their parsing boundary.
        DevError::Internal(format!("json: {err}"))
    }
}

impl From<rusqlite::Error> for DevError {
    fn from(err: rusqlite::Error) -> DevError {
        DevError::Database(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{DevError, ProcessEnding};
    use std::io;

    #[test]
    fn a_spawn_failure_does_not_read_as_a_signal_death() {
        let e = DevError::Spawn {
            program: "curl".into(),
            error: io::Error::from(io::ErrorKind::NotFound),
        };
        let msg = e.to_string();
        assert!(msg.starts_with("could not run curl: "), "{msg}");
        assert!(!msg.contains("signal"), "{msg}");
    }

    #[test]
    fn a_signal_death_is_said_once_from_the_ending() {
        let e = DevError::Subprocess {
            program: "cargo fmt".into(),
            ending: ProcessEnding::Signal(15),
            stderr: String::new(),
        };
        let msg = e.to_string();
        assert_eq!(msg.matches("signal").count(), 1, "{msg}");
        assert_eq!(msg, "cargo fmt killed by signal 15");
    }

    #[test]
    fn a_signal_death_with_output_keeps_the_output_after_the_sentence() {
        let e = DevError::Subprocess {
            program: "x".into(),
            ending: ProcessEnding::Signal(9),
            stderr: "boom".into(),
        };
        assert_eq!(
            e.to_string(),
            "x killed by signal 9 (SIGKILL - possible OOM kill): boom"
        );
    }

    #[test]
    fn stderr_that_looks_like_a_signal_does_not_change_the_rendering() {
        let e = DevError::Subprocess {
            program: "x".into(),
            ending: ProcessEnding::Code(2),
            stderr: "killed by signal 9".into(),
        };
        assert_eq!(e.to_string(), "x exited with code 2: killed by signal 9");
    }

    #[test]
    fn refused_renders_its_message_alone() {
        assert_eq!(
            DevError::Refused("no bench targets".into()).to_string(),
            "no bench targets"
        );
    }

    #[test]
    fn reported_renders_its_label_alone() {
        assert_eq!(
            DevError::Reported("tests failed".into()).to_string(),
            "tests failed"
        );
    }
}
