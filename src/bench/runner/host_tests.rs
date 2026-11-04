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
    }
}

fn script(source: &str) -> lua::Script {
    lua::load_script(source.as_bytes(), Some("test")).unwrap()
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
    let (iteration, _) = run_iteration(setup(&script, &root, &container));
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
    let (iteration, _) = run_iteration(setup(&script, &root, &container));
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
    let (iteration, _) = run_iteration(with_input);
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
    let (iteration, _) = run_iteration(setup(&script, &root, &container));
    let store::IterationOutcome::Failed { reason } = &iteration.outcome else {
        panic!("{iteration:?}");
    };
    assert!(reason.contains("sh exited 1"), "{reason}");
    assert!(iteration.record.is_none() && iteration.wall_us.is_some());
    assert_eq!(iteration.children.len(), 1);
}
