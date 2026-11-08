//! One iteration's host: what the script's builtins do, on the resource
//! protocol, with each child's sidecar evidence kept for the document.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::PREVIEW_BYTES;
use crate::bench::evidence;
use crate::bench::lua::{self, Abort, CallOptions, CallOutcome, HandleRef, Word};
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

    /// Whether the commit under test descends from `sha` (or is it). The sha
    /// is resolved first, so one git cannot name is an error, never a no.
    pub(super) fn ask(&mut self, sha: &str) -> Result<bool, Abort> {
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
    pub(super) measurement: store::Measurement,
    /// The report bytes the run may still capture.
    pub(super) report_budget: usize,
}

/// The most one hotpath report may hold.
pub(super) const REPORT_BYTES: usize = 16 * 1024 * 1024;

/// The most every report of a run may hold together as the store
/// serializes them: half the 256 MiB a results server takes in one
/// publication, which bounds the reports, not the whole run.
pub(super) const RUN_REPORT_BYTES: usize = 128 * 1024 * 1024;

/// Run one iteration: its owner lease, a fresh Lua state, the sample, the
/// release. Returns the document's iteration, the `since` answers, the
/// report budget left and the library modules the sample asked for.
pub(super) fn run_iteration(
    setup: IterationSetup<'_>,
) -> (store::Iteration, Since, usize, BTreeSet<String>) {
    let since = setup.since;
    let report_budget = setup.report_budget;
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
                report_budget,
                BTreeSet::new(),
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
        stages: BTreeMap::new(),
        report_budget,
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
        },
        host.since,
        host.report_budget,
        sample.modules,
    )
}

