//! One iteration's host: what the script's builtins do, on the resource
//! protocol, with each child's sidecar evidence kept for the document.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::PREVIEW_BYTES;
use crate::bench::evidence;
use crate::bench::lua::{self, Abort, CallOutcome, HandleRef, Word};
use crate::bench::resources::{Handle, Resources};
use crate::bench::store;
use crate::error::DevError;
use crate::output::child::Ending;

/// `since()`'s answers for one run: the commit under test, the live repo
/// that knows every commit, and each sha's answer once asked.
pub(super) struct Since {
    repo: PathBuf,
    under_test: crate::git::Commit,
    answers: BTreeMap<String, bool>,
}

impl Since {
    pub(super) fn new(repo: &Path, under_test: crate::git::Commit) -> Self {
        Self {
            repo: repo.to_path_buf(),
            under_test,
            answers: BTreeMap::new(),
        }
    }

    /// Whether the commit under test descends from `sha` (or is it). The sha
    /// is resolved first, so one git cannot name is an error, never a no.
    fn ask(&mut self, sha: &str) -> Result<bool, Abort> {
        if let Some(answer) = self.answers.get(sha) {
            return Ok(*answer);
        }
        let failed = |e: DevError| Abort::Failed(format!("since({sha:?}): {e}"));
        let ancestor = crate::git::resolve_commit(&self.repo, sha).map_err(failed)?;
        let answer =
            crate::git::is_ancestor(&self.repo, &ancestor, &self.under_test).map_err(failed)?;
        self.answers.insert(sha.to_owned(), answer);
        Ok(answer)
    }
}

/// What an iteration runs with.
pub(super) struct IterationSetup<'a> {
    pub(super) script: &'a lua::Script,
    /// `bench()`'s arguments; their handles index `inputs` in order.
    pub(super) args: &'a lua::Args,
    /// The bound file params' paths, the iteration's inputs.
    pub(super) inputs: &'a [PathBuf],
    /// `config()`'s table.
    pub(super) config: Arc<lua::Value>,
    /// The snapshot every child runs.
    pub(super) program: &'a Path,
    /// The live project root, every child's working directory.
    pub(super) cwd: &'a Path,
    /// Where the children's marker FIFOs are made.
    pub(super) state_root: &'a Path,
    /// Where the iteration's directory goes.
    pub(super) container: &'a Path,
    pub(super) index: usize,
    pub(super) since: Since,
}

/// Run one iteration: its owner lease, a fresh Lua state, the sample, the
/// release. Returns the document's iteration and the `since` answers.
pub(super) fn run_iteration(setup: IterationSetup<'_>) -> (store::Iteration, Since) {
    let since = setup.since;
    let resources = std::fs::create_dir_all(setup.container)
        .map_err(DevError::from)
        .and_then(|()| Resources::open(setup.container, setup.inputs));
    let resources = match resources {
        Ok(resources) => resources,
        Err(error) => {
            return (
                store::Iteration {
                    outcome: store::IterationOutcome::Failed {
                        reason: format!("the iteration could not be admitted: {error}"),
                    },
                    wall_us: None,
                    record: None,
                    children: Vec::new(),
                },
                since,
            );
        }
    };
    // The inputs are the first handles, in order, as the args' handles
    // index them.
    let handles = resources.inputs().to_vec();
    let host = IterationHost {
        resources: Some(resources),
        handles,
        config: setup.config,
        program: setup.program.to_path_buf(),
        cwd: setup.cwd.to_path_buf(),
        state_root: setup.state_root.to_path_buf(),
        block_start: None,
        children: Vec::new(),
        since,
    };
    let (sample, mut host) = lua::run_sample(setup.script, setup.args, host);
    if let Some(resources) = host.resources.take()
        && let Err(error) = resources.release()
    {
        // Left to a later scan, which reclaims it once nothing holds it.
        crate::output::warn(&format!(
            "iteration {} kept for a later scan: {error}",
            setup.index + 1
        ));
    }
    // A stop pending at the end of the iteration interrupts it whatever the
    // sample said: a completed block ran under it, and a failure may be the
    // stop's own doing (a git call or a child it cut short).
    let outcome = match sample.outcome {
        _ if crate::shutdown::should_unwind() => store::IterationOutcome::Interrupted,
        lua::SampleOutcome::Completed(verdict) => store::IterationOutcome::Completed { verdict },
        lua::SampleOutcome::Failed(reason) => store::IterationOutcome::Failed { reason },
        lua::SampleOutcome::Interrupted => store::IterationOutcome::Interrupted,
    };
    let record = match outcome {
        store::IterationOutcome::Completed { .. } => sample.record,
        _ => None,
    };
    (
        store::Iteration {
            outcome,
            wall_us: sample
                .wall
                .map(|w| i64::try_from(w.as_micros()).unwrap_or(i64::MAX)),
            record,
            children: host.children,
        },
        host.since,
    )
}

