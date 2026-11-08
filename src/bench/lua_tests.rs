#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

/// A host that records what the script asked for and answers from fixtures.
#[derive(Default)]
struct Fake {
    calls: Vec<Vec<Word>>,
    options: Vec<CallOptions>,
    exit: Option<i32>,
    stopped_at: Option<String>,
    stdout: Vec<u8>,
    since: Vec<String>,
    blocks: usize,
    config: Option<Value>,
    files: std::collections::BTreeMap<usize, Vec<u8>>,
    stops: usize,
    stop_now: bool,
    /// A stop pending from this stop check on (1-based).
    stop_from: Option<usize>,
}

impl Host for Fake {
    fn stop(&mut self) -> Result<(), Abort> {
        self.stops += 1;
        if self.stop_now || self.stop_from.is_some_and(|n| self.stops >= n) {
            Err(Abort::Interrupted)
        } else {
            Ok(())
        }
    }
    fn stop_requested() -> bool {
        false
    }
    fn begin_block(&mut self, _at: Instant) {
        self.blocks += 1;
    }
    fn binary(&mut self, words: Vec<Word>, options: CallOptions) -> Result<CallOutcome, Abort> {
        self.calls.push(words);
        let options_hotpath = options.hotpath.is_some();
        self.options.push(options);
        Ok(CallOutcome {
            program: "tool".into(),
            exit: self.exit,
            signal: None,
            stdout: self.stdout.clone(),
            stdout_truncated: false,
            stderr: b"line one\nlast words\n".to_vec(),
            stderr_truncated: false,
            stdout_file: Some(HandleRef {
                index: 9,
                path: "/it/stdout/0".into(),
            }),
            hotpath: options_hotpath.then(|| Capture {
                status: "captured",
                report: Some(Value::Table(vec![(
                    "functions_timing".into(),
                    Value::Table(vec![("calls".into(), Value::Number(3.0))]),
                )])),
                error: None,
            }),
            stopped_at: self.stopped_at.clone(),
            start_error: None,
        })
    }
    fn read(&mut self, handle: usize) -> Result<Vec<u8>, Abort> {
        self.files
            .get(&handle)
            .cloned()
            .ok_or_else(|| Abort::Failed(format!("read(): no file {handle}")))
    }
    fn write(&mut self, handle: usize, contents: &[u8]) -> Result<(), Abort> {
        self.files.insert(handle, contents.to_vec());
        Ok(())
    }
    fn filesize(&mut self, handle: usize) -> Result<u64, Abort> {
        Ok(self.files.get(&handle).map_or(0, |f| f.len() as u64))
    }
    fn since(&mut self, sha: &str) -> Result<bool, Abort> {
        self.since.push(sha.to_owned());
        if sha == "bad" {
            return Err(Abort::Failed("since(\"bad\"): no such commit".into()));
        }
        Ok(sha == "old")
    }
    fn output(&mut self, name: &str) -> Result<HandleRef, Abort> {
        Ok(HandleRef {
            index: 7,
            path: format!("/it/out/{name}"),
        })
    }
    fn scratch(&mut self, name: &str) -> Result<HandleRef, Abort> {
        Ok(HandleRef {
            index: 8,
            path: format!("/it/scratch/{name}"),
        })
    }
    fn config(&mut self) -> Result<std::sync::Arc<Value>, Abort> {
        Ok(std::sync::Arc::new(
            self.config.clone().unwrap_or(Value::Table(Vec::new())),
        ))
    }
}

fn run(source: &str, host: Fake) -> (Sample, Fake) {
    run_with(source, &Args::default(), host)
}

fn run_with(source: &str, args: &Args, host: Fake) -> (Sample, Fake) {
    let script = load_script(source.as_bytes(), Some("test.lua"), None).unwrap();
    run_sample(&script, args, host)
}

fn completed(sample: &Sample) -> serde_json::Map<String, serde_json::Value> {
    assert_eq!(sample.outcome, SampleOutcome::Completed(None), "{sample:?}");
    sample.record.clone().unwrap()
}

