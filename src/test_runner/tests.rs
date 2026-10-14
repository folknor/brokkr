#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use super::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn test_context() -> RunContext {
    RunContext::new(StopSignal::new(|| false), |_| {})
}

fn hung(reason: TimeoutReason, attribution: Attribution, elapsed: u64, ceiling: u64) -> HungTest {
    HungTest {
        reason,
        attribution,
        elapsed: Duration::from_secs(elapsed),
        ceiling: Duration::from_secs(ceiling),
        snapshot_dir: PathBuf::from("/snap"),
        cargo_pid: 7,
        test_pids: vec![9],
        snapshot_pid: Some(9),
        wchan: None,
        stack: None,
        snapshot_error: None,
    }
}

/// The report states each number once, omits /proc lines that were not
/// read, and carries no constant explanation.
#[test]
fn the_hung_test_report_is_terse() {
    let cwd = Path::new("/");
    let on_time = format_hung_test(
        &hung(
            TimeoutReason::Watched,
            Attribution::Exact {
                name: "t::x".into(),
            },
            20,
            20,
        ),
        cwd,
    );
    assert!(
        on_time.starts_with("test t::x exceeded the 20s per-test timeout\n"),
        "{on_time}"
    );
    assert!(
        !on_time.contains("wchan") && !on_time.contains("stack"),
        "{on_time}"
    );
    assert!(!on_time.contains("build time excluded"), "{on_time}");

    let late = format_hung_test(
        &hung(
            TimeoutReason::Watched,
            Attribution::Exact {
                name: "t::x".into(),
            },
            23,
            20,
        ),
        cwd,
    );
    assert!(
        late.starts_with("test t::x ran 23s, past the 20s per-test timeout\n"),
        "{late}"
    );

    let mut with_wchan = hung(TimeoutReason::Idle, Attribution::None, 300, 300);
    with_wchan.wchan = Some("futex_wait".into());
    let idle = format_hung_test(&with_wchan, cwd);
    assert!(
        idle.starts_with("no test in flight for 300s (the idle ceiling) - "),
        "{idle}"
    );
    assert!(idle.contains("/proc/9/wchan: futex_wait"), "{idle}");
    assert!(!idle.contains("/stack"), "{idle}");

    let wall = format_hung_test(
        &hung(
            TimeoutReason::Wall {
                blamed: vec!["a".into()],
            },
            Attribution::None,
            1800,
            1800,
        ),
        cwd,
    );
    assert!(
        wall.starts_with("the run exceeded its 1800s wall deadline and was killed\n"),
        "{wall}"
    );
    assert!(
        wall.contains("in flight when it expired (unverified): a"),
        "{wall}"
    );
}

/// A child that exits while a background process it started still holds
/// its stdout: the drain is cut after the grace period instead of waiting
/// for that process, and what arrived before the exit is kept.
#[test]
fn a_leaked_pipe_holder_does_not_hang_the_drain_join() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 15 & echo $!; echo captured")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let pipe = child.stdout.take().expect("piped stdout");
    let buf = Arc::new(Mutex::new(Vec::new()));
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let (b, t, c) = (Arc::clone(&buf), Arc::clone(&tracker), Arc::clone(&cancel));
    let drain = thread::spawn(move || drain_libtest_json(pipe, &b, &t, &c, None, |_| {}));
    child.wait().expect("sh exits");
    let started = Instant::now();
    assert!(
        join_drains(vec![drain], &cancel, &test_context()),
        "the leaked holder kept the pipe open"
    );
    assert!(started.elapsed() < DRAIN_GRACE + Duration::from_secs(2));
    let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(text.contains("captured"), "{text}");
    if let Some(pid) = text
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<u32>().ok())
    {
        crate::process::signal_pid(pid, libc::SIGKILL).ok();
    }
}

fn collecting_sink() -> (ObservationSink, Arc<Mutex<Vec<Observation>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_t = Arc::clone(&seen);
    let sink: ObservationSink = Arc::new(move |o| seen_t.lock().unwrap().push(o));
    (sink, seen)
}

