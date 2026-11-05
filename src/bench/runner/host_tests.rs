//! An iteration against real children spawned through the choke point: the
//! borrow, the spool preview, stderr, and a descendant's late markers.

#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

static NO_ARGS: std::sync::LazyLock<lua::Args> = std::sync::LazyLock::new(lua::Args::default);

fn setup<'a>(script: &'a lua::Script, root: &'a Path, container: &'a Path) -> IterationSetup<'a> {
    IterationSetup {
        script,
        args: &NO_ARGS,
        inputs: &[],
        config: Arc::new(lua::Value::Table(Vec::new())),
        program: Path::new("/bin/sh"),
        cwd: root,
        state_root: root,
        container,
        index: 0,
        since: Since::new(root, "0".repeat(40).parse().unwrap()),
        measurement: store::Measurement::Timing,
        report_budget: RUN_REPORT_BYTES,
    }
}

fn script(source: &str) -> lua::Script {
    lua::load_script(source.as_bytes(), Some("test"), None).unwrap()
}

/// `bench(params)` holding `params.input.file`, an input handle on `path`.
fn input_args(path: &Path) -> lua::Args {
    lua::Args {
        params: vec![(
            "input".into(),
            lua::Value::Table(vec![(
                "file".into(),
                lua::Value::Handle(lua::HandleRef {
                    index: 0,
                    path: path.display().to_string(),
                }),
            )]),
        )],
        keys: vec![("input".into(), lua::Value::Text("in".into()))],
    }
}

fn failure(iteration: &store::Iteration) -> &str {
    match &iteration.outcome {
        store::IterationOutcome::Failed { reason } => reason,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn record(iteration: &store::Iteration) -> serde_json::Map<String, serde_json::Value> {
    assert!(
        matches!(iteration.outcome, store::IterationOutcome::Completed { .. }),
        "{iteration:?}"
    );
    iteration.record.clone().unwrap()
}

/// An input declared written is refused at the call, through the runner:
/// inputs are read-only for the run.
#[test]
fn an_input_in_writes_is_refused_at_the_call() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "input-writes");
    let container = root.join("iterations");
    let input = root.join("in.txt");
    std::fs::write(&input, b"payload").unwrap();
    let args = input_args(&input);
    let script = script(
        "function bench(params)\n\
           binary_with({writes = {params.input.file}}, '-c', 'true')\n\
         end",
    );
    let inputs = [input.clone()];
    let mut with_input = setup(&script, &root, &container);
    with_input.args = &args;
    with_input.inputs = &inputs;
    let (iteration, _, _) = run_iteration(with_input);
    let reason = failure(&iteration);
    assert!(reason.contains("read-only for the run"), "{reason}");
    assert!(reason.starts_with("test:2: "), "{reason}");
    assert!(iteration.children.is_empty(), "refused before any spawn");
}

/// A child that turns its output into a link to the input, or a hard link
/// to it, cannot get brokkr to write the input: the link is refused, the
/// hard link cut.
#[test]
fn a_substituted_output_never_lets_write_reach_an_input() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "substituted");
    let container = root.join("iterations");
    let input = root.join("in.txt");
    std::fs::write(&input, b"payload").unwrap();
    let args = input_args(&input);
    let inputs = [input.clone()];
    for (link, expect_refusal) in [("ln -s", true), ("ln", false)] {
        let linking = script(&format!(
            "function bench(params)\n\
               local out = output('o')\n\
               binary('-c', '{link} \"$1\" \"$2\"', 'sh', params.input.file, out):ok()\n\
               write(out, 'clobbered')\n\
               record.back = read(out)\n\
             end"
        ));
        let mut with_input = setup(&linking, &root, &container);
        with_input.args = &args;
        with_input.inputs = &inputs;
        let (iteration, _, _) = run_iteration(with_input);
        if expect_refusal {
            let reason = failure(&iteration);
            assert!(reason.contains("read-only for the run"), "{reason}");
        } else {
            assert_eq!(record(&iteration)["back"], "clobbered");
        }
        assert_eq!(std::fs::read(&input).unwrap(), b"payload", "{link}");
    }
}