/// The params, their keys and `config()` reach the script; a handle among
/// them is a real handle to `binary()`, and config data cannot forge one.
#[test]
fn params_keys_and_config_reach_the_script() {
    let args = Args {
        params: vec![
            ("jobs".into(), Value::Text("4".into())),
            (
                "variant".into(),
                Value::Table(vec![
                    (
                        "file".into(),
                        Value::Handle(HandleRef {
                            index: 0,
                            path: "/d/x.pbf".into(),
                        }),
                    ),
                    ("seq".into(), Value::Number(4704.0)),
                ]),
            ),
            (
                "osc".into(),
                Value::List(vec![Value::Table(vec![(
                    "file".into(),
                    Value::Text("/d/1.osc".into()),
                )])]),
            ),
        ],
        keys: vec![
            ("jobs".into(), Value::Text("4".into())),
            ("variant".into(), Value::Text("indexed".into())),
            ("osc".into(), Value::List(vec![Value::Text("1".into())])),
        ],
    };
    let host = Fake {
        config: Some(Value::Table(vec![(
            "data".into(),
            Value::Table(vec![("n".into(), Value::Bool(true))]),
        )])),
        ..Fake::default()
    };
    let source = "top = config().data.n\n\
                  function bench(params, keys)\n\
                    record.jobs = params.jobs\n\
                    record.key = keys.variant\n\
                    record.seq = params.variant.seq\n\
                    record.path = params.variant.file.path\n\
                    record.osc = params.osc[1].file\n\
                    record.osc_key = keys.osc[1]\n\
                    record.top = top\n\
                    local c = config()\n\
                    c.data.n = false\n\
                    record.fresh = config().data.n\n\
                    binary(params.variant.file)\n\
                  end";
    let (sample, host) = run_with(source, &args, host);
    assert_eq!(sample.outcome, SampleOutcome::Completed(None), "{sample:?}");
    let record = sample.record.unwrap();
    assert_eq!(record["jobs"], "4");
    assert_eq!(record["key"], "indexed");
    assert_eq!(record["seq"], 4704);
    assert_eq!(record["path"], "/d/x.pbf");
    assert_eq!(record["osc"], "/d/1.osc");
    assert_eq!(record["osc_key"], "1");
    assert_eq!(record["top"], true);
    assert_eq!(record["fresh"], true, "each config() is a fresh copy");
    assert_eq!(host.calls, [vec![Word::Handle(0)]]);

    // A key that looks like brokkr's is only data: no table in config()
    // is a handle, whatever it holds.
    let lookalike = Fake {
        config: Some(Value::Table(vec![(HANDLE_MARK.into(), Value::Number(0.0))])),
        ..Fake::default()
    };
    let (sample, host) = run("function bench() binary(config()) end", lookalike);
    assert_eq!(sample.outcome, SampleOutcome::Completed(None), "{sample:?}");
    assert_eq!(
        host.calls,
        [vec![
            Word::Text(format!("--{HANDLE_MARK}")),
            Word::Text("0".into())
        ]]
    );
}

