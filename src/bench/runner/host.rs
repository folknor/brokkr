//! One iteration's host: what the script's builtins do, on the resource
//! protocol, with each child's sidecar evidence kept for the document.
//!
//! The iteration's owner lease covers the bound inputs, its first handles.
//! Every call is first judged by `crate::bench::policy` and then admitted
//! with the access it declared; `read()`, `write()` and `filesize()` go
//! through the same resource layer. A launch runs on a borrow, with a fresh
//! marker FIFO per child drained while the borrow's release waits for every
//! holder. A call's `stop_at` reaches the sidecar supervisor as an exact
//! marker, with a SIGTERM grace on a hotpath call so the child can write its
//! report, and its `deadline` as the wall limit, worded by [`overdue`].
//! Stderr snapshots, hotpath reports and host-trace readings each draw on a
//! run-wide [`Budget`], and the host trace (`crate::bench::host_trace`) is
//! read around each sample.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::PREVIEW_BYTES;
use crate::bench::evidence;
use crate::bench::host_trace;
use crate::bench::lua::{self, Abort, CallOptions, CallOutcome, HandleRef, Word};
use crate::bench::policy::{Calls, Declared};
use crate::bench::resources::{self, Handle, Resources};
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

    /// The same commit and repo with no answers yet: what stands in while
    /// this one is lent to a hook.
    pub(super) fn blank(&self) -> Self {
        Self::new(&self.repo, self.under_test.clone())
    }

    /// Whether the commit under test descends from `sha` (or is it). The sha
    /// is resolved first, so one git cannot name is an error, never a no.
    pub(super) fn ask(&mut self, sha: &str) -> Result<bool, Abort> {
        if let Some(answer) = self.answers.get(sha) {
            return Ok(*answer);
        }
        // A git the stop ended or refused is the stop's, not a failure;
        // every other error keeps its meaning, a stop racing it or not.
        let failed = |e: DevError| match e {
            DevError::Spawn { error, .. } if error.kind() == std::io::ErrorKind::Interrupted => {
                Abort::Interrupted
            }
            DevError::Interrupted => Abort::Interrupted,
            e => Abort::Failed(format!("since({sha:?}): {e}")),
        };
        let ancestor = crate::git::resolve_commit(&self.repo, sha).map_err(failed)?;
        stopping()?;
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
    pub(super) measurement: store::Measurement,
    /// What the run may still keep.
    pub(super) budget: Budget,
    /// The run so far: its uuid, project and bench name the cell cursor.
    pub(super) run: &'a store::RunDocument,
}

/// What a run may still keep of its children's output, spent across its
/// iterations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Budget {
    /// Hotpath report bytes, as the store serializes them.
    pub(super) reports: usize,
    /// Raw stderr tail bytes.
    pub(super) stderr: usize,
    /// Periodic host readings.
    pub(super) host_readings: usize,
}

impl Budget {
    /// A whole run's.
    pub(super) const RUN: Self = Self {
        reports: RUN_REPORT_BYTES,
        stderr: store::STDERR_RUN_BYTES,
        host_readings: crate::bench::host_trace::RUN_READINGS,
    };
}

/// How long a call marked `hotpath` and stopped at its `stop_at` marker has,
/// from the one SIGTERM, to exit and write its report before its process
/// group is SIGKILLed.
const STOP_REPORT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// The most one hotpath report may hold.
pub(super) const REPORT_BYTES: usize = 16 * 1024 * 1024;

/// The most every report of a run may hold together as the store
/// serializes them: half the 256 MiB a results server takes in one
/// publication, which bounds the reports, not the whole run.
pub(super) const RUN_REPORT_BYTES: usize = 128 * 1024 * 1024;