/// read() of an output a child made a link refuses, as does a spool
/// replaced after its call; stdin from a spool is the captured bytes, from
/// the start each time.
#[test]
fn spools_feed_onward_and_a_replaced_one_is_refused() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "spools");
    let container = root.join("iterations");
    let feeding = script(
        "function bench()\n\
           local r = binary('-c', 'printf hello'):ok()\n\
           record.size = filesize(r.stdout_file)\n\
           record.read = read(r.stdout_file)\n\
           local a = binary_with({stdin = r.stdout_file}, '-c', 'cat'):ok()\n\
           local b = binary_with({stdin = r.stdout_file}, '-c', 'cat'):ok()\n\
           record.fed = a.stdout .. b.stdout\n\
           local c = binary('-c', 'cat \"$1\"', 'sh', r.stdout_file):ok()\n\
           record.path = c.stdout\n\
           local e = binary_with({env = {GREETING = 'hi'}}, '-c', 'printf \"$GREETING\"'):ok()\n\
           record.env = e.stdout\n\
           return 'ok'\n\
         end",
    );
    let (iteration, _, _) = run_iteration(setup(&feeding, &root, &container));
    let found = record(&iteration);
    assert_eq!(found["size"], 5);
    assert_eq!(found["read"], "hello");
    assert_eq!(found["fed"], "hellohello");
    assert_eq!(found["path"], "hello");
    assert_eq!(found["env"], "hi");
    let env = &iteration.children[4].env;
    assert_eq!(env.get("GREETING").map(String::as_str), Some("hi"));
    assert!(env.contains_key(crate::sidecar::MARKER_FIFO_ENV));

    let replacing = script(
        "function bench()\n\
           local r = binary('-c', 'printf hello'):ok()\n\
           binary('-c', 'rm \"$1\"; printf other > \"$1\"', 'sh', r.stdout_file):ok()\n\
           read(r.stdout_file)\n\
         end",
    );
    let (iteration, _, _) = run_iteration(setup(&replacing, &root, &container));
    let reason = failure(&iteration);
    assert!(reason.contains("no longer the spool"), "{reason}");

    // A link onto the input passes authority (the input is read), so it is
    // the open, following no link, that refuses it.
    let input = root.join("in.txt");
    std::fs::write(&input, b"payload").unwrap();
    let args = input_args(&input);
    let inputs = [input.clone()];
    let linking = script(
        "function bench(params)\n\
           local out = output('o')\n\
           binary('-c', 'ln -s \"$1\" \"$2\"', 'sh', params.input.file, out):ok()\n\
           read(out)\n\
         end",
    );
    let mut with_input = setup(&linking, &root, &container);
    with_input.args = &args;
    with_input.inputs = &inputs;
    let (iteration, _, _) = run_iteration(with_input);
    let reason = failure(&iteration);
    assert!(
        reason.contains("is a link, which brokkr does not follow"),
        "{reason}"
    );
}

#[test]
fn a_child_runs_on_its_borrow_and_reaches_the_script() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "child");
    let container = root.join("iterations");
    let script = script(
        "function bench()\n\
           local r = binary('-c', 'printf out; echo err >&2; exit 3')\n\
           record.stdout = r.stdout\n\
           record.stderr = r.stderr\n\
           record.exit = r.exit\n\
           local out = output('o')\n\
           binary('-c', 'echo written > \"$1\"', 'sh', out):ok()\n\
           return 'done'\n\
         end",
    );
    let (iteration, _, _) = run_iteration(setup(&script, &root, &container));
    assert_eq!(
        iteration.outcome,
        store::IterationOutcome::Completed {
            verdict: Some("done".into())
        },
        "{iteration:?}"
    );
    let record = iteration.record.unwrap();
    assert_eq!(record["stdout"], "out");
    assert_eq!(record["stderr"], "err\n");
    assert_eq!(record["exit"], 3);
    assert_eq!(iteration.children.len(), 2);
    let child = &iteration.children[0];
    assert_eq!(child.ending, store::ChildEnding::Exited { code: 3 });
    assert_eq!(child.program, "/bin/sh");
    assert!(child.origin_offset_us >= 0 && iteration.wall_us.is_some());
    // The owner's release took the iteration directory with it.
    assert_eq!(std::fs::read_dir(&container).unwrap().count(), 0);
}

/// A hotpath report as the crate writes one, as a shell `printf` argument.
const REPORT: &str = r#"{"functions_timing":{"data":[{"name":"decode","calls":12,"avg":"2 ms","total":"24 ms","percent_total":"40%","p50":"1.8 ms","p95":"3.0 ms","p99":"3.8 ms"}]}}"#;

