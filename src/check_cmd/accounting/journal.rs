// Journal schema, durable journal I/O, and the live lane observation sink.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::output;
use crate::test_runner::{KillCause, ObsEvent, Observation, ObservationSink, StreamSource};
use super::super::watchdog_fired;
use super::plan::{AccountingPlan, BinaryUnit};

// ----- the journal -----

/// What a stream's records are about, resolved by the lane against its plan
/// when the observation arrives.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum StreamOrigin {
    /// A libtest stream of one planned binary. `one_test` is set when the
    /// process was launched for exactly that test, which is what makes a kill
    /// or a crash attributable to it.
    Binary {
        resolution: Option<String>,
        unit: BinaryUnit,
        one_test: Option<String>,
    },
    /// The engine lane: every record names its own binary.
    Engine { resolution: Option<String>, unit: BinaryUnit },
    /// rustdoc's stream: doctests, outside binary accounting.
    Doctest,
    /// Nothing in the plan accounts for this stream.
    Unattributed { detail: String },
}

/// Why something stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminationCause {
    /// A test crossed its own per-test deadline.
    PerTestDeadline,
    /// A run-level clock (idle, no completion, wall) with no defensible test
    /// to charge.
    RunDeadline,
    /// The phase watchdog.
    PhaseDeadline,
    /// Another execution's timeout stopped this one.
    SiblingTimeout,
    /// A failure elsewhere stopped the run before this was reached.
    FailFast,
    /// A cooperative interrupt (`brokkr kill`, Ctrl-C).
    Interrupt,
    /// The engine could not go on (its reporter failed).
    EngineError,
}

impl TerminationCause {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PerTestDeadline => "per_test_timeout",
            Self::RunDeadline => "run_deadline",
            Self::PhaseDeadline => "phase_deadline",
            Self::SiblingTimeout => "sibling_timeout",
            Self::FailFast => "fail_fast",
            Self::Interrupt => "interrupt",
            Self::EngineError => "engine_error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminationScope {
    Run,
    Lane,
    Stream,
}

/// A termination as a recorded event: what it stopped, and why. Never
/// inferred from a signal status - a SIGKILLed process says nothing about who
/// killed it, and the parallel lane's sibling cancellation looks exactly like
/// a crash from the outside.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Termination {
    pub(crate) scope: TerminationScope,
    pub(crate) lane: Option<usize>,
    pub(crate) stream: Option<u64>,
    pub(crate) cause: TerminationCause,
    /// The test this termination is charged to, when one is known.
    pub(crate) test: Option<String>,
    /// The binary that test ran in, when the recorder knows it. A per-test
    /// deadline with a binary is charged to that one execution and no other;
    /// one without (a shared stream's suspect) is charged by name, which is
    /// only ever unambiguous on a stream that is already one binary's.
    #[serde(default)]
    pub(crate) charged: Option<ChargedTo>,
}

/// The execution a termination names.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ChargedTo {
    pub(crate) resolution: Option<String>,
    pub(crate) unit: BinaryUnit,
}

/// One line of the journal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub(crate) enum JournalRecord {
    Opened {
        run_id: String,
    },
    LaneStarted {
        lane: usize,
    },
    Observed {
        lane: usize,
        stream: u64,
        origin: StreamOrigin,
        event: ObsEvent,
    },
    /// A directly executed process's exit.
    ProcessExited {
        lane: usize,
        stream: u64,
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// A process that could not be started.
    SpawnFailed {
        lane: usize,
        origin: StreamOrigin,
        detail: String,
    },
    Terminated(Termination),
    LaneFinished {
        lane: usize,
        passed: bool,
    },
    /// The lane's plan stopped describing what it ran: an earlier lane's build
    /// replaced its artifacts between the plan and the lane (only a run that
    /// is not certifying survives this - a certifying one refuses the lane).
    /// The lane was prepared again and ran that; its observations answer to
    /// a selection the plan does not hold, so reconciliation drops them and
    /// the lane's inventory is reported unavailable, with this reason.
    LaneSuperseded {
        lane: usize,
        reason: String,
    },
    /// The test phase is over. A journal without this line was cut short.
    Closed,
}

/// The journal of the run in flight. One process-wide file: every lane
/// appends through it, one JSON line per record, each written straight
/// through to the kernel - so a backstop exit, or anything short of the box
/// losing power, leaves every record up to the last one on disk.
static JOURNAL: std::sync::Mutex<Option<JournalFile>> = std::sync::Mutex::new(None);

