#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

const CONFIG: &str = r#"project = "p"
bench_tests = "tests"

[bench.cat]
script_file = "cat.lua"
target = "tool"
params.dataset = { from = "datasets", default = "denmark" }
params.variant = { from = "datasets.{dataset}.pbf", default = "indexed" }
params.mode = { validated = true, default = "fast" }
params.stop = { free = true }
"#;

const SCRIPT: &str = r#"
function validate(params)
  if params.mode == "bad" then fail("mode bad is not one the bench runs") end
end
function bench(params, keys)
  local out = output("counts.json")
  local options = {}
  if params.stop then options.stop_at = params.stop end
  local r = binary_with(options, "cat", {mode = params.mode}, params.variant.file, out):ok()
  if r.stopped_at then return "stopped at " .. r.stopped_at end
  record.size = filesize(params.variant.file)
  record.n = json.decode(read(out)).n
  record.key = keys.variant
  record.said = r.stdout
  if record.n ~= 3 then fail("expected 3 counts, saw " .. record.n) end
end
"#;

/// A project with one bench and a test file `source`, and every finding its
/// run of the test file makes.
fn findings(name: &str, source: &str) -> Vec<String> {
    let dir = std::fs::canonicalize(crate::test_scratch::scratch("bench_contract", name)).unwrap();
    std::fs::write(dir.join("brokkr.toml"), CONFIG).unwrap();
    std::fs::write(dir.join("cat.lua"), SCRIPT).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("tests/cat.lua"), source).unwrap();
    let (_, config, _, sources) = crate::config::load_with_sources(&dir).unwrap();
    assert_eq!(
        config.bench_tests.as_deref(),
        Some(dir.join("tests").as_path())
    );
    let (files, found) = crate::bench::discover_tests(&dir.join("tests"));
    assert!(found.is_empty(), "{found:?}");
    let context = Context {
        benches: config.benches.clone(),
        composed: sources.composed().clone(),
        project_root: dir.clone(),
        library: None,
    };
    run_files(context, &files)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect()
}

const SPEC: &str = r#"
local function spec(extra)
  local s = {
    bench = "cat",
    host = { datasets = { denmark = { pbf = { indexed = { file = "denmark.pbf" } } } } },
    calls = { { stdout = "ok\n", outputs = { ["counts.json"] = "{\"n\":3}" } } },
    files = { [project_root .. "/data/denmark.pbf"] = "PBFDATA" },
  }
  for k, v in pairs(extra or {}) do s[k] = v end
  return s
end
"#;

