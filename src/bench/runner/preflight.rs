//! A script's preflight: `preflight(params, keys)` run for every arm before
//! anything is built, so a host that cannot run a bench refuses the
//! invocation whole and stores nothing.
//!
//! Its children are external programs only (the build does not exist yet),
//! with no handle: no iteration, no output or scratch, no spool. Each runs on
//! a launch lease of its own that reserves nothing, inherited by the child, so
//! the call is finished only once nothing it started still holds that lease,
//! as a borrow's release tests a measured call; it dies with brokkr, carries
//! the orphan token and is stopped with its process group like any child. A
//! program that cannot be found, or that `execve` itself refuses
//! (`crate::process::spawn::spawn_exact` tells that stage apart from
//! brokkr's own setup), is a result the script reads (`start_error`), so a
//! preflight writes its own fix-it text; a failure of brokkr's own
//! machinery still ends the preflight.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::PREVIEW_BYTES;
use super::host::{Since, stop_pending, stopping};
use crate::bench::external;
use crate::bench::lua::{self, Abort, CallOptions, CallOutcome, HandleRef, Word};
use crate::error::DevError;
use crate::output::child::Ending;

/// The builtins' side for one arm's preflight.
pub(super) struct PreflightHost {
    pub(super) config: Arc<lua::Value>,
    /// The live project root, every child's working directory.
    pub(super) cwd: PathBuf,
    pub(super) since: Since,
    /// The arm's bound inputs, the only paths `page_cache()` takes.
    pub(super) inputs: Vec<PathBuf>,
}

/// A builtin `bench()` alone may call, reached from a preflight: the Lua
/// seam refuses it first, so this is never the reason a script sees.
fn bench_only(name: &str) -> Abort {
    Abort::Failed(format!("{name}() is only available inside bench()"))
}

impl PreflightHost {
    /// Launch one external program and wait for it and its lease.
    fn launch(
        &self,
        name: &str,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<CallOutcome, Abort> {
        // A NUL is the script's to answer for, never a failure of brokkr's
        // spawn, which would refuse it the same way.
        if argv.iter().any(|a| a.contains('\0')) {
            return Err(Abort::Failed(format!(
                "binary_with(): an argument to {name} holds a NUL byte"
            )));
        }
        let path = env
            .iter()
            .find(|(n, _)| n == "PATH")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("PATH").ok());
        let program = match external::resolve(name, &self.cwd, path.as_deref()) {
            Ok(program) => program,
            Err(error) => {
                return Ok(not_started(name, error.operation, None, error.to_string()));
            }
        };
        let program_str = program.to_str().map(str::to_owned).ok_or_else(|| {
            Abort::Failed(format!(
                "cannot start {name}: {} is not UTF-8",
                program.display()
            ))
        })?;
        let lease =
            crate::reservations::LaunchSpec::new(self.cwd.clone(), Vec::new(), Vec::new(), None)
                .launch(&program, argv, &self.cwd, &[])
                .map_err(|e| Abort::Machinery(format!("cannot admit {name}: {e}")))?;
        let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let arg_refs = lease.arg_refs();
        let guard = crate::shutdown::SigtermGuard::install();
        let command = crate::process::spawn::ChildPolicy::plain().exact(
            &program_str,
            &arg_refs,
            Some(&self.cwd),
            &env,
            crate::shutdown::ChildIsolation::ProcessGroup(&guard),
        );
        let start = Instant::now();
        let child = command
            .map_err(crate::process::spawn::ExactSpawnError::Setup)
            .and_then(|mut command| {
                command
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                crate::process::spawn::spawn_exact(command)
            });
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                drop(guard);
                // Never spawned: the lease has no holder to wait for.
                drop(lease.finish());
                return match error {
                    crate::process::spawn::ExactSpawnError::Exec(error)
                        if refuses_program(&error) =>
                    {
                        Ok(not_started(
                            name,
                            "execute",
                            error.raw_os_error(),
                            format!("execute: {program_str}: {error}"),
                        ))
                    }
                    crate::process::spawn::ExactSpawnError::Exec(error) => Err(Abort::Machinery(
                        format!("cannot start {name}: execute: {error}"),
                    )),
                    crate::process::spawn::ExactSpawnError::Setup(error) => {
                        Err(Abort::Machinery(format!("cannot start {name}: {error}")))
                    }
                };
            }
        };
        let hold = crate::lockfile::current_hold();
        if let Some(hold) = &hold {
            hold.set_child_pid(child.id());
        }
        let run = crate::output::child::supervise_child(
            child,
            name,
            start,
            crate::output::child::ChildLimit::None,
            crate::shutdown::ChildIsolation::ProcessGroup(&guard),
            crate::output::child::ChildOptions {
                capture_cap: crate::output::child::CaptureCap {
                    stdout: PREVIEW_BYTES,
                    stderr: PREVIEW_BYTES,
                },
                ..crate::output::child::ChildOptions::DEFAULT
            },
            |_| None,
        );
        if let Some(hold) = &hold {
            hold.clear_child_pid();
        }
        drop(guard);
        finished(name, &program, run, lease.finish())
    }
}

