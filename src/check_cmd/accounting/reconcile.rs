// Pure interpretation of the prepared plan and its journal.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use crate::test_runner::{
    ExecutionFailure, ExecutionOutcome, KillCause, LaneVerdict, ObsEvent, ProtocolIssue, StreamEnd,
    StreamOutcome, StreamSource, TestResult, lane_verdict,
};
use super::journal::{JournalRecord, StreamOrigin, Termination, TerminationCause, TerminationScope};
use super::plan::{AccountingPlan, BinaryUnit, ExecutionId, Inventory, LaneIdx, PairId};
use super::super::Resolution;

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
    /// A binary's stream reported a name again in a later suite: its harness
    /// ran the test twice.
    NameAcrossSlots,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExitDisposition {
    Signal(i32),
    Code(i32),
    Unknown,
}

/// Structured context for a reconciliation anomaly. The serializer emits the
/// same prose string as before, while reconciliation keeps the cause and its
/// data typed until presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AnomalyDetail {
    ExternalMessage(String),
    UnattributedEvents { reason: String },
    OverlappingSuite { unit: BinaryUnit },
    StartAfterTerminal { unit: BinaryUnit, test: String },
    DuplicateStart { unit: BinaryUnit, test: String },
    DuplicateExecution { unit: BinaryUnit, test: String, stream: u64 },
    UnplannedStart { unit: BinaryUnit, test: String },
    RepeatedTerminal { unit: BinaryUnit, test: String },
    TerminalWithoutStart { unit: BinaryUnit, test: String },
    NameAcrossSlots { unit: BinaryUnit, test: String },
    UnplannedTerminal { unit: BinaryUnit, test: String },
    SlowWarning { unit: BinaryUnit, test: String },
    SuiteSummaryWithoutOpen { unit: BinaryUnit },
    SuiteTotalsMismatch {
        unit: BinaryUnit,
        reported: (u64, u64, u64),
        observed: (u64, u64, u64),
    },
    MissingSuiteClosure { unit: BinaryUnit },
    UncleanExit { unit: BinaryUnit, disposition: ExitDisposition },
}

impl Display for AnomalyDetail {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExternalMessage(message) => f.write_str(message),
            Self::UnattributedEvents { reason } => {
                write!(f, "test records on a stream the plan cannot attribute ({reason})")
            }
            Self::OverlappingSuite { unit } => write!(f, "{}: a suite started inside an open suite", unit.id()),
            Self::StartAfterTerminal { unit, test } => {
                write!(f, "{}/{test} started again after its terminal record", unit.id())
            }
            Self::DuplicateStart { unit, test } => write!(f, "{}/{test} started twice", unit.id()),
            Self::DuplicateExecution { unit, test, stream } => write!(
                f,
                "{}/{test} was reported by a second harness invocation (stream {stream}); the plan expects it run once",
                unit.id()
            ),
            Self::UnplannedStart { unit, test } => {
                write!(f, "{}/{test} started but the plan does not expect it", unit.id())
            }
            Self::RepeatedTerminal { unit, test } => {
                write!(f, "{}/{test} reported a second terminal record", unit.id())
            }
            Self::TerminalWithoutStart { unit, test } => {
                write!(f, "{}/{test} reported a result it was never seen starting", unit.id())
            }
            Self::NameAcrossSlots { unit, test } => {
                write!(f, "{}/{test} was reported again in a later suite", unit.id())
            }
            Self::UnplannedTerminal { unit, test } => {
                write!(f, "{}/{test} reported a result the plan does not expect", unit.id())
            }
            Self::SlowWarning { unit, test } => {
                write!(f, "{}/{test} ran past libtest's 60s warning", unit.id())
            }
            Self::SuiteSummaryWithoutOpen { unit } => {
                write!(f, "{}: a suite summary with no open suite", unit.id())
            }
            Self::SuiteTotalsMismatch {
                unit,
                reported: (passed, failed, ignored),
                observed: (observed_passed, observed_failed, observed_ignored),
            } => write!(
                f,
                "{}: the suite claims {passed} passed, {failed} failed, {ignored} ignored; its records show {observed_passed}, {observed_failed}, {observed_ignored}",
                unit.id()
            ),
            Self::MissingSuiteClosure { unit } => {
                write!(f, "{}: the stream ended with its suite still open", unit.id())
            }
            Self::UncleanExit { unit, disposition } => {
                write!(f, "{}: the harness process {disposition} though every test it reported passed", unit.id())
            }
        }
    }
}