/// Run one iteration: its owner lease, a fresh Lua state, the sample, the
/// release. Returns the document's iteration, the `since` answers, the
/// budget left and the library modules the sample asked for.
pub(super) fn run_iteration(
    setup: IterationSetup<'_>,
) -> (store::Iteration, Since, Budget, BTreeSet<String>) {
    let since = setup.since;
    let budget = setup.budget;
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
                    host: None,
                },
                since,
                budget,
                BTreeSet::new(),
            );
        }
    };
    let trace = TraceStart::begin(
        setup.container,
        host_trace::cell(
            &setup.run.uuid,
            &setup.run.project,
            &setup.run.bench,
            setup.index,
        ),
        budget.host_readings,
    );
    // The inputs are the first handles, in order, as the args' handles
    // index them.
    let calls = Calls::new(resources.inputs().to_vec());
    let host = IterationHost {
        resources: Some(resources),
        calls,
        config: setup.config,
        program: setup.program.to_path_buf(),
        cwd: setup.cwd.to_path_buf(),
        state_root: setup.state_root.to_path_buf(),
        block_start: None,
        children: Vec::new(),
        stages: BTreeMap::new(),
        budget,
        since,
    };
    let (sample, mut host) = lua::run_sample(setup.script, setup.args, host);
    let observed = trace.finish(&mut host, setup.index);
    // A stop pending at the end of the iteration interrupts it whatever the
    // sample said: a completed block ran under it, and a failure may be the
    // stop's own doing (a git call or a child it cut short).
    let mut profile_failed = false;
    let outcome = match sample.outcome {
        _ if crate::shutdown::should_unwind() => store::IterationOutcome::Interrupted,
        lua::SampleOutcome::Completed(verdict) => {
            match profile_failure(setup.measurement, &host.children) {
                // A completed sample under --hotpath fails here, its children
                // and their captures stored as they came.
                Some(reason) => {
                    profile_failed = true;
                    store::IterationOutcome::Failed { reason }
                }
                None => store::IterationOutcome::Completed { verdict },
            }
        }
        lua::SampleOutcome::Failed(reason) => store::IterationOutcome::Failed { reason },
        lua::SampleOutcome::Interrupted => store::IterationOutcome::Interrupted,
    };
    // A record extracted is kept when only the --hotpath requirement failed
    // the iteration: it is the script's account of what it saw.
    let record = match (&outcome, profile_failed) {
        (store::IterationOutcome::Completed { .. }, _) | (_, true) => sample.record,
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
            host: observed,
        },
        host.since,
        host.budget,
        sample.modules,
    )
}

/// What an admitted iteration observed of the host before `bench()`: the
/// device, the cursor it found and replaced with its own in-progress one,
/// and its start reading.
struct TraceStart {
    device: Option<store::Device>,
    cell: store::Cell,
    previous: Option<store::Cursor>,
    cursor_written: bool,
    start: store::Reading,
    sampler: host_trace::Sampler,
    /// An instant and the boot clock read together, to place the block.
    anchor: (Instant, i64),
}

impl TraceStart {
    /// After `bench()`: stop sampling, read the end, release the
    /// iteration's directory, read again and record this cell's end.
    fn finish(self, host: &mut IterationHost, index: usize) -> Option<store::HostTrace> {
        let (readings, capped) = self.sampler.finish();
        let end = host_trace::read(self.device.as_ref());
        host.budget.host_readings = host.budget.host_readings.saturating_sub(readings.len());
        let anchor = self.anchor;
        let block_start_us = host.block_start.map(|at| {
            let after = i64::try_from(at.saturating_duration_since(anchor.0).as_micros())
                .unwrap_or(i64::MAX);
            anchor.1.saturating_add(after)
        });
        let mut release_completed = true;
        if let Some(resources) = host.resources.take()
            && let Err(error) = resources.release()
        {
            release_completed = false;
            // Left to a later scan, which reclaims it once nothing holds it.
            crate::output::warn(&format!(
                "iteration {} kept for a later scan: {error}",
                index + 1
            ));
        }
        let released = host_trace::read(self.device.as_ref());
        if let Err(error) = host_trace::write_cursor(&store::Cursor::Ended {
            cell: self.cell.clone(),
            device: self.device.clone(),
            reading: released.clone(),
            release_completed,
        }) {
            crate::output::warn(&format!("bench cell cursor not written: {error}"));
        }
        let mut trace = store::HostTrace {
            boot_id: self.cell.boot_id,
            device: self.device,
            previous: self.previous,
            cursor_written: self.cursor_written,
            block_start_us,
            start: Some(self.start),
            end: Some(end),
            released: Some(released),
            release_completed,
            readings,
            capped,
        };
        // A cursor file publication would refuse (another writer's, or
        // damaged) is dropped rather than costing the run; a trace still
        // refused after that is not stored.
        if trace.validate().is_err() && trace.previous.take().is_some() {
            crate::output::warn(
                "bench cell cursor was not usable; its predecessor is not recorded",
            );
        }
        match trace.validate() {
            Ok(()) => Some(trace),
            Err(error) => {
                crate::output::warn(&format!("host trace not stored: {error}"));
                None
            }
        }
    }

    fn begin(container: &Path, cell: store::Cell, budget: usize) -> Self {
        let device = host_trace::resolve_device(container);
        let previous = host_trace::read_cursor();
        let cursor_written =
            match host_trace::write_cursor(&store::Cursor::InProgress { cell: cell.clone() }) {
                Ok(()) => true,
                Err(error) => {
                    // The older entry must not outlive this cell unseen: a
                    // later one would take it as its predecessor across
                    // whatever this one does.
                    let cleared = host_trace::clear_cursor();
                    crate::output::warn(&format!(
                        "bench cell cursor not written: {error}{}",
                        if cleared {
                            "; the older entry was removed"
                        } else {
                            ""
                        }
                    ));
                    false
                }
            };
        let start = host_trace::read(device.as_ref());
        let sampler = host_trace::Sampler::start(device.clone(), budget);
        let anchor = (Instant::now(), host_trace::boottime_us());
        Self {
            device,
            cell,
            previous,
            cursor_written,
            start,
            sampler,
            anchor,
        }
    }
}

