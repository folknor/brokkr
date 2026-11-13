//! A preflight against real children spawned through the choke point: the
//! external program and its output, a start failure the script reads, the
//! builtins it may not reach, a descendant that outlives its call, and what
//! declares one.

#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

fn script(source: &str) -> lua::Script {
    lua::load_script(source.as_bytes(), Some("test"), None).unwrap()
}

fn run(source: &str, params: &lua::Args) -> lua::HookOutcome {
    // A scratch name is one test's alone: each run takes the next.
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = crate::test_scratch::scratch("bench_preflight", &format!("run-{n}"));
    let host = PreflightHost {
        config: Arc::new(lua::Value::Table(Vec::new())),
        cwd: root.clone(),
        since: Since::new(&root, "0".repeat(40).parse().unwrap()),
        inputs: Vec::new(),
    };
    lua::run_hook(lua::Hook::Preflight, &script(source), params, host)
        .0
        .outcome
}

/// page_cache() answers for a bound input alone: nil where the
/// prerequisites hold, the reason where the input is gone.
#[test]
fn page_cache_checks_a_bound_input_and_refuses_any_other_path() {
    let root = crate::test_scratch::scratch("bench_preflight", "page-cache");
    let (input, gone) = (root.join("in.pbf"), root.join("gone.pbf"));
    std::fs::write(&input, b"data").unwrap();
    let host = || PreflightHost {
        config: Arc::new(lua::Value::Table(Vec::new())),
        cwd: root.clone(),
        since: Since::new(&root, "0".repeat(40).parse().unwrap()),
        inputs: vec![input.clone(), gone.clone()],
    };
    let source = format!(
        "function preflight()\n\
           local why = page_cache({input:?}, 'warm')\n\
           if why then fail('warm: ' .. why) end\n\
           why = page_cache({gone:?}, 'cold')\n\
           if why then fail(why) end\n\
         end\n\
         function bench() end",
        input = input.display().to_string(),
        gone = gone.display().to_string(),
    );
    let why = refused(
        lua::run_hook(
            lua::Hook::Preflight,
            &script(&source),
            &lua::Args::default(),
            host(),
        )
        .0
        .outcome,
    );
    assert!(why.contains("gone.pbf") && !why.contains("warm: "), "{why}");
    let other =
        "function preflight() page_cache('/etc/hostname', 'warm') end\nfunction bench() end";
    let why = broken(
        lua::run_hook(
            lua::Hook::Preflight,
            &script(other),
            &lua::Args::default(),
            host(),
        )
        .0
        .outcome,
    );
    assert!(why.contains("not a bound input"), "{why}");
}

