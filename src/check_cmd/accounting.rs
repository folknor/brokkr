// Execution accounting - the record of what a run actually executed, kept apart
// from what it was supposed to cover. Every run that runs tests keeps one (the
// execution inventory and its journal); only a `certifies = "complete"` run is
// held to it as a gate, and every run's record is what the diagnostic
// continuation (continuation.rs) reads.
//
// Two claims, which the coverage audit used to mix:
//
// - POLICY COVERAGE: every (build shape, binary, test) pair the profile could
//   run was selected by some lane, or is legitimately excluded (ignored,
//   quarantined, curated). Decided from the PLAN - the whole profile prepared
//   and enumerated before the first test runs - never from which lanes the
//   test phase happened to reach. A lane a fail-fast never got to still
//   selects its pairs; what it did not do shows up below, as unobserved.
// - EXECUTION ACCOUNTING: every execution the plan expects ends as exactly one
//   outcome - passed, failed, timed_out, interrupted, ignored, unobserved -
//   taken from the typed observations the runner streams (libtest records, the
//   engine's events, the runner's own kills), journaled as they happen.
//
// A timeout is an accounted failure, never coverage passed. Interrupted and
// unobserved are unresolved. A record that is complete after a timeout
// certifies only that the failed run's record is complete, never the gate:
// the gate is the green invariant - every expected execution passed, no
// protocol or attribution anomaly, plan complete.
//
// The journal is the evidence and the reconciliation is pure: it reads the
// plan and the journal file, spawns nothing, builds nothing, arms no deadline.
// Which is what lets it run after a watchdog kill, when the shutdown flag
// refuses every new process - the audit that used to enumerate after the test
// phase could not run at all there.

// `BTreeMap`/`BTreeSet` come from the shared module scope (isolate.rs and
// coverage.rs import them; every check_cmd file is one module).
use std::io::Write as _;

use crate::test_runner::{KillCause, ObsEvent, Observation, ObservationSink, StreamEnd, StreamSource, TestResult};

/// What one test binary IS: its package, its normalized target kind and its
/// target name. The executable path is launch metadata, not identity - the
/// same binary rebuilt is the same unit, and two binaries of one package are
/// two units however their paths compare.
///
/// The kind is normalized the way cargo's selectors read it (`lib` for every
/// library flavour, proc-macro included), which is also nextest's id rule:
/// `rlib`/`cdylib`/`proc-macro` harnesses are the one library harness of
/// their package, and an id built from the raw kind used to orphan every lib
/// pair only the engine lane ran.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct BinaryUnit {
    pub(crate) package_id: String,
    pub(crate) package: String,
    pub(crate) kind: String,
    pub(crate) target: String,
}

impl BinaryUnit {
    pub(crate) fn of(binary: &TestBinary) -> Self {
        Self {
            package_id: binary.package_id.clone(),
            package: binary.package.clone(),
            kind: selector_kind_of(&binary.kind).to_owned(),
            target: binary.target.clone(),
        }
    }

    /// nextest's `RustBinaryId` rendering - the display form, and the key the
    /// engine's events are mapped back onto units by.
    pub(crate) fn id(&self) -> String {
        nextest_metadata::RustBinaryId::from_parts(
            &self.package,
            &nextest_metadata::RustTestBinaryKind::new(self.kind.clone()),
            &self.target,
        )
        .to_string()
    }
}

/// A sweep's build shape as a stable string: everything that decides what
/// cargo compiles ([`ResolvedSweep::build_shape_key`]), hashed. Two lanes of
/// one `[[check]]` entry share it; two shapes never do.
pub(crate) fn shape_id(sweep: &ResolvedSweep) -> String {
    format!(
        "{:016x}",
        xxhash_rust::xxh3::xxh3_64(format!("{:?}", sweep.build_shape_key()).as_bytes())
    )
}

/// The unit policy works on: one test, in one binary, of one cargo resolution
/// of one build shape. No conditional keying: the package-keyed and
/// binary-id-keyed pairs of the old audit were the same thing at two
/// granularities, and the coarse one merged two binaries' same-named tests.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct PairId {
    pub(crate) shape: String,
    pub(crate) resolution: Option<String>,
    pub(crate) unit: BinaryUnit,
    pub(crate) test: String,
}

/// The unit accounting works on: one expected execution of a pair, by one
/// lane. The same pair selected by two lanes is two executions, each of which
/// must be accounted for on its own. `attempt` is always 1 - no lane retries -
/// and is carried so a retry could never be silently folded into a first run.
///
/// The harness invocation is not part of the key; it is ENFORCED instead.
/// Within one lane a pair must run in exactly one harness process (its
/// binary's, or its own on the isolated lane, or the engine's one stream), and
/// reconciliation holds every execution to the first stream that reported it:
/// a start or terminal for the same execution on a second stream is a
/// `duplicate_execution` anomaly, never a second chance to pass or fail.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExecutionId {
    pub(crate) lane: usize,
    pub(crate) attempt: u32,
    pub(crate) pair: PairId,
}

/// How a lane executes its tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaneKind {
    /// `cargo test`, every harness isolated by the shim.
    Serial,
    /// Prebuilt binaries executed concurrently.
    Parallel,
    /// One process per test.
    Isolated,
    /// The nextest engine.
    Nextest,
    /// `cargo test --doc`: doctests only, no binary executions.
    DocOnly,
}

/// One planned test executable: where it is and what it was, so a lane can
/// prove before it runs that it is about to run what was enumerated.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PlannedArtifact {
    pub(crate) resolution: Option<String>,
    pub(crate) unit: BinaryUnit,
    pub(crate) executable: String,
    /// xxh3-64 of the file, hex.
    pub(crate) hash: String,
}

/// One shape resolution's universe.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShapeRecord {
    pub(crate) shape: String,
    pub(crate) resolution: Option<String>,
    pub(crate) label: String,
    /// Every lane of this shape is `curated = true`.
    pub(crate) curated: bool,
    /// The universe was enumerated and every lane of the shape prepared, so
    /// its classification is a finding rather than a guess.
    pub(crate) complete: bool,
    pub(crate) universe: Vec<(BinaryUnit, String)>,
    pub(crate) ignored: Vec<(BinaryUnit, String)>,
}

/// One lane's selection and the executions it implies.
///
/// Persisted through [`LaneRecordWire`]: a lane's pairs all share its shape,
/// and a pair repeats its whole binary unit (a package id URL, a kind, a
/// target) for every test, so the flat form of a workspace's plan was
/// megabytes per lane, per run - and every run, certifying or not, now keeps
/// one. The wire form groups the pairs by (resolution, binary) and drops the
/// shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(from = "LaneRecordWire", into = "LaneRecordWire")]
pub(crate) struct LaneRecord {
    pub(crate) lane: usize,
    pub(crate) label: String,
    pub(crate) kind: LaneKind,
    pub(crate) shape: String,
    /// The lane's inventory was prepared: its executions are known. False
    /// for a lane that could not be prepared and for one whose streams
    /// cannot be attributed to binaries ([`Self::unavailable`] says which).
    pub(crate) prepared: bool,
    /// Why this lane has no inventory, when it has none. Set for a lane a
    /// certifying claim would have refused (cargo-mediated parallelism, a
    /// serial lane with no harness shim) and for one whose preparation
    /// failed under a claim that does not require it: the lane still runs,
    /// and what it observed is reported as such, never as a made-up
    /// selection.
    pub(crate) unavailable: Option<String>,
    /// The lane did not run in this invocation by choice (a CLI `-p` its
    /// config rules out), so it expects nothing.
    pub(crate) skipped: Option<String>,
    /// Binaries of the lane that answer no libtest listing, so the tests
    /// they hold are outside the inventory: `(binary id, why)`. Never present
    /// on a certifying lane, which refuses them.
    pub(crate) unlisted: Vec<(String, String)>,
    pub(crate) include_ignored: bool,
    /// Will run doctests: a `doc_only` lane, or a serial lane under `[test]
    /// doctests = true` whose selection carries no target selector (one
    /// turns cargo's doctest pass off). See [`lane_runs_doctests`]. Only a
    /// carrier's lane may present a rustdoc stream on a strict session.
    pub(crate) doc_carrier: bool,
    /// The carrier must present at least one rustdoc stream to have
    /// completed: a `doc_only` lane always (cargo refuses `--doc` with no
    /// library), a serial carrier when its selection reaches a member whose
    /// cargo metadata marks a target doctested (`DocFacts::obliges` - never
    /// inferred from a library harness, which `test`/`doctest` set
    /// independently). The count beyond one is not knowable - doctests have no
    /// inventory - so one is the floor that tells "ran" from "never ran".
    pub(crate) doc_streams_required: bool,
    /// Every pair this lane is expected to execute.
    pub(crate) executions: Vec<PairId>,
    /// Pairs the lane's filters select that run as `ignored` (the lane does not
    /// lift `#[ignore]`): libtest still reports them, and that report is not an
    /// unplanned test.
    pub(crate) ignored_selected: Vec<PairId>,
    /// `#[bench]` functions the lane's filters admit: outside the claim, but a
    /// `cargo test` run executes each once in test mode and reports it like a
    /// test, so its records are neither accounted nor unplanned.
    pub(crate) outside_claim: Vec<PairId>,
    pub(crate) artifacts: Vec<PlannedArtifact>,
}

impl LaneRecord {
    /// A lane with nothing planned yet.
    pub(crate) fn empty(lane: usize, label: String, kind: LaneKind, shape: String) -> Self {
        Self {
            lane,
            label,
            kind,
            shape,
            prepared: false,
            unavailable: None,
            skipped: None,
            unlisted: Vec::new(),
            include_ignored: false,
            doc_carrier: false,
            doc_streams_required: false,
            executions: Vec::new(),
            ignored_selected: Vec::new(),
            outside_claim: Vec::new(),
            artifacts: Vec::new(),
        }
    }
}

/// One binary's tests of a lane, as persisted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PairGroup {
    resolution: Option<String>,
    unit: BinaryUnit,
    tests: Vec<String>,
}

/// Group pairs by (resolution, binary) in first-seen order.
fn group_pairs(pairs: &[PairId]) -> Vec<PairGroup> {
    let mut out: Vec<PairGroup> = Vec::new();
    for p in pairs {
        match out.iter_mut().rev().find(|g| g.resolution == p.resolution && g.unit == p.unit) {
            Some(g) => g.tests.push(p.test.clone()),
            None => out.push(PairGroup {
                resolution: p.resolution.clone(),
                unit: p.unit.clone(),
                tests: vec![p.test.clone()],
            }),
        }
    }
    out
}

fn ungroup_pairs(shape: &str, groups: Vec<PairGroup>) -> Vec<PairId> {
    groups
        .into_iter()
        .flat_map(|g| {
            let PairGroup { resolution, unit, tests } = g;
            tests.into_iter().map(move |test| PairId {
                shape: shape.to_owned(),
                resolution: resolution.clone(),
                unit: unit.clone(),
                test,
            })
        })
        .collect()
}