/// Why a completed sample of a `--hotpath` or `--alloc` run fails: no call
/// was marked, or a marked one did not capture. `None` for a timing run,
/// whose captures are evidence only.
fn profile_failure(measurement: store::Measurement, children: &[store::Child]) -> Option<String> {
    let flag = measurement.flag()?;
    let mut marked = children
        .iter()
        .filter_map(|c| c.hotpath.as_ref())
        .peekable();
    if marked.peek().is_none() {
        return Some(format!(
            "{flag}: no call of the iteration was marked hotpath"
        ));
    }
    let failed: Vec<String> = marked
        .filter_map(|capture| {
            let why = match &capture.outcome {
                store::CaptureOutcome::Captured { .. } => return None,
                store::CaptureOutcome::Missing => {
                    "no report was written (is the build instrumented?)".to_owned()
                }
                store::CaptureOutcome::Invalid { reason } => format!("invalid report: {reason}"),
                store::CaptureOutcome::Unreadable { reason } => {
                    format!("unreadable report: {reason}")
                }
                store::CaptureOutcome::OverBudget => "the report is over budget".to_owned(),
            };
            Some(format!(
                "stage {} occurrence {}: {why}",
                capture.stage, capture.occurrence
            ))
        })
        .collect();
    (!failed.is_empty()).then(|| format!("{flag}: {}", failed.join("; ")))
}

/// The builtins' side for one iteration.
struct IterationHost {
    resources: Option<Resources>,
    /// Every handle given out, and the call policy's state.
    calls: Calls,
    config: Arc<lua::Value>,
    program: PathBuf,
    cwd: PathBuf,
    state_root: PathBuf,
    block_start: Option<Instant>,
    children: Vec<store::Child>,
    /// Each stage's marked calls so far, whatever became of them.
    stages: BTreeMap<String, u32>,
    /// What the run may still keep.
    budget: Budget,
    since: Since,
}

impl IterationHost {
    /// What became of a finished marked call's report at `path`, read once
    /// nothing holds the call's borrow, and its projection for the script.
    /// Never an error: a failure is the capture's outcome.
    fn capture(&mut self, path: &Path) -> (store::CaptureOutcome, lua::Capture) {
        let limit = REPORT_BYTES.min(self.budget.reports);
        let read = match self.resources() {
            Ok(resources) => resources.read_report(path, limit),
            Err(Abort::Interrupted) => Err(DevError::Interrupted),
            Err(other) => Err(DevError::Internal(other.to_string())),
        };
        let outcome = match read {
            Err(error) => store::CaptureOutcome::Unreadable {
                reason: crate::output::with_absolute_paths(|| error.to_string()),
            },
            Ok(None) => store::CaptureOutcome::OverBudget,
            Ok(Some(bytes)) if bytes.is_empty() => store::CaptureOutcome::Missing,
            Ok(Some(bytes)) => match decode_report(&bytes) {
                Err(reason) => store::CaptureOutcome::Invalid { reason },
                // Charged as the store will hold it, which may be longer
                // than the file (`1e9` reserialized as `1000000000.0`).
                Ok((report, _)) if stored_len(&report) > self.budget.reports => {
                    store::CaptureOutcome::OverBudget
                }
                Ok((report, projection)) => {
                    self.budget.reports = self.budget.reports.saturating_sub(stored_len(&report));
                    return (
                        store::CaptureOutcome::Captured { report },
                        lua::Capture {
                            status: "captured",
                            report: Some(projection),
                            error: None,
                        },
                    );
                }
            },
        };
        let (status, error) = match &outcome {
            store::CaptureOutcome::Captured { .. } => ("captured", None),
            store::CaptureOutcome::Missing => ("missing", None),
            store::CaptureOutcome::Invalid { reason } => ("invalid", Some(reason.clone())),
            store::CaptureOutcome::Unreadable { reason } => ("unreadable", Some(reason.clone())),
            store::CaptureOutcome::OverBudget => ("over_budget", None),
        };
        (
            outcome,
            lua::Capture {
                status,
                report: None,
                error,
            },
        )
    }

    fn resources(&mut self) -> Result<&mut Resources, Abort> {
        self.resources
            .as_mut()
            .ok_or_else(|| Abort::Failed("the iteration's resources are gone".into()))
    }

    fn give(&mut self, handle: Handle) -> Result<HandleRef, Abort> {
        self.calls.handles.give(handle)
    }

    /// The handle the script's table carries `index` of.
    fn handle(&self, index: usize, function: &str) -> Result<Handle, Abort> {
        self.calls.handles.get(index, function)
    }

    /// A fresh marker FIFO for one child, and its path.
    fn marker_fifo(&self) -> Result<(crate::sidecar::SidecarFifo, String), Abort> {
        let fifo = crate::sidecar::SidecarFifo::create(
            &self.state_root,
            &crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES,
        )
        .map_err(|e| Abort::Failed(format!("cannot make the marker FIFO: {e}")))?;
        let path = fifo
            .path_str()
            .map_err(|e| Abort::Failed(format!("cannot name the marker FIFO: {e}")))?
            .to_owned();
        Ok((fifo, path))
    }