/// Run `f` while holding the journal mutex, the way a wedged writer would.
#[cfg(test)]
pub(crate) fn with_journal_locked<R>(f: impl FnOnce() -> R) -> R {
    let _guard = JOURNAL.lock().expect("journal lock");
    f()
}

/// A second descriptor on the open journal, for the one writer that must
/// never wait on [`JOURNAL`]: the phase watchdog, which fires exactly when
/// some thread may be wedged - possibly holding that mutex. Both descriptors
/// share one `O_APPEND` file description, so each `write(2)` lands whole at
/// the end of the file whichever path issued it.
///
/// The number is never closed once allocated: the next run's journal is
/// `dup2`'d onto it, so a watchdog that read the number can never write into
/// a file some later `open` reused it for. `JOURNAL_LIVE` says whether it
/// currently names an open run's journal.
static JOURNAL_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
static JOURNAL_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Point [`JOURNAL_FD`] at `file`.
fn publish_journal_fd(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    use std::sync::atomic::Ordering;
    let current = JOURNAL_FD.load(Ordering::Acquire);
    if current >= 0 {
        // SAFETY: both are open descriptors this process owns; dup2 replaces
        // what `current` refers to atomically, keeping the number.
        if unsafe { libc::dup2(file.as_raw_fd(), current) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    } else {
        // SAFETY: duplicating an open descriptor this process owns.
        let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        JOURNAL_FD.store(fd, Ordering::Release);
    }
    JOURNAL_LIVE.store(true, Ordering::Release);
    Ok(())
}

/// Set when the lock-free path failed to land a whole record. The audit
/// reads it as a journal error: the record it lost is the run's cause.
static JOURNAL_UNLOCKED_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a lock-free journal write failed during this process.
pub(crate) fn journal_unlocked_write_failed() -> bool {
    JOURNAL_UNLOCKED_FAILED.load(std::sync::atomic::Ordering::Acquire)
}

/// Write all of `buf` to `fd`: retried on `EINTR`, continued after a short
/// write, an error on anything else (a zero-length write included). A
/// failure part-way leaves a partial line at the end of the file, which the
/// reader reports rather than parses ([`read_journal`]).
pub(super) fn write_whole(fd: i32, mut buf: &[u8]) -> std::io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: the caller hands an open descriptor; the buffer is valid
        // for its length.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let n = usize::try_from(n).unwrap_or(0);
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "write returned 0"));
        }
        buf = &buf[n.min(buf.len())..];
    }
    Ok(())
}

/// Journal a record without taking any lock - the watchdog's path. A line
/// serialized beforehand, written whole ([`write_whole`]): it allocates, but
/// never waits on anything another thread can hold. `Ok` and a no-op when no
/// journal is open.
pub(crate) fn journal_append_unlocked(rec: &JournalRecord) -> std::io::Result<()> {
    use std::sync::atomic::Ordering;
    if !JOURNAL_LIVE.load(Ordering::Acquire) {
        return Ok(());
    }
    let fd = JOURNAL_FD.load(Ordering::Acquire);
    if fd < 0 {
        return Ok(());
    }
    let mut line = serde_json::to_vec(rec).map_err(std::io::Error::other)?;
    line.push(b'\n');
    // `fd` is open for the life of the process (see JOURNAL_FD).
    write_whole(fd, &line)
}

/// The record the phase watchdog leaves when it fires: a run-wide stop by
/// the deadline, journaled before anything is killed, so even a run whose
/// main thread never unwinds (the backstop exit) says why it stopped. A
/// write that fails is said on the forced error channel and flagged for the
/// audit ([`journal_unlocked_write_failed`]) - never silently dropped.
pub(crate) fn journal_watchdog_fired() {
    let rec = JournalRecord::Terminated(Termination {
        scope: TerminationScope::Run,
        lane: None,
        stream: None,
        cause: TerminationCause::PhaseDeadline,
        test: None,
        charged: None,
    });
    if let Err(e) = journal_append_unlocked(&rec) {
        JOURNAL_UNLOCKED_FAILED.store(true, std::sync::atomic::Ordering::Release);
        output::error_forced(&format!("the accounting journal did not take the deadline record: {e}"));
    }
}

