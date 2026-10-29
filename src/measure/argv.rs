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
    /// What each launch reserves (`crate::reservations`); `None` runs the
    /// argv as given.
    spec: Option<crate::reservations::LaunchSpec>,
}

impl<'a> Invocation<'a> {
    pub(crate) fn new(program: &'a Path, args: Vec<String>) -> Self {
        Self {
            program,
            args,
            env: Vec::new(),
            spec: None,
        }
    }

    /// An invocation each of whose launches reserves `spec`. Its argv may
    /// carry [`crate::reservations::PRIVATE_DIR_PLACEHOLDER`], replaced per
    /// launch by that launch's own private directory.
    pub(crate) fn reserved(
        program: &'a Path,
        args: Vec<String>,
        spec: crate::reservations::LaunchSpec,
    ) -> Self {
        Self {
            spec: Some(spec),
            ..Self::new(program, args)
        }
    }

    /// Admit one launch in `cwd`: its argv, and the reservation to finish
    /// once the child has been reaped.
    pub(in crate::measure) fn launch(
        &self,
        cwd: &Path,
    ) -> Result<crate::reservations::Launched, DevError> {
        self.launch_with(cwd, &[])
    }

    /// [`launch`](Self::launch), with `disposable` files the launcher adds
    /// beside the argv, such as a profiler's report: reserved while the
    /// launch lives and removed with its lease.
    pub(in crate::measure) fn launch_with(
        &self,
        cwd: &Path,
        disposable: &[std::path::PathBuf],
    ) -> Result<crate::reservations::Launched, DevError> {
        match &self.spec {
            Some(spec) => spec.launch(self.program, &self.args, cwd, disposable),
            None => Ok(crate::reservations::Launched::bare(self.args.clone())),
        }
    }

    #[cfg(test)]
    pub(crate) fn spec_for_test(&self) -> Option<&crate::reservations::LaunchSpec> {
        self.spec.as_ref()
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
        let launched = self.launch(cwd)?;
        let args = launched.arg_refs();
        let captured = if self.env.is_empty() {
            output::run_captured(&program, &args, cwd)
        } else {
            let env: Vec<(&str, &str)> = self
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            output::run_captured_with_env(&program, &args, cwd, &env)
        };
        // The child has been reaped (or never spawned); a lease it left held
        // is this run's failure even when the run itself succeeded.
        let finished = launched.finish();
        let captured = match captured {
            Ok(captured) => captured,
            Err(e) => {
                if let Err(release) = finished {
                    output::warn(&release.to_string());
                }
                return Err(e);
            }
        };
        finished?;
        Ok(captured)
    }

    /// Run to completion in `cwd` with stdout written to a new file at
    /// `stdout` (created, or truncated, here) and stderr captured bounded;
    /// see [`output::run_stdout_to_file`]. The launch lease is released (and
    /// a lease the child left held is this run's failure) before this
    /// returns, so the file is complete when the caller reads it.
    pub(crate) fn run_to_file(
        &self,
        cwd: &Path,
        stdout: &Path,
    ) -> Result<output::FileSinkRun, DevError> {
        let program = self.program_string();
        let sink = std::fs::File::create(stdout)
            .map_err(|e| DevError::Config(format!("cannot create {}: {e}", stdout.display())))?;
        let launched = self.launch(cwd)?;
        let args = launched.arg_refs();
        let env: Vec<(&str, &str)> = self
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let run = output::run_stdout_to_file(&program, &args, cwd, &env, sink);
        let finished = launched.finish();
        let run = match run {
            Ok(run) => run,
            Err(e) => {
                if let Err(release) = finished {
                    output::warn(&release.to_string());
                }
                return Err(e);
            }
        };
        finished?;
        Ok(run)
    }

    /// Run once in `cwd`, killed at `deadline`, each stream kept to `cap`
    /// (silently past it, so a caller that must see a longer stream asks for
    /// one byte more than it keeps), `unset` removed from the inherited
    /// environment, in its own process group. Unlike
    /// [`capture_in`](Self::capture_in), the release is never folded into a
    /// spawn failure: a launch whose admission or release did not establish a
    /// safe state is [`Supervised::Unsafe`], whatever else happened.
    pub(crate) fn capture_supervised(
        &self,
        cwd: &Path,
        deadline: std::time::Duration,
        cap: output::child::CaptureCap,
        unset: &[&str],
    ) -> Supervised {
        if crate::shutdown::should_unwind() {
            return Supervised::Interrupted;
        }
        let launched = match self.launch(cwd) {
            Ok(launched) => launched,
            // A refusal may come after the lease was made (a private
            // directory that could not be created), with its release in
            // doubt: never taken as a clean refusal.
            Err(e) => return Supervised::Unsafe(e),
        };
        let program = self.program_string();
        let env: Vec<(&str, &str)> = self
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let sigterm = crate::shutdown::SigtermGuard::install();
        let run = output::child::run_captured_bounded_unsetting(
            &program,
            &launched.arg_refs(),
            cwd,
            &env,
            unset,
            deadline,
            None,
            crate::shutdown::ChildIsolation::ProcessGroup(&sigterm),
            cap,
        );
        drop(sigterm);
        let released = launched.finish();
        match (run, released) {
            (Err(DevError::Interrupted), released) => {
                if let Err(e) = released {
                    output::warn(&e.to_string());
                }
                Supervised::Interrupted
            }
            (_, Err(e)) => Supervised::Unsafe(e),
            (Err(e), Ok(())) => Supervised::Failed(e),
            (Ok(capture), Ok(())) => Supervised::Ran(capture),
        }
    }