    /// What a call launches: the snapshot, or its `program` resolved, with
    /// that executable as observed.
    fn target(&self, call: &Declared) -> Result<(PathBuf, Option<store::Executable>), Abort> {
        match &call.program {
            None => Ok((self.program.clone(), None)),
            Some(name) => external_program(name, &self.cwd, &call.env).map(|(p, e)| (p, Some(e))),
        }
    }

    /// Launch one child and wait for it and its borrow; its evidence is kept
    /// whatever happens after the spawn.
    #[allow(clippy::too_many_lines)] // One launch in its order: admit, prepare, spawn, keep.
    fn launch(&mut self, call: &Declared) -> Result<CallOutcome, Abort> {
        let argv = call.words.as_slice();
        let failed = |what: &str, e: &dyn std::fmt::Display| Abort::Failed(format!("{what}: {e}"));
        // The hold first, before any lease, spool or cache is made for a
        // launch it would refuse.
        let hold = launch_hold()?;
        let (program, executable) = self.target(call)?;
        let program_str = program.display().to_string();
        let short = program
            .file_name()
            .map_or_else(|| program_str.clone(), |n| n.to_string_lossy().into_owned());
        let (mut fifo, fifo_path) = self.marker_fifo()?;
        let mut admitted = self
            .resources()?
            .call(
                &program,
                &call.access,
                call.stdin.as_ref(),
                call.hotpath.is_some(),
            )
            .map_err(|e| failed("the call was refused", &e))?;
        let stdin = admitted.take_stdin();
        let (call_stdout, call_stderr, stderr_spool) = child_streams(&mut admitted)?;
        let report = admitted.report().map(Path::to_path_buf);
        let report_str = report.as_deref().map(utf8_report).transpose()?;
        let mut env: Vec<(&str, &str)> =
            vec![(crate::sidecar::MARKER_FIFO_ENV, fifo_path.as_str())];
        env.extend(call.env.iter().map(|(n, v)| (n.as_str(), v.as_str())));
        env.extend(report_str.as_deref().map(hotpath_env).into_iter().flatten());
        let arg_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        // Prepared after admission and before the launch instant, so its
        // cost is inside the block but outside the child's wall.
        let prepared = self
            .prepare_cache(call, stdin.as_ref())
            .and_then(|prepared| stopping().map(|()| prepared));
        let cache = match prepared {
            Ok(cache) => cache,
            Err(abort) => {
                // Never spawned: the borrow has no holder to wait for.
                drop(admitted.finish());
                return Err(abort);
            }
        };
        let guard = crate::shutdown::SigtermGuard::install();
        let start = Instant::now();
        let child = spawn_child(
            &Launch {
                program: &program_str,
                args: &arg_refs,
                cwd: &self.cwd,
                env: &env,
                guard: &guard,
                external: executable.is_some(),
            },
            stdin.map_or_else(std::process::Stdio::null, std::process::Stdio::from),
            std::process::Stdio::from(call_stdout),
            std::process::Stdio::from(call_stderr),
        );
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                drop(guard);
                // Never spawned: the borrow has no holder to wait for.
                drop(admitted.finish());
                // The launch gate refused it under a committed stop.
                if matches!(error, DevError::Interrupted) {
                    return Err(Abort::Interrupted);
                }
                return Err(failed(&format!("cannot start {short}"), &error));
            }
        };
        let pid = child.id();
        if let Some(hold) = &hold {
            hold.set_child_pid(pid);
        }
        let open = crate::sidecar::supervise_sidecar(
            child,
            &mut fifo,
            self.children.len(),
            start,
            call.stop_at
                .as_deref()
                .map(crate::sidecar::StopRequest::exact),
            // A marked call stopped at a marker is asked to stop, so an
            // instrumented build unwinds and writes its report.
            call.hotpath.is_some().then_some(STOP_REPORT_GRACE),
            // A ceiling on the child's lifetime from `start`, the stop's
            // grace included.
            call.deadline,
            &crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES,
            // Neither stream is piped: both go to their spools, so nothing
            // is drained and the cap binds nothing.
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
        let finished = admitted.finish_observed(&mut || open.pump(&mut fifo));
        let run = open.finish(&mut fifo, finished.is_ok());
        self.keep_child(program_str, argv, &env, pid, start, &run, executable);
        if let Some(child) = self.children.last_mut() {
            child.cache = cache;
        }
        self.mark_measured(call.measured);
        let stderr = self.keep_stderr(&stderr_spool, finished.is_ok());
        // Judged after every launch that was reaped, whatever its exit, and
        // stored on the child before anything below can end the call.
        let projection = match (&call.hotpath, &report) {
            (Some(stage), Some(path)) => Some(self.keep_capture(stage, path, finished.is_ok())),
            _ => None,
        };
        let spool = finished.map_err(|e| {
            // An overdue child's kill explains the rest, so it is named first.
            let how = match &run.data.ending {
                Ending::Deadline(limit) => format!("{}, and", overdue(&short, *limit)),
                _ => format!("{short} exited but"),
            };
            failed(
                &format!("{how} something it started still holds its borrow"),
                &e,
            )
        })?;
        let mut outcome = call_outcome(short, &run, &spool, stderr)?;
        call.ended(&outcome.program, &outcome)?;
        outcome.stdout_file = Some(self.give(resources::spool_handle(&spool))?);
        outcome.stderr_file = Some(self.give(resources::spool_handle(&stderr_spool))?);
        outcome.hotpath = projection;
        Ok(outcome)
    }

    /// The last child's capture under `stage`, its occurrence counted
    /// whatever becomes of it: read from `path` when the call's borrow was
    /// released (`released`), else unreadable, since something may still
    /// write it. Returns what the script sees.
    fn keep_capture(&mut self, stage: &str, path: &Path, released: bool) -> lua::Capture {
        let seen = self.stages.entry(stage.to_owned()).or_insert(0);
        *seen += 1;
        let occurrence = *seen;
        let (outcome, shown) = if released {
            self.capture(path)
        } else {
            let reason = "something the child started still held its borrow".to_owned();
            (
                store::CaptureOutcome::Unreadable {
                    reason: reason.clone(),
                },
                lua::Capture {
                    status: "unreadable",
                    report: None,
                    error: Some(reason),
                },
            )
        };
        if let Some(child) = self.children.last_mut() {
            child.hotpath = Some(store::HotpathCapture {
                stage: stage.to_owned(),
                occurrence,
                outcome,
            });
        }
        shown
    }

    /// The last child's stderr, snapshotted from its spool: what the store
    /// keeps, set on the child whatever is read, and what the script sees,
    /// its first and last MiB, or why they could not be read. `released`
    /// is whether the call's borrow was released first.
    fn keep_stderr(
        &mut self,
        spool: &crate::reservations::Spool,
        released: bool,
    ) -> Result<(crate::reservations::Preview, Vec<u8>), String> {
        let tail = spool.tail(PREVIEW_BYTES);
        let capture = match &tail {
            Err(error) => store::StderrCapture {
                released,
                bytes: spool.size().ok(),
                tail: store::StderrTail::Unreadable {
                    reason: crate::output::with_absolute_paths(|| error.to_string()),
                },
            },
            Ok(tail) => {
                let keep = tail.bytes.len().min(store::STDERR_TAIL_BYTES);
                let kept = &tail.bytes[tail.bytes.len() - keep..];
                let stored = if keep > self.budget.stderr {
                    store::StderrTail::OverBudget
                } else {
                    self.budget.stderr -= keep;
                    let raw = u64::try_from(keep).unwrap_or(u64::MAX);
                    store::StderrTail::Kept {
                        text: String::from_utf8_lossy(kept).into_owned(),
                        raw,
                        cut: raw < tail.size,
                    }
                };
                store::StderrCapture {
                    released,
                    bytes: Some(tail.size),
                    tail: stored,
                }
            }
        };
        if let Some(child) = self.children.last_mut() {
            child.stderr = Some(capture);
        }
        let tail = tail.map_err(|e| e.to_string())?;
        let head = spool.preview(PREVIEW_BYTES).map_err(|e| e.to_string())?;
        Ok((head, tail.bytes))
    }

    /// Mark the last child as the iteration's measured call.
    fn mark_measured(&mut self, measured: bool) {
        if let Some(child) = self.children.last_mut() {
            child.measured = measured;
        }
    }

    /// A spawned child's evidence, for the document.
    #[allow(clippy::too_many_arguments)] // One child's whole evidence, each part its own.
    fn keep_child(
        &mut self,
        program: String,
        argv: &[String],
        env: &[(&str, &str)],
        pid: u32,
        start: Instant,
        run: &crate::sidecar::SidecarRunResult,
        executable: Option<store::Executable>,
    ) {
        let origin = self.offset(start);
        let added: BTreeMap<String, String> = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        self.children.push(store::Child {
            program,
            argv: crate::history::redact_secret_argv(argv),
            env: evidence::redacted_env(&added),
            pid: Some(pid),
            origin_offset_us: origin,
            wall_us: i64::try_from(run.elapsed.as_micros()).unwrap_or(i64::MAX),
            ending: evidence::child_ending(&run.data.ending),
            evidence: evidence::evidence(&run.data),
            hotpath: None,
            executable,
            stderr: None,
            measured: false,
            cache: None,
        });
    }

    /// `at` from the start of the block, in microseconds.
    fn offset(&self, at: Instant) -> i64 {
        self.block_start.map_or(0, |start| {
            i64::try_from(at.saturating_duration_since(start).as_micros()).unwrap_or(i64::MAX)
        })
    }

    /// Prepare the call's `cold` and `warm` inputs, admitted and about to be
    /// launched: `None` when it lists none. Policy already judged each an
    /// input the call passes (`crate::bench::policy`); each is opened as
    /// `read()` opens one and prepared through that descriptor; listings of
    /// one inode in one mode
    /// share an account and in both modes are refused, as is a prepared
    /// inode that a file the call declares written already names, and a
    /// stdin that is not the inode prepared for it.
    fn prepare_cache(
        &mut self,
        call: &Declared,
        stdin: Option<&std::fs::File>,
    ) -> Result<Option<store::CachePreparation>, Abort> {
        if call.cache.is_empty() {
            return Ok(None);
        }
        let mut targets: Vec<(crate::bench::cache::Target, (u64, u64))> = Vec::new();
        let mut stdin_inode = None;
        for (mode, handle) in &call.cache {
            let shown = handle.path().display().to_string();
            let what = format!("binary_with(): {} {shown}", mode.name());
            let file = self
                .resources()?
                .open_input(handle)
                .map_err(|e| Abort::Failed(format!("{what}: {e}")))?;
            let inode = crate::bench::cache::inode(&file)
                .map_err(|e| Abort::Failed(format!("{what}: cannot inspect it: {e}")))?;
            if call.stdin.as_ref() == Some(handle) {
                stdin_inode = Some((inode, shown.clone()));
            }
            match targets.iter_mut().find(|(_, seen)| *seen == inode) {
                Some((target, _)) if target.mode != *mode => {
                    return Err(Abort::Failed(format!(
                        "{what}: the same file is also listed {} ({})",
                        target.mode.name(),
                        target.paths.join(", ")
                    )));
                }
                Some((target, _)) => {
                    if !target.paths.contains(&shown) {
                        target.paths.push(shown);
                    }
                }
                None => targets.push((
                    crate::bench::cache::Target {
                        paths: vec![shown],
                        mode: *mode,
                        file,
                    },
                    inode,
                )),
            }
        }
        if let (Some((inode, shown)), Some(stdin)) = (stdin_inode, stdin) {
            let given = crate::bench::cache::inode(stdin)
                .map_err(|e| Abort::Failed(format!("cannot inspect the call's stdin: {e}")))?;
            if given != inode {
                return Err(Abort::Failed(format!(
                    "binary_with(): {shown} is the call's stdin, but the file opened as its stdin \
                     is not the one prepared: something replaced it"
                )));
            }
        }
        no_written_alias(call, &targets)?;
        let targets: Vec<crate::bench::cache::Target> =
            targets.into_iter().map(|(target, _)| target).collect();
        match crate::bench::cache::prepare(&targets, &stop_pending) {
            Ok((files, from, to)) => Ok(Some(store::CachePreparation {
                procedure: store::CACHE_PROCEDURE.to_owned(),
                scan_from_us: self.offset(from),
                scan_to_us: self.offset(to),
                files,
            })),
            Err(crate::bench::cache::Failure::Interrupted) => Err(Abort::Interrupted),
            Err(crate::bench::cache::Failure::Failed(why)) => Err(Abort::Failed(why)),
        }
    }
}