/// The journal is one per process, so two tests that open one would write
/// into each other's file. They take this first.
#[cfg(test)]
pub(crate) static JOURNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct JournalFile {
    file: std::fs::File,
    /// The first write that failed. Nothing after it is trusted, so nothing
    /// after it is written - including the closing line, which is what tells
    /// a reader the journal is incomplete.
    failed: Option<String>,
}

/// Where one run's plan and journal live.
#[derive(Debug, Clone)]
pub(crate) struct AccountingPaths {
    pub(crate) plan: PathBuf,
    pub(crate) journal: PathBuf,
}

// Retention. Every invocation that runs tests keeps a run - an inventory is
// kept for the runs nothing certifies too. The run in flight reads its own
// journal back to reconcile it against the plan it holds in memory; nothing
// reads an older run, and runs never overlap (they are opened under the brokkr
// lock). `plan.json` is written for inspection after the fact - the run's
// `prepare:` log line and the `--json` `source_run_id` point at it - so a
// finished run's record is kept only for that, and the newest few are enough.

/// Runs kept, newest first.
pub(super) const ACCOUNTING_RUNS_KEPT: usize = 10;

/// A new run id: wall-clock milliseconds and pid, sortable by start.
pub(crate) fn new_run_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{ms}-{}", std::process::id())
}

/// Where the runs live.
pub(crate) fn accounting_base(state_root: &Path) -> PathBuf {
    crate::clean::layout::ProjectState::new(state_root).accounting_dir()
}

/// A run id is `<ms>-<pid>`; anything else is not a run directory (and, as a
/// path component, must never be able to name another directory).
pub(crate) fn is_run_id(name: &str) -> bool {
    name.split_once('-').is_some_and(|(a, b)| {
        !a.is_empty()
            && a.bytes().all(|c| c.is_ascii_digit())
            && !b.is_empty()
            && b.bytes().all(|c| c.is_ascii_digit())
    })
}

/// Persist the plan and open the journal, before the first test runs.
pub(crate) fn accounting_open(state_root: &Path, plan: &AccountingPlan) -> Result<AccountingPaths, DevError> {
    let base = accounting_base(state_root);
    let dir = base.join(&plan.run_id);
    std::fs::create_dir_all(&dir)?;
    prune_accounting_runs(&base, &plan.run_id, ACCOUNTING_RUNS_KEPT);
    let plan_path = dir.join("plan.json");
    // Compact: a workspace's plan names every pair of every lane, and the
    // file is read by tools, not by eye.
    let body = serde_json::to_vec(plan)
        .map_err(|e| DevError::Build(format!("accounting plan did not serialize: {e}")))?;
    crate::atomic_write::replace(&plan_path, &body)?;
    let journal_path = dir.join("journal.jsonl");
    // Append mode: the watchdog writes through a second descriptor on the
    // same description, and O_APPEND is what keeps the two from overwriting
    // each other's lines.
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&journal_path)?;
    drop(file);
    let file = std::fs::OpenOptions::new().append(true).open(&journal_path)?;
    publish_journal_fd(&file)?;
    if let Ok(mut slot) = JOURNAL.lock() {
        *slot = Some(JournalFile { file, failed: None });
    }
    journal_append(&JournalRecord::Opened { run_id: plan.run_id.clone() });
    Ok(AccountingPaths { plan: plan_path, journal: journal_path })
}

/// The wall-clock milliseconds a run id starts with.
fn run_id_millis(name: &str) -> u128 {
    name.split_once('-').and_then(|(ms, _)| ms.parse::<u128>().ok()).unwrap_or(0)
}

/// Which of `names` (run ids, any order) retention removes: everything beyond
/// the newest `keep`. `opening`, the run being opened, never goes.
pub(super) fn runs_to_prune(names: &[String], opening: &str, keep: usize) -> Vec<String> {
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort_by_key(|n| std::cmp::Reverse((run_id_millis(n), n.as_str())));
    sorted
        .into_iter()
        .skip(keep)
        .filter(|n| n.as_str() != opening)
        .cloned()
        .collect()
}

/// Apply the retention rule to the run directories under `base`.
fn prune_accounting_runs(base: &Path, opening: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| is_run_id(n))
        .collect();
    for name in runs_to_prune(&names, opening, keep) {
        std::fs::remove_dir_all(base.join(name)).ok();
    }
}