impl Display for ExitDisposition {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signal(signal) => write!(f, "was killed by signal {signal}"),
            Self::Code(code) => write!(f, "exited with code {code}"),
            Self::Unknown => f.write_str("exited uncleanly"),
        }
    }
}

impl serde::Serialize for AnomalyDetail {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Anomaly {
    pub(crate) kind: AnomalyKind,
    pub(crate) lane: Option<LaneIdx>,
    pub(crate) detail: AnomalyDetail,
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
    /// The obliged resolutions that presented no stream (`the lane's
    /// selection` for a combined one).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) missing: Vec<String>,
    /// The libraries a derived carrier declined for their manifest's
    /// `doctest = false`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) declined: Vec<String>,
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
    /// The shared observation reduction for each lane. The phase verdict and
    /// the accounting gate read the same stream-derived outcome.
    pub(crate) lane_verdicts: BTreeMap<usize, LaneVerdict>,
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
            && self.lane_verdicts.values().all(|verdict| *verdict == LaneVerdict::Pass)
            && self.accounted.iter().all(|a| a.outcome == Outcome::Passed)
            && self.doctests.carriers.iter().all(|c| c.completed)
    }
}

#[derive(Default)]
struct StreamState {
    stream_id: u64,
    lane: usize,
    origin: Option<StreamOrigin>,
    kills: Vec<Termination>,
    reported_unattributed: bool,
    /// A decline record rather than a rustdoc stream: nothing ran on it.
    declined: bool,
}

fn reduced<'a>(
    stream: &StreamState,
    lanes: &'a BTreeMap<usize, ExecutionOutcome>,
) -> Option<&'a StreamOutcome> {
    lanes.get(&stream.lane)?.stream(stream.stream_id)
}

fn source_for(origin: &StreamOrigin) -> StreamSource {
    match origin {
        StreamOrigin::Binary { unit, .. } => StreamSource::Harness {
            executable: unit.id(),
        },
        StreamOrigin::Engine { .. } => StreamSource::Engine,
        StreamOrigin::Doctest { .. } => StreamSource::Rustdoc,
        // Mostly cargo's shared stream, and permissive like it; an unknown
        // harness or engine binary replayed so loses no verdict, since any test
        // record on an unattributed stream is already an anomaly.
        StreamOrigin::Unattributed { .. } => StreamSource::Cargo,
    }
}

#[derive(Default)]
struct ExecState {
    started_on: Option<u64>,
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExecKey {
    lane: LaneIdx,
    resolution: Resolution,
    unit: BinaryUnit,
    test: String,
}

impl ExecKey {
    fn new(lane: usize, resolution: Option<&str>, unit: &BinaryUnit, test: &str) -> Self {
        Self {
            lane: LaneIdx::new(lane),
            resolution: Resolution::from_option(resolution),
            unit: unit.clone(),
            test: test.to_owned(),
        }
    }

    fn resolution_option(&self) -> Option<String> {
        self.resolution.as_option().map(str::to_owned)
    }
}

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
    for lane in plan.lanes.iter().filter(|l| !superseded.contains_key(&l.lane.get())) {
        let Inventory::Prepared { executions, ignored_selected: ignored, outside_claim: outside, .. } =
            &lane.inventory
        else {
            continue;
        };
        for pair in executions {
            expected.insert(
                ExecKey::new(
                    lane.lane.get(),
                    pair.resolution.as_option(),
                    &pair.unit,
                    pair.test.as_str(),
                ),
                pair.clone(),
            );
        }
        for pair in ignored {
            ignored_selected.insert(ExecKey::new(
                lane.lane.get(),
                pair.resolution.as_option(),
                &pair.unit,
                pair.test.as_str(),
            ));
        }
        for pair in outside {
            outside_claim.insert(ExecKey::new(
                lane.lane.get(),
                pair.resolution.as_option(),
                &pair.unit,
                pair.test.as_str(),
            ));
        }
    }