/// Refuse a call one of whose declared writes, by its own path, already
/// names an inode it prepares. Only an existing file is inspected: a
/// directory written is not searched, and what a child links afterwards is
/// its own, which handles never confine.
fn no_written_alias(
    call: &Declared,
    targets: &[(crate::bench::cache::Target, (u64, u64))],
) -> Result<(), Abort> {
    for (path, access) in &call.access {
        if *access != crate::reservations::Access::Write {
            continue;
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(Abort::Failed(format!(
                    "binary_with(): cannot inspect {}, which the call writes, against the files \
                     it prepares: {e}",
                    crate::output::path(path)
                )));
            }
        };
        let written = (
            std::os::unix::fs::MetadataExt::dev(&meta),
            std::os::unix::fs::MetadataExt::ino(&meta),
        );
        if let Some((target, _)) = targets.iter().find(|(_, inode)| *inode == written) {
            return Err(Abort::Failed(format!(
                "binary_with(): {}, which the call writes, is the same file as {}, which it \
                 prepares {}",
                crate::output::path(path),
                target.paths.join(", "),
                target.mode.name()
            )));
        }
    }
    Ok(())
}

/// The descriptors a call's child writes its stdout and stderr through, and
/// its stderr spool, kept apart from the call so it is read whether or not
/// the borrow's release succeeds.
fn child_streams(
    call: &mut crate::reservations::Call,
) -> Result<(std::fs::File, std::fs::File, crate::reservations::Spool), Abort> {
    let failed = |what: &str, e: &std::io::Error| Abort::Failed(format!("{what}: {e}"));
    let stdout = call
        .stdout()
        .map_err(|e| failed("cannot open the stdout spool", &e))?;
    let stderr = call
        .stderr()
        .map_err(|e| failed("cannot open the stderr spool", &e))?;
    let spool = call
        .take_stderr_spool()
        .ok_or_else(|| Abort::Failed("the call's stderr spool is gone".into()))?;
    Ok((stdout, stderr, spool))
}