/// Append one record to the open journal; a no-op when none is open (a run
/// that never reached the point of opening one keeps no journal).
pub(crate) fn journal_append(rec: &JournalRecord) {
    let Ok(mut slot) = JOURNAL.lock() else {
        return;
    };
    let Some(journal) = slot.as_mut() else {
        return;
    };
    if journal.failed.is_some() {
        return;
    }
    let mut line = match serde_json::to_vec(rec) {
        Ok(l) => l,
        Err(e) => {
            journal.failed = Some(format!("a record did not serialize: {e}"));
            return;
        }
    };
    line.push(b'\n');
    // `File` is unbuffered: `write_all` is the syscall, so the line is the
    // kernel's the moment this returns. No fsync - surviving brokkr's own exit
    // is the requirement, not surviving the machine's.
    if let Err(e) = journal.file.write_all(&line) {
        journal.failed = Some(e.to_string());
    }
}

/// Close the journal: the line that says it is whole.
pub(crate) fn journal_close() {
    journal_append(&JournalRecord::Closed);
    JOURNAL_LIVE.store(false, std::sync::atomic::Ordering::Release);
    if let Ok(mut slot) = JOURNAL.lock() {
        *slot = None;
    }
}

/// Read a journal back: its records, whether it was closed, and what could
/// not be parsed. A journal with no closing line was cut short - by a
/// backstop exit, a crash, a write that failed - and says so.
///
/// Every record is a line ENDED by its newline, which is written with it in
/// one buffer: a trailing segment without one is a write that did not finish,
/// reported and never parsed - a truncated record can still be valid JSON
/// (a cut inside a trailing field's string is not, but a cut right after a
/// closing brace of a nested object can be), and parsing it would take a
/// half-written record as evidence.
pub(crate) fn read_journal(path: &Path) -> (Vec<JournalRecord>, bool, Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), false, vec![format!("{} could not be read", path.display())]);
    };
    let mut records = Vec::new();
    let mut errors = Vec::new();
    let mut segments: Vec<&str> = text.split('\n').collect();
    // `split` leaves the text after the last newline: empty for a whole
    // journal, the partial record otherwise.
    let tail = segments.pop().unwrap_or_default();
    for (i, line) in segments.iter().enumerate() {
        match serde_json::from_str::<JournalRecord>(line) {
            Ok(r) => records.push(r),
            Err(e) => errors.push(format!("journal line {}: {e}", i + 1)),
        }
    }
    if !tail.is_empty() {
        errors.push(format!(
            "journal line {}: a partial record (no line ending) - a write did not finish",
            segments.len() + 1
        ));
    }
    let closed = matches!(records.last(), Some(JournalRecord::Closed));
    (records, closed, errors)
}

/// The cause a cooperative stop is: the phase watchdog when it fired, an
/// interrupt otherwise. Decided here, where the watchdog is known, so the
/// runner never has to.
pub(crate) fn stop_cause() -> TerminationCause {
    if watchdog_fired().is_some() {
        TerminationCause::PhaseDeadline
    } else {
        TerminationCause::Interrupt
    }
}

/// Record why a run ended early with `error`, and return the termination that
/// decided it: a stop (interrupt, phase watchdog) is the run's, and anything
/// else that ends it - a blown budget, a refusal - is the run's too, carrying
/// the lane's own `decisive` termination when it recorded one (a per-test
/// timeout stops brokkr, and everything after it is unobserved because of
/// that timeout). Journaled run-wide, so every execution the run never
/// reached reads as unobserved for a stated reason.
pub(crate) fn record_run_stop(error: &DevError, decisive: Option<Termination>) -> Option<Termination> {
    let run = |cause| {
        let t = Termination { scope: TerminationScope::Run, lane: None, stream: None, cause, test: None, charged: None };
        journal_append(&JournalRecord::Terminated(t.clone()));
        t
    };
    let stop = match error {
        DevError::Interrupted => run(stop_cause()),
        _ if crate::shutdown::is_shutdown_requested() => run(stop_cause()),
        _ => run(decisive.as_ref().map_or(TerminationCause::FailFast, |t| t.cause)),
    };
    decisive.or(Some(stop))
}

/// One lane's handle on the journal. Every record goes to the journal (a
/// no-op when none is open); the tap itself keeps only the two facts the lane
/// reads back - the first termination, which decided the lane, and which
/// planned binaries presented a stream - so a run holds a bounded amount
/// however many records its tests produce. The evidence is the journal, never
/// this.
#[derive(Clone)]
pub(crate) struct LaneTap {
    lane: usize,
    state: std::sync::Arc<std::sync::Mutex<TapState>>,
}