/// The builtins' side for one iteration.
struct IterationHost {
    resources: Option<Resources>,
    /// Every handle given out, by the index the script's table carries.
    handles: Vec<Handle>,
    config: Arc<lua::Value>,
    program: PathBuf,
    cwd: PathBuf,
    state_root: PathBuf,
    block_start: Option<Instant>,
    children: Vec<store::Child>,
    since: Since,
}

impl IterationHost {
    fn resources(&mut self) -> Result<&mut Resources, Abort> {
        self.resources
            .as_mut()
            .ok_or_else(|| Abort::Failed("the iteration's resources are gone".into()))
    }

    fn give(&mut self, handle: Handle) -> Result<HandleRef, Abort> {
        let path = handle
            .path()
            .to_str()
            .ok_or_else(|| {
                Abort::Failed(format!(
                    "{} is not UTF-8, which a script cannot pass on",
                    handle.path().display()
                ))
            })?
            .to_owned();
        self.handles.push(handle);
        Ok(HandleRef {
            index: self.handles.len() - 1,
            path,
        })
    }

    /// Launch one child and wait for it and its borrow; its evidence is kept
    /// whatever happens after the spawn.
    fn launch(&mut self, argv: &[String], handles: &[Handle]) -> Result<CallOutcome, Abort> {
        let failed = |what: &str, e: &dyn std::fmt::Display| Abort::Failed(format!("{what}: {e}"));
        let program = self.program.clone();
        let program_str = program.display().to_string();
        let short = program
            .file_name()
            .map_or_else(|| program_str.clone(), |n| n.to_string_lossy().into_owned());
        let mut fifo = crate::sidecar::SidecarFifo::create(
            &self.state_root,
            &crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES,
        )
        .map_err(|e| failed("cannot make the marker FIFO", &e))?;
        let fifo_path = fifo
            .path_str()
            .map_err(|e| failed("cannot name the marker FIFO", &e))?
            .to_owned();
        let refs: Vec<&Handle> = handles.iter().collect();
        let call = self
            .resources()?
            .call(&program, &refs, &[], &[])
            .map_err(|e| failed("the call was refused", &e))?;
        let stdout = call
            .stdout()
            .map_err(|e| failed("cannot open the stdout spool", &e))?;
        let env = [(crate::sidecar::MARKER_FIFO_ENV, fifo_path.as_str())];
        let arg_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let guard = crate::shutdown::SigtermGuard::install();
        let command = crate::process::spawn::ChildPolicy::plain().command(
            &program_str,
            &arg_refs,
            Some(&self.cwd),
            &env,
            crate::shutdown::ChildIsolation::ProcessGroup(&guard),
        );
        let start = Instant::now();
        let child = command.and_then(|mut command| {
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::from(stdout))
                .stderr(std::process::Stdio::piped());
            crate::process::spawn::spawn(command, None)
        });
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                drop(guard);
                // Never spawned: the borrow has no holder to wait for.
                drop(call.finish());
                return Err(failed(&format!("cannot start {short}"), &error));
            }
        };
        let pid = child.id();
        let hold = crate::lockfile::current_hold();
        if let Some(hold) = &hold {
            hold.set_child_pid(pid);
        }
        let open = crate::sidecar::supervise_sidecar(
            child,
            &mut fifo,
            self.children.len(),
            start,
            None,
            None,
            &crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES,
            crate::output::child::CaptureCap {
                stdout: PREVIEW_BYTES,
                stderr: PREVIEW_BYTES,
            },
            &mut crate::sidecar::ProcSidecarSampler,
        );
        if let Some(hold) = &hold {
            hold.clear_child_pid();
        }
        drop(guard);
        // The borrow's release waits for every holder; meanwhile the FIFO is
        // drained, so a descendant still writing markers is never blocked on
        // it, and only once no holder is left is the capture closed.
        let mut open = open;
        let finished = call.finish_observed(&mut || open.pump(&mut fifo));
        let run = open.finish(&mut fifo, finished.is_ok());
        let origin = self.block_start.map_or(0, |at| {
            i64::try_from(start.saturating_duration_since(at).as_micros()).unwrap_or(i64::MAX)
        });
        let added: BTreeMap<String, String> = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        self.children.push(store::Child {
            program: program_str.clone(),
            argv: crate::history::redact_secret_argv(argv),
            env: evidence::redacted_env(&added),
            pid: Some(pid),
            origin_offset_us: origin,
            wall_us: i64::try_from(run.elapsed.as_micros()).unwrap_or(i64::MAX),
            ending: evidence::child_ending(&run.data.ending),
            evidence: evidence::evidence(&run.data),
        });
        let spool = finished.map_err(|e| {
            failed(
                &format!("{short} exited but something it started still holds its borrow"),
                &e,
            )
        })?;
        call_outcome(short, &run, &spool)
    }
}

