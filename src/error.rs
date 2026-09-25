use std::error::Error as StdError;
use std::fmt;
use std::io;

/// Error type for the dev tool.
///
/// No `Box` wrapping - this is a binary, not a library. Keeping it simple.
#[derive(Debug)]
pub enum DevError {
    /// An I/O error.
    Io(io::Error),
    /// Configuration file parse or validation error.
    Config(String),
    /// Build failure (cargo returned non-zero, or compilation error).
    Build(String),
    /// One or more preflight checks failed.
    Preflight(Vec<String>),
    /// A subprocess ran and ended unsuccessfully. `code` is `None` when it
    /// ended without an exit code - on Unix, a signal death; constructors that
    /// know the signal say so in `stderr`, since this variant does not carry it.
    Subprocess {
        program: String,
        code: Option<i32>,
        stderr: String,
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
    /// Lock file conflict (another dev instance is running).
    Lock(String),
    /// Database error (SQLite operations).
    Database(String),
    /// A verification check found a mismatch or unexpected result.
    Verify(String),
    /// A subprocess completed and the process should exit with this code.
    /// Used by passthrough commands (run, elivagar) to propagate exit codes
    /// without calling `process::exit` in handlers.
    ExitCode(i32),
    /// `brokkr kill` requested a cooperative shutdown (SIGTERM). The bench
    /// aborted mid-run, partial sidecar data was stored under the `dirty`
    /// alias, and `main` should run scratch cleanup and exit 130.
    Interrupted,
}

impl fmt::Display for DevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DevError::Io(err) => write!(f, "io: {err}"),
            DevError::Config(msg) => write!(f, "config: {msg}"),
            DevError::Build(msg) => write!(f, "build: {msg}"),
            DevError::Preflight(failures) => {
                write!(f, "preflight failed:")?;
                for failure in failures {
                    write!(f, "\n  - {failure}")?;
                }
                Ok(())
            }
            DevError::Subprocess {
                program,
                code,
                stderr,
            } => {
                write!(f, "{program}")?;
                // No exit code is a signal death on Unix, but the variant does
                // not carry the signal: constructors that know it put it in
                // `stderr` ("killed by signal 9"), and saying "killed by
                // signal" here as well doubled it.
                match code {
                    Some(c) => write!(f, " exited with code {c}")?,
                    None => write!(f, " ended without an exit code")?,
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            DevError::Spawn { program, error } => write!(f, "could not run {program}: {error}"),
            DevError::Reported(label) => write!(f, "{label}"),
            DevError::Lock(msg) => write!(f, "lock: {msg}"),
            DevError::Database(msg) => write!(f, "database: {msg}"),
            DevError::Verify(msg) => write!(f, "verify: {msg}"),
            DevError::ExitCode(_) => Ok(()),
            DevError::Interrupted => write!(f, "interrupted by shutdown request"),
        }
    }
}

impl StdError for DevError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            DevError::Io(err) | DevError::Spawn { error: err, .. } => Some(err),
            DevError::Config(_)
            | DevError::Build(_)
            | DevError::Preflight(_)
            | DevError::Subprocess { .. }
            | DevError::Lock(_)
            | DevError::Database(_)
            | DevError::Verify(_)
            | DevError::Reported(_)
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
        DevError::Build(format!("json: {err}"))
    }
}

impl From<rusqlite::Error> for DevError {
    fn from(err: rusqlite::Error) -> DevError {
        DevError::Database(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::DevError;
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
    fn a_signal_named_by_the_constructor_is_said_once() {
        let e = DevError::Subprocess {
            program: "cargo fmt".into(),
            code: None,
            stderr: "killed by signal 9".into(),
        };
        let msg = e.to_string();
        assert_eq!(msg.matches("signal").count(), 1, "{msg}");
    }

    #[test]
    fn reported_renders_its_label_alone() {
        assert_eq!(DevError::Reported("tests failed".into()).to_string(), "tests failed");
    }
}