fn drained_end(script: &str) -> (Option<StreamEnd>, String) {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let pipe = child.stdout.take().expect("piped stdout");
    let (sink, seen) = collecting_sink();
    let tap = StreamTap::new(&sink, StreamSource::Process);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let (b, t, c) = (Arc::clone(&buf), Arc::clone(&tracker), Arc::clone(&cancel));
    let drain = thread::spawn(move || drain_libtest_json(pipe, &b, &t, &c, Some(tap), |_| {}));
    child.wait().expect("sh exits");
    join_drains(vec![drain], &cancel, &test_context());
    let end = seen.lock().unwrap().iter().find_map(|o| match o.event {
        ObsEvent::StreamEnded { end } => Some(end),
        _ => None,
    });
    let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    (end, text)
}

/// The three ends of a stream are three facts. A drain the grace period
/// cancelled - a leaked process still holding the pipe - says so, because
/// whatever that stream had not delivered yet is lost; a writer that
/// closed says EOF.
#[test]
fn a_cancelled_drain_is_told_apart_from_an_eof() {
    let (end, text) = drained_end("sleep 15 & echo $!; echo captured");
    assert_eq!(end, Some(StreamEnd::Cancelled), "{text}");
    if let Some(pid) = text
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<u32>().ok())
    {
        crate::process::signal_pid(pid, libc::SIGKILL).ok();
    }
    let (end, _) = drained_end("echo done");
    assert_eq!(end, Some(StreamEnd::Eof));
}

/// Every recognised record reaches the sink typed, `test/timeout`
/// included, and nothing the reconstructor renders on its own does.
#[test]
fn recognised_records_reach_the_sink_typed() {
    let (sink, seen) = collecting_sink();
    let tracker = Mutex::new(TestTracker::default());
    let mut recon = JsonReconstructor {
        tap: Some(StreamTap::new(&sink, StreamSource::Process)),
        ..JsonReconstructor::default()
    };
    for ev in [
        r#"{"type":"suite","event":"started","test_count":2}"#,
        r#"{"type":"test","event":"started","name":"a::one"}"#,
        r#"{"type":"test","event":"timeout","name":"a::one"}"#,
        r#"{"type":"test","name":"a::one","event":"ok"}"#,
        r#"{"type":"test","event":"started","name":"a::two"}"#,
        r#"{"type":"test","name":"a::two","event":"failed","stdout":"boom"}"#,
        r#"{"type":"suite","event":"failed","passed":1,"failed":1,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.0}"#,
    ] {
        recon.observe(ev, &tracker);
    }
    let events: Vec<ObsEvent> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|o| o.event.clone())
        .collect();
    assert_eq!(
        events,
        vec![
            ObsEvent::SuiteStarted { test_count: 2 },
            ObsEvent::Started {
                name: "a::one".into()
            },
            ObsEvent::SlowWarning {
                name: "a::one".into()
            },
            ObsEvent::Finished {
                name: "a::one".into(),
                result: TestResult::Ok
            },
            ObsEvent::Started {
                name: "a::two".into()
            },
            ObsEvent::Finished {
                name: "a::two".into(),
                result: TestResult::Failed
            },
            ObsEvent::SuiteFinished {
                passed: 1,
                failed: 1,
                ignored: 0
            },
        ]
    );
}