/// One call's child as it is launched.
struct Launch<'a> {
    program: &'a str,
    args: &'a [&'a str],
    cwd: &'a Path,
    env: &'a [(&'a str, &'a str)],
    guard: &'a crate::shutdown::SigtermGuard,
    /// An external program, run exactly as resolved and recorded.
    external: bool,
}

/// Spawn a call's child in its own process group, stdout and stderr each to
/// its spool: the snapshot through the choke point, an external program
/// never through libc's `/bin/sh` fallback
/// (`crate::process::spawn::spawn_exact`).
fn spawn_child(
    launch: &Launch<'_>,
    stdin: std::process::Stdio,
    stdout: std::process::Stdio,
    stderr: std::process::Stdio,
) -> Result<std::process::Child, DevError> {
    let policy = crate::process::spawn::ChildPolicy::plain();
    let isolation = crate::shutdown::ChildIsolation::ProcessGroup(launch.guard);
    let cwd = Some(launch.cwd);
    if !launch.external {
        let mut command =
            policy.command(launch.program, launch.args, cwd, launch.env, isolation)?;
        command.stdin(stdin).stdout(stdout).stderr(stderr);
        return crate::process::spawn::spawn(command, None);
    }
    let mut command = policy.exact(launch.program, launch.args, cwd, launch.env, isolation)?;
    command.stdin(stdin).stdout(stdout).stderr(stderr);
    crate::process::spawn::spawn_exact(command).map_err(|error| match error {
        crate::process::spawn::ExactSpawnError::Setup(error) => error,
        crate::process::spawn::ExactSpawnError::Exec(error) => DevError::Spawn {
            program: format!("{} (execute)", launch.program),
            error,
        },
    })
}