fn capture(child: &store::Child) -> &store::HotpathCapture {
    child.hotpath.as_ref().unwrap()
}

/// A marked call gets the three variables and a report under `profile/`;
/// the report is stored on the child, the script sees it as `r.hotpath`,
/// and nothing fills `record.hotpath`. An unmarked call has no capture.
#[test]
fn a_marked_call_stores_its_report_on_the_child() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "hotpath");
    let container = root.join("iterations");
    let script = script(&format!(
        "function bench()\n\
           local r = binary_with({{hotpath = 'decode', env = {{R = '{REPORT}'}}}}, '-c',\n\
             'test \"$HOTPATH_METRICS_SERVER_OFF\" = true && test \"$HOTPATH_OUTPUT_FORMAT\" = json \
              && printf %s \"$R\" > \"$HOTPATH_OUTPUT_PATH\"'):ok()\n\
           record.calls = r.hotpath.functions_timing.data[1].calls\n\
           record.status = r.hotpath_status\n\
           binary('-c', 'true')\n\
         end"
    ));
    let mut with = setup(&script, &root, &container);
    with.measurement = store::Measurement::Hotpath;
    let (iteration, _, left) = run_iteration(with);
    let shown = record(&iteration);
    assert_eq!(shown["calls"], 12);
    assert_eq!(shown["status"], "captured");
    assert!(!shown.contains_key("hotpath"), "no auto-fill");
    let marked = capture(&iteration.children[0]);
    assert_eq!((marked.stage.as_str(), marked.occurrence), ("decode", 1));
    assert_eq!(
        marked.outcome,
        store::CaptureOutcome::Captured {
            report: serde_json::from_str(REPORT).unwrap()
        }
    );
    assert!(iteration.children[0].env["HOTPATH_OUTPUT_PATH"].contains("/profile/"));
    assert!(iteration.children[1].hotpath.is_none());
    assert_eq!(left, RUN_REPORT_BYTES - REPORT.len());
}

/// Whatever the exit, the capture is judged and stored and the script sees
/// its status; under timing nothing fails, under --hotpath an iteration
/// with a capture that failed, or with no marked call, does. Occurrences
/// count every marked call of a stage.
#[test]
fn capture_failures_are_latched_for_hotpath_and_evidence_for_timing() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "hotpath-latched");
    let container = root.join("iterations");
    let source = format!(
        "function bench()\n\
           local o = {{hotpath = 'stage', env = {{R = '{REPORT}'}}}}\n\
           local bare = binary_with({{hotpath = 'stage'}}, '-c', 'exit 4')\n\
           local bad = binary_with({{hotpath = 'stage'}}, '-c', 'echo nope > \"$HOTPATH_OUTPUT_PATH\"')\n\
           local late = binary_with(o, '-c', 'printf %s \"$R\" > \"$HOTPATH_OUTPUT_PATH\"; exit 2')\n\
           record.bare = bare.hotpath_status .. ' ' .. bare.exit\n\
           record.bad = bad.hotpath_status\n\
           record.bad_error = bad.hotpath_error ~= nil\n\
           record.late = late.hotpath_status\n\
         end"
    );
    let script = script(&source);
    let (timing, _, _) = run_iteration(setup(&script, &root, &container));
    let shown = record(&timing);
    assert_eq!(shown["bare"], "missing 4");
    assert_eq!(shown["bad"], "invalid");
    assert_eq!(shown["bad_error"], true);
    assert_eq!(shown["late"], "captured", "a nonzero exit keeps its report");
    let occurrences: Vec<u32> = timing.children.iter().map(|c| capture(c).occurrence).collect();
    assert_eq!(occurrences, [1, 2, 3]);

    let mut profiled = setup(&script, &root, &container);
    profiled.measurement = store::Measurement::Hotpath;
    let (iteration, _, _) = run_iteration(profiled);
    let why = failure(&iteration);
    assert!(
        why.contains("stage stage occurrence 1: no report was written")
            && why.contains("occurrence 2: invalid report"),
        "{why}"
    );
    assert_eq!(
        iteration.record.as_ref().unwrap()["bare"],
        "missing 4",
        "the script's record is kept"
    );
    assert_eq!(iteration.children.len(), 3, "the evidence is kept");

    let unmarked = self::script("function bench() binary('-c', 'true') end");
    let mut profiled = setup(&unmarked, &root, &container);
    profiled.measurement = store::Measurement::Hotpath;
    let (iteration, _, _) = run_iteration(profiled);
    assert!(failure(&iteration).contains("no call of the iteration was marked"));
}

