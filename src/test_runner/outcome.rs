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
    suite_counts: (u64, u64, u64),
    tests: BTreeMap<String, TestObservation>,
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
            suite_counts: (0, 0, 0),
            tests: BTreeMap::new(),
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
        let outcome = self.streams.entry(stream).or_insert_with(|| StreamOutcome {
            source,
            ..StreamOutcome::default()
        });
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

    pub(crate) fn test_result(&self, name: &str) -> Option<TestResult> {
        self.streams
            .values()
            .find_map(|stream| stream.tests.get(name).and_then(|test| test.terminal))
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
            .map(|(name, _)| name.clone())
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
                }
                self.suite_open = true;
                self.suites_started += 1;
                self.suite_counts = (0, 0, 0);
            }
            ObsEvent::Started { name } => {
                let state = self.tests.entry(name.clone()).or_default();
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
                let state = self.tests.entry(name.clone()).or_default();
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
                self.suites_finished += 1;
            }
            ObsEvent::SlowWarning { .. } => {}
        }
    }

    pub(crate) fn suite_open(&self) -> bool {
        self.suite_open
    }

    pub(crate) fn tests(&self) -> &BTreeMap<String, TestObservation> {
        &self.tests
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