fn refused(outcome: lua::HookOutcome) -> String {
    match outcome {
        lua::HookOutcome::Refused(why) => why,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn broken(outcome: lua::HookOutcome) -> String {
    match outcome {
        lua::HookOutcome::Broken(why) => why,
        other => panic!("expected a broken preflight, got {other:?}"),
    }
}

#[test]
fn a_preflight_runs_external_programs_and_passes_by_returning_nothing() {
    let _lock = crate::test_scratch::process_global_lock();
    let params = lua::Args {
        params: vec![("mode".into(), lua::Value::Text("uring".into()))],
        keys: vec![("mode".into(), lua::Value::Text("uring".into()))],
    };
    let outcome = run(
        "function preflight(params, keys)\n\
           local r = binary_with({program = 'sh', env = {X = 'y'}}, '-c', 'printf \"$X\"; exit 3')\n\
           if r.stdout ~= 'y' or r.exit ~= 3 or r.program ~= 'sh' then fail('saw ' .. r.stdout) end\n\
           if params.mode ~= 'uring' or keys.mode ~= 'uring' then fail('params') end\n\
           binary_with({program = 'sh'}, '-c', 'true'):ok()\n\
         end\n\
         function bench() end",
        &params,
    );
    assert_eq!(outcome, lua::HookOutcome::Passed);
}

/// A probe past its `deadline` is killed and refuses the preflight, as a
/// failed `r:ok()` does, rather than reading as a script error.
#[test]
fn a_probe_past_its_deadline_refuses_the_preflight() {
    let _lock = crate::test_scratch::process_global_lock();
    let started = std::time::Instant::now();
    let why = refused(run(
        "function preflight()\n\
           binary_with({program = 'sh', deadline = 0.3}, '-c', 'sleep 30 & wait')\n\
         end\n\
         function bench() end",
        &lua::Args::default(),
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert!(why.contains("sh passed its deadline of 300ms"), "{why}");
}

#[test]
fn a_program_that_cannot_start_is_a_result_the_script_reads() {
    let _lock = crate::test_scratch::process_global_lock();
    let why = refused(run(
        "function preflight()\n\
           local r = binary_with({program = 'brokkr-no-such-program'}, '-v')\n\
           if r.exit ~= nil or r.stdout ~= nil then fail('a result for a start failure') end\n\
           fail('install it: ' .. r.start_error)\n\
         end\n\
         function bench() end",
        &lua::Args::default(),
    ));
    assert!(
        why.starts_with("test:4: install it: resolve: brokkr-no-such-program is not on PATH"),
        "{why}"
    );
    let why = refused(run(
        "function preflight()\n\
           binary_with({program = 'brokkr-no-such-program'}):ok()\n\
         end\n\
         function bench() end",
        &lua::Args::default(),
    ));
    assert!(
        why.contains("brokkr-no-such-program could not start: resolve:"),
        "{why}"
    );
}

/// A file in no format the kernel runs, or one whose interpreter is
/// missing, is the program's own failure, read with its operation and errno
/// (never run through a shell, as libc would); a spawn that fails before the
/// exec (its working directory gone) is brokkr's, never a result the script
/// could ignore.
#[test]
fn an_exec_failure_is_the_programs_only_when_the_program_is_at_fault() {
    let _lock = crate::test_scratch::process_global_lock();
    let dir = crate::test_scratch::scratch("bench_preflight", "exec");
    let junk = dir.join("junk");
    std::fs::write(&junk, b"\x7fnot an executable format\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&junk, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let why = refused(run(
        &format!(
            "function preflight()\n\
               local r = binary_with({{program = '{}'}})\n\
               fail(r.start_operation .. ' ' .. tostring(r.start_errno) .. ' ' .. r.start_error)\n\
             end\n\
             function bench() end",
            junk.display()
        ),
        &lua::Args::default(),
    ));
    assert!(
        why.contains(&format!("execute {} execute: ", libc::ENOEXEC)),
        "exec refused it, and nothing ran it through a shell: {why}"
    );

    let orphan = dir.join("orphan");
    std::fs::write(&orphan, b"#!/brokkr/no/such/interpreter\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let why = refused(run(
        &format!(
            "function preflight()\n\
               local r = binary_with({{program = '{}'}})\n\
               fail(r.start_operation .. ' ' .. tostring(r.start_errno))\n\
             end\n\
             function bench() end",
            orphan.display()
        ),
        &lua::Args::default(),
    ));
    assert!(
        why.ends_with(&format!("execute {}", libc::ENOENT)),
        "a missing interpreter is the program's: {why}"
    );

    let outcome = run(
        "function preflight()\n\
           binary_with({program = 'sh'}, '-c', 'rm -rf \"$PWD\"'):ok()\n\
           binary_with({program = '/bin/sh'}, '-c', 'true')\n\
         end\n\
         function bench() end",
        &lua::Args::default(),
    );
    let lua::HookOutcome::Failed(why) = outcome else {
        panic!("a setup failure is brokkr's: {outcome:?}");
    };
    assert!(why.contains("cannot start /bin/sh"), "{why}");
}

#[test]
fn the_build_handles_and_measurement_are_out_of_reach() {
    let _lock = crate::test_scratch::process_global_lock();
    for (call, needle) in [
        ("binary('x')", "does not exist yet inside preflight()"),
        (
            "binary_with({}, 'x')",
            "does not exist yet inside preflight()",
        ),
        (
            "output('o')",
            "only available inside bench(), not inside preflight()",
        ),
        (
            "scratch('s')",
            "only available inside bench(), not inside preflight()",
        ),
        (
            "binary_with({program = 'sh', hotpath = 'x'}, '-c', 'true')",
            "hotpath is not available inside preflight()",
        ),
        (
            "binary_with({program = 'sh', stop_at = 'X_END'}, '-c', 'true')",
            "stop_at is not available inside preflight()",
        ),
    ] {
        let why = broken(run(
            &format!("function preflight()\n  {call}\nend\nfunction bench() end"),
            &lua::Args::default(),
        ));
        assert!(why.contains(needle), "{call}: {why}");
    }
    let why = broken(run(
        "binary_with({program = 'sh'}, '-c', 'true')\n\
         function preflight() end\n\
         function bench() end",
        &lua::Args::default(),
    ));
    assert!(why.contains("not at a chunk's top level"), "{why}");
}

#[test]
fn a_preflight_that_is_no_function_or_returns_a_value_is_broken() {
    let _lock = crate::test_scratch::process_global_lock();
    let why = broken(run(
        "if false then function preflight() end end\nfunction bench() end",
        &lua::Args::default(),
    ));
    assert!(why.contains("left it a nil"), "{why}");
    for (returned, needle) in [
        ("'ok'", "returned 1 value (the first a string)"),
        ("nil", "returned 1 value (the first a nil)"),
        ("nil, 'x'", "returned 2 values"),
    ] {
        let why = broken(run(
            &format!("function preflight() return {returned} end\nfunction bench() end"),
            &lua::Args::default(),
        ));
        assert!(why.contains(needle), "{returned}: {why}");
    }
    assert_eq!(
        run(
            "function preflight() return end\nfunction bench() end",
            &lua::Args::default()
        ),
        lua::HookOutcome::Passed
    );
    let why = broken(run(
        "function preflight() local x = nil; x.y = 1 end\nfunction bench() end",
        &lua::Args::default(),
    ));
    assert!(!why.is_empty());
}

/// A child whose descendant still holds its lease after it exited has not
/// finished: the preflight fails rather than pass with the work running on.
#[test]
fn a_descendant_still_holding_the_lease_fails_the_call() {
    let _lock = crate::test_scratch::process_global_lock();
    let outcome = run(
        "function preflight()\n\
           binary_with({program = 'sh'}, '-c', 'sleep 4 >/dev/null 2>&1 &')\n\
         end\n\
         function bench() end",
        &lua::Args::default(),
    );
    let lua::HookOutcome::Failed(why) = outcome else {
        panic!("a lease brokkr could not release is its own failure: {outcome:?}");
    };
    assert!(why.contains("still holds its lease"), "{why}");
}

#[test]
fn a_bare_assignment_anywhere_declares_a_preflight() {
    for (source, declared) in [
        ("function preflight() end", true),
        ("function bench() preflight = function() end end", true),
        ("if false then preflight = 1 end", true),
        ("local function preflight() end", false),
        ("_G.preflight = function() end", false),
        ("local t = {}; t.preflight = 1", false),
    ] {
        assert_eq!(script(source).has_preflight(), declared, "{source}");
    }
}