    /// [`capture_supervised`](Self::capture_supervised) with the sidecar
    /// attached: the child gets `fifo` as its marker FIFO, and its records
    /// and process samples are captured as run `run_idx`. The FIFO reader
    /// outlives the release, so a child that still writes is never cut off by
    /// a closed reader.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture_sidecar(
        &self,
        cwd: &Path,
        deadline: std::time::Duration,
        cap: output::child::CaptureCap,
        unset: &[&str],
        fifo: &mut crate::sidecar::SidecarFifo,
        run_idx: usize,
    ) -> Timed {
        if crate::shutdown::should_unwind() {
            return Timed::Interrupted;
        }
        let launched = match self.launch(cwd) {
            Ok(launched) => launched,
            Err(e) => return Timed::Unsafe(e),
        };
        let program = self.program_string();
        let fifo_path = match fifo.path_str() {
            Ok(path) => path.to_owned(),
            Err(e) => {
                return match launched.finish() {
                    Ok(()) => Timed::Failed(e),
                    Err(release) => Timed::Unsafe(release),
                };
            }
        };
        let mut env: Vec<(&str, &str)> = self
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        env.push((crate::sidecar::MARKER_FIFO_ENV, fifo_path.as_str()));
        let sigterm = crate::shutdown::SigtermGuard::install();
        let start = std::time::Instant::now();
        let child = output::child::spawn_captured_unsetting(
            &program,
            &launched.arg_refs(),
            cwd,
            &env,
            unset,
            crate::shutdown::ChildIsolation::ProcessGroup(&sigterm),
            output::SpawnProfile::Plain,
        );
        let child = match child {
            Ok(child) => child,
            Err(e) => {
                drop(sigterm);
                return match launched.finish() {
                    Ok(()) => Timed::Failed(e),
                    Err(release) => Timed::Unsafe(release),
                };
            }
        };
        let run = crate::sidecar::run_sidecar_with_deadline(
            child,
            fifo,
            run_idx,
            start,
            None,
            Some(deadline),
            &crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES,
            cap,
            &mut crate::sidecar::ProcSidecarSampler,
        );
        drop(sigterm);
        let released = launched.finish();
        Timed::Ran(Box::new(TimedCapture {
            capture: output::child::DeadlineCapture {
                captured: output::CapturedOutput {
                    ending: run.data.ending.clone(),
                    stdout: run.stdout,
                    stderr: run.stderr,
                    elapsed: run.elapsed,
                },
                output_cut: run.output_cut,
            },
            data: run.data,
            released,
        }))
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

/// What one [`Invocation::capture_supervised`] launch came to.
pub(crate) enum Supervised {
    /// The child ran to an ending and the launch was released.
    Ran(output::child::DeadlineCapture),
    /// Nothing ran to an ending (resolution, a probe or the spawn failed),
    /// and nothing was left admitted.
    Failed(DevError),
    /// Admission or release did not establish a safe state: some process
    /// may still hold the launch's paths.
    Unsafe(DevError),
    /// A stop request ended it; the release was attempted.
    Interrupted,
}

/// What one [`Invocation::capture_sidecar`] launch came to. Unlike
/// [`Supervised`], a child that was spawned is always `Ran`, its capture kept
/// whatever its release came to, so a trajectory is never lost to an
/// interrupt or an unsafe release; the caller judges `released` and the
/// ending.
pub(crate) enum Timed {
    Ran(Box<TimedCapture>),
    /// Nothing was spawned, and nothing was left admitted.
    Failed(DevError),
    /// Admission did not establish a safe state; nothing was spawned.
    Unsafe(DevError),
    /// A stop request came before the spawn.
    Interrupted,
}

/// A timed launch whose child ran to an ending.
pub(crate) struct TimedCapture {
    pub(crate) capture: output::child::DeadlineCapture,
    /// Its FIFO records, process samples and capture loss.
    pub(crate) data: crate::sidecar::SidecarData,
    /// The launch's release: an error leaves the lease held by some process.
    pub(crate) released: Result<(), DevError>,
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