/// What the script sees of a reaped preflight child once its lease was
/// released (`released`), or why the preflight ends.
fn finished(
    name: &str,
    program: &Path,
    run: crate::output::child::SupervisedChild,
    released: Result<(), DevError>,
) -> Result<CallOutcome, Abort> {
    let (exit, signal) = match &run.ending {
        Ending::Exited(code) => (Some(*code), None),
        Ending::Signalled(signal) => (None, Some(*signal)),
        Ending::Interrupted => return Err(Abort::Interrupted),
        other => {
            return Err(Abort::Machinery(format!(
                "{name} ended unexpectedly: {other:?}"
            )));
        }
    };
    released.map_err(|e| {
        Abort::Machinery(format!(
            "{name} exited but something it started still holds its lease: {e}"
        ))
    })?;
    Ok(CallOutcome {
        program: short(program),
        exit,
        signal,
        stdout: run.stdout,
        stdout_truncated: run.stdout_truncated,
        stderr: run.stderr,
        stderr_truncated: run.stderr_truncated || run.output_cut,
        stderr_tail: None,
        stdout_file: None,
        stderr_file: None,
        hotpath: None,
        stopped_at: None,
        start_error: None,
    })
}

fn short(program: &Path) -> String {
    program.file_name().map_or_else(
        || program.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Whether `execve`'s refusal is an answer about the program: not there or
/// not reachable, not permitted, no format the kernel runs, a missing or
/// broken interpreter or loader, busy being written. A resource failure
/// (memory, argument space, open files) is brokkr's, and stays terminal.
fn refuses_program(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(
            libc::ENOENT
                | libc::ENOTDIR
                | libc::ELOOP
                | libc::ENAMETOOLONG
                | libc::EACCES
                | libc::EPERM
                | libc::ENOEXEC
                | libc::ELIBBAD
                | libc::EISDIR
                | libc::ETXTBSY
        )
    )
}

/// A program that never started, as the script reads it.
fn not_started(
    name: &str,
    operation: &'static str,
    errno: Option<i32>,
    message: String,
) -> CallOutcome {
    CallOutcome {
        program: name.to_owned(),
        exit: None,
        signal: None,
        stdout: Vec::new(),
        stdout_truncated: false,
        stderr: Vec::new(),
        stderr_truncated: false,
        stderr_tail: None,
        stdout_file: None,
        stderr_file: None,
        hotpath: None,
        stopped_at: None,
        start_error: Some(lua::StartFailure {
            operation,
            errno,
            message,
        }),
    }
}

impl lua::Host for PreflightHost {
    fn stop(&mut self) -> Result<(), Abort> {
        stopping()
    }

    fn stop_requested() -> bool {
        stop_pending()
    }

    fn begin_block(&mut self, _at: Instant) {}

    fn binary(&mut self, words: Vec<Word>, options: CallOptions) -> Result<CallOutcome, Abort> {
        stopping()?;
        let Some(name) = options.program else {
            return Err(Abort::Failed(
                "binary() runs the build, which does not exist yet inside preflight()".into(),
            ));
        };
        let refused = |what: &str| {
            Abort::Failed(format!(
                "binary_with(): {what} is not available inside preflight(), which holds no handle \
                 and measures nothing"
            ))
        };
        if !options.reads.is_empty() || !options.writes.is_empty() || options.stdin.is_some() {
            return Err(refused("a handle"));
        }
        if options.hotpath.is_some() {
            return Err(refused("hotpath"));
        }
        if options.stop_at.is_some() {
            return Err(refused("stop_at"));
        }
        if options.measured {
            return Err(refused("measured"));
        }
        if !options.cold.is_empty() || !options.warm.is_empty() {
            return Err(refused("cold and warm"));
        }
        let mut argv = Vec::with_capacity(words.len());
        for word in words {
            match word {
                Word::Text(text) => argv.push(text),
                Word::Handle(_) => return Err(refused("a handle")),
            }
        }
        self.launch(&name, &argv, &options.env)
    }

    fn since(&mut self, sha: &str) -> Result<bool, Abort> {
        stopping()?;
        self.since.ask(sha)
    }

    fn output(&mut self, _: &str) -> Result<HandleRef, Abort> {
        Err(bench_only("output"))
    }

    fn scratch(&mut self, _: &str) -> Result<HandleRef, Abort> {
        Err(bench_only("scratch"))
    }

    fn config(&mut self) -> Result<Arc<lua::Value>, Abort> {
        stopping()?;
        Ok(Arc::clone(&self.config))
    }

    fn read(&mut self, _: usize) -> Result<Vec<u8>, Abort> {
        Err(bench_only("read"))
    }

    fn write(&mut self, _: usize, _: &[u8]) -> Result<(), Abort> {
        Err(bench_only("write"))
    }

    fn filesize(&mut self, _: usize) -> Result<u64, Abort> {
        Err(bench_only("filesize"))
    }

    fn page_cache(&mut self, path: &str, mode: lua::CacheMode) -> Result<Option<String>, Abort> {
        stopping()?;
        if !self.inputs.iter().any(|input| input.as_os_str() == path) {
            return Err(Abort::Failed(format!(
                "page_cache(): {path} is not a bound input of this arm: only an input's path, as \
                 params gives it, is checked"
            )));
        }
        let file = match crate::reservations::open_input(Path::new(path)) {
            Ok(file) => file,
            Err(e) => return Ok(Some(format!("{path}: {e}"))),
        };
        Ok(crate::bench::cache::prerequisites(&file, mode)
            .err()
            .map(|why| format!("{path} cannot be prepared {}: {why}", mode.name())))
    }
}

#[cfg(test)]
#[path = "preflight_tests.rs"]
mod tests;