/// A passing run: the real binder and view, argv with provenance, the
/// child's outputs read back, the record and verdict handed to the test.
#[test]
fn a_test_drives_the_real_script_through_bind_validate_and_bench() {
    let source = format!(
        "{SPEC}
test(\"completes\", function()
  local r = run(spec())
  assert(r.outcome == \"completed\", r.reason)
  assert(r.stage == \"bench\")
  local c = r.calls[1]
  assert(c.argv[1] == \"cat\" and c.argv[2] == \"--mode\" and c.argv[3] == \"fast\")
  assert(c.argv[4] == project_root .. \"/data/denmark.pbf\", c.argv[4])
  assert(c.origins[1] == \"text\" and c.origins[4] == \"input\" and c.origins[5] == \"output\")
  assert(c.stop_at == nil and c.measured == false)
  assert(r.record.n == 3 and r.record.size == 7 and r.record.key == \"indexed\")
  assert(r.record.said == \"ok\\n\")
  assert(r.verdict == nil)
end)
test(\"stops where it asked to\", function()
  local s = spec({{params = {{\"--stop\", \"LOADED\"}}}})
  s.calls = {{ {{ stopped_at = \"LOADED\" }} }}
  local r = run(s)
  assert(r.verdict == \"stopped at LOADED\", r.reason)
  assert(r.calls[1].stop_at == \"LOADED\")
end)
test(\"a stop that never fires fails\", function()
  local s = spec({{params = {{\"--stop\", \"LOADED\"}}}})
  s.calls = {{ {{ exit = 0 }} }}
  local r = run(s)
  assert(r.outcome == \"failed\", r.outcome)
  assert(string.find(r.reason, \"exited 0 before its stop_at marker LOADED\", 1, true), r.reason)
end)
test(\"a wrong count is the script's failure\", function()
  local s = spec()
  s.calls[1].outputs[\"counts.json\"] = \"{{\\\"n\\\":2}}\"
  local r = run(s)
  assert(r.outcome == \"failed\")
  assert(string.find(r.reason, \"expected 3 counts, saw 2\"), r.reason)
  assert(r.record == nil)
end)
test(\"validate refuses before anything runs\", function()
  local s = spec({{params = {{\"--mode\", \"bad\"}}}})
  s.calls = {{}}
  local r = run(s)
  assert(r.stage == \"validate\" and r.outcome == \"refused\", r.stage)
  assert(#r.calls == 0)
end)
test(\"a bind refusal is a result\", function()
  local s = spec({{params = {{\"--variant\", \"missing\"}}}})
  s.calls = {{}}
  local r = run(s)
  assert(r.stage == \"bind\" and r.outcome == \"refused\", r.stage)
end)
"
    );
    let found = findings("passing", &source);
    assert!(found.is_empty(), "{found:#?}");
}

/// A fixture the script contradicts fails the test case, never the
/// script: each of these is a finding naming its case.
#[test]
fn fixture_faults_fail_the_case_and_the_file_goes_on() {
    let source = format!(
        "{SPEC}
test(\"misspelt\", function() run(spec({{cals = {{}}}})) end)
test(\"unasked\", function()
  local s = spec()
  s.calls[2] = {{ exit = 0 }}
  run(s)
end)
test(\"unrequested stop\", function()
  local s = spec()
  s.calls = {{ {{ stopped_at = \"LOADED\" }} }}
  run(s)
end)
test(\"no contents\", function()
  local s = spec()
  s.files = {{}}
  run(s)
end)
test(\"no such bench\", function() run(spec({{bench = \"dog\"}})) end)
test(\"bad host\", function() run(spec({{host = {{ datasets = 3 }}}})) end)
test(\"pre-made output\", function()
  local s = spec()
  s.files[\"/brokkr-contract/iteration/out/counts.json\"] = \"x\"
  run(s)
end)
test(\"output path\", function()
  local s = spec()
  s.calls[1].outputs = {{ [\"/brokkr-contract/iteration/scratch/s\"] = \"x\" }}
  run(s)
end)
test(\"passes\", function() assert(run(spec()).outcome == \"completed\") end)
"
    );
    let found = findings("faults", &source);
    let expect = [
        ("misspelt", "has no key cals"),
        ("unasked", "every declared response must be asked for"),
        ("unrequested stop", "the call asked to stop at no marker"),
        ("no contents", "gives no contents for it"),
        ("no such bench", "no bench named dog; declared: cat"),
        ("bad host", "the spec's host"),
        ("pre-made output", "which is no input this arm binds"),
        ("output path", "which is no name output() takes"),
    ];
    assert_eq!(found.len(), expect.len(), "{found:#?}");
    for ((case, says), finding) in expect.iter().zip(&found) {
        assert!(
            finding.starts_with("bench_tests ") && finding.contains(&format!("test {case:?}")),
            "{finding}"
        );
        assert!(finding.contains(says), "{case}: {finding}");
    }
}

/// A file that declares no case, or fails outside one, is a finding; a
/// case inside a case is refused.
#[test]
fn a_file_without_cases_or_failing_outside_them_is_a_finding() {
    let found = findings("empty", "local x = 1\n");
    assert!(
        found.len() == 1 && found[0].contains("declares no test case"),
        "{found:#?}"
    );
    let found = findings("outside", "error(\"setup broke\")\n");
    assert!(
        found.len() == 1 && found[0].contains("setup broke"),
        "{found:#?}"
    );
    let found = findings(
        "nested",
        "test(\"outer\", function() test(\"inner\", function() end) end)\n",
    );
    assert!(
        found.len() == 1 && found[0].contains("inside another case"),
        "{found:#?}"
    );
    let found = findings("builtins", "test(\"x\", function() binary(\"cat\") end)\n");
    assert!(
        found.len() == 1 && found[0].contains("attempt to call a nil value"),
        "{found:#?}"
    );
}