/// The external program `name` names for a child in `cwd` whose call adds
/// `env`: resolved on the child's own `PATH` (`crate::bench::external`), and
/// its file observed. A program that cannot be resolved fails the call, so
/// the iteration: a measured block whose program is missing is a failed
/// measurement, and presence is the preflight's to decide.
pub(super) fn external_program(
    name: &str,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<(PathBuf, store::Executable), Abort> {
    let path = env
        .iter()
        .find(|(n, _)| n == "PATH")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("PATH").ok());
    let resolved = crate::bench::external::resolve(name, cwd, path.as_deref())
        .map_err(|e| Abort::Failed(format!("cannot start {name}: {e}")))?;
    let shown = resolved.to_str().ok_or_else(|| {
        Abort::Failed(format!(
            "cannot start {name}: {} is not UTF-8",
            crate::output::path(&resolved)
        ))
    })?;
    let executable = store::Executable {
        path: shown.to_owned(),
        identity: crate::bench::external::observe(&resolved),
    };
    Ok((resolved, executable))
}

/// A report's length as the store serializes it.
fn stored_len(report: &serde_json::Value) -> usize {
    serde_json::to_vec(report).map_or(usize::MAX, |bytes| bytes.len())
}

/// The report path as the child is told it.
fn utf8_report(path: &Path) -> Result<String, Abort> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        Abort::Failed(format!(
            "the report path {} is not UTF-8",
            crate::output::path(path)
        ))
    })
}