/// What the script sees of a finished child, or why the sample ends.
fn call_outcome(
    short: String,
    run: &crate::sidecar::SidecarRunResult,
    spool: &crate::reservations::Spool,
) -> Result<CallOutcome, Abort> {
    let (exit, signal) = match &run.data.ending {
        Ending::Exited(code) => (Some(*code), None),
        Ending::Signalled(signal) => (None, Some(*signal)),
        Ending::Interrupted => return Err(Abort::Interrupted),
        Ending::WaitFailed(why) => {
            return Err(Abort::Failed(format!("waiting for {short} failed: {why}")));
        }
        other => {
            return Err(Abort::Failed(format!(
                "{short} ended unexpectedly: {other:?}"
            )));
        }
    };
    let preview = spool
        .preview(PREVIEW_BYTES)
        .map_err(|e| Abort::Failed(format!("cannot read {short}'s stdout spool: {e}")))?;
    Ok(CallOutcome {
        program: short,
        exit,
        signal,
        stdout: preview.bytes,
        stdout_truncated: preview.truncated,
        stderr: run.stderr.clone(),
        stderr_truncated: run.stderr_truncated || run.output_cut,
    })
}

/// Every builtin's first step: a pending stop ends the sample here, the one
/// place a script spinning in Lua meets it.
fn stopping() -> Result<(), Abort> {
    if crate::shutdown::should_unwind() || !crate::shutdown::admits_new_work() {
        Err(Abort::Interrupted)
    } else {
        Ok(())
    }
}

impl lua::Host for IterationHost {
    fn begin_block(&mut self, at: Instant) {
        self.block_start = Some(at);
    }

    fn binary(&mut self, words: Vec<Word>) -> Result<CallOutcome, Abort> {
        stopping()?;
        let mut argv = Vec::with_capacity(words.len());
        let mut handles = Vec::new();
        for word in words {
            match word {
                Word::Text(text) => argv.push(text),
                Word::Handle(index) => {
                    let handle = self.handles.get(index).cloned().ok_or_else(|| {
                        Abort::Failed(format!("binary(): handle {index} was never given out"))
                    })?;
                    argv.push(handle.path().display().to_string());
                    handles.push(handle);
                }
            }
        }
        self.launch(&argv, &handles)
    }

    fn since(&mut self, sha: &str) -> Result<bool, Abort> {
        stopping()?;
        self.since.ask(sha)
    }

    fn output(&mut self, name: &str) -> Result<HandleRef, Abort> {
        stopping()?;
        let handle = self
            .resources()?
            .output(name)
            .map_err(|e| Abort::Failed(format!("output({name:?}): {e}")))?;
        self.give(handle)
    }

    fn scratch(&mut self, name: &str) -> Result<HandleRef, Abort> {
        stopping()?;
        let handle = self
            .resources()?
            .scratch(name)
            .map_err(|e| Abort::Failed(format!("scratch({name:?}): {e}")))?;
        self.give(handle)
    }

    fn config(&mut self) -> Result<Arc<lua::Value>, Abort> {
        stopping()?;
        Ok(Arc::clone(&self.config))
    }
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
