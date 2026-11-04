#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

/// A host that records what the script asked for and answers from fixtures.
#[derive(Default)]
struct Fake {
    calls: Vec<Vec<Word>>,
    exit: Option<i32>,
    stdout: Vec<u8>,
    since: Vec<String>,
    blocks: usize,
}

impl Host for Fake {
    fn begin_block(&mut self, _at: Instant) {
        self.blocks += 1;
    }
    fn binary(&mut self, words: Vec<Word>) -> Result<CallOutcome, Abort> {
        self.calls.push(words);
        Ok(CallOutcome {
            program: "tool".into(),
            exit: self.exit,
            signal: None,
            stdout: self.stdout.clone(),
            stdout_truncated: false,
            stderr: b"line one\nlast words\n".to_vec(),
            stderr_truncated: false,
        })
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
}

fn run(source: &str, host: Fake) -> (Sample, Fake) {
    let script = load_script(source.as_bytes(), Some("test.lua")).unwrap();
    run_sample(&script, host)
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
        compile(b"function bench()\n  return nil\nend\nerror('ran')\n"),
        Ok(())
    );
}

#[test]
fn a_syntax_error_names_its_line() {
    let err = compile(b"function bench()\n  return (\nend\n").unwrap_err();
    assert_eq!(err.line, 3, "{err}");
    assert!(!err.message.is_empty());
}

#[test]
fn a_misspelt_builtin_still_compiles() {
    assert_eq!(
        compile(b"function bench() return binray('x') end\n"),
        Ok(())
    );
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
    assert_eq!(reason(&sample), "tool exited 2: last words");
}

#[test]
fn fail_always_carries_a_reason() {
    let (sample, _) = run("function bench() fail('no good') end", Fake::default());
    assert_eq!(reason(&sample), "no good");
    let (sample, _) = run("function bench() fail('') end", Fake::default());
    assert_eq!(reason(&sample), "fail() called without a reason");
    let (sample, _) = run("function bench() fail() end", Fake::default());
    assert_eq!(reason(&sample), "fail() called without a reason");
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
    )
    .unwrap();
    let (first, host) = run_sample(&script, Fake::default());
    let (second, _) = run_sample(&script, host);
    assert_eq!(first.record, second.record);
    assert_eq!(second.record.unwrap()["n"], 1);
}
