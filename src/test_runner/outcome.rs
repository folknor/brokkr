//! One reduction of the typed observation stream, used by lane verdicts and
//! by the durable accounting reconciliation.

use std::collections::BTreeMap;

use super::{
    AttributionRefusal, ExecutionFailure, KillCause, ObsEvent, Observation, StreamEnd,
    StreamSource, TestResult,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaneVerdict {
    Pass,
    Fail,
    Incomplete,
    BudgetBlown,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolIssue {
    OverlappingSuite,
    SuiteSummaryWithoutOpen,
    SuiteTotalsMismatch {
        reported: (u64, u64, u64),
        observed: (u64, u64, u64),
    },
    DuplicateStart {
        name: String,
    },
    StartAfterTerminal {
        name: String,
    },
    RepeatedTerminal {
        name: String,
    },
    TerminalWithoutStart {
        name: String,
    },
    /// A name already reported in an earlier slot reported again in another,
    /// on a stream that carries one binary. Its harness ran a test twice; only
    /// cargo's own stream may legitimately reuse a name, one binary's suite
    /// after another's.
    NameAcrossSlots {
        name: String,
    },
}

impl std::fmt::Display for ProtocolIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverlappingSuite => f.write_str("a suite started inside an open suite"),
            Self::SuiteSummaryWithoutOpen => {
                f.write_str("a suite summary arrived with no open suite")
            }
            Self::SuiteTotalsMismatch {
                reported: (passed, failed, ignored),
                observed: (observed_passed, observed_failed, observed_ignored),
            } => write!(
                f,
                "a suite claimed {passed} passed, {failed} failed, {ignored} ignored; its records show {observed_passed}, {observed_failed}, {observed_ignored}"
            ),
            Self::DuplicateStart { name } => write!(f, "{name} started twice"),
            Self::StartAfterTerminal { name } => write!(f, "{name} started again after its result"),
            Self::RepeatedTerminal { name } => write!(f, "{name} reported a second result"),
            Self::TerminalWithoutStart { name } => {
                write!(f, "{name} reported a result it was never seen starting")
            }
            Self::NameAcrossSlots { name } => {
                write!(
                    f,
                    "{name} was reported again in a later suite of the same binary"
                )
            }
        }
    }
}

/// Where on a stream a record sits: inside the suite libtest opened (by
/// ordinal), or outside any suite. A test's identity on a stream is its slot
/// and its name, so cargo's stream - one binary's suite after another's - can
/// carry two binaries' tests of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Outside,
    Suite(u64),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ExecutionOutcome {
    streams: BTreeMap<u64, StreamOutcome>,
}

#[derive(Clone, Debug)]
pub(crate) struct StreamOutcome {
    source: StreamSource,
    suites_started: u64,
    suites_finished: u64,
    suite_open: bool,
    /// The slot records land in now. Opened only by a valid suite start, so an
    /// overlapping start keeps the open suite's namespace.
    slot: Slot,
    suite_counts: (u64, u64, u64),
    tests: BTreeMap<(Slot, String), TestObservation>,
    /// The slots each name appeared in, so recurrence and lookup by name do
    /// not scan the stream.
    slots_by_name: BTreeMap<String, Vec<Slot>>,
    end: Option<StreamEnd>,
    exit: Option<(Option<i32>, Option<i32>)>,
    kills: Vec<KillCause>,
    refusals: Vec<AttributionRefusal>,
    run_failures: Vec<ExecutionFailure>,
    issues: Vec<ProtocolIssue>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TestObservation {
    pub(crate) started: bool,
    pub(crate) terminal: Option<TestResult>,
}

impl Default for StreamOutcome {
    fn default() -> Self {
        Self {
            source: StreamSource::Process,
            suites_started: 0,
            suites_finished: 0,
            suite_open: false,
            slot: Slot::Outside,
            suite_counts: (0, 0, 0),
            tests: BTreeMap::new(),
            slots_by_name: BTreeMap::new(),
            end: None,
            exit: None,
            kills: Vec::new(),
            refusals: Vec::new(),
            run_failures: Vec::new(),
            issues: Vec::new(),
        }
    }
}

impl ExecutionOutcome {
    pub(crate) fn observe(&mut self, observation: &Observation) {
        self.observe_event(
            observation.stream,
            observation.source.clone(),
            &observation.event,
        );
    }