/// [`LaneRecord`] as it sits on disk. The defaulted fields are the ones a
/// reader of an older record may not find.
#[derive(serde::Serialize, serde::Deserialize)]
struct LaneRecordWire {
    lane: usize,
    label: String,
    kind: LaneKind,
    shape: String,
    prepared: bool,
    #[serde(default)]
    unavailable: Option<String>,
    #[serde(default)]
    skipped: Option<String>,
    #[serde(default)]
    unlisted: Vec<(String, String)>,
    include_ignored: bool,
    doc_carrier: bool,
    #[serde(default)]
    doc_streams_required: bool,
    executions: Vec<PairGroup>,
    ignored_selected: Vec<PairGroup>,
    #[serde(default)]
    outside_claim: Vec<PairGroup>,
    artifacts: Vec<PlannedArtifact>,
}

impl From<LaneRecord> for LaneRecordWire {
    fn from(l: LaneRecord) -> Self {
        Self {
            executions: group_pairs(&l.executions),
            ignored_selected: group_pairs(&l.ignored_selected),
            outside_claim: group_pairs(&l.outside_claim),
            lane: l.lane,
            label: l.label,
            kind: l.kind,
            shape: l.shape,
            prepared: l.prepared,
            unavailable: l.unavailable,
            skipped: l.skipped,
            unlisted: l.unlisted,
            include_ignored: l.include_ignored,
            doc_carrier: l.doc_carrier,
            doc_streams_required: l.doc_streams_required,
            artifacts: l.artifacts,
        }
    }
}

impl From<LaneRecordWire> for LaneRecord {
    fn from(w: LaneRecordWire) -> Self {
        Self {
            executions: ungroup_pairs(&w.shape, w.executions),
            ignored_selected: ungroup_pairs(&w.shape, w.ignored_selected),
            outside_claim: ungroup_pairs(&w.shape, w.outside_claim),
            lane: w.lane,
            label: w.label,
            kind: w.kind,
            shape: w.shape,
            prepared: w.prepared,
            unavailable: w.unavailable,
            skipped: w.skipped,
            unlisted: w.unlisted,
            include_ignored: w.include_ignored,
            doc_carrier: w.doc_carrier,
            doc_streams_required: w.doc_streams_required,
            artifacts: w.artifacts,
        }
    }
}

/// A dead `skip`/`only` filter found during preparation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DeadFilterRecord {
    pub(crate) sweeps: String,
    pub(crate) origin: String,
    pub(crate) label: String,
}

/// The whole invocation, prepared: written to disk before the first test runs.
///
/// Two products live here and are kept apart. The EXECUTION INVENTORY - each
/// lane's selected executions, the binaries they live in and the artifacts'
/// identity - is prepared for every enumerable lane of every run and
/// journaled against, certifying or not.
/// The POLICY UNIVERSE (`shapes`, `dead_filters`) is prepared only under a
/// certifying claim, because only that claim is about what the profile does
/// not select.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct AccountingPlan {
    pub(crate) run_id: String,
    /// The run is held to a `certifies = "complete"` claim: its policy
    /// universe was prepared, its lanes were held to the attribution rules
    /// that claim needs, and its executions are held to the green invariant.
    /// A plan without it is an inventory and nothing more.
    #[serde(default)]
    pub(crate) certifying: bool,
    /// False when anything could not be prepared; `incomplete` says what.
    pub(crate) complete: bool,
    pub(crate) incomplete: Vec<String>,
    pub(crate) shapes: Vec<ShapeRecord>,
    pub(crate) lanes: Vec<LaneRecord>,
    pub(crate) dead_filters: Vec<DeadFilterRecord>,
}

impl AccountingPlan {
    pub(crate) fn lane(&self, lane: usize) -> Option<&LaneRecord> {
        self.lanes.iter().find(|l| l.lane == lane)
    }
}

// xxh3-64 of a file's contents: the runner's, since the strict harness shim
// checks a harness's content at its handshake too.
use crate::test_runner::hash_file;

/// Whether a lane will run doctests - the one definition the plan's carrier
/// flag and the strict shim's rustdoc admission share. A `doc_only` lane does;
/// a serial libtest lane does under `[test] doctests = true` unless its
/// selection names a target (`run_one_test_sweep` then leaves cargo's doctest
/// pass off, and `--tests` is not appended either); no other lane runs any.
pub(crate) fn lane_runs_doctests(sweep: &ResolvedSweep, doctests: bool) -> bool {
    match lane_kind(sweep) {
        LaneKind::DocOnly => true,
        // The package selection, profile and feature flags never name a
        // target, so the lane's own target filters decide it, whatever
        // packages the lane selects.
        LaneKind::Serial => doctests && !has_target_selector(&sweep.cargo_test_filters),
        LaneKind::Parallel | LaneKind::Isolated | LaneKind::Nextest => false,
    }
}

/// Every way the artifacts a lane is about to run differ from the ones the
/// plan enumerated: a planned executable missing or rebuilt with other
/// content, or an executable the plan never saw. Path AND content, because a
/// shape whose difference cargo does not hash into the file name (an env var a
/// build script reads) rebuilds the same path in place - and the binary at
/// that path is then another shape's, which no path comparison can see.
pub(crate) fn artifact_drift(
    planned: &[PlannedArtifact],
    current: &[(Option<String>, String, String)],
) -> Vec<String> {
    let mut out = Vec::new();
    let now: BTreeMap<(&Option<String>, &str), &str> = current
        .iter()
        .map(|(res, path, hash)| ((res, path.as_str()), hash.as_str()))
        .collect();
    for p in planned {
        match now.get(&(&p.resolution, p.executable.as_str())) {
            None => out.push(format!("{} ({}) is gone", p.unit.id(), p.executable)),
            Some(hash) if *hash != p.hash => out.push(format!(
                "{} ({}) was rebuilt with different content",
                p.unit.id(),
                p.executable
            )),
            Some(_) => {}
        }
    }
    let known: BTreeSet<(&Option<String>, &str)> =
        planned.iter().map(|p| (&p.resolution, p.executable.as_str())).collect();
    for (res, path, _) in current {
        if !known.contains(&(res, path.as_str())) {
            out.push(format!("{path} was not in the plan"));
        }
    }
    out
}

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

impl Termination {
    /// Whether this termination is a per-test deadline charged to exactly
    /// this execution.
    fn charges(&self, resolution: &Option<String>, unit: &BinaryUnit, test: &str) -> bool {
        self.cause == TerminationCause::PerTestDeadline
            && self.test.as_deref() == Some(test)
            && self.charged.as_ref().is_none_or(|c| &c.resolution == resolution && &c.unit == unit)
    }

    /// What this termination means for an execution it did not charge: a
    /// per-test deadline elsewhere is a sibling's timeout, anything else is its
    /// own cause.
    fn detail_for(&self, resolution: &Option<String>, unit: &BinaryUnit, test: &str) -> Detail {
        if self.cause == TerminationCause::PerTestDeadline && !self.charges(resolution, unit, test) {
            Detail::SiblingTimeout
        } else {
            Detail::of_cause(self.cause)
        }
    }
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
fn write_whole(fd: i32, mut buf: &[u8]) -> std::io::Result<()> {
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
const ACCOUNTING_RUNS_KEPT: usize = 10;

/// A new run id: wall-clock milliseconds and pid, sortable by start.
pub(crate) fn new_run_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{ms}-{}", std::process::id())
}

/// Where the runs live.
pub(crate) fn accounting_base(state_root: &Path) -> PathBuf {
    state_root.join(".brokkr").join("accounting")
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
    prune_accounting_runs(&base, &plan.run_id);
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
/// the newest [`ACCOUNTING_RUNS_KEPT`]. `opening`, the run being opened, never
/// goes.
fn runs_to_prune(names: &[String], opening: &str) -> Vec<String> {
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort_by_key(|n| std::cmp::Reverse((run_id_millis(n), n.as_str())));
    sorted
        .into_iter()
        .skip(ACCOUNTING_RUNS_KEPT)
        .filter(|n| n.as_str() != opening)
        .cloned()
        .collect()
}

/// Apply the retention rule to the run directories under `base`.
fn prune_accounting_runs(base: &Path, opening: &str) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| is_run_id(n))
        .collect();
    for name in runs_to_prune(&names, opening) {
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

// ----- outcomes -----

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Passed,
    Failed,
    TimedOut,
    Interrupted,
    Ignored,
    Unobserved,
}

impl Outcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
            Self::Ignored => "ignored",
            Self::Unobserved => "unobserved",
        }
    }
}

/// Why an outcome is what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Detail {
    Assertion,
    Signal,
    SpawnError,
    PerTestDeadline,
    /// A run-level clock with no test to charge.
    RunDeadline,
    PhaseDeadline,
    SiblingTimeout,
    FailFast,
    /// The drain stopped reading with the pipe still open.
    StreamTruncated,
    ReadError,
    Interrupt,
    /// The stream ended normally without this test's terminal record - its
    /// harness died, with no termination brokkr recorded to explain it.
    MissingTerminal,
    EngineError,
}

impl Detail {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Assertion => "assertion",
            Self::Signal => "signal",
            Self::SpawnError => "spawn_error",
            Self::PerTestDeadline => "per_test_deadline",
            Self::RunDeadline => "run_deadline",
            Self::PhaseDeadline => "phase_deadline",
            Self::SiblingTimeout => "sibling_timeout",
            Self::FailFast => "fail_fast",
            Self::StreamTruncated => "stream_truncated",
            Self::ReadError => "read_error",
            Self::Interrupt => "interrupt",
            Self::MissingTerminal => "missing_terminal",
            Self::EngineError => "engine_error",
        }
    }

    fn of_cause(cause: TerminationCause) -> Self {
        match cause {
            TerminationCause::PerTestDeadline => Self::PerTestDeadline,
            TerminationCause::RunDeadline => Self::RunDeadline,
            TerminationCause::PhaseDeadline => Self::PhaseDeadline,
            TerminationCause::SiblingTimeout => Self::SiblingTimeout,
            TerminationCause::FailFast => Self::FailFast,
            TerminationCause::Interrupt => Self::Interrupt,
            TerminationCause::EngineError => Self::EngineError,
        }
    }
}

/// A protocol or attribution transition that cannot happen in an honest,
/// attributable run. Each one fails the green invariant; none is ever
/// deduplicated away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnomalyKind {
    DuplicateStart,
    StartAfterTerminal,
    RepeatedTerminal,
    TerminalWithoutStart,
    /// One expected execution reported on a second stream: two harness
    /// invocations ran what the plan expects run once.
    DuplicateExecution,
    /// A record for a test the plan does not expect from this binary.
    UnplannedTest,
    OverlappingSuite,
    MissingSuiteClosure,
    SuiteTotalsMismatch,
    /// libtest's slow-test warning: past 60s under a 20s cap.
    SlowWarning,
    /// Test records on a stream nothing in the plan accounts for.
    UnattributedEvents,
    AttributionError,
    /// A harness process exited nonzero, or by a signal, though every test it
    /// reported passed, nothing recorded stopped it and it left no test
    /// unfinished: a teardown abort, an `exit(1)` after the last test. The
    /// tests' own results are whole and green; the process they ran in is not,
    /// and a pass that ignored that would be the pass of a crashed run.
    UncleanExit,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Anomaly {
    pub(crate) kind: AnomalyKind,
    pub(crate) lane: Option<usize>,
    pub(crate) detail: String,
}

