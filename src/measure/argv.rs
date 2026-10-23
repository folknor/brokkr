//! Render subprocess argument vectors for measured rows, and the resolved
//! invocation every measured spawn executes.

use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::output::{self, CapturedOutput};

/// The executable a binary plan was built into, as the measure runner hands
/// it to [`PlannedCommand`](crate::measure::runner::PlannedCommand), tagged
/// with the project it was built for. Opaque on purpose, enforced by types
/// rather than by a lint: a pbfhogg executable negotiates its flags, so it
/// must never be paired with arguments that skipped that. Its path is
/// readable only with a [`HandleKey`](crate::pbfhogg::binary::HandleKey),
/// which only the pbfhogg handle can make, and [`Invocation::built`] refuses
/// a pbfhogg executable outright.
#[derive(Clone, Copy)]
pub(crate) struct Executable<'a> {
    path: &'a Path,
    /// Built for pbfhogg, whose flags are negotiated through its handle.
    negotiated: bool,
}

impl<'a> Executable<'a> {
    /// Made by the measurement session that built `path`; `negotiated` says
    /// it is a pbfhogg build. A path is the scarce part: the session hands a
    /// pbfhogg one only to its handle.
    pub(crate) fn new(path: &'a Path, negotiated: bool) -> Self {
        Self { path, negotiated }
    }

    /// The path, for the one handle that may hold it.
    pub(crate) fn raw_path(self, _key: crate::pbfhogg::binary::HandleKey) -> &'a Path {
        self.path
    }

    /// What the runner would hand a plan, for tests outside `crate::measure`.
    #[cfg(test)]
    pub(crate) fn for_test(path: &'a Path, project: &crate::project::Project) -> Self {
        Self::new(path, *project == crate::project::Project::Pbfhogg)
    }
}

impl std::fmt::Debug for Executable<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Executable(..)")
    }
}

/// A reference tool brokkr runs by bare name from `PATH`. A closed set, so
/// code that may only spawn named tools cannot choose an arbitrary program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalTool {
    Osmium,
    Osmconvert,
}

impl ExternalTool {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Osmium => "osmium",
            Self::Osmconvert => "osmconvert",
        }
    }
}

/// An executable a named resolver outside the calling module located: a Java
/// runtime, a baseline built from its own manifest. Its constructor is the
/// resolver's, not the caller's.
pub(crate) struct ResolvedExecutable {
    path: PathBuf,
}

impl ResolvedExecutable {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl std::fmt::Debug for ResolvedExecutable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResolvedExecutable(..)")
    }
}

/// A program together with the arguments resolved for it - the only thing
/// the measured spawn paths execute.
///
/// The program is borrowed from whatever resolved the arguments (the runner's
/// built [`Executable`], a project handle such as
/// [`PbfhoggBinary`](crate::pbfhogg::binary::PbfhoggBinary) that negotiated
/// flags against that executable, a [`ResolvedExecutable`]), so an invocation
/// cannot outlive the resolution it came from, and the program a row records
/// is the program that ran. Nothing path-bearing is readable outside
/// `crate::measure`: callers spawn through [`capture_in`](Self::capture_in)
/// or the harness, and name failures through [`check`](Self::check).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Invocation<'a> {
    program: &'a Path,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl<'a> Invocation<'a> {
    pub(crate) fn new(program: &'a Path, args: Vec<String>) -> Self {
        Self {
            program,
            args,
            env: Vec::new(),
        }
    }

    /// The runner's built executable with a plan's argv. A pbfhogg
    /// executable is refused: its flags are negotiated, so it becomes an
    /// invocation only through
    /// [`PbfhoggBinary`](crate::pbfhogg::binary::PbfhoggBinary).
    pub(crate) fn built(executable: Executable<'a>, args: Vec<String>) -> Result<Self, DevError> {
        if executable.negotiated {
            return Err(DevError::Internal(
                "a pbfhogg executable runs only through PbfhoggBinary, which negotiates \
                 its flags; Invocation::built refuses it"
                    .into(),
            ));
        }
        Ok(Self::new(executable.path, args))
    }

    /// A reference tool from `PATH`.
    pub(crate) fn tool(tool: ExternalTool, args: Vec<String>) -> Invocation<'static> {
        Invocation::new(Path::new(tool.name()), args)
    }

    /// An executable a named resolver produced.
    pub(crate) fn resolved(executable: &'a ResolvedExecutable, args: Vec<String>) -> Self {
        Self::new(&executable.path, args)
    }

    /// Add one environment variable for the child.
    pub(crate) fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// Run to completion with captured output, in `cwd`. Does not check the
    /// exit status; see [`check`](Self::check).
    pub(crate) fn capture_in(&self, cwd: &Path) -> Result<CapturedOutput, DevError> {
        let program = self.program_string();
        let args = self.arg_refs();
        if self.env.is_empty() {
            output::run_captured(&program, &args, cwd)
        } else {
            let env: Vec<(&str, &str)> = self
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            output::run_captured_with_env(&program, &args, cwd, &env)
        }
    }

    /// Fail unless `captured` (this invocation's output) succeeded, naming
    /// the program.
    pub(crate) fn check(&self, captured: &CapturedOutput) -> Result<(), DevError> {
        captured.check_success(&self.program_string())
    }

    /// The `cli_args` column value for this invocation.
    pub(crate) fn cli_args(&self) -> String {
        format_cli_args(&self.program_string(), &self.arg_refs())
    }

    pub(in crate::measure) fn program(&self) -> &'a Path {
        self.program
    }

    pub(in crate::measure) fn program_string(&self) -> String {
        self.program.display().to_string()
    }

    /// Refuse to run this invocation on a path that cannot pass its
    /// environment, rather than silently running a different command.
    pub(in crate::measure) fn require_no_env(&self, path: &str) -> Result<(), DevError> {
        if self.env.is_empty() {
            Ok(())
        } else {
            Err(DevError::Internal(format!(
                "{path} cannot pass an invocation's environment; run it with capture_in"
            )))
        }
    }

    pub(in crate::measure) fn arg_refs(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }

    #[cfg(test)]
    pub(crate) fn program_for_test(&self) -> &'a Path {
        self.program
    }

    #[cfg(test)]
    pub(crate) fn args_for_test(&self) -> &[String] {
        &self.args
    }
}

impl std::fmt::Debug for Invocation<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invocation")
            .field("args", &self.args)
            .finish_non_exhaustive()
    }
}

/// Format a program path and argument slice into a single command-line string.
///
/// Quotes arguments that contain spaces. Used to populate `BenchConfig.cli_args`.
pub fn format_cli_args(program: &str, args: &[&str]) -> String {
    let mut parts = Vec::with_capacity(1 + args.len());
    parts.push(maybe_quote(program));
    for arg in args {
        parts.push(maybe_quote(arg));
    }
    parts.join(" ")
}

pub(super) fn maybe_quote(s: &str) -> String {
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s.to_owned()
    }
}