/// The variables a marked call's child gets, whatever the build: an
/// instrumented one never starts the metrics server or prints its table,
/// and writes its report to `report`.
fn hotpath_env(report: &str) -> [(&'static str, &str); 3] {
    [
        ("HOTPATH_METRICS_SERVER_OFF", "true"),
        ("HOTPATH_OUTPUT_FORMAT", "json"),
        ("HOTPATH_OUTPUT_PATH", report),
    ]
}

/// A report as the store keeps it, a JSON object as parsed, and as the
/// script sees it, through the strict decoding `json.decode` uses; refused
/// when either refuses it.
fn decode_report(bytes: &[u8]) -> Result<(serde_json::Value, lua::Value), String> {
    let projection = crate::bench::json::decode(bytes)?;
    let report: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if !report.is_object() {
        return Err("the report is not a JSON object".to_owned());
    }
    Ok((report, projection))
}

/// Why a call whose child outlived its `deadline` ends its sample or its
/// preflight: the call's declared bound passed, as a failed `r:ok()` is a
/// failure rather than a script error.
pub(super) fn overdue(short: &str, limit: std::time::Duration) -> String {
    format!(
        "{short} passed its deadline of {}: brokkr killed its process group",
        crate::output::format_duration(limit)
    )
}

/// What the script sees of a finished child, or why the sample ends.
fn call_outcome(
    short: String,
    run: &crate::sidecar::SidecarRunResult,
    spool: &crate::reservations::Spool,
    stderr: Result<(crate::reservations::Preview, Vec<u8>), String>,
) -> Result<CallOutcome, Abort> {
    // A stop marker wins over whatever the child did after it: the phase it
    // ends was complete, and the rest is what the stop cuts short.
    let (exit, signal, stopped_at) = match &run.data.ending {
        Ending::Exited(code) => (Some(*code), None, None),
        Ending::Signalled(signal) => (None, Some(*signal), None),
        Ending::StopMarker(marker) => (None, None, Some(marker.clone())),
        Ending::Interrupted => return Err(Abort::Interrupted),
        Ending::Deadline(limit) => return Err(Abort::Fail(overdue(&short, *limit))),
        Ending::WaitFailed(why) => {
            return Err(Abort::Failed(format!("waiting for {short} failed: {why}")));
        }
    };
    let preview = spool
        .preview(PREVIEW_BYTES)
        .map_err(|e| Abort::Failed(format!("cannot read {short}'s stdout spool: {e}")))?;
    let (stderr, stderr_tail) =
        stderr.map_err(|e| Abort::Failed(format!("cannot read {short}'s stderr spool: {e}")))?;
    Ok(CallOutcome {
        program: short,
        exit,
        signal,
        stdout: preview.bytes,
        stdout_truncated: preview.truncated,
        stderr: stderr.bytes,
        stderr_truncated: stderr.truncated,
        stderr_tail: Some(stderr_tail),
        stdout_file: None,
        stderr_file: None,
        hotpath: None,
        stopped_at,
        start_error: None,
    })
}

/// Whether a stop is pending: Ctrl-C, `brokkr kill`, or a committed stop
/// (every committed state unwinds).
pub(super) fn stop_pending() -> bool {
    crate::shutdown::should_unwind()
}

/// The hold a launch records its child on, taken before anything is made for
/// the launch: a guard in hand stamps the child whatever the stop state, so a
/// stop committed between the spawn and the stamp cannot hide it from the
/// lock record `brokkr kill --hard` reads. A stop committed before this lookup
/// refuses the launch, and one committed after it is refused at the spawn
/// (`crate::process::spawn`'s launch gate). A bench runs only under the
/// lock, so a missing hold is brokkr's own bug; unit tests alone launch
/// without one, their children unrecorded.
pub(super) fn launch_hold() -> Result<Option<crate::lockfile::LockGuard>, Abort> {
    match crate::lockfile::hold_for_new_work() {
        Ok(hold) => Ok(Some(hold)),
        Err(crate::lockfile::NoHold::Stopped) => Err(Abort::Interrupted),
        #[cfg(test)]
        Err(crate::lockfile::NoHold::Absent) => Ok(None),
        #[cfg(not(test))]
        Err(crate::lockfile::NoHold::Absent) => Err(Abort::Machinery(
            "a bench launch outside the brokkr lock - this is a brokkr bug: \
             `brokkr bench` takes the lock before it runs a script"
                .to_owned(),
        )),
    }
}

/// Every builtin's first step: a pending stop ends the sample here.
pub(super) fn stopping() -> Result<(), Abort> {
    if stop_pending() {
        Err(Abort::Interrupted)
    } else {
        Ok(())
    }
}

impl lua::Host for IterationHost {
    fn stop(&mut self) -> Result<(), Abort> {
        stopping()
    }

    fn stop_requested() -> bool {
        stop_pending()
    }

    fn begin_block(&mut self, at: Instant) {
        self.block_start = Some(at);
    }

    fn binary(&mut self, words: Vec<Word>, options: CallOptions) -> Result<CallOutcome, Abort> {
        stopping()?;
        let declared = self.calls.declare(words, options)?;
        self.launch(&declared)
    }

    fn since(&mut self, sha: &str) -> Result<bool, Abort> {
        stopping()?;
        self.since.ask(sha)
    }

    fn read(&mut self, index: usize) -> Result<Vec<u8>, Abort> {
        stopping()?;
        let handle = self.handle(index, "read()")?;
        self.resources()?
            .read(&handle, lua::MAX_STRING_BYTES)
            .map_err(|e| Abort::Failed(format!("read(): {e}")))
    }

    fn write(&mut self, index: usize, contents: &[u8]) -> Result<(), Abort> {
        stopping()?;
        let handle = self.handle(index, "write()")?;
        self.resources()?
            .write(&handle, contents)
            .map_err(|e| Abort::Failed(format!("write(): {e}")))
    }

    fn filesize(&mut self, index: usize) -> Result<u64, Abort> {
        stopping()?;
        let handle = self.handle(index, "filesize()")?;
        self.resources()?
            .filesize(&handle)
            .map_err(|e| Abort::Failed(format!("filesize(): {e}")))
    }

    fn page_cache(&mut self, _: &str, _: lua::CacheMode) -> Result<Option<String>, Abort> {
        // The seam refuses it inside bench() first.
        Err(Abort::Failed(
            "page_cache() is only available inside preflight()".into(),
        ))
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