/// One expected execution's account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Accounted {
    pub(crate) id: ExecutionId,
    pub(crate) outcome: Outcome,
    pub(crate) detail: Option<Detail>,
}

/// One planned doctest carrier: whether it is known to have run its doctests
/// to the end - its lane finished with nothing stopping it, at least one
/// rustdoc stream presented where the lane's selection holds a doctested target, and
/// every rustdoc stream closed by its writer with its suite closed. Not
/// whether its doctests passed: a failing doctest fails the test verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct CarrierState {
    pub(crate) lane: String,
    pub(crate) completed: bool,
    /// The rustdoc streams this carrier presented.
    pub(crate) streams: usize,
}

/// What the doctest streams said. Doctests cannot be enumerated, so there is
/// no inventory to account against: these are observations, not accounting.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct DoctestObserved {
    pub(crate) passed: u64,
    pub(crate) failed: u64,
    pub(crate) ignored: u64,
    pub(crate) carriers: Vec<CarrierState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccountingStatus {
    /// Every expected execution has a resolved outcome and nothing anomalous
    /// was seen. Says the record is whole - not that the run passed.
    Complete,
    /// Something is unresolved: interrupted or unobserved executions, an
    /// incomplete plan, a journal cut short.
    Incomplete,
    /// The record cannot be trusted: a protocol or attribution anomaly.
    Violated,
}

impl AccountingStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
            Self::Violated => "violated",
        }
    }
}

/// The reconciliation of a plan against its journal.
#[derive(Debug, Clone)]
pub(crate) struct Reconciliation {
    pub(crate) accounted: Vec<Accounted>,
    pub(crate) anomalies: Vec<Anomaly>,
    pub(crate) doctests: DoctestObserved,
    pub(crate) plan_complete: bool,
    pub(crate) journal_closed: bool,
    pub(crate) journal_errors: Vec<String>,
    pub(crate) first_termination: Option<Termination>,
    /// Lanes whose plan stopped describing what they ran, by index, with the
    /// reason ([`JournalRecord::LaneSuperseded`]). Their executions are not
    /// in `accounted` and their observations were not read.
    pub(crate) superseded: BTreeMap<usize, String>,
    /// Streams whose records may not all have been read: brokkr stopped
    /// reading with the pipe open, a read failed, or the stream never said
    /// how it ended. What was lost cannot be known - a later duplicate, an
    /// unplanned test, a suite total - so such a record is never whole,
    /// however complete the terminals that did arrive look.
    pub(crate) truncated_streams: Vec<String>,
}

impl Reconciliation {
    pub(crate) fn count(&self, outcome: Outcome) -> usize {
        self.accounted.iter().filter(|a| a.outcome == outcome).count()
    }

    pub(crate) fn status(&self) -> AccountingStatus {
        if !self.anomalies.is_empty() {
            return AccountingStatus::Violated;
        }
        let unresolved = self
            .accounted
            .iter()
            .any(|a| matches!(a.outcome, Outcome::Interrupted | Outcome::Unobserved));
        if unresolved
            || !self.truncated_streams.is_empty()
            || !self.plan_complete
            || !self.journal_closed
            || !self.journal_errors.is_empty()
        {
            return AccountingStatus::Incomplete;
        }
        AccountingStatus::Complete
    }

    /// The green invariant: the plan complete, every expected execution
    /// passed, nothing anomalous - and every planned doctest carrier known to
    /// have completed. `ignored` fails it as surely as `failed` - an expected
    /// execution is one the lane promised to run. The carrier condition sits
    /// here rather than in [`Self::status`] because the status is the binary
    /// accounting's, and doctests have no inventory to account against: what
    /// can be certified is only that the carrier ran to the end.
    ///
    /// And no termination on the record. Every cause one can carry is a stop
    /// (a deadline, an interrupt, a fail-fast, an engine error), and a
    /// stopped run is not a gate, even where every expected execution had
    /// already passed when it came (the watchdog firing after the last test,
    /// say: everything finished, and the run still did not).
    pub(crate) fn green(&self) -> bool {
        self.status() == AccountingStatus::Complete
            && self.first_termination.is_none()
            && self.accounted.iter().all(|a| a.outcome == Outcome::Passed)
            && self.doctests.carriers.iter().all(|c| c.completed)
    }
}

/// Per-name protocol state within one stream.
#[derive(Default, Clone, Copy)]
struct NameState {
    started: bool,
    terminal: bool,
}

#[derive(Default)]
struct StreamState {
    lane: usize,
    origin: Option<StreamOrigin>,
    suite_open: bool,
    suite_counts: (u64, u64, u64),
    /// Terminal records that were neither a pass nor an ignore.
    bad_terminals: u64,
    names: BTreeMap<(Option<BinaryUnit>, String), NameState>,
    end: Option<StreamEnd>,
    exit: Option<(Option<i32>, Option<i32>)>,
    kills: Vec<Termination>,
    reported_unattributed: bool,
    doctest_suites_unclosed: bool,
}

#[derive(Default)]
struct ExecState {
    started_on: Option<u64>,
    terminal: Option<TestResult>,
    /// The stream that first reported this execution - its harness
    /// invocation. Every later record must come from it.
    owner: Option<u64>,
    /// Streams already reported as a duplicate execution, so one second
    /// harness is one anomaly rather than one per record.
    duplicated_on: BTreeSet<u64>,
}

impl ExecState {
    /// Claim this execution for `stream`: `Ok` when `stream` is (now) its
    /// owner, `Err(first)` when another stream already owns it, `first` being
    /// whether this is the first time `stream` trespassed.
    fn claim(&mut self, stream: u64) -> Result<(), bool> {
        match self.owner {
            None => {
                self.owner = Some(stream);
                Ok(())
            }
            Some(s) if s == stream => Ok(()),
            // `Err(true)` the first time this stream trespasses.
            Some(_) => Err(self.duplicated_on.insert(stream)),
        }
    }
}

type ExecKey = (usize, Option<String>, BinaryUnit, String);