/// A report past the run's remaining budget is over budget, not read, and
/// spends nothing.
#[test]
fn a_report_past_the_budget_is_over_budget() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "hotpath-budget");
    let container = root.join("iterations");
    let script = script(&format!(
        "function bench()\n\
           record.status = binary_with({{hotpath = 's', env = {{R = '{REPORT}'}}}}, '-c',\n\
             'printf %s \"$R\" > \"$HOTPATH_OUTPUT_PATH\"').hotpath_status\n\
         end"
    ));
    let mut tight = setup(&script, &root, &container);
    tight.report_budget = 8;
    let (iteration, _, left) = run_iteration(tight);
    assert_eq!(record(&iteration)["status"], "over_budget");
    assert_eq!(
        capture(&iteration.children[0]).outcome,
        store::CaptureOutcome::OverBudget
    );
    assert_eq!(left, 8);
}

#[test]
fn a_descendants_late_markers_land_in_its_childs_evidence() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "late-marker");
    let container = root.join("iterations");
    let script = script(
        "function bench()\n\
           binary('-c', 'echo \"1 EARLY\" > \"$BROKKR_MARKER_FIFO\"; \
             (sleep 0.3; echo \"2 LATE\" > \"$BROKKR_MARKER_FIFO\") >/dev/null 2>&1 &'):ok()\n\
           binary('-c', 'true'):ok()\n\
         end",
    );
    let (iteration, _, _) = run_iteration(setup(&script, &root, &container));
    assert!(
        matches!(iteration.outcome, store::IterationOutcome::Completed { .. }),
        "{iteration:?}"
    );
    let names = |child: &store::Child| -> Vec<String> {
        child
            .evidence
            .records
            .iter()
            .filter_map(|r| match r {
                store::Record::Marker { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(names(&iteration.children[0]), ["EARLY", "LATE"]);
    assert!(!iteration.children[0].evidence.loss.fifo_unread);
    assert!(
        names(&iteration.children[1]).is_empty(),
        "a fresh FIFO per child"
    );
}

/// A bound file reaches the child through its input handle, the first
/// handle the iteration holds.
#[test]
fn an_input_handle_reaches_the_child() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "input");
    let container = root.join("iterations");
    let input = root.join("in.txt");
    std::fs::write(&input, b"payload").unwrap();
    let args = lua::Args {
        params: vec![(
            "input".into(),
            lua::Value::Table(vec![(
                "file".into(),
                lua::Value::Handle(lua::HandleRef {
                    index: 0,
                    path: input.display().to_string(),
                }),
            )]),
        )],
        keys: vec![("input".into(), lua::Value::Text("in".into()))],
    };
    let script = script(
        "function bench(params, keys)\n\
           local r = binary('-c', 'cat \"$1\"', 'sh', params.input.file):ok()\n\
           record.read = r.stdout\n\
           record.key = keys.input\n\
         end",
    );
    let inputs = [input.clone()];
    let mut with_input = setup(&script, &root, &container);
    with_input.args = &args;
    with_input.inputs = &inputs;
    let (iteration, _, _) = run_iteration(with_input);
    assert!(
        matches!(iteration.outcome, store::IterationOutcome::Completed { .. }),
        "{iteration:?}"
    );
    let record = iteration.record.unwrap();
    assert_eq!(record["read"], "payload");
    assert_eq!(record["key"], "in");
    assert_eq!(iteration.children[0].argv[3], input.display().to_string());
}

#[test]
fn a_failed_iteration_keeps_its_children_and_never_extracts_record() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("bench_runner", "failed");
    let container = root.join("iterations");
    let script = script("function bench() record.x = 1; binary('-c', 'exit 1'):ok() end");
    let (iteration, _, _) = run_iteration(setup(&script, &root, &container));
    let store::IterationOutcome::Failed { reason } = &iteration.outcome else {
        panic!("{iteration:?}");
    };
    assert!(reason.contains("sh exited 1"), "{reason}");
    assert!(iteration.record.is_none() && iteration.wall_us.is_some());
    assert_eq!(iteration.children.len(), 1);
}