    pub(crate) fn observe_event(&mut self, stream: u64, source: StreamSource, event: &ObsEvent) {
        let test_event = matches!(
            event,
            ObsEvent::SuiteStarted { .. }
                | ObsEvent::SuiteFinished { .. }
                | ObsEvent::Started { .. }
                | ObsEvent::Finished { .. }
        );
        let outcome = self.streams.entry(stream).or_insert_with(|| StreamOutcome {
            source: source.clone(),
            ..StreamOutcome::default()
        });
        // A kill or an exit can reach a stream first, under a placeholder
        // source; the stream's own records name what it really is, and that
        // decides whether a name may recur across its suites.
        if test_event && !outcome.has_test_activity() {
            outcome.source = source;
        }
        outcome.observe(event);
    }

    pub(crate) fn stream(&self, id: u64) -> Option<&StreamOutcome> {
        self.streams.get(&id)
    }

    pub(crate) fn counts(&self) -> (u64, u64, u64) {
        self.streams.values().fold((0, 0, 0), |mut counts, stream| {
            for test in stream.tests.values() {
                match test.terminal {
                    Some(TestResult::Ok) => counts.0 += 1,
                    Some(TestResult::Ignored) => counts.2 += 1,
                    Some(TestResult::Failed | TestResult::TimedOut | TestResult::Interrupted) => {
                        counts.1 += 1;
                    }
                    None => {}
                }
            }
            counts
        })
    }

    pub(crate) fn terminal_count(&self) -> usize {
        self.streams
            .values()
            .flat_map(|stream| stream.tests.values())
            .filter(|test| test.terminal.is_some())
            .count()
    }

    /// The result of the one test by this name. `None` when no stream or
    /// several report it - a second occurrence, finished or not, makes the
    /// name ambiguous, and an ambiguous name proves nothing passed.
    pub(crate) fn test_result(&self, name: &str) -> Option<TestResult> {
        let mut holders = self
            .streams
            .values()
            .filter(|stream| stream.slots_by_name.contains_key(name));
        let only = holders.next()?;
        if holders.next().is_some() {
            return None;
        }
        only.test(name)?.terminal
    }

    /// Every reason the run failed that no red test states:
    /// [`Self::record_reasons`] and then [`Self::exit_reasons`].
    pub(crate) fn structural_reasons(&self) -> Vec<String> {
        let mut reasons = self.record_reasons();
        reasons.extend(self.exit_reasons());
        reasons
    }

    /// What is wrong with the record itself: protocol issues, attribution
    /// refusals, engine failures.
    pub(crate) fn record_reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        for (id, stream) in &self.streams {
            let label = stream.label(*id);
            for issue in &stream.issues {
                reasons.push(format!("{label}: {issue}"));
            }
            for refusal in &stream.refusals {
                reasons.push(format!("{label}: {}", refusal.detail()));
            }
            for failure in &stream.run_failures {
                match failure {
                    ExecutionFailure::EngineError => {
                        reasons.push(format!("{label}: the test engine failed"));
                    }
                }
            }
        }
        reasons
    }

    /// Each unsuccessful exit or signal no red test of its own (or, for
    /// cargo's, anywhere) and no kill explains, in a
    /// run that saw tests: cargo killed after green suites (its own stream
    /// carries none when harnesses are isolated), an `exit(1)` after the last
    /// test, a death mid-test - naming the tests it left unfinished.
    pub(crate) fn exit_reasons(&self) -> Vec<String> {
        // A run with no test activity anywhere (a build that failed) has its
        // own report. A red test explains its own harness's exit, and cargo's
        // - cargo exits nonzero for a harness that failed one - but never
        // another harness's.
        if !self.has_test_activity() {
            return Vec::new();
        }
        let red = |stream: &StreamOutcome| {
            stream.tests.values().any(|test| {
                matches!(
                    test.terminal,
                    Some(TestResult::Failed | TestResult::TimedOut)
                )
            })
        };
        let red_anywhere = self.streams.values().any(red);
        let mut reasons = Vec::new();
        let mut cargo = Vec::new();
        for (id, stream) in &self.streams {
            let is_cargo = matches!(stream.source, StreamSource::Cargo);
            if !stream.kills.is_empty() || red(stream) || (is_cargo && red_anywhere) {
                continue;
            }
            let into = if is_cargo { &mut cargo } else { &mut reasons };
            let how = match stream.exit {
                Some((_, Some(signal))) => format!("was killed by signal {signal}"),
                Some((Some(code), None)) if code != 0 => format!("exited with code {code}"),
                _ => continue,
            };
            // A death mid-test names what was running: the suspect.
            let unfinished: Vec<&str> = stream
                .tests
                .iter()
                .filter(|(_, test)| test.started && test.terminal.is_none())
                .map(|((_, name), _)| name.as_str())
                .collect();
            let label = stream.label(*id);
            into.push(if unfinished.is_empty() {
                format!("{label}: the process {how} though no test failed")
            } else {
                format!(
                    "{label}: the process {how} while running {}",
                    unfinished.join(", ")
                )
            });
        }
        // A harness that died is why cargo exited unsuccessfully; cargo's own
        // exit is a reason only when no harness gives one.
        if reasons.is_empty() {
            reasons = cargo;
        }
        reasons
    }

