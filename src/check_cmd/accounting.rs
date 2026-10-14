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

// Root-level imports used by other files included into check_cmd.
use crate::test_runner::{ObsEvent, TestResult};

mod accounting {
    // Keep accounting concerns in their own modules and re-export the public
    // surface for the other check_cmd phases.
    mod plan;
    mod journal;
    mod reconcile;
    mod report_json;

    pub(crate) use journal::{
        AccountingPaths, ChargedTo, JournalRecord, LaneTap, StreamOrigin, Termination,
        TerminationCause, TerminationScope, accounting_open,
        journal_append, journal_close, journal_unlocked_write_failed,
        journal_watchdog_fired, new_run_id, read_journal, record_run_stop, stop_cause,
    };
    #[cfg(test)]
    pub(crate) use journal::{JOURNAL_TEST_LOCK, accounting_base, is_run_id, with_journal_locked};
    pub(crate) use plan::*;
    pub(crate) use reconcile::*;
    pub(crate) use report_json::*;

#[cfg(test)]
mod accounting_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::profile::{PlannedSweep, SweepDecl};
    use crate::test_runner::{hash_file, ObsEvent, StreamEnd, TestResult};
    use super::super::{test_binary_for_tests, Resolution};

    fn unit(package: &str, kind: &str, target: &str) -> BinaryUnit {
        BinaryUnit {
            package_id: format!("path+file:///x/{package}#{package}@0.1.0"),
            package: package.into(),
            kind: kind.into(),
            target: target.into(),
        }
    }

    fn pair(u: &BinaryUnit, test: &str) -> PairId {
        PairId {
            shape: "s".into(),
            resolution: Resolution::Combined,
            unit: u.clone(),
            test: test.into(),
        }
    }

    fn lane(idx: usize, executions: Vec<PairId>) -> LaneRecord {
        let mut lane = LaneRecord::empty(
            LaneIdx::new(idx),
            format!("lane{idx}"),
            LaneKind::Parallel,
            "s".into(),
        );
        lane.inventory = Inventory::Prepared {
            executions,
            ignored_selected: Vec::new(),
            outside_claim: Vec::new(),
            artifacts: Vec::new(),
            unlisted: Vec::new(),
        };
        lane
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
            .find(|a| a.id.lane.get() == lane && a.id.pair.test.as_str() == test)
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
        if let Inventory::Prepared { ignored_selected, .. } = &mut l.inventory {
            *ignored_selected = vec![pair(&u, "slow_manual")];
        }
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
        if let Inventory::Prepared { outside_claim, .. } = &mut l.inventory {
            *outside_claim = vec![pair(&u, "bench_parse")];
        }
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
        journal::write_whole(file.as_raw_fd(), &body).unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), body.len());
        assert!(journal::write_whole(-1, b"lost\n").is_err());
    }

    /// Validation case: cross-shape artifact replacement. A shape whose
    /// difference cargo does not hash into the file name rebuilds the same
    /// path in place; the content hash is what sees it.
    #[test]
    fn a_planned_artifact_rebuilt_in_place_is_drift() {
        let u = unit("core", "lib", "core");
        let planned = vec![PlannedArtifact::new(None, u, "/t/debug/deps/core-1".into(), "aaaa".into())];
        let same = vec![ArtifactFingerprint::new(None, "/t/debug/deps/core-1".into(), "aaaa".into())];
        assert!(artifact_drift(&planned, &same).is_empty());
        let replaced = vec![ArtifactFingerprint::new(None, "/t/debug/deps/core-1".into(), "bbbb".into())];
        let drift = artifact_drift(&planned, &replaced);
        assert_eq!(drift.len(), 1);
        assert!(drift[0].contains("different content"), "{drift:?}");
        let moved = vec![ArtifactFingerprint::new(None, "/t/debug/deps/core-2".into(), "aaaa".into())];
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
        l.inventory = Inventory::DocTests;
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
        // The suite's tally is checked against the tests the stream showed, so
        // a doctest that passed is a started and finished record, not a bare
        // summary.
        records.push(rustdoc(1, 5, started("src/lib.rs - doc (line 1)")));
        records.push(rustdoc(1, 5, finished("src/lib.rs - doc (line 1)", TestResult::Ok)));
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
        let planned = |declaration| PlannedSweep::resolve(declaration, false);
        let serial = planned(SweepDecl { label: "serial".into(), ..SweepDecl::default() });
        assert!(lane_runs_doctests(&serial, true));
        assert!(!lane_runs_doctests(&serial, false));
        let narrowed = planned(SweepDecl {
            label: "narrowed".into(),
            filters: crate::profile::LaneFilters {
                targets: vec![crate::profile::TargetName::new("cli")],
                ..Default::default()
            },
            ..SweepDecl::default()
        });
        assert!(!lane_runs_doctests(&narrowed, true), "a target selector turns doctests off");
        let doc = planned(SweepDecl {
            label: "doc".into(),
            lane_exec: crate::profile::LaneExec::doc_only(),
            ..SweepDecl::default()
        });
        assert!(lane_runs_doctests(&doc, false));
        let parallel = planned(SweepDecl {
            label: "p".into(),
            lane_exec: crate::profile::LaneExec::Parallel { budget: 4 },
            ..SweepDecl::default()
        });
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
        if let Inventory::Prepared { ignored_selected, unlisted, .. } = &mut l.inventory {
            *ignored_selected = vec![pair(&b, "slow")];
            *unlisted = vec![("core::custom".into(), "no listing".into())];
        }
        let p = AccountingPlan { certifying: true, ..plan(vec![l]) };
        let json = serde_json::to_string(&p).unwrap();
        // The unit's package id is written once per binary, not once per test.
        assert_eq!(json.matches("path+file:///x/core#core@0.1.0").count(), 2 + 1, "{json}");
        let back: AccountingPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lanes[0].inventory.executions(), p.lanes[0].inventory.executions());
        assert_eq!(back.lanes[0].inventory.ignored_selected(), p.lanes[0].inventory.ignored_selected());
        assert_eq!(back.lanes[0].inventory, p.lanes[0].inventory);
        assert!(back.certifying);
    }

    /// Retention: only the newest [`journal::ACCOUNTING_RUNS_KEPT`] runs survive, and the
    /// run being opened survives whatever its age.
    #[test]
    fn retention_keeps_only_the_newest_runs() {
        let name = |i: u32| format!("{}-{i}", 1_700_000_000_000_u64 + u64::from(i) * 60_000);
        let keep = 6;
        let few: Vec<String> = (0..keep).map(|i| name(u32::try_from(i).unwrap())).collect();
        assert!(journal::runs_to_prune(&few, "none", keep).is_empty());
        let many: Vec<String> = (0..25_u32).map(name).collect();
        let pruned = journal::runs_to_prune(&many, "none", keep);
        assert_eq!(pruned.len(), 25 - keep, "{pruned:?}");
        // Newest first: what goes is the oldest.
        assert!(pruned.iter().all(|p| many.iter().position(|m| m == p).unwrap() < 25 - keep));
        // The run being opened is never removed.
        let opening = many[0].clone();
        assert!(!journal::runs_to_prune(&many, &opening, keep).contains(&opening));
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
        assert_eq!(kept, journal::ACCOUNTING_RUNS_KEPT, "the opening run counts among the newest");
        assert!(paths.plan.exists() && paths.journal.exists());
        let (records, closed, errors) = read_journal(&paths.journal);
        assert!(closed && errors.is_empty(), "{errors:?}");
        assert!(records.contains(&JournalRecord::LaneStarted { lane: 31_001 }));
    }
}

}

pub(crate) use accounting::*;