#[derive(Default)]
struct TapState {
    first_termination: Option<Termination>,
    /// Bounded by the lane's planned binaries: only attributed streams add.
    seen_units: BTreeSet<BinaryUnit>,
}

impl LaneTap {
    pub(crate) fn new(lane: usize) -> Self {
        Self { lane, state: std::sync::Arc::default() }
    }

    pub(crate) fn lane(&self) -> usize {
        self.lane
    }

    pub(crate) fn record(&self, rec: JournalRecord) {
        journal_append(&rec);
        if let Ok(mut s) = self.state.lock() {
            match rec {
                JournalRecord::Terminated(t) if s.first_termination.is_none() => {
                    s.first_termination = Some(t);
                }
                JournalRecord::Observed { origin: StreamOrigin::Binary { unit, .. }, .. } => {
                    s.seen_units.insert(unit);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn terminate(&self, scope: TerminationScope, cause: TerminationCause, test: Option<String>) {
        self.record(JournalRecord::Terminated(Termination {
            scope,
            lane: Some(self.lane),
            stream: None,
            cause,
            test,
            charged: None,
        }));
    }

    /// The planned binaries that presented an attributed stream so far.
    pub(crate) fn seen_units(&self) -> BTreeSet<BinaryUnit> {
        self.state.lock().map(|s| s.seen_units.clone()).unwrap_or_default()
    }

    /// The first termination this lane recorded: the one that decided it.
    pub(crate) fn decisive_termination(&self) -> Option<Termination> {
        self.state.lock().ok().and_then(|s| s.first_termination.clone())
    }

    /// A sink for one runner call. `origin` resolves each stream's source
    /// against the lane's plan. `shared` marks a cargo-launched run, whose
    /// own (`Process`) stream is cargo's: a kill charged there took the whole
    /// cargo group down, so it is lane-wide rather than one stream's.
    pub(crate) fn sink(
        &self,
        origin: impl Fn(&StreamSource) -> StreamOrigin + Send + Sync + 'static,
        shared: bool,
    ) -> ObservationSink {
        let tap = self.clone();
        std::sync::Arc::new(move |o: Observation| {
            let origin = origin(&o.source);
            let rec = match o.event {
                ObsEvent::Killed { cause, test } => {
                    let one_test = match &origin {
                        StreamOrigin::Binary { one_test, .. } => one_test.clone(),
                        _ => None,
                    };
                    let (cause, test) = match (cause, one_test) {
                        // A process launched for one test: whatever clock
                        // killed it was that test's own.
                        (KillCause::PerTest | KillCause::NoCompletion | KillCause::Idle | KillCause::Wall, Some(t)) => {
                            (TerminationCause::PerTestDeadline, Some(t))
                        }
                        (KillCause::PerTest, None) => (TerminationCause::PerTestDeadline, test),
                        (KillCause::NoCompletion | KillCause::Idle | KillCause::Wall, None) => {
                            (TerminationCause::RunDeadline, None)
                        }
                        (KillCause::Cancelled, _) => (TerminationCause::SiblingTimeout, None),
                        (KillCause::Stopped, _) => (stop_cause(), None),
                    };
                    let lane_wide = shared && o.source == StreamSource::Process;
                    // A kill on one binary's stream is that binary's: the
                    // test it names is charged there and nowhere else.
                    let charged = match (&origin, &test, lane_wide) {
                        (StreamOrigin::Binary { resolution, unit, .. }, Some(_), false) => {
                            Some(ChargedTo { resolution: resolution.clone(), unit: unit.clone() })
                        }
                        _ => None,
                    };
                    JournalRecord::Terminated(Termination {
                        scope: if lane_wide { TerminationScope::Lane } else { TerminationScope::Stream },
                        lane: Some(tap.lane),
                        stream: Some(o.stream),
                        cause,
                        test,
                        charged,
                    })
                }
                ObsEvent::Exited { code, signal } => {
                    JournalRecord::ProcessExited { lane: tap.lane, stream: o.stream, code, signal }
                }
                event => JournalRecord::Observed { lane: tap.lane, stream: o.stream, origin, event },
            };
            tap.record(rec);
        })
    }
}