/// Why a completed sample of a `--hotpath` run fails: no call was marked,
/// or a marked one did not capture. `None` for a timing run, whose
/// captures are evidence only.
fn profile_failure(measurement: store::Measurement, children: &[store::Child]) -> Option<String> {
    if measurement != store::Measurement::Hotpath {
        return None;
    }
    let mut marked = children
        .iter()
        .filter_map(|c| c.hotpath.as_ref())
        .peekable();
    if marked.peek().is_none() {
        return Some("--hotpath: no call of the iteration was marked hotpath".to_owned());
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
    (!failed.is_empty()).then(|| format!("--hotpath: {}", failed.join("; ")))
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
    /// Each stage's marked calls so far, whatever became of them.
    stages: BTreeMap<String, u32>,
    /// The report bytes the run may still capture.
    report_budget: usize,
    since: Since,
}

impl IterationHost {
    /// What became of a finished marked call's report at `path`, read once
    /// nothing holds the call's borrow, and its projection for the script.
    /// Never an error: a failure is the capture's outcome.
    fn capture(&mut self, path: &Path) -> (store::CaptureOutcome, lua::Capture) {
        let limit = REPORT_BYTES.min(self.report_budget);
        let read = match self.resources() {
            Ok(resources) => resources.read_report(path, limit),
            Err(Abort::Interrupted) => Err(DevError::Interrupted),
            Err(other) => Err(DevError::Internal(other.to_string())),
        };
        let outcome = match read {
            Err(error) => store::CaptureOutcome::Unreadable {
                reason: error.to_string(),
            },
            Ok(None) => store::CaptureOutcome::OverBudget,
            Ok(Some(bytes)) if bytes.is_empty() => store::CaptureOutcome::Missing,
            Ok(Some(bytes)) => match decode_report(&bytes) {
                Err(reason) => store::CaptureOutcome::Invalid { reason },
                // Charged as the store will hold it, which may be longer
                // than the file (`1e9` reserialized as `1000000000.0`).
                Ok((report, _)) if stored_len(&report) > self.report_budget => {
                    store::CaptureOutcome::OverBudget
                }
                Ok((report, projection)) => {
                    self.report_budget = self.report_budget.saturating_sub(stored_len(&report));
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

    /// The handle the script's table carries `index` of.
    fn handle(&self, index: usize, function: &str) -> Result<Handle, Abort> {
        self.handles
            .get(index)
            .cloned()
            .ok_or_else(|| Abort::Failed(format!("{function}: handle {index} was never given out")))
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
    fn launch(&mut self, argv: &[String], call: &Declared) -> Result<CallOutcome, Abort> {
        let failed = |what: &str, e: &dyn std::fmt::Display| Abort::Failed(format!("{what}: {e}"));
        let (program, executable) = self.target(call)?;
        let program_str = program.display().to_string();
        let short = program
            .file_name()
            .map_or_else(|| program_str.clone(), |n| n.to_string_lossy().into_owned());
        let (mut fifo, fifo_path) = self.marker_fifo()?;
        let argv_handles: Vec<&Handle> = call.argv.iter().collect();
        let reads: Vec<&Handle> = call.reads.iter().collect();
        let writes: Vec<&Handle> = call.writes.iter().collect();
        let mut admitted = self
            .resources()?
            .call(
                &program,
                &argv_handles,
                &reads,
                &writes,
                call.stdin.as_ref(),
                call.hotpath.is_some(),
            )
            .map_err(|e| failed("the call was refused", &e))?;
        let stdin = admitted.take_stdin();
        let call_stdout = admitted
            .stdout()
            .map_err(|e| failed("cannot open the stdout spool", &e))?;
        let report = admitted.report().map(Path::to_path_buf);
        let report_str = report.as_deref().map(utf8_report).transpose()?;
        let mut env: Vec<(&str, &str)> =
            vec![(crate::sidecar::MARKER_FIFO_ENV, fifo_path.as_str())];
        env.extend(call.env.iter().map(|(n, v)| (n.as_str(), v.as_str())));
        env.extend(report_str.as_deref().map(hotpath_env).into_iter().flatten());
        let arg_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
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
        );
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                drop(guard);
                // Never spawned: the borrow has no holder to wait for.
                drop(admitted.finish());
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
            call.stop_at.as_deref(),
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
        let finished = admitted.finish_observed(&mut || open.pump(&mut fifo));
        let run = open.finish(&mut fifo, finished.is_ok());
        self.keep_child(program_str, argv, &env, pid, start, &run, executable);
        // Judged after every launch that was reaped, whatever its exit, and
        // stored on the child before anything below can end the call.
        let projection = match (&call.hotpath, &report) {
            (Some(stage), Some(path)) => Some(self.keep_capture(stage, path, finished.is_ok())),
            _ => None,
        };
        let spool = finished.map_err(|e| {
            failed(
                &format!("{short} exited but something it started still holds its borrow"),
                &e,
            )
        })?;
        let mut outcome = call_outcome(short, &run, &spool)?;
        outcome.stdout_file = Some(self.give(resources::spool_handle(&spool))?);
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
        let origin = self.block_start.map_or(0, |at| {
            i64::try_from(start.saturating_duration_since(at).as_micros()).unwrap_or(i64::MAX)
        });
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
        });
    }
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

/// Spawn a call's child in its own process group, stderr piped: the
/// snapshot through the choke point, an external program never through
/// libc's `/bin/sh` fallback (`crate::process::spawn::spawn_exact`).
fn spawn_child(
    launch: &Launch<'_>,
    stdin: std::process::Stdio,
    stdout: std::process::Stdio,
) -> Result<std::process::Child, DevError> {
    let policy = crate::process::spawn::ChildPolicy::plain();
    let isolation = crate::shutdown::ChildIsolation::ProcessGroup(launch.guard);
    let cwd = Some(launch.cwd);
    if !launch.external {
        let mut command = policy.command(launch.program, launch.args, cwd, launch.env, isolation)?;
        command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(std::process::Stdio::piped());
        return crate::process::spawn::spawn(command, None);
    }
    let mut command = policy.exact(launch.program, launch.args, cwd, launch.env, isolation)?;
    command
        .stdin(stdin)
        .stdout(stdout)
        .stderr(std::process::Stdio::piped());
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
            resolved.display()
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
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Abort::Failed(format!("the report path {} is not UTF-8", path.display())))
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

/// One call's handles as declared: those in its argv, its `reads`, `writes`
/// and `stdin`, and its environment additions.
struct Declared {
    argv: Vec<Handle>,
    reads: Vec<Handle>,
    writes: Vec<Handle>,
    stdin: Option<Handle>,
    env: Vec<(String, String)>,
    /// The stage a `hotpath` call is marked with.
    hotpath: Option<String>,
    /// The exact span-end marker that ends the child.
    stop_at: Option<String>,
    /// The external program the call runs in place of the snapshot.
    program: Option<String>,
}

/// What the script sees of a finished child, or why the sample ends.
fn call_outcome(
    short: String,
    run: &crate::sidecar::SidecarRunResult,
    spool: &crate::reservations::Spool,
) -> Result<CallOutcome, Abort> {
    // A stop marker wins over whatever the child did after it: the phase it
    // ends was complete, and the rest is what the stop cuts short.
    let (exit, signal, stopped_at) = match &run.data.ending {
        Ending::Exited(code) => (Some(*code), None, None),
        Ending::Signalled(signal) => (None, Some(*signal), None),
        Ending::StopMarker(marker) => (None, None, Some(marker.clone())),
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
        stdout_file: None,
        hotpath: None,
        stopped_at,
        start_error: None,
    })
}

/// Whether a stop is pending: Ctrl-C, `brokkr kill`, or a committed stop.
pub(super) fn stop_pending() -> bool {
    crate::shutdown::should_unwind() || !crate::shutdown::admits_new_work()
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
        let mut argv = Vec::with_capacity(words.len());
        let mut declared = Declared {
            argv: Vec::new(),
            reads: Vec::new(),
            writes: Vec::new(),
            stdin: None,
            env: options.env,
            hotpath: options.hotpath,
            stop_at: options.stop_at,
            program: options.program,
        };
        for word in words {
            match word {
                Word::Text(text) => argv.push(text),
                Word::Handle(index) => {
                    let handle = self.handle(index, "binary()")?;
                    argv.push(handle.path().display().to_string());
                    declared.argv.push(handle);
                }
            }
        }
        for index in options.reads {
            declared
                .reads
                .push(self.handle(index, "binary_with(): reads")?);
        }
        for index in options.writes {
            declared
                .writes
                .push(self.handle(index, "binary_with(): writes")?);
        }
        if let Some(index) = options.stdin {
            declared.stdin = Some(self.handle(index, "binary_with(): stdin")?);
        }
        self.launch(&argv, &declared)
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