/// Reconcile a plan against its journal. Pure: reads nothing but its
/// arguments.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(crate) fn reconcile(
    plan: &AccountingPlan,
    records: &[JournalRecord],
    journal_closed: bool,
    journal_errors: Vec<String>,
) -> Reconciliation {
    let mut expected: BTreeMap<ExecKey, PairId> = BTreeMap::new();
    let mut ignored_selected: BTreeSet<ExecKey> = BTreeSet::new();
    let mut outside_claim: BTreeSet<ExecKey> = BTreeSet::new();
    // A superseded lane answers to a selection the plan does not hold, so it
    // expects nothing and what it observed is not read.
    let superseded: BTreeMap<usize, String> = records
        .iter()
        .filter_map(|r| match r {
            JournalRecord::LaneSuperseded { lane, reason } => Some((*lane, reason.clone())),
            _ => None,
        })
        .collect();
    for lane in plan.lanes.iter().filter(|l| !superseded.contains_key(&l.lane)) {
        for pair in &lane.executions {
            expected.insert(
                (lane.lane, pair.resolution.clone(), pair.unit.clone(), pair.test.clone()),
                pair.clone(),
            );
        }
        for pair in &lane.ignored_selected {
            ignored_selected.insert((lane.lane, pair.resolution.clone(), pair.unit.clone(), pair.test.clone()));
        }
        for pair in &lane.outside_claim {
            outside_claim.insert((lane.lane, pair.resolution.clone(), pair.unit.clone(), pair.test.clone()));
        }
    }

    let mut anomalies: Vec<Anomaly> = Vec::new();
    let mut anomaly = |kind: AnomalyKind, lane: Option<usize>, detail: String| {
        anomalies.push(Anomaly { kind, lane, detail });
    };
    let mut streams: BTreeMap<u64, StreamState> = BTreeMap::new();
    let mut execs: BTreeMap<ExecKey, ExecState> = BTreeMap::new();
    let mut lane_terms: BTreeMap<usize, Vec<Termination>> = BTreeMap::new();
    let mut run_terms: Vec<Termination> = Vec::new();
    let mut first_termination: Option<Termination> = None;
    let mut spawn_failures: Vec<(usize, StreamOrigin)> = Vec::new();
    // Per lane, whether it finished with nothing yet recorded that stopped
    // it - decided when its `LaneFinished` arrives, so a termination a later
    // lane caused does not reach back and unsettle a carrier that had ended.
    let mut lane_finished_clean: BTreeMap<usize, bool> = BTreeMap::new();
    let mut doctests = DoctestObserved::default();
    let duplicate = |execs: &mut BTreeMap<ExecKey, ExecState>, key: &ExecKey, stream: u64| -> Option<String> {
        let e = execs.entry(key.clone()).or_default();
        match e.claim(stream) {
            Ok(()) => None,
            Err(true) => Some(format!(
                "{}/{} was reported by a second harness invocation (stream {stream}); the plan \
                 expects it run once",
                key.2.id(),
                key.3
            )),
            Err(false) => None,
        }
    };

    for rec in records {
        match rec {
            JournalRecord::Observed { lane, .. } if superseded.contains_key(lane) => {}
            JournalRecord::Observed { lane, stream, origin, event } => {
                let st = streams.entry(*stream).or_default();
                st.lane = *lane;
                if st.origin.is_none() {
                    st.origin = Some(origin.clone());
                }
                let (resolution, unit, libtest) = match origin {
                    StreamOrigin::Binary { resolution, unit, .. } => (resolution.clone(), unit.clone(), true),
                    StreamOrigin::Engine { resolution, unit } => (resolution.clone(), unit.clone(), false),
                    StreamOrigin::Doctest => {
                        match event {
                            ObsEvent::SuiteStarted { .. } => st.doctest_suites_unclosed = true,
                            ObsEvent::SuiteFinished { .. } => st.doctest_suites_unclosed = false,
                            ObsEvent::Finished { result, .. } => match result {
                                TestResult::Ok => doctests.passed += 1,
                                TestResult::Ignored => doctests.ignored += 1,
                                _ => doctests.failed += 1,
                            },
                            ObsEvent::StreamEnded { end } => st.end = Some(*end),
                            ObsEvent::AttributionError { detail } => {
                                anomaly(AnomalyKind::AttributionError, Some(*lane), detail.clone());
                            }
                            _ => {}
                        }
                        continue;
                    }
                    StreamOrigin::Unattributed { detail } => {
                        match event {
                            ObsEvent::AttributionError { detail } => {
                                anomaly(AnomalyKind::AttributionError, Some(*lane), detail.clone());
                            }
                            ObsEvent::StreamEnded { end } => st.end = Some(*end),
                            ObsEvent::SuiteStarted { .. }
                            | ObsEvent::SuiteFinished { .. }
                            | ObsEvent::Started { .. }
                            | ObsEvent::Finished { .. }
                            | ObsEvent::SlowWarning { .. }
                                if !st.reported_unattributed =>
                            {
                                st.reported_unattributed = true;
                                anomaly(
                                    AnomalyKind::UnattributedEvents,
                                    Some(*lane),
                                    format!("test records on a stream the plan cannot attribute ({detail})"),
                                );
                            }
                            _ => {}
                        }
                        continue;
                    }
                };
                let unit_key = (!libtest).then(|| unit.clone());
                match event {
                    ObsEvent::SuiteStarted { .. } => {
                        if libtest {
                            if st.suite_open {
                                anomaly(
                                    AnomalyKind::OverlappingSuite,
                                    Some(*lane),
                                    format!("{}: a suite started inside an open suite", unit.id()),
                                );
                            }
                            st.suite_open = true;
                            st.suite_counts = (0, 0, 0);
                        }
                    }
                    ObsEvent::Started { name } => {
                        let ns = st.names.entry((unit_key, name.clone())).or_default();
                        if ns.terminal {
                            anomaly(
                                AnomalyKind::StartAfterTerminal,
                                Some(*lane),
                                format!("{}/{name} started again after its terminal record", unit.id()),
                            );
                        } else if ns.started {
                            anomaly(
                                AnomalyKind::DuplicateStart,
                                Some(*lane),
                                format!("{}/{name} started twice", unit.id()),
                            );
                        }
                        ns.started = true;
                        let key = (*lane, resolution.clone(), unit.clone(), name.clone());
                        if expected.contains_key(&key) {
                            if let Some(d) = duplicate(&mut execs, &key, *stream) {
                                anomaly(AnomalyKind::DuplicateExecution, Some(*lane), d);
                            }
                            let e = execs.entry(key).or_default();
                            if e.started_on.is_none() && e.owner == Some(*stream) {
                                e.started_on = Some(*stream);
                            }
                        } else if !ignored_selected.contains(&key) && !outside_claim.contains(&key) {
                            anomaly(
                                AnomalyKind::UnplannedTest,
                                Some(*lane),
                                format!("{}/{name} started but the plan does not expect it", unit.id()),
                            );
                        }
                    }
                    ObsEvent::Finished { name, result } => {
                        let ns = st.names.entry((unit_key, name.clone())).or_default();
                        if ns.terminal {
                            anomaly(
                                AnomalyKind::RepeatedTerminal,
                                Some(*lane),
                                format!("{}/{name} reported a second terminal record", unit.id()),
                            );
                        } else if !ns.started {
                            anomaly(
                                AnomalyKind::TerminalWithoutStart,
                                Some(*lane),
                                format!("{}/{name} reported a result it was never seen starting", unit.id()),
                            );
                        }
                        ns.terminal = true;
                        if !matches!(result, TestResult::Ok | TestResult::Ignored) {
                            st.bad_terminals += 1;
                        }
                        if libtest && st.suite_open {
                            match result {
                                TestResult::Ok => st.suite_counts.0 += 1,
                                TestResult::Ignored => st.suite_counts.2 += 1,
                                _ => st.suite_counts.1 += 1,
                            }
                        }
                        let key = (*lane, resolution.clone(), unit.clone(), name.clone());
                        if expected.contains_key(&key) {
                            if let Some(d) = duplicate(&mut execs, &key, *stream) {
                                anomaly(AnomalyKind::DuplicateExecution, Some(*lane), d);
                            }
                            let e = execs.entry(key).or_default();
                            // The owning stream's first terminal stands. A
                            // second one is an anomaly, never a correction:
                            // on the same stream a `repeated_terminal` above,
                            // on another stream a `duplicate_execution` - whose
                            // result, pass or fail, is never taken.
                            if e.terminal.is_none() && e.owner == Some(*stream) {
                                e.terminal = Some(*result);
                            }
                        } else if !(*result == TestResult::Ignored && ignored_selected.contains(&key))
                            && !outside_claim.contains(&key)
                        {
                            anomaly(
                                AnomalyKind::UnplannedTest,
                                Some(*lane),
                                format!("{}/{name} reported a result the plan does not expect", unit.id()),
                            );
                        }
                    }
                    ObsEvent::SlowWarning { name } => anomaly(
                        AnomalyKind::SlowWarning,
                        Some(*lane),
                        format!("{}/{name} ran past libtest's 60s warning", unit.id()),
                    ),
                    ObsEvent::SuiteFinished { passed, failed, ignored } => {
                        if libtest {
                            if !st.suite_open {
                                anomaly(
                                    AnomalyKind::SuiteTotalsMismatch,
                                    Some(*lane),
                                    format!("{}: a suite summary with no open suite", unit.id()),
                                );
                            } else if st.suite_counts != (*passed, *failed, *ignored) {
                                let (p, f, i) = st.suite_counts;
                                anomaly(
                                    AnomalyKind::SuiteTotalsMismatch,
                                    Some(*lane),
                                    format!(
                                        "{}: the suite claims {passed} passed, {failed} failed, \
                                         {ignored} ignored; its records show {p}, {f}, {i}",
                                        unit.id()
                                    ),
                                );
                            }
                            st.suite_open = false;
                        }
                    }
                    ObsEvent::StreamEnded { end } => st.end = Some(*end),
                    ObsEvent::Exited { code, signal } => st.exit = Some((*code, *signal)),
                    ObsEvent::Killed { .. } => {}
                    ObsEvent::AttributionError { detail } => {
                        anomaly(AnomalyKind::AttributionError, Some(*lane), detail.clone());
                    }
                }
            }
            JournalRecord::ProcessExited { lane, stream, code, signal } => {
                let st = streams.entry(*stream).or_default();
                st.lane = *lane;
                st.exit = Some((*code, *signal));
            }
            JournalRecord::SpawnFailed { lane, origin, .. } => spawn_failures.push((*lane, origin.clone())),
            JournalRecord::Terminated(t) => {
                if first_termination.is_none() {
                    first_termination = Some(t.clone());
                }
                match (t.scope, t.stream, t.lane) {
                    (TerminationScope::Stream, Some(s), _) => streams.entry(s).or_default().kills.push(t.clone()),
                    (TerminationScope::Lane, _, Some(l)) => lane_terms.entry(l).or_default().push(t.clone()),
                    _ => run_terms.push(t.clone()),
                }
            }
            JournalRecord::LaneFinished { lane, .. } => {
                let stopped = lane_terms.contains_key(lane)
                    || !run_terms.is_empty()
                    || streams.values().any(|s| s.lane == *lane && !s.kills.is_empty());
                lane_finished_clean.insert(*lane, !stopped);
            }
            JournalRecord::Opened { .. }
            | JournalRecord::LaneStarted { .. }
            | JournalRecord::LaneSuperseded { .. }
            | JournalRecord::Closed => {}
        }
    }

    // A libtest stream its writer closed with the suite still open, with
    // nothing recorded to explain it: the harness ended mid-suite on its own.
    // A stream brokkr stopped reading (cancelled) or could not read is
    // explained by that - its missing records are the truncation's, and its
    // executions say so.
    for st in streams.values() {
        let Some(StreamOrigin::Binary { unit, .. }) = &st.origin else {
            continue;
        };
        let explained = !st.kills.is_empty()
            || lane_terms.contains_key(&st.lane)
            || !run_terms.is_empty();
        if st.suite_open && st.end == Some(StreamEnd::Eof) && !explained {
            anomaly(
                AnomalyKind::MissingSuiteClosure,
                Some(st.lane),
                format!("{}: the stream ended with its suite still open", unit.id()),
            );
        }
    }

    // A harness process that ended badly with every test it reported green and
    // nothing recorded to explain it. A failed test, a kill, a stop, or a test
    // left unfinished already says why the process ended as it did; this is
    // the exit nothing else accounts for.
    for st in streams.values() {
        let Some(StreamOrigin::Binary { unit, .. }) = &st.origin else {
            continue;
        };
        let Some((code, signal)) = st.exit else {
            continue;
        };
        let explained = !st.kills.is_empty() || lane_terms.contains_key(&st.lane) || !run_terms.is_empty();
        let unclean = signal.is_some() || code.is_some_and(|c| c != 0);
        if unclean
            && !explained
            && st.bad_terminals == 0
            && !st.names.is_empty()
            && st.names.values().all(|n| n.terminal)
        {
            let how = match (code, signal) {
                (_, Some(s)) => format!("was killed by signal {s}"),
                (Some(c), None) => format!("exited with code {c}"),
                (None, None) => "exited uncleanly".to_owned(),
            };
            anomaly(
                AnomalyKind::UncleanExit,
                Some(st.lane),
                format!("{}: the harness process {how} though every test it reported passed", unit.id()),
            );
        }
    }

    // Streams whose record may be short. Doctest and attributed binary
    // streams must say how they ended, and must have ended by their writer
    // closing them; any stream that says it was cut short or failed to read
    // is short too, attributed or not.
    let mut truncated_streams: Vec<String> = Vec::new();
    for (id, st) in &streams {
        let name = match &st.origin {
            Some(StreamOrigin::Binary { unit, .. }) => Some(unit.id()),
            Some(StreamOrigin::Doctest) => Some("rustdoc".to_owned()),
            Some(StreamOrigin::Engine { .. } | StreamOrigin::Unattributed { .. }) | None => None,
        };
        let label = plan.lane(st.lane).map_or_else(|| format!("lane {}", st.lane), |l| l.label.clone());
        match (st.end, &name) {
            (Some(StreamEnd::Cancelled), _) => truncated_streams.push(format!(
                "{label}: stream {id}{} was cut short - brokkr stopped reading with the pipe open",
                name.as_ref().map_or_else(String::new, |n| format!(" ({n})"))
            )),
            (Some(StreamEnd::ReadError), _) => truncated_streams.push(format!(
                "{label}: stream {id}{} ended in a read error",
                name.as_ref().map_or_else(String::new, |n| format!(" ({n})"))
            )),
            (None, Some(n)) => truncated_streams.push(format!(
                "{label}: stream {id} ({n}) never recorded how it ended"
            )),
            _ => {}
        }
    }

    for lane in plan.lanes.iter().filter(|l| l.doc_carrier) {
        let doc_streams: Vec<&StreamState> = streams
            .values()
            .filter(|s| s.lane == lane.lane && matches!(s.origin, Some(StreamOrigin::Doctest)))
            .collect();
        // `all` over no streams is vacuously true, which is exactly the
        // carrier that never ran: where the plan says the lane holds a
        // library, its doctests must have presented at least one stream.
        let completed = lane_finished_clean.get(&lane.lane).copied().unwrap_or(false)
            && (!lane.doc_streams_required || !doc_streams.is_empty())
            && doc_streams.iter().all(|s| !s.doctest_suites_unclosed && s.end == Some(StreamEnd::Eof));
        doctests.carriers.push(CarrierState { lane: lane.label.clone(), completed, streams: doc_streams.len() });
    }

    let mut accounted = Vec::with_capacity(expected.len());
    for (key, pair) in &expected {
        let (lane, resolution, unit, test) = key;
        let state = execs.get(key);
        let mine = |s: &StreamState| match &s.origin {
            Some(StreamOrigin::Binary { resolution: r, unit: u, one_test }) => {
                s.lane == *lane && r == resolution && u == unit && one_test.as_ref().is_none_or(|t| t == test)
            }
            Some(StreamOrigin::Engine { .. }) => s.lane == *lane,
            _ => false,
        };
        let my_streams: Vec<&StreamState> = streams.values().filter(|s| mine(s)).collect();
        let lane_term = lane_terms.get(lane).and_then(|v| v.first());
        let run_term = run_terms.first();
        // Every termination read from this execution's point of view: a
        // per-test deadline charged to it is its own, one charged elsewhere
        // is a sibling's timeout.
        let seen_as = |t: &Termination| t.detail_for(resolution, unit, test);
        let wider = || lane_term.or(run_term).map(seen_as);
        let stream_kill = |s: &StreamState| s.kills.first().map(seen_as);
        let (outcome, detail) = if let Some(result) = state.and_then(|s| s.terminal) {
            match result {
                TestResult::Ok => (Outcome::Passed, None),
                TestResult::Failed => (Outcome::Failed, Some(Detail::Assertion)),
                TestResult::Ignored => (Outcome::Ignored, None),
                TestResult::TimedOut => (Outcome::TimedOut, Some(Detail::PerTestDeadline)),
                TestResult::Interrupted => (Outcome::Interrupted, Some(wider().unwrap_or(Detail::FailFast))),
            }
        } else if my_streams.iter().any(|s| s.kills.iter().any(|k| k.charges(resolution, unit, test)))
            || lane_terms.get(lane).is_some_and(|v| v.iter().any(|t| t.charges(resolution, unit, test)))
        {
            (Outcome::TimedOut, Some(Detail::PerTestDeadline))
        } else if spawn_failures.iter().any(|(l, o)| {
            *l == *lane
                && match o {
                    StreamOrigin::Binary { resolution: r, unit: u, one_test } => {
                        r == resolution && u == unit && one_test.as_ref().is_none_or(|t| t == test)
                    }
                    _ => false,
                }
        }) {
            (Outcome::Failed, Some(Detail::SpawnError))
        } else if let Some(started_on) = state.and_then(|s| s.started_on) {
            let s = streams.get(&started_on);
            let one_test = s.is_some_and(|s| {
                matches!(&s.origin, Some(StreamOrigin::Binary { one_test: Some(_), .. }))
            });
            let detail = s
                .and_then(stream_kill)
                .or_else(wider)
                .or_else(|| match s.and_then(|s| s.end) {
                    Some(StreamEnd::Cancelled) => Some(Detail::StreamTruncated),
                    Some(StreamEnd::ReadError) => Some(Detail::ReadError),
                    _ => None,
                });
            match (detail, s.and_then(|s| s.exit)) {
                (Some(d), _) => (Outcome::Interrupted, Some(d)),
                // A crash of a process that ran this one test is that test's
                // failure; in a shared process it is only a suspect's.
                (None, Some((_, Some(_)))) if one_test => (Outcome::Failed, Some(Detail::Signal)),
                (None, Some((_, Some(_)))) => (Outcome::Interrupted, Some(Detail::Signal)),
                (None, _) => (Outcome::Interrupted, Some(Detail::MissingTerminal)),
            }
        } else {
            let one_test_crash = my_streams.iter().find_map(|s| match (&s.origin, s.exit) {
                (Some(StreamOrigin::Binary { one_test: Some(_), .. }), Some((code, signal)))
                    if s.kills.is_empty() && (signal.is_some() || code.is_some_and(|c| c != 0)) =>
                {
                    Some(if signal.is_some() { Detail::Signal } else { Detail::MissingTerminal })
                }
                _ => None,
            });
            match one_test_crash {
                Some(d) => (Outcome::Failed, Some(d)),
                None => {
                    let detail = my_streams
                        .iter()
                        .find_map(|s| stream_kill(s))
                        .or_else(wider)
                        .or_else(|| {
                            my_streams.iter().find_map(|s| match s.end {
                                Some(StreamEnd::Cancelled) => Some(Detail::StreamTruncated),
                                Some(StreamEnd::ReadError) => Some(Detail::ReadError),
                                _ => None,
                            })
                        });
                    (Outcome::Unobserved, detail)
                }
            }
        };
        accounted.push(Accounted {
            id: ExecutionId { lane: *lane, attempt: 1, pair: pair.clone() },
            outcome,
            detail,
        });
    }

    Reconciliation {
        accounted,
        anomalies,
        doctests,
        plan_complete: plan.complete,
        journal_closed,
        journal_errors,
        first_termination,
        superseded,
        truncated_streams,
    }
}