fn reason(sample: &Sample) -> &str {
    match &sample.outcome {
        SampleOutcome::Failed(why) => why,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn a_valid_chunk_compiles_and_runs_nothing() {
    // `error()` at top level would fail if the chunk ran.
    assert_eq!(
        check(b"function bench()\n  return nil\nend\nerror('ran')\n", None),
        Ok(NameCheck::default())
    );
}

#[test]
fn a_syntax_error_names_its_line() {
    let err = check(b"function bench()\n  return (\nend\n", None).unwrap_err();
    assert_eq!(err.line, 3, "{err}");
    assert!(!err.message.is_empty());
}

#[test]
fn a_misspelt_builtin_is_a_problem_at_its_line() {
    let checked = check(b"function bench()\n  return binray('x')\nend\n", None).unwrap();
    assert_eq!(checked.problems.len(), 1, "{checked:?}");
    assert_eq!(checked.problems[0].line, 2);
    assert!(
        checked.problems[0]
            .message
            .starts_with("binray is not defined")
    );
}

/// The config file's lines: TOML whose script starts on line 3.
fn mapped_lines(name: &str) -> crate::bench::script_lines::LineMap {
    let dir = crate::test_scratch::scratch("bench_lua", name);
    let file = dir.join("brokkr.toml");
    let source = "function bench()\n  local x = nil\n  x()\nend\n";
    std::fs::write(
        &file,
        format!("[bench.cat]\nscript = \"\"\"\n{source}\"\"\"\n"),
    )
    .unwrap();
    crate::bench::script_lines::locate(&file, "cat", source).unwrap()
}

/// A library of `modules` (name with `/`, source) in its own directory.
fn library(name: &str, modules: &[(&str, &str)]) -> Arc<crate::bench::library::Library> {
    let dir = crate::test_scratch::scratch("bench_lua_lib", name);
    for (module, source) in modules {
        let path = dir.join(format!("{module}.lua"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    Arc::new(crate::bench::library::Library::read(&dir).unwrap())
}

fn run_library(
    source: &str,
    library: &Arc<crate::bench::library::Library>,
    host: Fake,
) -> (Sample, Fake) {
    let script = load_script(source.as_bytes(), Some("test.lua"), None)
        .unwrap()
        .with_library(Arc::clone(library));
    run_sample(&script, &Args::default(), host)
}

/// `require` loads a module once per State and hands back the same export
/// every time; a nested module runs in the phase that required it; every
/// name asked for is recorded, a failed one too.
#[test]
fn require_loads_a_module_once_and_records_what_it_asked_for() {
    let lib = library(
        "loads",
        &[
            (
                "count",
                "local M = { n = 0 }\nfunction M.bump() M.n = M.n + 1 return M.n end\nreturn M",
            ),
            ("io/uring", "local c = require('count')\nreturn { bump = c.bump }"),
            ("launches", "binary('x')\nreturn {}"),
        ],
    );
    let (sample, _) = run_library(
        "local count = require('count')\n\
         function bench()\n\
           require('io.uring').bump()\n\
           record.n = count.bump()\n\
           record.same = require('count') == count\n\
         end",
        &lib,
        Fake::default(),
    );
    let found = completed(&sample);
    assert_eq!(found["n"], 2, "one module, one state");
    assert_eq!(found["same"], true);
    assert_eq!(
        sample.modules.iter().cloned().collect::<Vec<_>>(),
        ["count", "io.uring"]
    );

    let (sample, _) = run_library(
        "require('launches')\nfunction bench() end",
        &lib,
        Fake::default(),
    );
    let why = reason(&sample);
    assert!(why.contains("not at the script's top level"), "{why}");
    assert!(sample.modules.contains("launches"), "a failed load is recorded");
    let (sample, host) = run_library(
        "function bench() require('launches') end",
        &lib,
        Fake::default(),
    );
    assert!(matches!(sample.outcome, SampleOutcome::Completed(_)), "{sample:?}");
    assert_eq!(host.calls.len(), 1, "inside bench() a module may launch");
}

/// A cycle, a missing module, one that does not compile, one that returns
/// anything but one non-nil value, a malformed name and no library at all
/// are each refused, naming why; a module's own error comes back as it is,
/// at the module's own line even under the script's line map.
#[test]
fn require_refuses_what_is_not_one_clean_export() {
    let lib = library(
        "refuses",
        &[
            ("a", "return require('b')"),
            ("b", "return require('a')"),
            ("broken", "return ("),
            ("none", "local x = 1"),
            ("two", "return 1, 2"),
            ("raises", "local t = nil\nlocal y = t.field\nreturn {}"),
        ],
    );
    for (name, needle) in [
        ("a", "a cycle, a -> b -> a"),
        ("absent", "holds no module absent"),
        ("broken", "does not compile"),
        ("none", "returned 0 values"),
        ("two", "returned 2 values"),
        ("../x", "a module name is dot-separated"),
    ] {
        let (sample, _) = run_library(
            &format!("function bench() require('{name}') end"),
            &lib,
            Fake::default(),
        );
        let why = reason(&sample);
        assert!(why.contains(needle), "{name}: {why}");
    }
    let (sample, _) = run(
        "function bench() require('a') end",
        Fake::default(),
    );
    assert!(reason(&sample).contains("no bench_lib is configured"));

    let lines = mapped_lines("module-lines");
    let source = "function bench()\n  local x = nil\n  require('raises')\nend\n";
    let script = load_script(source.as_bytes(), Some("brokkr.toml"), Some(lines))
        .unwrap()
        .with_library(Arc::clone(&lib));
    let (sample, _) = run_sample(&script, &Args::default(), Fake::default());
    let why = reason(&sample);
    assert!(why.contains("raises.lua:2:"), "the module's own line: {why}");
    assert!(why.contains("brokkr.toml:5:"), "the script's mapped line: {why}");
}

/// A module defines no global and reads no `_G` or unknown name; the
/// builtins and dellingr's library are its to read.
#[test]
fn a_module_is_checked_against_the_builtins_alone() {
    let problems = check_module(
        b"helper = 1\nlocal x = _G\nlocal y = undefined_thing\nlocal z = string.len(fail and 'x')\nreturn {}",
        "m.lua",
    )
    .unwrap();
    let found: Vec<(u32, &str)> = problems
        .iter()
        .map(|p| (p.line, p.message.as_str()))
        .collect();
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(found[0].0 == 1 && found[0].1.contains("assigns the global helper"));
    assert!(found[1].0 == 2 && found[1].1.contains("reads _G"));
    assert!(found[2].0 == 3 && found[2].1.contains("undefined_thing is not defined"));
    assert!(check_module(b"return (", "m.lua").is_err());
    assert!(check_module(b"local M = {}\nfunction M.f() return require('x') end\nreturn M", "m.lua")
        .unwrap()
        .is_empty());
}

/// With a map, compile errors, a Lua error's traceback and a builtin's
/// failure all name the config file's lines.
#[test]
fn errors_name_the_config_files_lines() {
    let lines = mapped_lines("lines");
    let err = check(b"function bench()\n  return (\nend\n", Some(&lines)).unwrap_err();
    assert_eq!(err.line, 5, "{err}");

    let source = "function bench()\n  local x = nil\n  x()\nend\n";
    let script = load_script(source.as_bytes(), Some("brokkr.toml"), Some(lines.clone())).unwrap();
    let (sample, _) = run_sample(&script, &Args::default(), Fake::default());
    let why = reason(&sample);
    assert!(why.starts_with("brokkr.toml:5:"), "{why}");
    assert!(why.contains("\n\tbrokkr.toml:5: in "), "{why}");

    let source = "function bench()\n  local x = 1\n  fail('no good')\nend\n";
    let script = load_script(source.as_bytes(), Some("brokkr.toml"), Some(lines)).unwrap();
    let (sample, _) = run_sample(&script, &Args::default(), Fake::default());
    assert_eq!(reason(&sample), "brokkr.toml:5: no good");
}

#[test]
fn verdicts_are_nil_or_strings_and_the_block_is_timed() {
    let (sample, host) = run("function bench() end", Fake::default());
    assert_eq!(sample.outcome, SampleOutcome::Completed(None));
    assert!(sample.wall.is_some() && sample.record == Some(serde_json::Map::new()));
    assert_eq!(host.blocks, 1);

    let (sample, _) = run("function bench() return 'differs' end", Fake::default());
    assert_eq!(
        sample.outcome,
        SampleOutcome::Completed(Some("differs".into()))
    );
    let (sample, _) = run("function bench() return '' end", Fake::default());
    assert_eq!(
        sample.outcome,
        SampleOutcome::Completed(Some(String::new()))
    );

    let (sample, _) = run("function bench() return 3 end", Fake::default());
    assert!(reason(&sample).contains("returned a number"), "{sample:?}");
    assert!(sample.wall.is_some() && sample.record.is_none());
}

#[test]
fn a_script_without_bench_or_failing_at_top_level_never_starts_the_block() {
    let (sample, host) = run("x = 1", Fake::default());
    assert!(reason(&sample).contains("no bench() function"));
    assert_eq!((sample.wall, host.blocks), (None, 0));

    let (sample, _) = run("error('broken')\nfunction bench() end", Fake::default());
    assert!(reason(&sample).contains("broken"), "{sample:?}");
    assert_eq!(sample.wall, None);
}

#[test]
fn resource_builtins_raise_at_top_level_and_since_does_not() {
    let (sample, host) = run(
        "local _ = binary('x')\nfunction bench() end",
        Fake::default(),
    );
    assert!(
        reason(&sample).contains("only available inside bench()"),
        "{sample:?}"
    );
    assert!(host.calls.is_empty());

    let (sample, _) = run(
        "local o = output('x')\nfunction bench() end",
        Fake::default(),
    );
    assert!(reason(&sample).contains("only available inside bench()"));

    let (sample, host) = run(
        "NEW = since('old')\nfunction bench() if NEW then return 'new' end end",
        Fake::default(),
    );
    assert_eq!(sample.outcome, SampleOutcome::Completed(Some("new".into())));
    assert_eq!(host.since, ["old"]);

    let (sample, _) = run("since('bad')\nfunction bench() end", Fake::default());
    assert!(reason(&sample).contains("no such commit"), "{sample:?}");
}

#[test]
fn arguments_and_flag_tables_become_argv_words() {
    let (sample, host) = run(
        "function bench()\n\
           local out = output('o.pbf')\n\
           binary('cat', 4, 2.5, out, {jobs = 4, v = true, quiet = false, osc = {'1', '2'}, output = out})\n\
         end",
        Fake::default(),
    );
    assert_eq!(sample.outcome, SampleOutcome::Completed(None), "{sample:?}");
    let text = |s: &str| Word::Text(s.into());
    assert_eq!(
        host.calls,
        vec![vec![
            text("cat"),
            text("4"),
            text("2.5"),
            Word::Handle(7),
            text("--jobs"),
            text("4"),
            text("--osc"),
            text("1"),
            text("--osc"),
            text("2"),
            text("--output"),
            Word::Handle(7),
            text("-v"),
        ]]
    );

    for source in [
        "function bench() binary('x', {osc = {first = 'a'}}) end",
        "function bench() binary('x', {osc = {'a', extra = 'b'}}) end",
        "function bench() binary('x', {osc = {[1] = 'a', [3] = 'c'}}) end",
    ] {
        let (sample, host) = run(source, Fake::default());
        assert!(reason(&sample).contains("list"), "{source}: {sample:?}");
        assert!(host.calls.is_empty(), "refused before the call");
    }
    let (sample, _) = run("function bench() binary({[1] = 'x'}) end", Fake::default());
    assert!(reason(&sample).contains("flag names"), "{sample:?}");
    let (sample, _) = run("function bench() binary(nil) end", Fake::default());
    assert!(reason(&sample).contains("is a nil"), "{sample:?}");
}

#[test]
fn the_result_carries_the_ending_and_ok_raises_unless_it_exited_zero() {
    let host = Fake {
        exit: Some(0),
        stdout: vec![0xff, b'a'],
        ..Fake::default()
    };
    let (sample, _) = run(
        "function bench()\n\
           local r = binary('x'):ok()\n\
           record.len = #r.stdout\n\
           record.exit = r.exit\n\
           record.cut = r.stdout_truncated\n\
         end",
        host,
    );
    assert_eq!(sample.outcome, SampleOutcome::Completed(None), "{sample:?}");
    let record = sample.record.unwrap();
    assert_eq!(record["len"], 2, "bytes cross the seam exactly");
    assert_eq!(
        (record["exit"].clone(), record["cut"].clone()),
        (0.into(), false.into())
    );

    let host = Fake {
        exit: Some(2),
        ..Fake::default()
    };
    let (sample, _) = run("function bench() binary('x'):ok() end", host);
    assert_eq!(reason(&sample), "test.lua:1: tool exited 2: last words");
}

#[test]
fn fail_always_carries_a_reason() {
    let (sample, _) = run("function bench() fail('no good') end", Fake::default());
    assert_eq!(reason(&sample), "test.lua:1: no good");
    let (sample, _) = run("function bench() fail('') end", Fake::default());
    assert_eq!(
        reason(&sample),
        "test.lua:1: fail() called without a reason"
    );
    let (sample, _) = run("function bench()\n\n fail() end", Fake::default());
    assert_eq!(
        reason(&sample),
        "test.lua:3: fail() called without a reason"
    );
}

#[test]
fn record_takes_the_shape_the_store_admits() {
    let (sample, _) = run(
        "function bench() record.a = {1, 2, {x = true}}; record.s = 'v'; record.f = 0.5 end",
        Fake::default(),
    );
    assert_eq!(
        serde_json::Value::Object(sample.record.unwrap()),
        serde_json::json!({"a": [1, 2, {"x": true}], "s": "v", "f": 0.5})
    );

    let refused = |source: &str, needle: &str| {
        let (sample, _) = run(source, Fake::default());
        assert!(reason(&sample).contains(needle), "{source}: {sample:?}");
        assert!(sample.record.is_none() && sample.wall.is_some());
    };
    refused("function bench() record = {1, 2} end", "not a list");
    refused("function bench() record = 3 end", "must stay a table");
    refused(
        "function bench() record.self = record end",
        "refers to itself",
    );
    refused(
        "function bench() local t = record\n for i = 1, 70 do t.n = {}; t = t.n end end",
        "deeper than 64",
    );
    refused(
        "function bench() record.f = function() end end",
        "holds a function",
    );
    refused("function bench() record.x = 1/0 end", "JSON cannot");
    refused("function bench() record.l = {1, nil, 3} end", "gaps");
    refused("function bench() record.m = {1, a = 2} end", "mixes");

    // 63 tables below the root is the deepest the store takes.
    let (sample, _) = run(
        "function bench() local t = record\n for i = 1, 63 do t.n = {}; t = t.n end end",
        Fake::default(),
    );
    assert!(sample.record.is_some(), "{sample:?}");
}

#[test]
fn a_fresh_state_each_sample_forgets_the_last_one() {
    let script = load_script(
        b"function bench() COUNT = (COUNT or 0) + 1; record.n = COUNT end",
        None,
        None,
    )
    .unwrap();
    let (first, host) = run_sample(&script, &Args::default(), Fake::default());
    let (second, _) = run_sample(&script, &Args::default(), host);
    assert_eq!(first.record, second.record);
    assert_eq!(second.record.unwrap()["n"], 1);
}

/// `binary_with`'s options reach the host as handles and sorted env; the
/// rest of its arguments are `binary()`'s grammar.
#[test]
fn binary_with_takes_options_then_binarys_arguments() {
    let (sample, host) = run(
        "function bench()\n\
           local out = output('o')\n\
           local r = binary('x')\n\
           binary_with({env = {B = 2, A = 'one'}, reads = {r.stdout_file}, writes = {out},\n\
                        stdin = r.stdout_file, program = 'lua5.4'}, 'cat', {env = 'flag'})\n\
         end",
        Fake::default(),
    );
    assert_eq!(sample.outcome, SampleOutcome::Completed(None), "{sample:?}");
    let text = |s: &str| Word::Text(s.into());
    assert_eq!(host.calls[1], [text("cat"), text("--env"), text("flag")]);
    assert_eq!(
        host.options[1],
        CallOptions {
            env: vec![("A".into(), "one".into()), ("B".into(), "2".into())],
            reads: vec![9],
            writes: vec![7],
            stdin: Some(9),
            hotpath: None,
            stop_at: None,
            program: Some("lua5.4".into()),
        }
    );
    assert_eq!(host.options[0], CallOptions::default());

    for (options, needle) in [
        ("{wrtes = {}}", "unknown option \"wrtes\""),
        ("{reads = {'/a/path'}}", "a path string declares nothing"),
        ("{reads = output('o')}", "list of handles"),
        ("{stdin = '/x'}", "expected a handle"),
        ("{env = {BROKKR_MARKER_FIFO = 'x'}}", "is brokkr's"),
        ("{env = {['1X'] = 'x'}}", "is not [A-Za-z_]"),
        ("{env = {X = 1/0}}", "finite number"),
        ("{env = {X = 'a\\0b'}}", "NUL"),
        ("'x'", "expected a table of options"),
        ("{hotpath = true}", "expected a stage name"),
        ("{stop_at = 'SETUP'}", "not an exact span-end marker"),
        ("{stop_at = '-SETUP'}", "not an exact span-end marker"),
        ("{stop_at = '_END'}", "not an exact span-end marker"),
        ("{stop_at = '@SETUP_END'}", "not an exact span-end marker"),
        ("{stop_at = '%SETUP_END'}", "not an exact span-end marker"),
        ("{stop_at = 1}", "expected a marker name"),
        ("{program = 1}", "expected a program name or a path"),
        ("{program = ''}", "empty or holds a NUL"),
        (
            "{stop_at = 'SETUP_END', hotpath = 's'}",
            "hotpath and stop_at exclude each other",
        ),
        ("{hotpath = 'Decode'}", "is not a name"),
        (
            "{hotpath = 'decode', env = {HOTPATH_OUTPUT_PATH = 'x'}}",
            "beside hotpath",
        ),
    ] {
        let (sample, host) = run(
            &format!("function bench() binary_with({options}, 'x') end"),
            Fake::default(),
        );
        assert!(reason(&sample).contains(needle), "{options}: {sample:?}");
        assert!(host.calls.is_empty(), "{options}: refused before the call");
    }
}

/// `hotpath = STAGE` reaches the host, and the capture comes back as the
/// result's `hotpath_status` and `hotpath` table; a plain call has neither.
#[test]
fn binary_with_hotpath_hands_back_the_capture() {
    let (sample, host) = run(
        "function bench()\n\
           local r = binary_with({hotpath = 'decode', env = {OTHER = 'x'}}, 'x')\n\
           record.calls = r.hotpath.functions_timing.calls\n\
           record.status = r.hotpath_status\n\
           local p = binary('y')\n\
           record.plain = p.hotpath == nil and p.hotpath_status == nil\n\
         end",
        Fake::default(),
    );
    let record = completed(&sample);
    assert_eq!(record["calls"], 3);
    assert_eq!(record["status"], "captured");
    assert_eq!(record["plain"], true);
    assert_eq!(host.options[0].hotpath.as_deref(), Some("decode"));
}

/// `stop_at` reaches the host; a child stopped at its marker passes
/// `r:ok()` and carries `stopped_at` with no exit.
#[test]
fn a_child_stopped_at_its_marker_is_ok() {
    let host = Fake {
        stopped_at: Some("SETUP_END".into()),
        ..Fake::default()
    };
    let (sample, host) = run(
        "function bench()\n\
           local r = binary_with({stop_at = 'SETUP_END'}, 'x'):ok()\n\
           record.at = r.stopped_at\n\
           record.exit = r.exit == nil\n\
         end",
        host,
    );
    let record = completed(&sample);
    assert_eq!(record["at"], "SETUP_END");
    assert_eq!(record["exit"], true);
    assert_eq!(host.options[0].stop_at.as_deref(), Some("SETUP_END"));
}

/// read, write and filesize take handles only, inside bench() only.
#[test]
fn file_builtins_take_handles_inside_bench() {
    let (sample, host) = run(
        "function bench()\n\
           local out = output('o')\n\
           write(out, 'payload')\n\
           record.back = read(out)\n\
           record.size = filesize(out)\n\
         end",
        Fake::default(),
    );
    let record = completed(&sample);
    assert_eq!(record["back"], "payload");
    assert_eq!(record["size"], 7);
    assert_eq!(host.files[&7], b"payload");

    for source in [
        "local x = read\nx({})\nfunction bench() end",
        "function bench() read('/etc/passwd') end",
        "function bench() write(output('o'), 3) end",
        "function bench() filesize() end",
    ] {
        let (sample, _) = run(source, Fake::default());
        assert!(
            matches!(sample.outcome, SampleOutcome::Failed(_)),
            "{source}: {sample:?}"
        );
    }
    let (sample, _) = run("x = filesize({})\nfunction bench() end", Fake::default());
    assert!(
        reason(&sample).contains("only available inside bench()"),
        "{sample:?}"
    );
}

/// JSON round-trips arrays, empty arrays and nulls, is allowed at top
/// level, and recognises brokkr's own values by identity, not by keys.
#[test]
fn json_keeps_arrays_and_nulls_and_data_stays_data() {
    let (sample, _) = run(
        "TOP = json.encode({b = {}, a = 1})\n\
         function bench()\n\
           local v = json.decode('{\"x\":[],\"y\":[1,null,3],\"z\":null,\"__brokkr_handle\":0}')\n\
           record.top = TOP\n\
           record.again = json.encode(v)\n\
           record.null = v.z == json.null\n\
           record.second = v.y[2] == json.null\n\
           record.len = #v.y\n\
           record.decoded = v\n\
           record.meta = getmetatable(v.x)\n\
           record.list = json.encode({1, 2})\n\
         end",
        Fake::default(),
    );
    let record = completed(&sample);
    assert_eq!(record["top"], r#"{"a":1,"b":{}}"#);
    assert_eq!(
        record["again"],
        r#"{"__brokkr_handle":0,"x":[],"y":[1,null,3],"z":null}"#
    );
    assert_eq!(
        (record["null"].clone(), record["second"].clone()),
        (true.into(), true.into())
    );
    assert_eq!(record["len"], 3);
    assert_eq!(
        record["decoded"],
        serde_json::json!({"__brokkr_handle": 0, "x": [], "y": [1, null, 3], "z": null})
    );
    assert_eq!(record["meta"], false, "brokkr's metatable is protected");
    assert_eq!(record["list"], "[1,2]");

    let refused = |source: &str, needle: &str| {
        let (sample, _) = run(source, Fake::default());
        assert!(reason(&sample).contains(needle), "{source}: {sample:?}");
    };
    refused(
        "function bench() json.decode('{\"a\":1,\"a\":2}') end",
        "given twice",
    );
    refused("function bench() json.decode('[1') end", "json.decode()");
    refused("function bench() json.encode(output('o')) end", "h.path");
    refused(
        "function bench() rawset(json.null, 'x', 1); json.encode({json.null}) end",
        "fields set",
    );
    refused(
        "function bench() local a = json.decode('[]'); a.k = 1; json.encode(a) end",
        "string keys in a JSON array",
    );
    refused(
        "function bench() setmetatable(json.decode('[]'), {}) end",
        "protected",
    );
    refused(
        "function bench() record = json.decode('[]') end",
        "not a list",
    );
    refused("function bench() record = json.null end", "not json.null");

    // Replacing json.null changes what the script compares against, not
    // what decode produces.
    let (sample, _) = run(
        "function bench()\n\
           local real = json.null\n\
           json.null = {}\n\
           record.same = json.decode('null') == real\n\
         end",
        Fake::default(),
    );
    assert_eq!(completed(&sample)["same"], true);
}

/// json.null is a value, not a container: it takes no nesting level.
#[test]
fn a_null_at_the_deepest_level_round_trips() {
    let deep = format!("{}null{}", "[".repeat(64), "]".repeat(64));
    let (sample, _) = run(
        &format!("function bench() record.s = json.encode(json.decode('{deep}')) end"),
        Fake::default(),
    );
    assert_eq!(completed(&sample)["s"], deep);
}

/// A handle's table is recognised by its protected metatable alone.
#[test]
fn a_handle_is_its_metatable_not_its_keys() {
    let (sample, host) = run(
        "function bench()\n\
           local out = output('o')\n\
           record.path = out.path\n\
           record.meta = getmetatable(out)\n\
           binary(out, {output = out}, {__brokkr_handle = 7})\n\
         end",
        Fake::default(),
    );
    let record = completed(&sample);
    assert_eq!(record["path"], "/it/out/o");
    assert_eq!(record["meta"], false);
    assert_eq!(
        host.calls[0],
        [
            Word::Handle(7),
            Word::Text("--output".into()),
            Word::Handle(7),
            Word::Text("--__brokkr_handle".into()),
            Word::Text("7".into()),
        ]
    );
    let (sample, _) = run(
        "function bench() record.h = output('o') end",
        Fake::default(),
    );
    assert!(reason(&sample).contains("h.path"), "{sample:?}");
}

/// Every builtin but print asks for a stop before its arguments.
#[test]
fn every_builtin_but_print_meets_a_pending_stop() {
    for call in [
        "binary(nil)",
        "binary_with(nil)",
        "since(nil)",
        "fail()",
        "output(nil)",
        "scratch(nil)",
        "config()",
        "read(nil)",
        "write(nil)",
        "filesize(nil)",
        "json.encode(nil)",
        "json.decode(nil)",
    ] {
        let host = Fake {
            stop_now: true,
            ..Fake::default()
        };
        let (sample, _) = run(&format!("function bench() {call} end"), host);
        assert_eq!(sample.outcome, SampleOutcome::Interrupted, "{call}");
    }
    // r:ok() on a call made before the stop: the stop meets ok() first, so
    // the failing exit is never reported.
    let host = Fake {
        exit: Some(2),
        stop_from: Some(2),
        ..Fake::default()
    };
    let (sample, host) = run("function bench() binary('x'):ok() end", host);
    assert_eq!(sample.outcome, SampleOutcome::Interrupted);
    assert_eq!((host.calls.len(), host.stops), (1, 2));
    let host = Fake {
        stop_now: true,
        ..Fake::default()
    };
    let (sample, host) = run("function bench() print('x') end", host);
    assert_eq!(sample.outcome, SampleOutcome::Completed(None));
    assert_eq!(host.stops, 0);
}

/// A host whose watcher probe always reports a pending stop, while its
/// builtins never do: only the State's interrupt can end a spin.
struct Spinner;

impl Host for Spinner {
    fn stop(&mut self) -> Result<(), Abort> {
        Ok(())
    }
    fn stop_requested() -> bool {
        true
    }
    fn begin_block(&mut self, _at: Instant) {}
    fn binary(&mut self, _: Vec<Word>, _: CallOptions) -> Result<CallOutcome, Abort> {
        Err(Abort::Failed("no binary".into()))
    }
    fn since(&mut self, _: &str) -> Result<bool, Abort> {
        Ok(false)
    }
    fn output(&mut self, _: &str) -> Result<HandleRef, Abort> {
        Err(Abort::Failed("no output".into()))
    }
    fn scratch(&mut self, _: &str) -> Result<HandleRef, Abort> {
        Err(Abort::Failed("no scratch".into()))
    }
    fn config(&mut self) -> Result<std::sync::Arc<Value>, Abort> {
        Ok(std::sync::Arc::new(Value::Table(Vec::new())))
    }
    fn read(&mut self, _: usize) -> Result<Vec<u8>, Abort> {
        Err(Abort::Failed("no read".into()))
    }
    fn write(&mut self, _: usize, _: &[u8]) -> Result<(), Abort> {
        Ok(())
    }
    fn filesize(&mut self, _: usize) -> Result<u64, Abort> {
        Ok(0)
    }
}

/// A script spinning in pure Lua, calling no builtin, is interrupted, at
/// the top level and inside `bench()`, in a loop and in endless tail
/// recursion, which has no backward jump.
#[test]
fn a_stop_interrupts_a_script_spinning_in_pure_lua() {
    for source in [
        "while true do end function bench() end",
        "function bench() while true do end end",
        "local function f() return f() end function bench() return f() end",
    ] {
        let script = load_script(source.as_bytes(), Some("test.lua"), None).unwrap();
        let (sample, _) = run_sample(&script, &Args::default(), Spinner);
        assert_eq!(sample.outcome, SampleOutcome::Interrupted, "{source}");
    }
}