    pub(crate) fn has_test_activity(&self) -> bool {
        self.streams.values().any(StreamOutcome::has_test_activity)
    }

    pub(crate) fn failed_tests(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .streams
            .values()
            .flat_map(|stream| stream.tests.iter())
            .filter(|(_, test)| test.terminal == Some(TestResult::Failed))
            .map(|((_, name), _)| name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub(crate) fn incomplete_reason(&self) -> String {
        if self.streams.is_empty() || !self.has_test_activity() {
            return "no libtest suite or terminal test record was observed".into();
        }
        for stream in self
            .streams
            .values()
            .filter(|stream| stream.has_test_activity())
        {
            match stream.end {
                Some(StreamEnd::Cancelled) => {
                    return "brokkr stopped reading a test stream before it reached EOF".into();
                }
                Some(StreamEnd::ReadError) => {
                    return "reading a test observation stream failed".into();
                }
                None => return "a test observation stream never recorded how it ended".into(),
                Some(StreamEnd::Eof) => {}
            }
            if stream.suite_open {
                return "a test suite started but did not report its summary".into();
            }
            if stream.requires_suite()
                && (stream.suites_started == 0 || stream.suites_started != stream.suites_finished)
            {
                return "the observed test suites did not all report their summaries".into();
            }
            if stream
                .tests
                .values()
                .any(|test| test.started && test.terminal.is_none())
            {
                return "a started test did not report a terminal result".into();
            }
        }
        "the observation stream did not contain a complete test run".into()
    }
}

impl StreamOutcome {
    fn observe(&mut self, event: &ObsEvent) {
        match event {
            ObsEvent::SuiteStarted { .. } => {
                if self.suite_open {
                    self.issues.push(ProtocolIssue::OverlappingSuite);
                } else {
                    self.slot = Slot::Suite(self.suites_started);
                }
                self.suite_open = true;
                self.suites_started += 1;
                self.suite_counts = (0, 0, 0);
            }
            ObsEvent::Started { name } => {
                self.note_slot(name);
                let state = self.tests.entry((self.slot, name.clone())).or_default();
                if state.terminal.is_some() {
                    self.issues
                        .push(ProtocolIssue::StartAfterTerminal { name: name.clone() });
                } else if state.started {
                    self.issues
                        .push(ProtocolIssue::DuplicateStart { name: name.clone() });
                }
                state.started = true;
            }
            ObsEvent::Finished { name, result } => {
                self.note_slot(name);
                let state = self.tests.entry((self.slot, name.clone())).or_default();
                if state.terminal.is_some() {
                    self.issues
                        .push(ProtocolIssue::RepeatedTerminal { name: name.clone() });
                } else if !state.started {
                    self.issues
                        .push(ProtocolIssue::TerminalWithoutStart { name: name.clone() });
                }
                if state.terminal.is_none() {
                    state.terminal = Some(*result);
                }
                if self.suite_open {
                    match result {
                        TestResult::Ok => self.suite_counts.0 += 1,
                        TestResult::Ignored => self.suite_counts.2 += 1,
                        _ => self.suite_counts.1 += 1,
                    }
                }
            }
            ObsEvent::StreamEnded { end } => self.end = Some(*end),
            ObsEvent::Exited { code, signal } => self.exit = Some((*code, *signal)),
            ObsEvent::Killed { cause, .. } => self.kills.push(*cause),
            ObsEvent::RunFailed { reason } => self.run_failures.push(*reason),
            ObsEvent::AttributionRefused { refusal } => self.refusals.push(refusal.clone()),
            ObsEvent::SuiteFinished {
                passed,
                failed,
                ignored,
            } => {
                if !self.suite_open {
                    self.issues.push(ProtocolIssue::SuiteSummaryWithoutOpen);
                } else if self.suite_counts != (*passed, *failed, *ignored) {
                    self.issues.push(ProtocolIssue::SuiteTotalsMismatch {
                        reported: (*passed, *failed, *ignored),
                        observed: self.suite_counts,
                    });
                }
                self.suite_open = false;
                self.slot = Slot::Outside;
                self.suites_finished += 1;
            }
            ObsEvent::SlowWarning { .. } => {}
        }
    }

    /// Before `name`'s first record in the current slot: on a stream that
    /// carries one binary, the name already reported in another slot is that
    /// binary's harness running it again. Raised once per slot it recurs in.
    fn note_slot(&mut self, name: &str) {
        let slots = self.slots_by_name.entry(name.to_owned()).or_default();
        if slots.contains(&self.slot) {
            return;
        }
        if !slots.is_empty() && !matches!(self.source, StreamSource::Cargo) {
            self.issues.push(ProtocolIssue::NameAcrossSlots {
                name: name.to_owned(),
            });
        }
        slots.push(self.slot);
    }

    fn label(&self, id: u64) -> String {
        format!("stream {id} ({})", self.source.describe())
    }

    pub(crate) fn suite_open(&self) -> bool {
        self.suite_open
    }

    /// Every test observation on this stream, one per slot a name appeared in.
    pub(crate) fn tests(&self) -> impl Iterator<Item = &TestObservation> {
        self.tests.values()
    }

    /// The one observation of `name` on this stream; `None` when it never
    /// appeared or appeared in more than one slot.
    pub(crate) fn test(&self, name: &str) -> Option<&TestObservation> {
        match self.slots_by_name.get(name)?.as_slice() {
            [slot] => self.tests.get(&(*slot, name.to_owned())),
            _ => None,
        }
    }

    pub(crate) fn end(&self) -> Option<StreamEnd> {
        self.end
    }

    pub(crate) fn exit(&self) -> Option<(Option<i32>, Option<i32>)> {
        self.exit
    }

    pub(crate) fn issues(&self) -> &[ProtocolIssue] {
        &self.issues
    }

    pub(crate) fn has_test_activity(&self) -> bool {
        self.suites_started > 0 || !self.tests.is_empty()
    }

    fn requires_suite(&self) -> bool {
        !matches!(self.source, StreamSource::Engine)
    }

    fn complete(&self) -> bool {
        if !self.has_test_activity() {
            return false;
        }
        if self.end != Some(StreamEnd::Eof) || self.suite_open {
            return false;
        }
        if self.requires_suite()
            && (self.suites_started == 0 || self.suites_started != self.suites_finished)
        {
            return false;
        }
        self.tests
            .values()
            .all(|test| !test.started || test.terminal.is_some())
    }

    fn has_failure(&self) -> bool {
        self.tests.values().any(|test| {
            matches!(
                test.terminal,
                Some(TestResult::Failed | TestResult::TimedOut)
            )
        }) || self
            .exit
            .is_some_and(|(code, signal)| signal.is_some() || code.is_some_and(|code| code != 0))
    }
}

/// The only lane-level verdict, derived from the typed observations. Human
/// output reconstructed from JSON remains available for diagnostics, never as
/// evidence that a run passed or finished.
pub(crate) fn lane_verdict(outcome: &ExecutionOutcome) -> LaneVerdict {
    let streams: Vec<&StreamOutcome> = outcome.streams.values().collect();
    if streams
        .iter()
        .any(|stream| stream.kills.contains(&KillCause::Stopped))
    {
        return LaneVerdict::Stopped;
    }
    if streams.iter().any(|stream| {
        stream.kills.iter().any(|cause| {
            matches!(
                cause,
                KillCause::PerTest | KillCause::NoCompletion | KillCause::Idle | KillCause::Wall
            )
        }) || stream
            .tests
            .values()
            .any(|test| test.terminal == Some(TestResult::TimedOut))
    }) {
        return LaneVerdict::BudgetBlown;
    }
    if streams.iter().any(|stream| {
        !stream.issues.is_empty() || !stream.refusals.is_empty() || !stream.run_failures.is_empty()
    }) {
        return LaneVerdict::Fail;
    }
    if streams.iter().any(|stream| stream.has_failure()) {
        return LaneVerdict::Fail;
    }
    let active: Vec<_> = streams
        .iter()
        .copied()
        .filter(|stream| stream.has_test_activity())
        .collect();
    if active.is_empty() || active.iter().any(|stream| !stream.complete()) {
        return LaneVerdict::Incomplete;
    }
    if streams
        .iter()
        .any(|stream| stream.kills.contains(&KillCause::Cancelled))
    {
        return LaneVerdict::Incomplete;
    }
    if streams.iter().any(|stream| {
        stream
            .tests
            .values()
            .any(|test| test.terminal == Some(TestResult::Interrupted))
    }) {
        return LaneVerdict::Incomplete;
    }
    LaneVerdict::Pass
}