#[test]
fn a_forged_suite_start_cannot_reset_the_anti_forgery_guard() {
    let mut tracker = TestTracker::default();
    tracker.observe_suite_start();
    tracker.observe_start("a::hung".to_owned());

    // The loop a hung test could otherwise run: re-announce a suite, then
    // re-announce itself, forever. With the previous suite still open, the
    // suite record is not a boundary, so neither the once-per-name guard nor
    // the clock is reset.
    for _ in 0..5 {
        tracker.last_progress = Instant::now()
            .checked_sub(TEST_TIMEOUT + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        tracker.observe_suite_start();
        tracker.observe_start("a::hung".to_owned());
        assert!(
            tracker.timed_out(TEST_TIMEOUT).is_some(),
            "a replayed suite start must not buy the test more time"
        );
    }

    // A real boundary - the suite summarised first - still resets.
    tracker.observe_suite_end();
    tracker.observe_suite_start();
    tracker.observe_start("a::hung".to_owned());
    assert!(
        tracker.timed_out(TEST_TIMEOUT).is_none(),
        "a suite that closed and reopened is a genuine boundary"
    );
}

#[test]
fn the_same_name_in_a_second_binary_still_counts_as_progress() {
    let mut tracker = TestTracker::default();
    tracker.observe_suite_start();
    tracker.observe_start("tests::works".to_owned());
    tracker.observe_result("tests::works", true);

    // Second binary of the same invocation, same test path, clock wound past
    // the cap in between. The first suite summarises first, as a real stream
    // does - that summary is what makes the next start a genuine boundary.
    tracker.observe_suite_end();
    tracker.observe_suite_start();
    tracker.last_progress = Instant::now()
        .checked_sub(TEST_TIMEOUT + Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    tracker.observe_start("tests::works".to_owned());
    assert!(
        tracker.timed_out(TEST_TIMEOUT).is_none(),
        "a fresh suite's occurrence of the same name is real progress"
    );
}

#[test]
fn effective_test_threads_uses_last_value() {
    let args = vec![
        "--test-threads=1".to_owned(),
        "--test-threads".to_owned(),
        "4".to_owned(),
    ];
    assert_eq!(effective_test_threads(&args).unwrap(), Some(4));
}

#[test]
fn sanitize_test_name_for_path_component() {
    assert_eq!(
        sanitize_path_component("src/lib.rs - foo::bar (line 42)"),
        "src_lib.rs_-_foo_bar_line_42"
    );
}

#[test]
fn idle_wedge_fires_only_with_nothing_in_flight() {
    let long_ago = Instant::now()
        .checked_sub(IDLE_TIMEOUT + Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut tracker = TestTracker {
        idle_since: long_ago,
        ..TestTracker::default()
    };
    let Expiry::Idle { elapsed } = tracker.timed_out(TEST_TIMEOUT).expect("idle expiry") else {
        panic!("the idle clock should be identified by type");
    };
    assert!(elapsed >= IDLE_TIMEOUT);

    // A start marker ends the idle window and switches to per-test aging.
    tracker.observe_start("a::fast".to_owned());
    assert!(tracker.timed_out(TEST_TIMEOUT).is_none());
    tracker.observe_result("a::fast", true);
    assert!(
        tracker.timed_out(TEST_TIMEOUT).is_none(),
        "idle window restarts at the result"
    );
}

/// The wall deadline must fire with an entirely empty tracker: no announced
/// start, no announced result, nothing for either event-driven ceiling to
/// age. That is the case the whole struct exists for - a run whose lifecycle
/// events were lost or never emitted must still be bounded, and the bound
/// must come from a clock no input can reach.
#[cfg(target_os = "linux")]
#[test]
fn the_wall_deadline_fires_with_no_events_at_all() {
    use std::os::unix::process::CommandExt;

    let root = test_root("watchdog_wall_deadline");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 60 & wait")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn shell");
    let cargo_pid = child.id();

    // Deliberately pristine: `timed_out` would report the idle wedge only
    // after IDLE_TIMEOUT (five minutes), and per-test aging needs a start
    // that never comes. Neither can explain a kill here.
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let done = Arc::new(AtomicBool::new(false));
    let hung = Arc::new(Mutex::new(None::<HungTest>));

    watchdog_loop_with_timing(
        root.clone(),
        cargo_pid,
        Leader::Cargo,
        Arc::clone(&tracker),
        Arc::clone(&done),
        Arc::clone(&hung),
        Ceilings {
            per_test: Duration::from_secs(3600),
            wall: Some(Duration::from_millis(40)),
            shape: WallShape::SharedHarness,
        },
        Duration::from_millis(5),
        Instant::now(),
        None,
        &test_context(),
    );
    child.wait().ok();

    let hung = hung
        .lock()
        .unwrap()
        .clone()
        .expect("wall deadline must produce a verdict");
    assert_eq!(hung.reason, TimeoutReason::Wall { blamed: Vec::new() });
    assert_eq!(
        hung.attribution,
        Attribution::None,
        "the wall names no offender"
    );
    assert_eq!(hung.ceiling, Duration::from_millis(40));
    assert!(
        wait_for_process_group_exit(cargo_pid),
        "the process group must be killed"
    );
}

/// The hole a lost START record opened. With no start, `current` stays empty,
/// so the per-test clock has nothing to age - and the only remaining bound was
/// the five-minute idle ceiling, letting a test run for minutes under a
/// twenty-second contract. Once a suite announces itself, brokkr must see a
/// completion at least every `per_test`.
#[test]
fn a_lost_start_record_does_not_escape_the_per_test_cap() {
    let long_ago = Instant::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or_else(Instant::now);
    let mut tracker = TestTracker::default();
    // A suite announced itself, then nothing: no start, no result. Exactly
    // what a swallowed start marker leaves behind.
    tracker.observe_suite_start();
    tracker.last_progress = long_ago;

    let Expiry::NoCompletion { elapsed } = tracker
        .timed_out(TEST_TIMEOUT)
        .expect("execution under way with no completion must blow the budget")
    else {
        panic!("the no-completion clock should be identified by type");
    };
    assert!(elapsed >= TEST_TIMEOUT);

    // And before any suite announces itself there is no test to bill, so the
    // per-test cap must NOT fire - that window belongs to the idle ceiling.
    let fresh = TestTracker {
        last_progress: long_ago,
        ..TestTracker::default()
    };
    assert!(
        fresh
            .timed_out(TEST_TIMEOUT)
            .is_none_or(|expiry| matches!(expiry, Expiry::Idle { .. })),
        "before execution begins only the idle ceiling applies"
    );
}

/// A summary alone must not disarm the no-progress clock: a live test can
/// print `suite/ok`. Only a process boundary (an isolated harness's exit)
/// retires a tracker.
#[test]
fn suite_summary_alone_cannot_disarm_a_live_process_clock() {
    let mut tracker = TestTracker::default();
    tracker.observe_suite_start();
    tracker.observe_suite_end();
    tracker.last_progress = Instant::now() - TEST_TIMEOUT;
    assert!(matches!(
        tracker.timed_out(TEST_TIMEOUT),
        Some(Expiry::NoCompletion { .. })
    ));
}

/// A suite that is slow to reach its first test has broken no budget. Arming
/// the no-progress clock on the suite event alone killed it: suite at t=0,
/// first test starting at t=2 and finishing at t=19 was killed at t=20 for
/// consuming 19 seconds of its 20. An observed START has to count as progress.
#[test]
fn a_slow_suite_startup_is_not_a_blown_budget() {
    let mut tracker = TestTracker::default();
    tracker.observe_suite_start();
    // Pretend the suite announced itself 19 seconds ago and the first test
    // started 2 seconds later - so 17 seconds into a 20 second budget.
    tracker.last_progress = Instant::now()
        .checked_sub(Duration::from_secs(19))
        .unwrap_or_else(Instant::now);
    tracker.observe_start("a::first".to_owned());
    assert!(
        tracker.timed_out(TEST_TIMEOUT).is_none(),
        "a test that started 0s ago has spent none of its budget"
    );
}

/// The forgery bound. A test can write lifecycle-shaped records to the same
/// stdout the protocol uses, so a naive clock that any completion refreshes
/// lets a test re-emit one on a timer and run until the sweep backstop.
/// Refreshes are once per name, so a forger can spend only as many as there
/// are distinct names it invents.
#[test]
fn a_repeated_forged_completion_cannot_refresh_the_clock() {
    let mut tracker = TestTracker::default();
    tracker.observe_suite_start();
    tracker.observe_start("a::one".to_owned());
    tracker.observe_result("a::one", true);

    // Wind the clock back past the cap, then replay the same records - which
    // is exactly what a test looping `println!` of a captured event does.
    tracker.last_progress = Instant::now()
        .checked_sub(TEST_TIMEOUT + Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    tracker.observe_start("a::one".to_owned());
    tracker.observe_result("a::one", true);

    assert!(matches!(
        tracker.timed_out(TEST_TIMEOUT),
        Some(Expiry::NoCompletion { .. })
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn an_expired_per_test_cap_kills_even_with_the_wall_far_away() {
    use std::os::unix::process::CommandExt;

    let root = test_root("watchdog_expired_per_test_cap");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 60 & wait")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn shell");
    let cargo_pid = child.id();

    // A test that has apparently been running an hour: the per-test ceiling
    // is long expired, so an implementation that lets attribution terminate
    // kills the group here.
    let long_ago = Instant::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or_else(Instant::now);
    let tracker = Arc::new(Mutex::new(TestTracker {
        current: HashMap::from([("a::apparently_overdue".to_owned(), long_ago)]),
        completed: Vec::new(),
        ever_observed: true,
        executing: true,
        last_progress: Instant::now(),
        seen: HashSet::new(),
        finished: HashSet::new(),
        in_suite: false,
        idle_since: Instant::now(),
    }));
    let done = Arc::new(AtomicBool::new(false));
    let hung = Arc::new(Mutex::new(None::<HungTest>));

    // The wall is an hour away and must not be what saves or condemns this
    // run: the per-test cap alone has to fire.
    watchdog_loop_with_timing(
        root.clone(),
        cargo_pid,
        Leader::Cargo,
        Arc::clone(&tracker),
        Arc::clone(&done),
        Arc::clone(&hung),
        Ceilings {
            per_test: Duration::from_millis(10),
            wall: Some(Duration::from_secs(3600)),
            shape: WallShape::SharedHarness,
        },
        Duration::from_millis(5),
        Instant::now(),
        None,
        &test_context(),
    );
    child.wait().ok();

    let hung = hung
        .lock()
        .unwrap()
        .clone()
        .expect("the per-test cap must fire");
    assert_eq!(
        hung.reason,
        TimeoutReason::Watched,
        "the cap records the clock that fired"
    );
    assert_eq!(
        hung.attribution,
        Attribution::Suspect {
            name: "a::apparently_overdue".to_owned(),
            stale: false,
        },
        "a shared stream name is a suspect, not exact attribution"
    );
    assert_eq!(hung.ceiling, Duration::from_millis(10));
    assert!(
        wait_for_process_group_exit(cargo_pid),
        "exceeding the per-test cap must kill the run"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn watchdog_kills_process_group_and_writes_snapshot() {
    use std::os::unix::process::CommandExt;

    let root = test_root("watchdog_kill_snapshot");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 60 & wait")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn shell");
    let cargo_pid = child.id();
    let child_pids = wait_for_direct_children(cargo_pid);
    assert!(!child_pids.is_empty(), "shell did not spawn sleep child");

    let started = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let tracker = Arc::new(Mutex::new(TestTracker {
        current: HashMap::from([("watchdog::hangs".to_owned(), started)]),
        completed: Vec::new(),
        ever_observed: true,
        executing: true,
        last_progress: Instant::now(),
        seen: HashSet::new(),
        finished: HashSet::new(),
        in_suite: false,
        idle_since: started,
    }));
    let done = Arc::new(AtomicBool::new(false));
    let hung = Arc::new(Mutex::new(None::<HungTest>));

    watchdog_loop_with_timing(
        root.clone(),
        cargo_pid,
        Leader::Cargo,
        Arc::clone(&tracker),
        Arc::clone(&done),
        Arc::clone(&hung),
        // One process, one test, named by the caller: the only shape whose
        // verdict names a test, since a shared-harness wall expiry cannot
        // know which test to blame. `wall` is also what kills now, so the
        // ceiling has to be there for this test to exercise a kill at all.
        Ceilings::one_test(Duration::from_millis(20), "watchdog::hangs"),
        Duration::from_millis(5),
        Instant::now(),
        None,
        &test_context(),
    );
    child.wait().ok();

    let hung = hung.lock().unwrap().clone().expect("hung result");
    assert_eq!(
        hung.attribution,
        Attribution::Exact {
            name: "watchdog::hangs".to_owned(),
        }
    );
    assert!(hung.snapshot_dir.exists());
    assert!(hung.snapshot_dir.join("proc-status.txt").exists());
    assert!(hung.snapshot_pid.is_some());
    assert!(
        wait_for_process_group_exit(cargo_pid),
        "process group {cargo_pid} survived watchdog kill"
    );
}

// A directly executed test binary is the hung harness itself; its children
// are whatever the tests spawned. The snapshot must examine the leader,
// where under cargo it examines cargo's first child.
#[cfg(target_os = "linux")]
#[test]
fn a_direct_launch_snapshots_the_leader_not_its_child() {
    use std::os::unix::process::CommandExt;

    let root = test_root("direct_leader_snapshot");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 60 & wait")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn shell");
    let leader = child.id();
    let children = wait_for_direct_children(leader);
    assert!(!children.is_empty(), "shell did not spawn sleep child");

    let reason = TimeoutReason::Watched;
    let attribution = Attribution::Exact {
        name: "direct::hangs".to_owned(),
    };
    let tick = Duration::from_millis(1);
    let direct = capture_hung_test(
        &root,
        leader,
        Leader::TestBinary,
        reason.clone(),
        attribution.clone(),
        tick,
        tick,
    );
    let cargo = capture_hung_test(
        &root,
        leader,
        Leader::Cargo,
        reason,
        attribution,
        tick,
        tick,
    );
    crate::process::signal_process_group(leader, libc::SIGKILL).ok();
    child.wait().ok();

    assert_eq!(direct.snapshot_pid, Some(leader));
    assert_eq!(cargo.snapshot_pid, children.first().copied());
}

/// Drive a JSON event stream through the reconstructor, returning the
/// emitted human lines and the set of tests still in-flight in the tracker
/// (i.e. what the watchdog would age).
fn drive_recon(events: &[&str]) -> (Vec<String>, Vec<String>) {
    let tracker = Mutex::new(TestTracker::default());
    let mut recon = JsonReconstructor::default();
    let mut lines = Vec::new();
    for ev in events {
        lines.extend(recon.observe(ev, &tracker));
    }
    let mut in_flight: Vec<String> = tracker.lock().unwrap().current.keys().cloned().collect();
    in_flight.sort();
    (lines, in_flight)
}

/// The concatenation case. libtest writes a record and its newline as one
/// write with no boundary against preceding output, so a test that prints
/// without a trailing newline lands its text on the same line as the next
/// event. Requiring the line to START with `{` silently dropped that event:
/// the test stayed in-flight, its duration ran to the next transition, and
/// the per-test clock aged the wrong thing.
#[test]
fn an_event_glued_after_unterminated_output_is_still_consumed() {
    let (lines, in_flight) = drive_recon(&[
        r#"{"type":"suite","event":"started","test_count":1}"#,
        r#"{"type":"test","event":"started","name":"a::one"}"#,
        // `print!("breadcrumb")` with no newline, then libtest's record.
        r#"breadcrumb{"type":"test","name":"a::one","event":"ok"}"#,
        r#"{"type":"suite","event":"ok","passed":1,"failed":0,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.01}"#,
    ]);
    assert!(
        in_flight.is_empty(),
        "the glued terminal event must clear the tracker"
    );
    assert!(
        lines.iter().any(|l| l == "breadcrumb"),
        "the test's own output must survive verbatim: {lines:?}"
    );
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let parsed = crate::test_runner::parse_test_output(&refs);
    assert_eq!(parsed.passed, 1, "the event must be counted: {lines:?}");
}

/// Test output that itself ends in JSON, immediately before a real record.
/// Only the later boundary parses cleanly to end-of-line, which is why the
/// scan runs right to left.
#[test]
fn json_shaped_test_output_before_a_real_event_is_preserved() {
    let (lines, in_flight) = drive_recon(&[
        r#"{"type":"suite","event":"started","test_count":1}"#,
        r#"{"type":"test","event":"started","name":"a::one"}"#,
        r#"{"cfg":"mine"}{"type":"test","name":"a::one","event":"ok"}"#,
    ]);
    assert!(
        in_flight.is_empty(),
        "the real event must still be consumed"
    );
    assert!(
        lines.iter().any(|l| l == r#"{"cfg":"mine"}"#),
        "the test's JSON-shaped output must survive verbatim: {lines:?}"
    );
}

/// Neither arbitrary JSON nor cargo's own artifact records are libtest
/// events, however JSON-shaped they are.
#[test]
fn non_libtest_json_is_passed_through_not_interpreted() {
    let (lines, in_flight) = drive_recon(&[
        r#"{"type":"suite","event":"started","test_count":1}"#,
        r#"{"type":"test","event":"started","name":"a::one"}"#,
        // cargo's message-format record: `reason`, no `type`.
        r#"{"reason":"compiler-artifact","package_id":"p"}"#,
        // An object with a `type` that is not one of libtest's.
        r#"{"type":"telemetry","event":"ok","name":"a::one"}"#,
    ]);
    assert_eq!(
        in_flight,
        vec!["a::one"],
        "no impostor may clear a test from the tracker"
    );
    assert!(
        lines.iter().any(|l| l.contains("compiler-artifact")),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("telemetry")), "{lines:?}");
}

#[test]
fn recon_passing_suite_round_trips_to_parseable_text() {
    let (lines, in_flight) = drive_recon(&[
        r#"{"type":"suite","event":"started","test_count":2}"#,
        r#"{"type":"test","event":"started","name":"a::one"}"#,
        r#"{"type":"test","event":"started","name":"a::two"}"#,
        r#"{"type":"test","name":"a::one","event":"ok"}"#,
        r#"{"type":"test","name":"a::two","event":"ok"}"#,
        r#"{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":0,"measured":0,"filtered_out":3,"exec_time":0.01}"#,
    ]);
    assert!(in_flight.is_empty(), "all tests should have finished");
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let parsed = crate::test_runner::parse_test_output(&refs);
    assert_eq!(parsed.passed, 2);
    assert_eq!(parsed.failed, 0);
    assert_eq!(parsed.filtered_out, 3);
    assert_eq!(parsed.suites, 1);
}

#[test]
fn recon_failure_renders_failures_block_with_panic() {
    let (lines, _) = drive_recon(&[
        r#"{"type":"suite","event":"started","test_count":1}"#,
        r#"{"type":"test","event":"started","name":"a::boom"}"#,
        r#"{"type":"test","name":"a::boom","event":"failed","stdout":"thread 'a::boom' panicked at src/lib.rs:10:5:\nassertion failed\n"}"#,
        r#"{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.02}"#,
    ]);
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let parsed = crate::test_runner::parse_test_output(&refs);
    assert_eq!(parsed.failed, 1);
    assert_eq!(parsed.failures.len(), 1, "panic detail recovered: {refs:?}");
    assert_eq!(parsed.failures[0].name, "a::boom");
    assert!(
        parsed.failures[0]
            .location
            .as_deref()
            .is_some_and(|l| l.contains("src/lib.rs:10:5")),
        "location parsed: {:?}",
        parsed.failures[0].location
    );
}

/// libtest emits `started` for an `#[ignore]`d test, then `ignored`. The
/// test leaves the in-flight set but must not land in `completed`, which
/// the parallel lane sums as its pass count: counting it reported ignored
/// tests as passed and hid an all-ignored binary from the empty-unit note.
#[test]
fn an_ignored_test_is_not_counted_as_completed() {
    let tracker = Mutex::new(TestTracker::default());
    let mut recon = JsonReconstructor::default();
    for ev in [
        r#"{"type":"suite","event":"started","test_count":2}"#,
        r#"{"type":"test","event":"started","name":"a::skipped"}"#,
        r#"{"type":"test","name":"a::skipped","event":"ignored"}"#,
        r#"{"type":"test","event":"started","name":"a::ran"}"#,
        r#"{"type":"test","name":"a::ran","event":"ok"}"#,
    ] {
        recon.observe(ev, &tracker);
    }
    let t = tracker.lock().unwrap();
    assert!(t.current.is_empty(), "an ignored test is not in flight");
    let names: Vec<&str> = t.completed.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["a::ran"]);
}

#[test]
fn recon_leaves_started_test_in_flight_for_watchdog() {
    // A test that started but never emitted a terminating event is what the
    // per-test watchdog must be able to age and blame.
    let (_, in_flight) = drive_recon(&[
        r#"{"type":"suite","event":"started","test_count":2}"#,
        r#"{"type":"test","event":"started","name":"a::done"}"#,
        r#"{"type":"test","event":"started","name":"a::hangs"}"#,
        r#"{"type":"test","name":"a::done","event":"ok"}"#,
    ]);
    assert_eq!(in_flight, vec!["a::hangs".to_owned()]);
}

#[test]
fn recon_passes_cargo_json_and_stray_output_through() {
    // cargo's own message-format=json line (has `reason`, no `type`) and a
    // non-JSON stray print must survive verbatim - the former for the
    // `--json` path, the latter for `--nocapture` test output.
    let (lines, _) = drive_recon(&[
        r#"{"reason":"compiler-artifact","target":{"name":"brokkr"}}"#,
        "hello from a test under --nocapture",
    ]);
    assert_eq!(
        lines,
        vec![
            r#"{"reason":"compiler-artifact","target":{"name":"brokkr"}}"#.to_owned(),
            "hello from a test under --nocapture".to_owned(),
        ]
    );
}

/// A fresh scratch dir for one test. `name` must be unique within this
/// module - see `crate::test_scratch`.
fn test_root(name: &str) -> PathBuf {
    crate::test_scratch::scratch("test-runner", name)
}

fn wait_for_direct_children(parent: u32) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let pids = direct_child_pids(parent);
        if !pids.is_empty() || Instant::now() >= deadline {
            return pids;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_process_group_exit(pgid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !process_group_exists(pgid) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    !process_group_exists(pgid)
}

fn process_group_exists(pgid: u32) -> bool {
    crate::process::signal_process_group(pgid, 0).unwrap_or(true)
}