    let mut anomalies: Vec<Anomaly> = Vec::new();
    let mut anomaly = |kind: AnomalyKind, lane: Option<usize>, detail: AnomalyDetail| {
        anomalies.push(Anomaly { kind, lane: lane.map(LaneIdx::new), detail });
    };
    let mut streams: BTreeMap<u64, StreamState> = BTreeMap::new();
    let mut lane_outcomes: BTreeMap<usize, ExecutionOutcome> = BTreeMap::new();
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
    // Per lane, the libraries a derived carrier declined for their manifest's
    // `doctest = false`.
    let mut declined: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let duplicate =
        |execs: &mut BTreeMap<ExecKey, ExecState>, key: &ExecKey, stream: u64| -> Option<AnomalyDetail> {
        let e = execs.entry(key.clone()).or_default();
        match e.claim(stream) {
            Ok(()) => None,
            Err(true) => Some(AnomalyDetail::DuplicateExecution {
                unit: key.unit.clone(),
                test: key.test.clone(),
                stream,
            }),
            Err(false) => None,
        }
    };

    for rec in records {
        match rec {
            JournalRecord::Observed { lane, .. } if superseded.contains_key(lane) => {}
            JournalRecord::Observed { lane, stream, origin, event } => {
                lane_outcomes
                    .entry(*lane)
                    .or_default()
                    .observe_event(*stream, source_for(origin), event);
                let st = streams.entry(*stream).or_default();
                st.stream_id = *stream;
                st.lane = *lane;
                if st.origin.is_none() {
                    st.origin = Some(origin.clone());
                }
                let (resolution, unit) = match origin {
                    StreamOrigin::Binary { resolution, unit, .. } => (resolution.clone(), unit.clone()),
                    StreamOrigin::Engine { resolution, unit } => (resolution.clone(), unit.clone()),
                    StreamOrigin::Doctest { .. } => {
                        match event {
                            ObsEvent::Finished { result, .. } => match result {
                                TestResult::Ok => doctests.passed += 1,
                                TestResult::Ignored => doctests.ignored += 1,
                                _ => doctests.failed += 1,
                            },
                            // Policy evidence, not a stream: it presents nothing
                            // a carrier owes, and is never held to a suite.
                            ObsEvent::DoctestDeclined { package } => {
                                st.declined = true;
                                declined.entry(*lane).or_default().push(package.clone());
                            }
                            ObsEvent::AttributionRefused { refusal } => {
                                anomaly(
                                    AnomalyKind::AttributionError,
                                    Some(*lane),
                                    AnomalyDetail::ExternalMessage(refusal.detail()),
                                );
                            }
                            _ => {}
                        }
                        continue;
                    }
                    StreamOrigin::Unattributed { detail } => {
                        match event {
                            ObsEvent::AttributionRefused { refusal } => {
                                anomaly(
                                    AnomalyKind::AttributionError,
                                    Some(*lane),
                                    AnomalyDetail::ExternalMessage(refusal.detail()),
                                );
                            }
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
                                    AnomalyDetail::UnattributedEvents { reason: detail.clone() },
                                );
                            }
                            _ => {}
                        }
                        continue;
                    }
                };
                match event {
                    ObsEvent::Started { name } => {
                        let key = ExecKey::new(*lane, resolution.as_deref(), &unit, name);
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
                                AnomalyDetail::UnplannedStart { unit: unit.clone(), test: name.clone() },
                            );
                        }
                    }
                    ObsEvent::Finished { name, result } => {
                        let key = ExecKey::new(*lane, resolution.as_deref(), &unit, name);
                        if expected.contains_key(&key) {
                            if let Some(d) = duplicate(&mut execs, &key, *stream) {
                                anomaly(AnomalyKind::DuplicateExecution, Some(*lane), d);
                            }
                        } else if !(*result == TestResult::Ignored && ignored_selected.contains(&key))
                            && !outside_claim.contains(&key)
                        {
                            anomaly(
                                AnomalyKind::UnplannedTest,
                                Some(*lane),
                                AnomalyDetail::UnplannedTerminal { unit: unit.clone(), test: name.clone() },
                            );
                        }
                    }
                    ObsEvent::SlowWarning { name } => anomaly(
                        AnomalyKind::SlowWarning,
                        Some(*lane),
                        AnomalyDetail::SlowWarning { unit: unit.clone(), test: name.clone() },
                    ),
                    ObsEvent::SuiteStarted { .. }
                    | ObsEvent::SuiteFinished { .. }
                    | ObsEvent::StreamEnded { .. }
                    | ObsEvent::Exited { .. }
                    | ObsEvent::Killed { .. }
                    | ObsEvent::RunFailed { .. }
                    | ObsEvent::DoctestDeclined { .. } => {}
                    ObsEvent::AttributionRefused { refusal } => {
                        anomaly(
                            AnomalyKind::AttributionError,
                            Some(*lane),
                            AnomalyDetail::ExternalMessage(refusal.detail()),
                        );
                    }
                }
            }
            JournalRecord::ProcessExited { lane, stream, code, signal } => {
                let st = streams.entry(*stream).or_default();
                st.stream_id = *stream;
                st.lane = *lane;
                let source = st
                    .origin
                    .as_ref()
                    .map_or(StreamSource::Process, source_for);
                lane_outcomes
                    .entry(*lane)
                    .or_default()
                    .observe_event(
                        *stream,
                        source,
                        &ObsEvent::Exited { code: *code, signal: *signal },
                    );
            }
            JournalRecord::SpawnFailed { lane, origin, .. } => spawn_failures.push((*lane, origin.clone())),
            JournalRecord::Terminated(t) => {
                if first_termination.is_none() {
                    first_termination = Some(t.clone());
                }
                if let Some(lane) = t.lane {
                    let event = match t.cause {
                        TerminationCause::PerTestDeadline => ObsEvent::Killed {
                            cause: KillCause::PerTest,
                            test: t.test.clone(),
                        },
                        TerminationCause::RunDeadline => ObsEvent::Killed {
                            cause: KillCause::Wall,
                            test: t.test.clone(),
                        },
                        TerminationCause::PhaseDeadline | TerminationCause::Interrupt => {
                            ObsEvent::Killed { cause: KillCause::Stopped, test: None }
                        }
                        TerminationCause::SiblingTimeout | TerminationCause::FailFast => {
                            ObsEvent::Killed { cause: KillCause::Cancelled, test: None }
                        }
                        TerminationCause::EngineError => ObsEvent::RunFailed {
                            reason: ExecutionFailure::EngineError,
                        },
                    };
                    lane_outcomes
                        .entry(lane)
                        .or_default()
                        .observe_event(t.stream.unwrap_or_default(), StreamSource::Process, &event);
                }
                match (t.scope, t.stream, t.lane) {
                    (TerminationScope::Stream, Some(s), lane) => {
                        let st = streams.entry(s).or_default();
                        st.stream_id = s;
                        if let Some(lane) = lane {
                            st.lane = lane;
                        }
                        st.kills.push(t.clone());
                    }
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

    // Protocol consistency is reduced once from observations, then translated
    // into plan-aware accounting anomalies for the report.
    for st in streams.values() {
        let Some(unit) = (match &st.origin {
            Some(StreamOrigin::Binary { unit, .. } | StreamOrigin::Engine { unit, .. }) => Some(unit),
            _ => None,
        }) else {
            continue;
        };
        let Some(outcome) = reduced(st, &lane_outcomes) else {
            continue;
        };
        for issue in outcome.issues() {
            let (kind, detail) = match issue {
                ProtocolIssue::OverlappingSuite => (
                    AnomalyKind::OverlappingSuite,
                    AnomalyDetail::OverlappingSuite { unit: (*unit).clone() },
                ),
                ProtocolIssue::SuiteSummaryWithoutOpen => (
                    AnomalyKind::SuiteTotalsMismatch,
                    AnomalyDetail::SuiteSummaryWithoutOpen { unit: (*unit).clone() },
                ),
                ProtocolIssue::SuiteTotalsMismatch { reported, observed } => (
                    AnomalyKind::SuiteTotalsMismatch,
                    AnomalyDetail::SuiteTotalsMismatch {
                        unit: (*unit).clone(),
                        reported: *reported,
                        observed: *observed,
                    },
                ),
                ProtocolIssue::DuplicateStart { name } => (
                    AnomalyKind::DuplicateStart,
                    AnomalyDetail::DuplicateStart { unit: (*unit).clone(), test: name.clone() },
                ),
                ProtocolIssue::StartAfterTerminal { name } => (
                    AnomalyKind::StartAfterTerminal,
                    AnomalyDetail::StartAfterTerminal { unit: (*unit).clone(), test: name.clone() },
                ),
                ProtocolIssue::RepeatedTerminal { name } => (
                    AnomalyKind::RepeatedTerminal,
                    AnomalyDetail::RepeatedTerminal { unit: (*unit).clone(), test: name.clone() },
                ),
                ProtocolIssue::TerminalWithoutStart { name } => (
                    AnomalyKind::TerminalWithoutStart,
                    AnomalyDetail::TerminalWithoutStart { unit: (*unit).clone(), test: name.clone() },
                ),
                ProtocolIssue::NameAcrossSlots { name } => (
                    AnomalyKind::NameAcrossSlots,
                    AnomalyDetail::NameAcrossSlots { unit: (*unit).clone(), test: name.clone() },
                ),
            };
            anomaly(kind, Some(st.lane), detail);
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
        let observed = reduced(st, &lane_outcomes);
        if observed.is_some_and(|o| o.suite_open() && o.end() == Some(StreamEnd::Eof)) && !explained {
            anomaly(
                AnomalyKind::MissingSuiteClosure,
                Some(st.lane),
                AnomalyDetail::MissingSuiteClosure { unit: unit.clone() },
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
        let Some((code, signal)) = reduced(st, &lane_outcomes).and_then(StreamOutcome::exit) else {
            continue;
        };
        let Some(observed) = reduced(st, &lane_outcomes) else {
            continue;
        };
        let explained = !st.kills.is_empty() || lane_terms.contains_key(&st.lane) || !run_terms.is_empty();
        let unclean = signal.is_some() || code.is_some_and(|c| c != 0);
        let has_bad_terminal = observed.tests().any(|test| {
            test.terminal.is_some_and(|result| !matches!(result, TestResult::Ok | TestResult::Ignored))
        });
        if unclean
            && !explained
            && !has_bad_terminal
            && observed.tests().next().is_some()
            && observed.tests().all(|test| test.terminal.is_some())
        {
            let disposition = match (code, signal) {
                (_, Some(signal)) => ExitDisposition::Signal(signal),
                (Some(code), None) => ExitDisposition::Code(code),
                (None, None) => ExitDisposition::Unknown,
            };
            anomaly(
                AnomalyKind::UncleanExit,
                Some(st.lane),
                AnomalyDetail::UncleanExit { unit: unit.clone(), disposition },
            );
        }
    }

    // Streams whose record may be short. Doctest and attributed binary
    // streams must say how they ended, and must have ended by their writer
    // closing them; any stream that says it was cut short or failed to read
    // is short too, attributed or not.
    let mut truncated_streams: Vec<String> = Vec::new();
    for (id, st) in streams.iter().filter(|(_, st)| !st.declined) {
        let name = match &st.origin {
            Some(StreamOrigin::Binary { unit, .. }) => Some(unit.id()),
            Some(StreamOrigin::Doctest { .. }) => Some("rustdoc".to_owned()),
            Some(StreamOrigin::Engine { .. } | StreamOrigin::Unattributed { .. }) | None => None,
        };
        let label = plan
            .lane(LaneIdx::new(st.lane))
            .map_or_else(|| format!("lane {}", st.lane), |l| l.label.clone());
        let end = reduced(st, &lane_outcomes).and_then(StreamOutcome::end);
        match (end, &name) {
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
        let doc_streams: Vec<(&StreamState, Resolution)> = streams
            .values()
            .filter(|s| s.lane == lane.lane.get() && !s.declined)
            .filter_map(|s| match &s.origin {
                Some(StreamOrigin::Doctest { resolution }) => {
                    Some((s, Resolution::from_option(resolution.as_deref())))
                }
                _ => None,
            })
            .collect();
        // `all` over no streams is vacuously true, which is exactly the
        // carrier that never ran: every resolution the plan says holds a
        // doctested library must have presented a stream of its own - one
        // package's stream does not stand in for another's.
        let missing: Vec<String> = lane
            .doc_required
            .iter()
            .filter(|required| !doc_streams.iter().any(|(_, r)| r == *required))
            .map(|r| r.as_option().unwrap_or("the lane's selection").to_owned())
            .collect();
        // Every stream that did present is held to a clean end, required or
        // not: a `--doc` run over a library the plan did not oblige is still
        // a run.
        let completed = lane_finished_clean.get(&lane.lane.get()).copied().unwrap_or(false)
            && missing.is_empty()
            && doc_streams.iter().all(|(s, _)| {
                reduced(s, &lane_outcomes)
                    .is_some_and(|o| !o.suite_open() && o.end() == Some(StreamEnd::Eof))
            });
        doctests.carriers.push(CarrierState {
            lane: lane.label.clone(),
            completed,
            streams: doc_streams.len(),
            missing,
            declined: declined.remove(&lane.lane.get()).unwrap_or_default(),
        });
    }

    let mut accounted = Vec::with_capacity(expected.len());
    for (key, pair) in &expected {
        let lane = key.lane.get();
        let resolution = key.resolution_option();
        let unit = &key.unit;
        let test = key.test.as_str();
        let state = execs.get(key);
        let terminal = state
            .and_then(|state| state.owner)
            .and_then(|stream_id| streams.get(&stream_id))
            .and_then(|stream| reduced(stream, &lane_outcomes))
            .and_then(|stream| stream.test(test))
            .and_then(|test| test.terminal);
        let mine = |s: &StreamState| match &s.origin {
            Some(StreamOrigin::Binary { resolution: r, unit: u, one_test }) => {
                s.lane == lane && r == &resolution && u == unit && one_test.as_ref().is_none_or(|t| t == test)
            }
            Some(StreamOrigin::Engine { resolution: r, unit: u }) => {
                s.lane == lane && r == &resolution && u == unit
            }
            _ => false,
        };
        let my_streams: Vec<&StreamState> = streams.values().filter(|s| mine(s)).collect();
        let lane_term = lane_terms.get(&lane).and_then(|v| v.first());
        let run_term = run_terms.first();
        // Every termination read from this execution's point of view: a
        // per-test deadline charged to it is its own, one charged elsewhere
        // is a sibling's timeout.
        let seen_as = |t: &Termination| t.detail_for(&resolution, unit, test);
        let wider = || lane_term.or(run_term).map(seen_as);
        let stream_kill = |s: &StreamState| s.kills.first().map(seen_as);
        let (outcome, detail) = if let Some(result) = terminal {
            match result {
                TestResult::Ok => (Outcome::Passed, None),
                TestResult::Failed => (Outcome::Failed, Some(Detail::Assertion)),
                TestResult::Ignored => (Outcome::Ignored, None),
                TestResult::TimedOut => (Outcome::TimedOut, Some(Detail::PerTestDeadline)),
                TestResult::Interrupted => (Outcome::Interrupted, Some(wider().unwrap_or(Detail::FailFast))),
            }
        } else if my_streams.iter().any(|s| s.kills.iter().any(|k| k.charges(&resolution, unit, test)))
            || lane_terms.get(&lane).is_some_and(|v| v.iter().any(|t| t.charges(&resolution, unit, test)))
        {
            (Outcome::TimedOut, Some(Detail::PerTestDeadline))
        } else if spawn_failures.iter().any(|(l, o)| {
            *l == lane
                && match o {
                    StreamOrigin::Binary { resolution: r, unit: u, one_test } => {
                        r == &resolution && u == unit && one_test.as_ref().is_none_or(|t| t == test)
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
                .or_else(|| match s.and_then(|s| reduced(s, &lane_outcomes)).and_then(StreamOutcome::end) {
                    Some(StreamEnd::Cancelled) => Some(Detail::StreamTruncated),
                    Some(StreamEnd::ReadError) => Some(Detail::ReadError),
                    _ => None,
                });
            match (detail, s.and_then(|s| reduced(s, &lane_outcomes)).and_then(StreamOutcome::exit)) {
                (Some(d), _) => (Outcome::Interrupted, Some(d)),
                // A crash of a process that ran this one test is that test's
                // failure; in a shared process it is only a suspect's.
                (None, Some((_, Some(_)))) if one_test => (Outcome::Failed, Some(Detail::Signal)),
                (None, Some((_, Some(_)))) => (Outcome::Interrupted, Some(Detail::Signal)),
                (None, _) => (Outcome::Interrupted, Some(Detail::MissingTerminal)),
            }
        } else {
            let one_test_crash = my_streams.iter().find_map(|s| match (
                &s.origin,
                reduced(s, &lane_outcomes).and_then(StreamOutcome::exit),
            ) {
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
                            my_streams.iter().find_map(|s| match reduced(s, &lane_outcomes)
                                .and_then(StreamOutcome::end)
                            {
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
            id: ExecutionId { lane: key.lane, attempt: 1, pair: pair.clone() },
            outcome,
            detail,
        });
    }

    let lane_verdicts = lane_outcomes
        .iter()
        .map(|(lane, outcome)| (*lane, lane_verdict(outcome)))
        .collect();

    Reconciliation {
        accounted,
        anomalies,
        doctests,
        plan_complete: plan.complete,
        journal_closed,
        journal_errors,
        first_termination,
        lane_verdicts,
        superseded,
        truncated_streams,
    }
}