// ----- the `--json` summary, schema 2 -----

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct TerminationSummary {
    pub(crate) kind: String,
    pub(crate) scope: String,
}

impl TerminationSummary {
    /// A recorded termination, with its scope named for a reader: the lane's
    /// label for a lane or stream, `run` otherwise.
    pub(crate) fn of(t: &Termination, label: impl Fn(usize) -> Option<String>) -> Self {
        let scope = match (t.scope, t.lane) {
            (TerminationScope::Run, _) | (_, None) => "run".to_owned(),
            (_, Some(l)) => label(l).unwrap_or_else(|| format!("lane {l}")),
        };
        Self { kind: t.cause.as_str().to_owned(), scope }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PolicyCoverage {
    pub(crate) status: &'static str,
    pub(crate) plan_complete: bool,
    pub(crate) pairs: usize,
    pub(crate) selected: usize,
    pub(crate) ignored: usize,
    pub(crate) quarantined: usize,
    pub(crate) curated: usize,
    pub(crate) orphaned: usize,
    pub(crate) dead_filters: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ExecutionAccounting {
    /// Accounting covers enumerable binary tests only; doctests are their own
    /// block.
    pub(crate) scope: &'static str,
    pub(crate) status: &'static str,
    pub(crate) expected_executions: usize,
    pub(crate) passed: usize,
    pub(crate) failed: usize,
    pub(crate) timed_out: usize,
    pub(crate) interrupted: usize,
    pub(crate) ignored: usize,
    pub(crate) unobserved: usize,
    pub(crate) anomalies: usize,
}

impl ExecutionAccounting {
    pub(crate) fn of(r: &Reconciliation) -> Self {
        Self {
            scope: "binary_tests",
            status: r.status().as_str(),
            expected_executions: r.accounted.len(),
            passed: r.count(Outcome::Passed),
            failed: r.count(Outcome::Failed),
            timed_out: r.count(Outcome::TimedOut),
            interrupted: r.count(Outcome::Interrupted),
            ignored: r.count(Outcome::Ignored),
            unobserved: r.count(Outcome::Unobserved),
            anomalies: r.anomalies.len(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DoctestAccounting {
    /// Always `unavailable`: doctests cannot be enumerated, so there is no
    /// inventory - and an unknown inventory is never reported as zero.
    pub(crate) inventory: &'static str,
    /// Always `unknown`, for the same reason.
    pub(crate) accounting: &'static str,
    pub(crate) observed: DoctestObserved,
}

/// The schema-2 blocks a complete-profile run reports.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AccountingBlocks {
    pub(crate) policy_coverage: PolicyCoverage,
    pub(crate) execution_accounting: ExecutionAccounting,
    pub(crate) doctests: DoctestAccounting,
}

#[cfg(test)]
mod accounting_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::test_runner::{ObsEvent, StreamEnd, TestResult};

    fn unit(package: &str, kind: &str, target: &str) -> BinaryUnit {
        BinaryUnit {
            package_id: format!("path+file:///x/{package}#{package}@0.1.0"),
            package: package.into(),
            kind: kind.into(),
            target: target.into(),
        }
    }

    fn pair(u: &BinaryUnit, test: &str) -> PairId {
        PairId { shape: "s".into(), resolution: None, unit: u.clone(), test: test.into() }
    }

    fn lane(idx: usize, executions: Vec<PairId>) -> LaneRecord {
        LaneRecord {
            prepared: true,
            executions,
            ..LaneRecord::empty(idx, format!("lane{idx}"), LaneKind::Parallel, "s".into())
        }
    }

    fn plan(lanes: Vec<LaneRecord>) -> AccountingPlan {
        AccountingPlan { run_id: "t".into(), complete: true, lanes, ..AccountingPlan::default() }
    }

    fn obs(lane: usize, stream: u64, u: &BinaryUnit, event: ObsEvent) -> JournalRecord {
        JournalRecord::Observed {
            lane,
            stream,
            origin: StreamOrigin::Binary { resolution: None, unit: u.clone(), one_test: None },
            event,
        }
    }

    fn started(name: &str) -> ObsEvent {
        ObsEvent::Started { name: name.into() }
    }

    fn finished(name: &str, result: TestResult) -> ObsEvent {
        ObsEvent::Finished { name: name.into(), result }
    }

    fn outcome_of(r: &Reconciliation, lane: usize, test: &str) -> (Outcome, Option<Detail>) {
        let a = r
            .accounted
            .iter()
            .find(|a| a.id.lane == lane && a.id.pair.test == test)
            .unwrap();
        (a.outcome, a.detail)
    }

    /// A clean suite: started, ok, summarised, closed.
    fn clean_run(lane: usize, stream: u64, u: &BinaryUnit, tests: &[&str]) -> Vec<JournalRecord> {
        let mut out = vec![obs(lane, stream, u, ObsEvent::SuiteStarted { test_count: tests.len() as u64 })];
        for t in tests {
            out.push(obs(lane, stream, u, started(t)));
            out.push(obs(lane, stream, u, finished(t, TestResult::Ok)));
        }
        out.push(obs(
            lane,
            stream,
            u,
            ObsEvent::SuiteFinished { passed: tests.len() as u64, failed: 0, ignored: 0 },
        ));
        out.push(obs(lane, stream, u, ObsEvent::StreamEnded { end: StreamEnd::Eof }));
        out
    }

    #[test]
    fn a_clean_run_is_green() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a"), pair(&u, "b")])]);
        let mut records = clean_run(0, 1, &u, &["a", "b"]);
        records.push(JournalRecord::Closed);
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(r.count(Outcome::Passed), 2);
        assert_eq!(r.status(), AccountingStatus::Complete);
        assert!(r.green());
    }

    /// Validation case: a harness that passes every test and then exits
    /// nonzero (or dies by a signal) during teardown is not a pass. The tests'
    /// own records are whole and green; the process is not, and nothing else
    /// on the record explains it - an anomaly, so the run is not green. A
    /// nonzero exit that a failed test, a kill or a stop explains is not
    /// double-counted, and a clean exit is clean.
    #[test]
    fn a_harness_exiting_badly_after_every_test_passed_is_an_anomaly() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a")])]);
        let exited = |code: Option<i32>, signal: Option<i32>| JournalRecord::ProcessExited { lane: 0, stream: 1, code, signal };
        let run = |extra: JournalRecord| {
            let mut records = clean_run(0, 1, &u, &["a"]);
            records.push(extra);
            records.push(JournalRecord::Closed);
            reconcile(&p, &records, true, Vec::new())
        };

        let clean = run(exited(Some(0), None));
        assert!(clean.anomalies.is_empty() && clean.green());

        for bad in [exited(Some(101), None), exited(None, Some(6))] {
            let r = run(bad);
            assert_eq!(outcome_of(&r, 0, "a").0, Outcome::Passed, "the test itself passed");
            assert_eq!(r.anomalies.len(), 1, "{:?}", r.anomalies);
            assert_eq!(r.anomalies[0].kind, AnomalyKind::UncleanExit);
            assert_eq!(r.status(), AccountingStatus::Violated);
            assert!(!r.green());
        }

        // A failed test already explains the nonzero exit.
        let mut failing = vec![
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 1 }),
            obs(0, 1, &u, started("a")),
            obs(0, 1, &u, finished("a", TestResult::Failed)),
            obs(0, 1, &u, ObsEvent::SuiteFinished { passed: 0, failed: 1, ignored: 0 }),
            obs(0, 1, &u, ObsEvent::StreamEnded { end: StreamEnd::Eof }),
        ];
        failing.push(exited(Some(101), None));
        let r = reconcile(&p, &failing, true, Vec::new());
        assert!(r.anomalies.is_empty(), "{:?}", r.anomalies);

        // So does a stop the run recorded.
        let mut stopped = clean_run(0, 1, &u, &["a"]);
        stopped.push(exited(None, Some(9)));
        stopped.push(JournalRecord::Terminated(Termination {
            scope: TerminationScope::Run,
            lane: None,
            stream: None,
            cause: TerminationCause::PhaseDeadline,
            test: None,
            charged: None,
        }));
        let r = reconcile(&p, &stopped, true, Vec::new());
        assert!(r.anomalies.is_empty(), "{:?}", r.anomalies);
    }

    /// Validation case: two same-named tests in different binaries of one
    /// package are two pairs. Under the old package keying they merged into
    /// one - a pass in one binary counted for the other.
    #[test]
    fn two_binaries_of_one_package_with_one_test_name_are_two_pairs() {
        let lib = unit("infra", "lib", "infra");
        let it = unit("infra", "test", "cache_redis");
        assert_ne!(pair(&lib, "serial_tests::t"), pair(&it, "serial_tests::t"));
        let p = plan(vec![lane(0, vec![pair(&lib, "serial_tests::t"), pair(&it, "serial_tests::t")])]);
        // Only the lib binary reports it.
        let records = clean_run(0, 1, &lib, &["serial_tests::t"]);
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(r.accounted.len(), 2);
        assert_eq!(r.count(Outcome::Passed), 1);
        assert_eq!(r.count(Outcome::Unobserved), 1);
        assert!(!r.green());
    }

    /// Validation case: one pair selected by two lanes is two expected
    /// executions, each accounted on its own.
    #[test]
    fn one_pair_in_two_lanes_is_two_executions() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a")]), lane(1, vec![pair(&u, "a")])]);
        let records = clean_run(0, 1, &u, &["a"]);
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(r.accounted.len(), 2);
        assert_eq!(outcome_of(&r, 0, "a").0, Outcome::Passed);
        assert_eq!(outcome_of(&r, 1, "a").0, Outcome::Unobserved);
    }

    /// Validation case: a terminal record with no start still records its
    /// outcome, and the transition is an anomaly that fails the invariant.
    #[test]
    fn a_terminal_without_a_start_records_its_outcome_and_an_anomaly() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a")])]);
        let records = vec![
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 1 }),
            obs(0, 1, &u, finished("a", TestResult::Ok)),
            obs(0, 1, &u, ObsEvent::SuiteFinished { passed: 1, failed: 0, ignored: 0 }),
            obs(0, 1, &u, ObsEvent::StreamEnded { end: StreamEnd::Eof }),
            JournalRecord::Closed,
        ];
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(outcome_of(&r, 0, "a").0, Outcome::Passed);
        assert_eq!(r.anomalies.len(), 1);
        assert_eq!(r.anomalies[0].kind, AnomalyKind::TerminalWithoutStart);
        assert_eq!(r.status(), AccountingStatus::Violated);
        assert!(!r.green());
    }

    /// Never silently deduplicated: two identical `ok` records are a
    /// repeated terminal, a start after one is its own anomaly, and so is a
    /// suite whose totals disagree with its records.
    #[test]
    fn protocol_transitions_are_each_an_anomaly() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a")])]);
        let records = vec![
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 1 }),
            obs(0, 1, &u, started("a")),
            obs(0, 1, &u, finished("a", TestResult::Ok)),
            obs(0, 1, &u, finished("a", TestResult::Ok)),
            obs(0, 1, &u, started("a")),
            obs(0, 1, &u, started("ghost")),
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 1 }),
            obs(0, 1, &u, ObsEvent::SuiteFinished { passed: 5, failed: 0, ignored: 0 }),
        ];
        let r = reconcile(&p, &records, true, Vec::new());
        let kinds: Vec<AnomalyKind> = r.anomalies.iter().map(|a| a.kind).collect();
        assert!(kinds.contains(&AnomalyKind::RepeatedTerminal), "{kinds:?}");
        assert!(kinds.contains(&AnomalyKind::StartAfterTerminal), "{kinds:?}");
        assert!(kinds.contains(&AnomalyKind::UnplannedTest), "{kinds:?}");
        assert!(kinds.contains(&AnomalyKind::OverlappingSuite), "{kinds:?}");
        assert!(kinds.contains(&AnomalyKind::SuiteTotalsMismatch), "{kinds:?}");
    }

    /// An ignored test the lane's filters select but does not lift is not an
    /// unplanned record: libtest reports every matched name.
    #[test]
    fn an_ignored_selected_test_reporting_ignored_is_expected() {
        let u = unit("core", "lib", "core");
        let mut l = lane(0, vec![pair(&u, "a")]);
        l.ignored_selected = vec![pair(&u, "slow_manual")];
        let p = plan(vec![l]);
        let mut records = vec![obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 2 })];
        records.push(obs(0, 1, &u, started("a")));
        records.push(obs(0, 1, &u, finished("a", TestResult::Ok)));
        records.push(obs(0, 1, &u, started("slow_manual")));
        records.push(obs(0, 1, &u, finished("slow_manual", TestResult::Ignored)));
        records.push(obs(0, 1, &u, ObsEvent::SuiteFinished { passed: 1, failed: 0, ignored: 1 }));
        records.push(obs(0, 1, &u, ObsEvent::StreamEnded { end: StreamEnd::Eof }));
        let r = reconcile(&p, &records, true, Vec::new());
        assert!(r.anomalies.is_empty(), "{:?}", r.anomalies);
        assert!(r.green());
    }

    /// A `#[bench]` run once in test mode reports like a test, but it is
    /// outside the claim: neither an execution nor an unplanned record.
    #[test]
    fn a_benchmark_in_test_mode_is_outside_the_claim() {
        let u = unit("core", "lib", "core");
        let mut l = lane(0, vec![pair(&u, "a")]);
        l.outside_claim = vec![pair(&u, "bench_parse")];
        let p = plan(vec![l]);
        let mut records = clean_run(0, 1, &u, &["a", "bench_parse"]);
        // The suite's own tally counts the bench as passed, as libtest does.
        records.retain(|r| !matches!(r, JournalRecord::Observed { event: ObsEvent::SuiteFinished { .. }, .. }));
        records.insert(
            records.len() - 1,
            obs(0, 1, &u, ObsEvent::SuiteFinished { passed: 2, failed: 0, ignored: 0 }),
        );
        let r = reconcile(&p, &records, true, Vec::new());
        assert!(r.anomalies.is_empty(), "{:?}", r.anomalies);
        assert_eq!(r.accounted.len(), 1);
        assert!(r.green());
    }

    /// Validation case: the drain cut short by its grace period marks the
    /// in-flight execution `stream_truncated` - never passed, never a guess at
    /// a failure.
    #[test]
    fn a_cancelled_drain_marks_the_in_flight_execution_stream_truncated() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a"), pair(&u, "b")])]);
        let records = vec![
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 2 }),
            obs(0, 1, &u, started("a")),
            obs(0, 1, &u, ObsEvent::StreamEnded { end: StreamEnd::Cancelled }),
            JournalRecord::Closed,
        ];
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(outcome_of(&r, 0, "a"), (Outcome::Interrupted, Some(Detail::StreamTruncated)));
        assert_eq!(outcome_of(&r, 0, "b"), (Outcome::Unobserved, Some(Detail::StreamTruncated)));
        assert_eq!(r.status(), AccountingStatus::Incomplete);
    }

    /// The per-test cap naming a watched in-flight test times that test out;
    /// its siblings in the killed process are interrupted, queued ones never
    /// observed. A run-level clock with no defensible identity times nobody
    /// out.
    #[test]
    fn a_per_test_kill_is_charged_to_its_test_and_no_other() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a"), pair(&u, "b"), pair(&u, "c")])]);
        let kill = |cause, test: Option<&str>| {
            JournalRecord::Terminated(Termination {
                scope: TerminationScope::Stream,
                lane: Some(0),
                stream: Some(1),
                cause,
                test: test.map(str::to_owned),
                charged: None,
            })
        };
        let base = vec![
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 3 }),
            obs(0, 1, &u, started("a")),
            obs(0, 1, &u, started("b")),
        ];
        let mut records = base.clone();
        records.push(kill(TerminationCause::PerTestDeadline, Some("a")));
        records.push(obs(0, 1, &u, ObsEvent::StreamEnded { end: StreamEnd::Eof }));
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(outcome_of(&r, 0, "a"), (Outcome::TimedOut, Some(Detail::PerTestDeadline)));
        assert_eq!(outcome_of(&r, 0, "b"), (Outcome::Interrupted, Some(Detail::SiblingTimeout)));
        assert_eq!(outcome_of(&r, 0, "c"), (Outcome::Unobserved, Some(Detail::SiblingTimeout)));

        let mut records = base;
        records.push(kill(TerminationCause::RunDeadline, None));
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(r.count(Outcome::TimedOut), 0, "no test is invented from a suspect");
        assert_eq!(outcome_of(&r, 0, "a"), (Outcome::Interrupted, Some(Detail::RunDeadline)));
    }

    /// Validation case: the engine's immediate fail-fast. The failing test is
    /// failed, the one the cancellation killed is interrupted, the one never
    /// started unobserved - none of them passed.
    #[test]
    fn engine_fail_fast_leaves_interrupted_and_unobserved_not_passed() {
        let u = unit("core", "test", "suite");
        let mut l = lane(0, vec![pair(&u, "a"), pair(&u, "b"), pair(&u, "c")]);
        l.kind = LaneKind::Nextest;
        let p = plan(vec![l]);
        let engine = |event| JournalRecord::Observed {
            lane: 0,
            stream: 9,
            origin: StreamOrigin::Engine { resolution: None, unit: u.clone() },
            event,
        };
        let records = vec![
            engine(started("a")),
            engine(started("b")),
            engine(finished("a", TestResult::Failed)),
            JournalRecord::Terminated(Termination {
                scope: TerminationScope::Lane,
                lane: Some(0),
                stream: None,
                cause: TerminationCause::FailFast,
                test: None,
                charged: None,
            }),
            engine(finished("b", TestResult::Interrupted)),
            JournalRecord::Closed,
        ];
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(outcome_of(&r, 0, "a").0, Outcome::Failed);
        assert_eq!(outcome_of(&r, 0, "b"), (Outcome::Interrupted, Some(Detail::FailFast)));
        assert_eq!(outcome_of(&r, 0, "c"), (Outcome::Unobserved, Some(Detail::FailFast)));
        assert_eq!(r.count(Outcome::Passed), 0);
        assert!(!r.green());
    }

    /// A one-test process: a crash is that test's failure, and any clock that
    /// killed it was its own deadline.
    #[test]
    fn a_one_test_process_attributes_its_crash_and_its_kill() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a"), pair(&u, "b")])]);
        let one = |stream, test: &str, event| JournalRecord::Observed {
            lane: 0,
            stream,
            origin: StreamOrigin::Binary { resolution: None, unit: u.clone(), one_test: Some(test.into()) },
            event,
        };
        let records = vec![
            one(1, "a", ObsEvent::SuiteStarted { test_count: 1 }),
            one(1, "a", started("a")),
            JournalRecord::ProcessExited { lane: 0, stream: 1, code: None, signal: Some(11) },
            one(2, "b", ObsEvent::SuiteStarted { test_count: 1 }),
            JournalRecord::Terminated(Termination {
                scope: TerminationScope::Stream,
                lane: Some(0),
                stream: Some(2),
                cause: TerminationCause::PerTestDeadline,
                test: Some("b".into()),
                charged: None,
            }),
        ];
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(outcome_of(&r, 0, "a"), (Outcome::Failed, Some(Detail::Signal)));
        assert_eq!(outcome_of(&r, 0, "b"), (Outcome::TimedOut, Some(Detail::PerTestDeadline)));
    }

    /// Validation case: a watchdog during execution. The journal keeps what
    /// ran; the in-flight execution is interrupted by the phase deadline,
    /// every later lane unobserved - never passed - and the record reads
    /// incomplete, which no verdict can turn green.
    #[test]
    fn a_watchdog_during_execution_keeps_the_journal_and_resolves_nothing_as_passed() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a"), pair(&u, "b")]), lane(1, vec![pair(&u, "c")])]);
        let records = vec![
            JournalRecord::Opened { run_id: "t".into() },
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 2 }),
            obs(0, 1, &u, started("a")),
            obs(0, 1, &u, finished("a", TestResult::Ok)),
            obs(0, 1, &u, started("b")),
            JournalRecord::Terminated(Termination {
                scope: TerminationScope::Run,
                lane: None,
                stream: None,
                cause: TerminationCause::PhaseDeadline,
                test: None,
                charged: None,
            }),
        ];
        // No closing line: the journal file is read back exactly as written.
        let dir = crate::test_scratch::scratch("accounting", "watchdog_journal");
        let path = dir.join("journal.jsonl");
        let body: String = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect();
        std::fs::write(&path, body).unwrap();
        let (read, closed, errors) = read_journal(&path);
        assert_eq!(read, records);
        assert!(!closed, "a journal with no closing line was cut short");
        let r = reconcile(&p, &read, closed, errors);
        assert_eq!(outcome_of(&r, 0, "a").0, Outcome::Passed);
        assert_eq!(outcome_of(&r, 0, "b"), (Outcome::Interrupted, Some(Detail::PhaseDeadline)));
        assert_eq!(outcome_of(&r, 1, "c"), (Outcome::Unobserved, Some(Detail::PhaseDeadline)));
        assert_eq!(r.status(), AccountingStatus::Incomplete);
        assert!(!r.green());
        let t = TerminationSummary::of(r.first_termination.as_ref().unwrap(), |_| None);
        assert_eq!((t.kind.as_str(), t.scope.as_str()), ("phase_deadline", "run"));
    }

    /// An incomplete plan can never be green, whatever the journal says.
    #[test]
    fn an_incomplete_plan_is_never_green() {
        let u = unit("core", "lib", "core");
        let mut p = plan(vec![lane(0, vec![pair(&u, "a")])]);
        p.complete = false;
        let r = reconcile(&p, &clean_run(0, 1, &u, &["a"]), true, Vec::new());
        assert_eq!(r.status(), AccountingStatus::Incomplete);
        assert!(!r.green());
    }

    /// A run-level termination recorded after every execution finished - the
    /// watchdog firing once the last test passed - still prevents green: the
    /// record is whole, but the run was stopped.
    #[test]
    fn a_termination_after_every_execution_finished_is_not_green() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a")])]);
        let mut records = clean_run(0, 1, &u, &["a"]);
        records.push(JournalRecord::LaneFinished { lane: 0, passed: true });
        records.push(JournalRecord::Terminated(Termination {
            scope: TerminationScope::Run,
            lane: None,
            stream: None,
            cause: TerminationCause::PhaseDeadline,
            test: None,
            charged: None,
        }));
        records.push(JournalRecord::Closed);
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(r.count(Outcome::Passed), 1);
        assert_eq!(r.status(), AccountingStatus::Complete, "the record itself is whole");
        assert!(!r.green(), "a stopped run is never green");
    }

    /// A record whose write did not finish leaves a trailing segment with no
    /// line ending. It is reported, never parsed - even where the cut left
    /// text that happens to be valid JSON.
    #[test]
    fn a_partial_trailing_record_is_reported_not_parsed() {
        let dir = crate::test_scratch::scratch("accounting", "partial_tail");
        let path = dir.join("journal.jsonl");
        let opened = serde_json::to_string(&JournalRecord::Opened { run_id: "t".into() }).unwrap();
        let closed = serde_json::to_string(&JournalRecord::Closed).unwrap();
        // A whole record, then one that parses but was never terminated.
        std::fs::write(&path, format!("{opened}\n{closed}")).unwrap();
        let (read, is_closed, errors) = read_journal(&path);
        assert_eq!(read, vec![JournalRecord::Opened { run_id: "t".into() }]);
        assert!(!is_closed, "an unterminated closing line does not close the journal");
        assert!(errors.iter().any(|e| e.contains("partial record")), "{errors:?}");
        // A malformed tail mid-record, too.
        std::fs::write(&path, format!("{opened}\n{{\"record\":\"lane_sta")).unwrap();
        let (_, _, errors) = read_journal(&path);
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    /// The lock-free writer writes whole or reports: a closed descriptor is an
    /// error, never a silently dropped record, and a whole write lands as one
    /// line.
    #[test]
    fn the_lock_free_write_is_whole_or_an_error() {
        use std::os::fd::AsRawFd as _;
        let dir = crate::test_scratch::scratch("accounting", "write_whole");
        let path = dir.join("out");
        let file = std::fs::File::create(&path).unwrap();
        let body = vec![b'x'; 256 * 1024];
        write_whole(file.as_raw_fd(), &body).unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), body.len());
        assert!(write_whole(-1, b"lost\n").is_err());
    }

    /// Validation case: cross-shape artifact replacement. A shape whose
    /// difference cargo does not hash into the file name rebuilds the same
    /// path in place; the content hash is what sees it.
    #[test]
    fn a_planned_artifact_rebuilt_in_place_is_drift() {
        let u = unit("core", "lib", "core");
        let planned = vec![PlannedArtifact {
            resolution: None,
            unit: u,
            executable: "/t/debug/deps/core-1".into(),
            hash: "aaaa".into(),
        }];
        let same = vec![(None, "/t/debug/deps/core-1".to_owned(), "aaaa".to_owned())];
        assert!(artifact_drift(&planned, &same).is_empty());
        let replaced = vec![(None, "/t/debug/deps/core-1".to_owned(), "bbbb".to_owned())];
        let drift = artifact_drift(&planned, &replaced);
        assert_eq!(drift.len(), 1);
        assert!(drift[0].contains("different content"), "{drift:?}");
        let moved = vec![(None, "/t/debug/deps/core-2".to_owned(), "aaaa".to_owned())];
        assert_eq!(artifact_drift(&planned, &moved).len(), 2);
    }

    #[test]
    fn a_file_hash_tracks_content_not_path() {
        let dir = crate::test_scratch::scratch("accounting", "file_hash");
        let a = dir.join("a");
        std::fs::write(&a, b"one").unwrap();
        let first = hash_file(&a).unwrap();
        std::fs::write(&a, b"two").unwrap();
        assert_ne!(hash_file(&a).unwrap(), first);
    }

    /// The engine ids units map to are nextest's own, lib flavours folded.
    #[test]
    fn unit_ids_are_nextests() {
        assert_eq!(unit("pkg", "lib", "pkg").id(), "pkg");
        assert_eq!(unit("pkg", "test", "cache").id(), "pkg::cache");
        assert_eq!(unit("pkg", "bin", "cli").id(), "pkg::bin/cli");
        let b = test_binary_for_tests("pkg", "rlib", "pkg");
        assert_eq!(BinaryUnit::of(&b).id(), "pkg");
        let b = test_binary_for_tests("pkg", "proc-macro", "pkg");
        assert_eq!(BinaryUnit::of(&b).kind, "lib");
    }

    /// Every expected test reported `ok`, and then the drain was cancelled
    /// (or a read failed, or the stream never said how it ended). What
    /// followed is lost - a duplicate, an unplanned test, the suite total -
    /// so the record is not whole and cannot be green, whatever the
    /// terminals that did arrive say.
    #[test]
    fn a_truncated_stream_is_never_whole_even_with_every_terminal_ok() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a")])]);
        for end in [Some(StreamEnd::Cancelled), Some(StreamEnd::ReadError), None] {
            let mut records = vec![
                obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 1 }),
                obs(0, 1, &u, started("a")),
                obs(0, 1, &u, finished("a", TestResult::Ok)),
            ];
            if let Some(end) = end {
                records.push(obs(0, 1, &u, ObsEvent::StreamEnded { end }));
            }
            records.push(JournalRecord::Closed);
            let r = reconcile(&p, &records, true, Vec::new());
            assert_eq!(outcome_of(&r, 0, "a").0, Outcome::Passed, "{end:?}");
            assert_eq!(r.truncated_streams.len(), 1, "{end:?}");
            assert_eq!(r.status(), AccountingStatus::Incomplete, "{end:?}");
            assert!(!r.green(), "{end:?}");
        }
    }

    /// Two valid streams running one planned execution: the first stream's
    /// pass stands, the second's failure is never taken, and the second
    /// invocation is an anomaly - not one pass with nothing said.
    #[test]
    fn one_execution_on_two_streams_is_a_duplicate_execution() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a")])]);
        let mut records = clean_run(0, 1, &u, &["a"]);
        records.push(obs(0, 2, &u, ObsEvent::SuiteStarted { test_count: 1 }));
        records.push(obs(0, 2, &u, started("a")));
        records.push(obs(0, 2, &u, finished("a", TestResult::Failed)));
        records.push(obs(0, 2, &u, ObsEvent::SuiteFinished { passed: 0, failed: 1, ignored: 0 }));
        records.push(obs(0, 2, &u, ObsEvent::StreamEnded { end: StreamEnd::Eof }));
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(outcome_of(&r, 0, "a").0, Outcome::Passed);
        let dups = r.anomalies.iter().filter(|a| a.kind == AnomalyKind::DuplicateExecution).count();
        assert_eq!(dups, 1, "one second invocation, one anomaly: {:?}", r.anomalies);
        assert_eq!(r.status(), AccountingStatus::Violated);
        assert!(!r.green());
    }

    fn carrier_lane(idx: usize, required: bool) -> LaneRecord {
        let mut l = lane(idx, Vec::new());
        l.kind = LaneKind::DocOnly;
        l.doc_carrier = true;
        l.doc_streams_required = required;
        l
    }

    fn rustdoc(lane: usize, stream: u64, event: ObsEvent) -> JournalRecord {
        JournalRecord::Observed { lane, stream, origin: StreamOrigin::Doctest, event }
    }

    /// A planned carrier whose lane passed but never presented a rustdoc
    /// stream did not run its doctests: `all` over no streams used to read
    /// that as completed. Completion is now established, and a carrier not
    /// known to have completed fails the green invariant.
    #[test]
    fn a_carrier_with_no_rustdoc_stream_has_not_completed() {
        let u = unit("core", "lib", "core");
        let p = plan(vec![lane(0, vec![pair(&u, "a")]), carrier_lane(1, true)]);
        let mut records = clean_run(0, 1, &u, &["a"]);
        records.push(JournalRecord::LaneStarted { lane: 1 });
        records.push(JournalRecord::LaneFinished { lane: 1, passed: true });
        records.push(JournalRecord::Closed);
        let r = reconcile(&p, &records, true, Vec::new());
        assert_eq!(r.doctests.carriers.len(), 1);
        assert!(!r.doctests.carriers[0].completed);
        assert_eq!(r.status(), AccountingStatus::Complete, "binary accounting is whole");
        assert!(!r.green(), "but the carrier is not established");

        // The same carrier with its stream, closed by rustdoc, completes.
        let mut records = clean_run(0, 1, &u, &["a"]);
        records.push(JournalRecord::LaneStarted { lane: 1 });
        records.push(rustdoc(1, 5, ObsEvent::SuiteStarted { test_count: 1 }));
        records.push(rustdoc(1, 5, ObsEvent::SuiteFinished { passed: 1, failed: 0, ignored: 0 }));
        records.push(rustdoc(1, 5, ObsEvent::StreamEnded { end: StreamEnd::Eof }));
        records.push(JournalRecord::LaneFinished { lane: 1, passed: true });
        records.push(JournalRecord::Closed);
        let r = reconcile(&p, &records, true, Vec::new());
        assert!(r.doctests.carriers[0].completed);
        assert!(r.green());
    }

    /// A carrier whose rustdoc stream was cut short, or whose lane was
    /// stopped, has not completed either.
    #[test]
    fn a_stopped_or_truncated_carrier_has_not_completed() {
        let p = plan(vec![carrier_lane(0, true)]);
        let truncated = vec![
            JournalRecord::LaneStarted { lane: 0 },
            rustdoc(0, 5, ObsEvent::SuiteStarted { test_count: 1 }),
            rustdoc(0, 5, ObsEvent::StreamEnded { end: StreamEnd::Cancelled }),
            JournalRecord::LaneFinished { lane: 0, passed: false },
            JournalRecord::Closed,
        ];
        let r = reconcile(&p, &truncated, true, Vec::new());
        assert!(!r.doctests.carriers[0].completed);
        let stopped = vec![
            JournalRecord::LaneStarted { lane: 0 },
            JournalRecord::Terminated(Termination {
                scope: TerminationScope::Run,
                lane: None,
                stream: None,
                cause: TerminationCause::Interrupt,
                test: None,
                charged: None,
            }),
            JournalRecord::LaneFinished { lane: 0, passed: false },
            JournalRecord::Closed,
        ];
        let r = reconcile(&p, &stopped, true, Vec::new());
        assert!(!r.doctests.carriers[0].completed);
        assert!(!r.green());
    }

    /// The carrier flag says whether the lane will actually run doctests: a
    /// serial lane under `doctests = true` does, unless a target selector
    /// switches cargo's doctest pass off; a direct lane never does.
    #[test]
    fn only_lanes_that_run_doctests_are_carriers() {
        let serial = ResolvedSweep { label: "serial".into(), ..ResolvedSweep::default() };
        assert!(lane_runs_doctests(&serial, true));
        assert!(!lane_runs_doctests(&serial, false));
        let narrowed = ResolvedSweep {
            label: "narrowed".into(),
            cargo_test_filters: vec!["--test".into(), "cli".into()],
            ..ResolvedSweep::default()
        };
        assert!(!lane_runs_doctests(&narrowed, true), "a target selector turns doctests off");
        let doc = ResolvedSweep { label: "doc".into(), doc_only: true, ..ResolvedSweep::default() };
        assert!(lane_runs_doctests(&doc, false));
        let parallel = ResolvedSweep { label: "p".into(), parallel_budget: Some(4), ..ResolvedSweep::default() };
        assert!(!lane_runs_doctests(&parallel, true));
    }

    /// A termination recorded after a carrier's lane finished - a later
    /// lane's failure - does not reach back and unsettle it.
    #[test]
    fn a_later_termination_does_not_unsettle_a_finished_carrier() {
        let p = plan(vec![carrier_lane(0, true)]);
        let records = vec![
            JournalRecord::LaneStarted { lane: 0 },
            rustdoc(0, 5, ObsEvent::SuiteStarted { test_count: 0 }),
            rustdoc(0, 5, ObsEvent::SuiteFinished { passed: 0, failed: 0, ignored: 0 }),
            rustdoc(0, 5, ObsEvent::StreamEnded { end: StreamEnd::Eof }),
            JournalRecord::LaneFinished { lane: 0, passed: true },
            JournalRecord::Terminated(Termination {
                scope: TerminationScope::Run,
                lane: None,
                stream: None,
                cause: TerminationCause::PerTestDeadline,
                test: None,
                charged: None,
            }),
            JournalRecord::Closed,
        ];
        let r = reconcile(&p, &records, true, Vec::new());
        assert!(r.doctests.carriers[0].completed);
    }

    /// The plan persists a lane's pairs grouped by binary and reads them back
    /// as the flat pairs reconciliation works on - same pairs, same order -
    /// without repeating the binary unit for every test.
    #[test]
    fn a_plan_round_trips_through_its_compact_form() {
        let a = unit("core", "lib", "core");
        let b = unit("core", "test", "suite");
        let mut l = lane(0, vec![pair(&a, "x"), pair(&a, "y"), pair(&b, "x")]);
        l.ignored_selected = vec![pair(&b, "slow")];
        l.unavailable = Some("why".into());
        l.unlisted = vec![("core::custom".into(), "no listing".into())];
        let p = AccountingPlan { certifying: true, ..plan(vec![l]) };
        let json = serde_json::to_string(&p).unwrap();
        // The unit's package id is written once per binary, not once per test.
        assert_eq!(json.matches("path+file:///x/core#core@0.1.0").count(), 2 + 1, "{json}");
        let back: AccountingPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lanes[0].executions, p.lanes[0].executions);
        assert_eq!(back.lanes[0].ignored_selected, p.lanes[0].ignored_selected);
        assert_eq!(back.lanes[0].unavailable.as_deref(), Some("why"));
        assert_eq!(back.lanes[0].unlisted, p.lanes[0].unlisted);
        assert!(back.certifying);
    }

    /// Retention: only the newest [`ACCOUNTING_RUNS_KEPT`] runs survive, and the
    /// run being opened survives whatever its age.
    #[test]
    fn retention_keeps_only_the_newest_runs() {
        let name = |i: u32| format!("{}-{i}", 1_700_000_000_000_u64 + u64::from(i) * 60_000);
        let few: Vec<String> = (0..u32::try_from(ACCOUNTING_RUNS_KEPT).unwrap()).map(name).collect();
        assert!(runs_to_prune(&few, "none").is_empty());
        let many: Vec<String> = (0..25_u32).map(name).collect();
        let pruned = runs_to_prune(&many, "none");
        assert_eq!(pruned.len(), 25 - ACCOUNTING_RUNS_KEPT, "{pruned:?}");
        // Newest first: what goes is the oldest.
        assert!(pruned.iter().all(|p| many.iter().position(|m| m == p).unwrap() < 25 - ACCOUNTING_RUNS_KEPT));
        // The run being opened is never removed.
        let opening = many[0].clone();
        assert!(!runs_to_prune(&many, &opening).contains(&opening));
        assert!(is_run_id("1700000000000-42"));
        for bad in ["", "-", "1-", "-1", "../x", "1-2/../3", "a-b", "1-2-3"] {
            assert!(!is_run_id(bad), "{bad}");
        }
    }

    /// Opening a run writes its plan and journal, prunes the run directories
    /// beyond retention, and leaves the journal readable back.
    #[test]
    fn opening_a_run_prunes_old_records_and_journals() {
        let _journal = JOURNAL_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = crate::test_scratch::scratch("accounting", "open_prunes");
        let u = unit("core", "lib", "core");
        let base = accounting_base(&dir);
        for i in 0..20_u32 {
            std::fs::create_dir_all(base.join(format!("{}-{i}", 1_000_000_000_000_u64 + u64::from(i)))).unwrap();
        }
        let mut p = plan(vec![lane(31_001, vec![pair(&u, "a")])]);
        p.run_id = "2000000000100-9".into();
        let paths = accounting_open(&dir, &p).unwrap();
        journal_append(&JournalRecord::LaneStarted { lane: 31_001 });
        journal_close();
        let kept = std::fs::read_dir(&base).unwrap().count();
        assert_eq!(kept, ACCOUNTING_RUNS_KEPT, "the opening run counts among the newest");
        assert!(paths.plan.exists() && paths.journal.exists());
        let (records, closed, errors) = read_journal(&paths.journal);
        assert!(closed && errors.is_empty(), "{errors:?}");
        assert!(records.contains(&JournalRecord::LaneStarted { lane: 31_001 }));
    }
}
