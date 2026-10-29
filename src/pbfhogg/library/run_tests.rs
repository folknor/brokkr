#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{Places, Report, printable, verify};
use crate::pbfhogg::binary::{LibraryHarness, PbfhoggBinary};
use crate::pbfhogg::library::describe;

const DESCRIBE: &str = r#"{"protocol": 1, "scenarios": [
  {"id": "repeat-cat", "description": "cat once", "deadline_s": 60,
   "roles": [
     {"id": "input", "kind": "pbf", "source": "dataset", "variant": "indexed"},
     {"id": "out-a", "kind": "pbf", "source": "output", "owner": "a"},
     {"id": "tmp-a", "kind": "directory", "source": "scratch", "owner": "a"}
   ],
   "calls": [{"id": "a", "label": "cat input -> out-a"}],
   "assertions": [{"id": "a.outcome", "call": "a"}, {"id": "whole", "call": null}]}
]}"#;

const SCENARIO_DOC: &str = r#"{"protocol": 1, "scenario": "repeat-cat",
 "plan": {"calls": {"a": {"command": "cat", "input": "input"}}},
 "assertions": [{"id": "whole", "status": "pass", "detail": ""}],
 "diagnostics": []}"#;

const CALL_DOC: &str = r#"{"protocol": 1, "call": "a", "entry_us": 10, "return_us": 20, "seal_us": 30,
 "outcome": {"status": "ok"},
 "publications": [{"ordinal": 1, "decision": "authorized", "members": [{"role": "out-a", "effect": "installed"}]}],
 "cancel": null,
 "seal": {"state": "sealed"},
 "snapshot": {"writer_blocks": 3},
 "stream": {"state": "sealed", "drops": 0},
 "context": {"parallelism": 4},
 "diagnostics": [{"at_us": 12, "kind": "note", "key": "sort.x", "message": "hello", "truncated": false}],
 "assertions": [{"id": "a.outcome", "status": "pass", "detail": ""}]}"#;

/// The call stream a sealed call `a` of `CALL_DOC` emits: one phase marker,
/// then its sealing emission and terminal record.
const STREAM: &str = "15 %a CAT_START\n25 %a @@writer_blocks=3\n26 %a !sealed drops=0";

/// A fixture harness: checks its argv and environment, writes its output
/// role and `stream` to the FIFO, then `body` (which writes the artefacts)
/// and exits `code`.
fn script_with(stream: &str, body: &str, code: i32) -> String {
    format!(
        r#"[ "$1" = "--test-harness" ] || exit 9
[ "$3" = "--private-dir" ] && [ -d "$4" ] || exit 8
[ -d "$BROKKR_HARNESS_ARTEFACT_DIR" ] || exit 7
[ -p "$BROKKR_MARKER_FIFO" ] || exit 6
[ "$(pwd)" = "$(dirname "$2")/cwd" ] || exit 5
A="$BROKKR_HARNESS_ARTEFACT_DIR"
R=$(dirname "$2")
: > "$R/roles/out-a"
cat > "$BROKKR_MARKER_FIFO" <<'EOF'
{stream}
EOF
{body}
exit {code}"#
    )
}

fn script(body: &str, code: i32) -> String {
    script_with(STREAM, body, code)
}

/// Write the scenario and call documents given.
fn documents(scenario: &str, call: &str) -> String {
    format!(
        "mkdir \"$A/calls\"\ncat > \"$A/scenario.json\" <<'EOF'\n{scenario}\nEOF\ncat > \"$A/calls/a.json\" <<'EOF'\n{call}\nEOF"
    )
}

struct Fixture {
    root: PathBuf,
    harness: LibraryHarness,
    cli: PbfhoggBinary,
    input: PathBuf,
}

/// A fake pbfhogg CLI. `cat` (which accepts `--overwrite` and
/// `--staging-dir`, not `--tmp-dir`) runs the `cat-extra` script beside it
/// when there is one, then writes `pbf` to its `-o` path;
/// `digest FILE` prints a four-line report whose hashes are the file's MD5,
/// so equal files have equal reports. Every call is logged.
const FAKE_CLI: &str = r#"#!/bin/sh
echo "$*" >> "$0.log"
case "$1" in
  cat|digest) ;;
  *) exit 2 ;;
esac
case "$2" in
  --help) exit 0 ;;
  --hel) exit 2 ;;
  --overwrite|--staging-dir=.) exit 0 ;;
  --tmp-dir=.) exit 2 ;;
esac
if [ "$1" = digest ]; then
  if [ -x "$0.digest-extra" ]; then . "$0.digest-extra"; fi
  [ -f "$2" ] || exit 2
  h=$(md5sum < "$2" | cut -c1-32)
  printf 'xxh128 v1 %s\nnode 1 %s\nway 0 %s\nrelation 0 %s\n' "$h" "$h" "$h" "$h"
  exit 0
fi
out=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then out="$a"; fi
  prev="$a"
done
if [ -x "$0.cat-extra" ]; then . "$0.cat-extra"; fi
printf 'pbf' > "$out"
"#;

fn fixture(name: &str, body: &str) -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let root = crate::test_scratch::scratch("pbfhogg-library-run", name);
    let path = root.join("harness");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cli_path = root.join("pbfhogg");
    std::fs::write(&cli_path, FAKE_CLI).unwrap();
    std::fs::set_permissions(&cli_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let input = root.join("input.osm.pbf");
    std::fs::write(&input, b"pbf").unwrap();
    for dir in ["library", "state"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let harness = LibraryHarness::fixture(&path, &root).unwrap();
    let cli = PbfhoggBinary::fixture(&cli_path, &root).unwrap();
    Fixture {
        root,
        harness,
        cli,
        input,
    }
}

/// Give the fake CLI's `cat` an extra step, run before it writes its output.
fn cat_extra(f: &Fixture, script: &str) {
    extra(f, "cat", script);
}

/// The same for `digest`, run before it reads its file.
fn digest_extra(f: &Fixture, script: &str) {
    extra(f, "digest", script);
}

fn extra(f: &Fixture, sub: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = f.root.join(format!("pbfhogg.{sub}-extra"));
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn run(f: &Fixture) -> Report {
    run_described(f, DESCRIBE)
}

fn run_described(f: &Fixture, describe_doc: &str) -> Report {
    let scenario = describe::parse(describe_doc.as_bytes())
        .unwrap()
        .scenarios
        .remove(0);
    let datasets = BTreeMap::from([("input".to_owned(), f.input.clone())]);
    verify(
        &f.harness,
        &f.cli,
        &scenario,
        &datasets,
        &Places {
            library_root: &f.root.join("library"),
            fifo_dir: &f.root.join("state"),
        },
        None,
        &mut None,
    )
    .unwrap()
}

fn failing(name: &str, body: &str) -> Report {
    let f = fixture(name, body);
    let report = run(&f);
    assert!(!report.passed, "passed: {:?}", report.results);
    assert!(report.kept && report.run_dir.is_dir());
    report
}

fn has(report: &Report, needle: &str) -> bool {
    report.failures.iter().any(|f| f.contains(needle))
}

#[test]
fn a_passing_run_is_removed_and_reports_its_results() {
    let f = fixture("pass", &script(&documents(SCENARIO_DOC, CALL_DOC), 0));
    let report = run(&f);
    assert!(report.passed, "{:?}", report.failures);
    assert!(!report.kept && !report.run_dir.exists());
    let ids: Vec<String> = report
        .results
        .iter()
        .map(|(c, r)| format!("{}/{}", c.as_deref().unwrap_or("-"), r.id))
        .collect();
    assert_eq!(ids, ["-/whole", "a/a.outcome", "a/brokkr.seal"]);
    assert_eq!(report.diagnostics, ["a note sort.x: hello"]);
    assert_eq!(report.streams.len(), 1);
    assert!(
        report.streams[0].starts_with("a pass (not judged); phases CAT_START "),
        "{:?}",
        report.streams
    );
}

/// The describe document with call `a` claiming its stream.
fn claimed() -> String {
    DESCRIBE.replacen(
        r#""label": "cat input -> out-a"}"#,
        r#""label": "cat input -> out-a", "stream": ["counters.v1"]}"#,
        1,
    )
}

#[test]
fn a_claimed_stream_adds_brokkr_stream_and_its_verdict_judges() {
    let f = fixture(
        "stream-pass",
        &script(&documents(SCENARIO_DOC, CALL_DOC), 0),
    );
    let report = run_described(&f, &claimed());
    assert!(report.passed, "{:?}", report.failures);
    let ids: Vec<&str> = report.results.iter().map(|(_, r)| r.id.as_str()).collect();
    assert_eq!(ids, ["whole", "a.outcome", "brokkr.seal", "brokkr.stream"]);

    let wrong = STREAM.replacen("@@writer_blocks=3", "@@writer_blocks=4", 1);
    let f = fixture(
        "stream-fail",
        &script_with(&wrong, &documents(SCENARIO_DOC, CALL_DOC), 0),
    );
    let report = run_described(&f, &claimed());
    assert!(!report.passed);
    assert!(
        has(
            &report,
            "a/brokkr.stream: fail\n    | sealing emits \"writer_blocks\" = 4"
        ),
        "{:?}",
        report.failures
    );
    assert!(
        !has(&report, "exited 0"),
        "a brokkr result never makes exit 0 inconsistent"
    );

    // The same defect in a call that claims nothing is reported, not judged.
    let f = fixture(
        "stream-unclaimed",
        &script_with(&wrong, &documents(SCENARIO_DOC, CALL_DOC), 0),
    );
    let report = run(&f);
    assert!(report.passed, "{:?}", report.failures);
    assert!(report.streams[0].starts_with("a fail (not judged)"));
}

#[test]
fn a_record_of_an_undeclared_call_fails_the_run_whatever_the_exit() {
    let stream = format!("{STREAM}\n17 %other X");
    let report = failing(
        "undeclared",
        &script_with(
            &stream,
            "cat > \"$A/refusal.json\" <<'EOF'\n{\"protocol\": 1, \"reason\": \"no\"}\nEOF",
            2,
        ),
    );
    assert!(
        has(
            &report,
            "records of call \"other\", which describe does not declare"
        ),
        "{:?}",
        report.failures
    );
}

#[test]
fn the_request_binds_every_role_and_a_failed_run_keeps_its_evidence() {
    let body = format!(
        "cp \"$2\" \"$A/../../request-copy\"\n{}",
        documents(
            SCENARIO_DOC,
            &CALL_DOC.replacen(r#""status": "pass""#, r#""status": "fail""#, 1)
        )
    );
    let report = failing("fail", &script(&body, 1));
    assert!(has(&report, "a/a.outcome: fail"), "{:?}", report.failures);
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report.run_dir.join("../request-copy")).unwrap())
            .unwrap();
    assert_eq!(request["protocol"], 1);
    assert_eq!(request["iteration"], 0);
    let bindings = request["bindings"].as_object().unwrap();
    assert_eq!(bindings.len(), 3);
    assert!(
        bindings["out-a"]
            .as_str()
            .unwrap()
            .ends_with("/roles/out-a")
    );
    assert!(
        bindings["input"]
            .as_str()
            .unwrap()
            .ends_with("/input.osm.pbf")
    );
    let evidence = report.run_dir.join("evidence");
    let stream = std::fs::read_to_string(evidence.join("stream.txt")).unwrap();
    assert_eq!(stream, format!("# records, in arrival order\n{STREAM}\n"));
    assert!(
        std::fs::read_to_string(evidence.join("failures.txt"))
            .unwrap()
            .contains("fail")
    );
}

#[test]
fn an_exit_that_disagrees_with_the_results_fails() {
    let report = failing("exit1", &script(&documents(SCENARIO_DOC, CALL_DOC), 1));
    assert!(has(&report, "exited 1 but every result it reported passed"));
    let call = CALL_DOC.replacen(r#""status": "pass""#, r#""status": "unevaluated""#, 1);
    let report = failing("exit0", &script(&documents(SCENARIO_DOC, &call), 0));
    assert!(has(
        &report,
        "exited 0 but reported a result that did not pass"
    ));
    assert!(has(&report, "a/a.outcome: unevaluated"));
}

#[test]
fn any_output_fails_and_is_printed_framed() {
    let body = format!(
        "{}\nprintf 'noise\\nmore' >&2",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("stderr", &script(&body, 0));
    assert!(has(&report, "wrote to stderr"), "{:?}", report.failures);
    assert!(
        has(&report, "    | noise\n    | more"),
        "{:?}",
        report.failures
    );
}

#[test]
fn a_seal_that_did_not_complete_fails_as_brokkr_seal() {
    let call = CALL_DOC
        .replacen(
            r#""seal": {"state": "sealed"}"#,
            r#""seal": {"state": "outstanding", "held": {"session": 1, "writer": 2}}"#,
            1,
        )
        .replacen(
            r#""snapshot": {"writer_blocks": 3}"#,
            r#""snapshot": null"#,
            1,
        )
        .replacen(
            r#""state": "sealed", "drops": 0"#,
            r#""state": "open", "drops": 0"#,
            1,
        );
    let report = failing(
        "seal",
        &script_with("15 %a CAT_START", &documents(SCENARIO_DOC, &call), 0),
    );
    assert!(
        has(
            &report,
            "a/brokkr.seal: fail\n    | still held at the seal: 1 session, 2 writer"
        ),
        "{:?}",
        report.failures
    );
    assert!(
        !has(&report, "exited 0"),
        "a brokkr result never makes exit 0 inconsistent"
    );
}

#[test]
fn a_missing_call_document_and_a_misplaced_result_fail() {
    let body =
        format!("mkdir \"$A/calls\"\ncat > \"$A/scenario.json\" <<'EOF'\n{SCENARIO_DOC}\nEOF");
    let report = failing("missing", &script(&body, 0));
    assert!(
        has(&report, "calls/a.json is missing"),
        "{:?}",
        report.failures
    );

    // The scenario-level result reported in the call's document.
    let scenario = SCENARIO_DOC.replacen(
        r#"[{"id": "whole", "status": "pass", "detail": ""}]"#,
        "[]",
        1,
    );
    let call = CALL_DOC.replacen(
        r#"[{"id": "a.outcome", "status": "pass", "detail": ""}]"#,
        r#"[{"id": "a.outcome", "status": "pass", "detail": ""}, {"id": "whole", "status": "pass", "detail": ""}]"#,
        1,
    );
    let report = failing("misplaced", &script(&documents(&scenario, &call), 0));
    assert!(
        has(
            &report,
            "calls/a.json reports \"whole\", which belongs to scenario.json"
        ),
        "{:?}",
        report.failures
    );
    assert!(has(&report, "scenario.json has no result for \"whole\""));
}

#[test]
fn a_run_directory_replaced_by_a_symlink_is_never_entered() {
    let body = format!(
        "{}\nmv \"$R\" \"$R.real\"\nln -s \"$R.real\" \"$R\"",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("run-link", &script(&body, 0));
    assert!(
        has(&report, "is no longer the directory"),
        "{:?}",
        report.failures
    );
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    let real = report.run_dir.with_file_name("run-1.real");
    assert!(real.join("artefacts").is_dir());
    assert!(
        !real.join("evidence").exists(),
        "nothing was written through it"
    );
}

#[test]
fn a_directory_replaced_by_a_symlink_is_never_traversed() {
    let body = format!(
        "{}\nmkdir \"$R/../decoy\"\n: > \"$R/../decoy/planted\"\nrm -r \"$R/roles\"\nln -s \"$R/../decoy\" \"$R/roles\"",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("roles-link", &script(&body, 0));
    assert!(
        has(&report, "roles has the wrong type"),
        "{:?}",
        report.failures
    );
    assert!(
        !has(&report, "planted"),
        "the link was followed: {:?}",
        report.failures
    );

    // On exit 2, a refusal behind a symlinked artefact directory is not read.
    let body = "mkdir \"$R/../elsewhere\"\ncat > \"$R/../elsewhere/refusal.json\" <<'EOF'\n{\"protocol\": 1, \"reason\": \"planted\"}\nEOF\nrmdir \"$A\"\nln -s \"$R/../elsewhere\" \"$A\"";
    let report = failing("artefacts-link", &script(body, 2));
    assert!(
        has(&report, "artefact directory is not a directory"),
        "{:?}",
        report.failures
    );
    assert!(!has(&report, "planted"), "{:?}", report.failures);
}

#[test]
fn the_layout_check_catches_what_survived_in_the_run_directory() {
    let body = format!(
        "{}\n: > cwd-leftover\n: > \"$R/stray\"\nmkdir \"$R/roles/out-a.tmp\"\necho x >> \"$2\"",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("layout", &script(&body, 0));
    for needle in [
        "cwd/ is not empty",
        "an unexpected entry \"stray\"",
        "roles/\"out-a.tmp\" is not a bound role",
        "request.json was changed",
    ] {
        assert!(has(&report, needle), "{needle}: {:?}", report.failures);
    }
}

/// `CALL_DOC` with call `a` publishing nothing.
fn unpublished() -> String {
    CALL_DOC.replacen(
        r#"[{"ordinal": 1, "decision": "authorized", "members": [{"role": "out-a", "effect": "installed"}]}]"#,
        "[]",
        1,
    )
}

#[test]
fn an_output_role_never_created_is_legal() {
    let body = format!(
        "rm \"$R/roles/out-a\"\n{}",
        documents(SCENARIO_DOC, &unpublished())
    );
    let f = fixture("absent-output", &script(&body, 0));
    let report = run(&f);
    assert!(report.passed, "{:?}", report.failures);
}

#[test]
fn publications_must_agree_with_the_output_roles_on_disk() {
    // Installed, but the role is absent.
    let body = format!(
        "rm \"$R/roles/out-a\"\n{}",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("installed-absent", &script(&body, 0));
    assert!(
        has(
            &report,
            "call a: by its publications output out-a holds a published generation, but it is \
             absent"
        ),
        "{:?}",
        report.failures
    );

    // Nothing published, but the role exists: written outside a publication.
    let report = failing(
        "unpublished-present",
        &script(&documents(SCENARIO_DOC, &unpublished()), 0),
    );
    assert!(
        has(&report, "output out-a holds absent, but it is present"),
        "{:?}",
        report.failures
    );

    // A displacement needs a destination to move aside.
    let call = CALL_DOC.replacen(r#""effect": "installed""#, r#""effect": "displaced""#, 1);
    let body = format!("rm \"$R/roles/out-a\"\n{}", documents(SCENARIO_DOC, &call));
    let report = failing("displaced-nothing", &script(&body, 0));
    assert!(
        has(
            &report,
            "publication 1 reports displaced out-a, which had no destination"
        ),
        "{:?}",
        report.failures
    );
}

/// `DESCRIBE` with `out-a` seeded.
fn seeded(describe_doc: &str) -> String {
    describe_doc.replacen(
        r#""source": "output", "owner": "a"}"#,
        r#""source": "output", "owner": "a", "seeded": true}"#,
        1,
    )
}

/// A harness that checks out-a starts as its seed, then runs `then` and
/// writes `call` as a's document.
fn seed_harness(name: &str, then: &str, call: &str) -> Fixture {
    let body = format!(
        "[ \"$(cat \"$R/roles/out-a\")\" = 'brokkr-library-seed-v1 out-a' ] || exit 4\n{then}\n{}",
        documents(SCENARIO_DOC, call)
    );
    // `script` empties out-a first; this harness must meet the seed.
    fixture(
        name,
        &script(&body, 0).replacen(": > \"$R/roles/out-a\"\n", "", 1),
    )
}

#[test]
fn a_seeded_role_starts_as_its_seed_and_its_effects_are_held_against_it() {
    // Replaced: installed, and no longer the seed.
    let f = seed_harness("seed-replaced", "printf pbf > \"$R/roles/out-a\"", CALL_DOC);
    let report = run_described(&f, &seeded(DESCRIBE));
    assert!(report.passed, "{:?}", report.failures);

    // Untouched: nothing published, still the seed.
    let f = seed_harness("seed-kept", "", &unpublished());
    let report = run_described(&f, &seeded(DESCRIBE));
    assert!(report.passed, "{:?}", report.failures);

    // Claimed installed, but the seed is still there.
    let f = seed_harness("seed-claimed", "", CALL_DOC);
    let report = run_described(&f, &seeded(DESCRIBE));
    assert!(
        has(&report, "but it is its seed, and a seed is never published"),
        "{:?}",
        report.failures
    );

    // Displaced: the seed moved aside and not put back.
    let call = CALL_DOC.replacen(r#""effect": "installed""#, r#""effect": "displaced""#, 1);
    let f = seed_harness("seed-displaced", "rm \"$R/roles/out-a\"", &call);
    let report = run_described(&f, &seeded(DESCRIBE));
    assert!(report.passed, "{:?}", report.failures);
}

#[test]
fn a_refusal_is_reported_with_its_reason() {
    let body = "cat > \"$A/refusal.json\" <<'EOF'\n{\"protocol\": 1, \"reason\": \"cannot bind tmp-a\"}\nEOF";
    let report = failing("refusal", &script(body, 2));
    assert!(
        has(
            &report,
            "could not run the request, exit 2: cannot bind tmp-a"
        ),
        "{:?}",
        report.failures
    );
}

#[test]
fn a_wall_longer_than_the_process_fails() {
    let call = CALL_DOC.replacen(r#""seal_us": 30"#, r#""seal_us": 999000000"#, 1);
    let report = failing("wall", &script(&documents(SCENARIO_DOC, &call), 0));
    assert!(has(&report, "seal_us - entry_us"), "{:?}", report.failures);
}

#[test]
fn a_plan_naming_the_run_directory_fails() {
    let scenario = SCENARIO_DOC.replacen(r#""input": "input""#, r#""input": "RUNDIR""#, 1);
    let body = format!(
        "{}\nsed -i \"s|RUNDIR|$R/x|\" \"$A/scenario.json\"",
        documents(&scenario, CALL_DOC)
    );
    let report = failing("path", &script(&body, 0));
    assert!(has(&report, "run-specific path"), "{:?}", report.failures);
}

#[test]
fn a_harness_past_its_deadline_is_killed_and_fails() {
    let f = fixture("deadline", &script("sleep 30", 0));
    let mut scenario = describe::parse(DESCRIBE.as_bytes())
        .unwrap()
        .scenarios
        .remove(0);
    scenario.deadline_s = 1;
    let datasets = BTreeMap::from([("input".to_owned(), f.input.clone())]);
    let report = verify(
        &f.harness,
        &f.cli,
        &scenario,
        &datasets,
        &Places {
            library_root: &f.root.join("library"),
            fifo_dir: &f.root.join("state"),
        },
        None,
        &mut None,
    )
    .unwrap();
    assert!(!report.passed);
    assert!(
        has(&report, "killed at its 1.0s deadline"),
        "{:?}",
        report.failures
    );
}

#[test]
fn a_lease_held_past_the_harness_leaves_the_directory_untouched() {
    // A descendant keeps the inherited lease descriptor past the release
    // window, so nothing in the run directory may be trusted or written.
    // A layout breach and a malformed document are left too: neither may be
    // looked at.
    let body = "mkdir \"$A/calls\"\necho '{' > \"$A/scenario.json\"\n: > \"$R/stray\"\n(sleep 4 </dev/null >/dev/null 2>&1 &)";
    let report = failing("held", &script(body, 0));
    assert!(has(&report, "was not released"), "{:?}", report.failures);
    assert!(!has(&report, "stray"), "{:?}", report.failures);
    assert!(!has(&report, "artefacts"), "{:?}", report.failures);
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    assert!(!report.run_dir.join("evidence").exists());
}

#[test]
fn an_unsafe_replay_keeps_the_results_read_before_it() {
    // The replay's descendant keeps the lease past its release: the run
    // directory is left alone, but the harness's documents were read before
    // the replay, so their results and the stream verdicts stand.
    let f = fixture("replay-held", &equal_harness());
    cat_extra(&f, "(sleep 4 </dev/null >/dev/null 2>&1 &)\n");
    // Call `a` claims its stream as well as declaring a counterpart.
    let described = DESCRIBE.replacen(
        r#""label": "cat input -> out-a"}"#,
        &format!(r#""label": "cat input -> out-a", "stream": ["counters.v1"], "cli": {CAT_CLI}}}"#),
        1,
    );
    let report = run_described(&f, &described);
    assert!(!report.passed);
    assert!(
        has(
            &report,
            "the replay launch of call a did not establish a safe state"
        ),
        "{:?}",
        report.failures
    );
    let results: Vec<String> = report
        .results
        .iter()
        .map(|(c, r)| {
            format!(
                "{}/{} {}",
                c.as_deref().unwrap_or("-"),
                r.id,
                r.status.name()
            )
        })
        .collect();
    assert_eq!(
        results,
        [
            "-/whole pass",
            "a/a.outcome pass",
            "a/brokkr.seal pass",
            "a/brokkr.stream pass"
        ]
    );
    assert!(!report.run_dir.join("evidence").exists());
}

/// The describe document with call `a` declaring `cli` as its counterpart.
fn with_cli(cli: &str) -> String {
    DESCRIBE.replacen(
        r#""label": "cat input -> out-a"}"#,
        &format!(r#""label": "cat input -> out-a", "cli": {cli}}}"#),
        1,
    )
}

const CAT_CLI: &str = r#"{"argv": ["cat", {"input": "input"}, "-o", {"output": "out-a"}],
  "policy_differences": ["the CLI raises NOFILE"]}"#;

/// A harness whose output role holds what the fake CLI writes.
fn equal_harness() -> String {
    equal_harness_then("")
}

fn equal_harness_then(extra: &str) -> String {
    script(
        &format!(
            "printf pbf > \"$R/roles/out-a\"\n{}\n{extra}",
            documents(SCENARIO_DOC, CALL_DOC)
        ),
        0,
    )
}

fn cli_log(f: &Fixture) -> Vec<String> {
    std::fs::read_to_string(f.root.join("pbfhogg.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.contains("--help"))
        .map(str::to_owned)
        .collect()
}

fn cli_result(report: &Report) -> &super::Outcome {
    &report
        .results
        .iter()
        .find(|(c, r)| c.as_deref() == Some("a") && r.id == "brokkr.cli")
        .expect("a/brokkr.cli")
        .1
}

#[test]
fn an_equal_counterpart_passes_after_one_replay_and_two_digests() {
    let f = fixture("cli-pass", &equal_harness());
    let report = run_described(&f, &with_cli(CAT_CLI));
    assert!(report.passed, "{:?}", report.failures);
    assert!(!report.run_dir.exists(), "removed with the run");
    let ids: Vec<&str> = report.results.iter().map(|(_, r)| r.id.as_str()).collect();
    assert_eq!(ids, ["whole", "a.outcome", "brokkr.seal", "brokkr.cli"]);
    let detail = &cli_result(&report).detail;
    assert!(detail.contains("out-a: equal, 1 nodes"), "{detail}");
    assert!(detail.contains("declared policy differences: the CLI raises NOFILE"));
    let log = cli_log(&f);
    assert_eq!(log.len(), 3, "{log:?}");
    let replay = &log[0];
    assert!(
        replay.starts_with("cat ") && replay.contains("/input.osm.pbf -o "),
        "{replay}"
    );
    assert!(
        replay.contains("/replay-a/outputs/out-a --overwrite --staging-dir "),
        "{replay}"
    );
    assert!(
        replay.contains("/replay-a/outputs/.brokkr-pbfhogg-"),
        "{replay}"
    );
    assert!(log[1].starts_with("digest ") && log[1].ends_with("/roles/out-a"));
    assert!(log[2].starts_with("digest ") && log[2].ends_with("/replay-a/outputs/out-a"));
}

#[test]
fn unequal_outputs_fail_naming_what_differs() {
    // The harness leaves out-a empty; the CLI writes `pbf`.
    let f = fixture(
        "cli-unequal",
        &script(&documents(SCENARIO_DOC, CALL_DOC), 0),
    );
    let report = run_described(&f, &with_cli(CAT_CLI));
    assert!(!report.passed);
    assert!(
        has(
            &report,
            "a/brokkr.cli: fail\n    | out-a differs:\n    |   content hash: library"
        ),
        "{:?}",
        report.failures
    );
    assert!(has(&report, "node: library 1 "), "{:?}", report.failures);
    assert!(
        !has(&report, "exited 0"),
        "a brokkr result never makes the exit inconsistent"
    );
    assert!(
        report.run_dir.join("replay-a/outputs/out-a").is_file(),
        "kept as evidence"
    );
}

#[test]
fn a_failing_or_littering_replay_fails_before_any_digest() {
    let f = fixture("cli-exit", &equal_harness());
    cat_extra(&f, "echo broke >&2\nexit 1\n");
    let report = run_described(&f, &with_cli(CAT_CLI));
    assert!(
        has(
            &report,
            "a/brokkr.cli: fail\n    | the counterpart exited 1, stderr:\n    | broke"
        ),
        "{:?}",
        report.failures
    );
    assert_eq!(cli_log(&f).len(), 1, "no digest after a failed replay");

    let f = fixture("cli-litter", &equal_harness());
    cat_extra(&f, ": > leftover\n");
    let report = run_described(&f, &with_cli(CAT_CLI));
    assert!(has(&report, "cwd is not empty"), "{:?}", report.failures);
    assert_eq!(cli_log(&f).len(), 1, "no digest after a breach");

    // A failing replay is still checked for what it left.
    let f = fixture("cli-exit-litter", &equal_harness());
    cat_extra(&f, ": > leftover\nexit 1\n");
    let report = run_described(&f, &with_cli(CAT_CLI));
    assert!(
        has(&report, "the counterpart exited 1"),
        "{:?}",
        report.failures
    );
    assert!(has(&report, "cwd is not empty"), "{:?}", report.failures);
}

#[test]
fn a_refused_digest_fails_and_keeps_its_defect_beside_a_breach() {
    let f = fixture("cli-digest-refused", &equal_harness());
    digest_extra(&f, "echo unsorted >&2\nexit 2\n");
    let report = run_described(&f, &with_cli(CAT_CLI));
    for needle in [
        "digest of the library output out-a: exited 2, stderr:\n    | unsorted",
        "digest of the CLI output out-a: exited 2",
    ] {
        assert!(has(&report, needle), "{needle}: {:?}", report.failures);
    }

    let f = fixture("cli-digest-litter", &equal_harness());
    digest_extra(&f, ": > leftover\nexit 2\n");
    let report = run_described(&f, &with_cli(CAT_CLI));
    assert!(
        has(&report, "digest of the library output out-a: exited 2"),
        "{:?}",
        report.failures
    );
    assert!(has(&report, "cwd is not empty"), "{:?}", report.failures);
    assert_eq!(cli_log(&f).len(), 2, "nothing launched after the breach");
}

/// The describe document with a generated directory `parts` beside the
/// roles, call `a` declaring `cli`.
fn with_parts(cli: &str) -> String {
    with_cli(cli).replacen(
        r#"{"id": "tmp-a", "kind": "directory", "source": "scratch", "owner": "a"}"#,
        r#"{"id": "tmp-a", "kind": "directory", "source": "scratch", "owner": "a"},
     {"id": "parts", "kind": "directory", "source": "generated"}"#,
        1,
    )
}

const PARTS_CLI: &str = r#"{"argv": ["cat", {"files_in": "parts"}, "-o", {"output": "out-a"}]}"#;

#[test]
fn files_in_expands_a_generated_directory_sorted_by_name() {
    let f = fixture(
        "cli-files-in",
        &equal_harness_then(": > \"$R/roles/parts/b.pbf\"\n: > \"$R/roles/parts/a.pbf\""),
    );
    let report = run_described(&f, &with_parts(PARTS_CLI));
    assert!(report.passed, "{:?}", report.failures);
    let log = cli_log(&f);
    let replay = &log[0];
    let a = replay.find("/parts/a.pbf").unwrap();
    let b = replay.find("/parts/b.pbf").unwrap();
    assert!(a < b, "{replay}");

    // Empty: not replayed, unevaluated.
    let f = fixture("cli-files-in-empty", &equal_harness());
    let report = run_described(&f, &with_parts(PARTS_CLI));
    assert!(
        has(
            &report,
            "a/brokkr.cli: unevaluated\n    | the generated directory parts is empty"
        ),
        "{:?}",
        report.failures
    );
    assert!(cli_log(&f).is_empty());

    // A missing library output outranks the missing input.
    let f = fixture(
        "cli-files-in-precedence",
        &script(
            &format!(
                "rm \"$R/roles/out-a\"\n{}",
                documents(SCENARIO_DOC, CALL_DOC)
            ),
            0,
        ),
    );
    let report = run_described(&f, &with_parts(PARTS_CLI));
    assert!(
        has(
            &report,
            "a/brokkr.cli: fail\n    | the library left no output out-a"
        ),
        "{:?}",
        report.failures
    );
}

#[test]
fn a_replay_that_changes_the_run_directory_is_caught_afterwards() {
    let f = fixture("cli-request", &equal_harness());
    cat_extra(&f, "echo x >> \"$(dirname \"$out\")/../../request.json\"\n");
    let report = run_described(&f, &with_cli(CAT_CLI));
    assert!(
        has(&report, "request.json was changed"),
        "{:?}",
        report.failures
    );
    assert_eq!(cli_result(&report).status, super::Status::Pass);
}

#[test]
fn a_missing_library_output_fails_without_a_replay() {
    let f = fixture(
        "cli-absent",
        &script(
            &format!(
                "rm \"$R/roles/out-a\"\n{}",
                documents(SCENARIO_DOC, CALL_DOC)
            ),
            0,
        ),
    );
    let report = run_described(&f, &with_cli(CAT_CLI));
    assert!(
        has(
            &report,
            "a/brokkr.cli: fail\n    | the library left no output out-a"
        ),
        "{:?}",
        report.failures
    );
    assert!(cli_log(&f).is_empty());
}

#[test]
fn nothing_is_replayed_past_a_layout_breach() {
    let f = fixture("cli-layout", &equal_harness_then(": > \"$R/stray\""));
    let report = run_described(&f, &with_cli(CAT_CLI));
    assert!(
        has(
            &report,
            "a/brokkr.cli: unevaluated\n    | not replayed: the run directory failed"
        ),
        "{:?}",
        report.failures
    );
    assert!(cli_log(&f).is_empty());
}

const REFUSAL_CLI: &str = r#"{"argv": ["cat", {"input": "input"}, "-o", {"output": "out-a"}],
  "expect": "refusal", "exit": 2}"#;

/// `CALL_DOC` with call `a` returning an error and publishing nothing.
fn errored() -> String {
    unpublished().replacen(
        r#""outcome": {"status": "ok"}"#,
        r#""outcome": {"status": "error", "kind": "nofile", "message": "too many open files"}"#,
        1,
    )
}

/// A harness whose call refused: out-a left absent.
fn refused_harness() -> String {
    script(
        &format!("rm \"$R/roles/out-a\"\n{}", documents(SCENARIO_DOC, &errored())),
        0,
    )
}

#[test]
fn a_refusal_counterpart_passes_when_both_sides_refuse_and_publish_nothing() {
    let f = fixture("refusal-pass", &refused_harness());
    cat_extra(&f, "echo 'Error: too many open files' >&2\nexit 2\n");
    let report = run_described(&f, &with_cli(REFUSAL_CLI));
    assert!(report.passed, "{:?}", report.failures);
    let detail = &cli_result(&report).detail;
    assert!(
        detail.contains("refused as declared, exit 2: Error: too many open files"),
        "{detail}"
    );
    let log = cli_log(&f);
    assert_eq!(log.len(), 1, "a refusal is never digested: {log:?}");
}

#[test]
fn a_refusal_counterpart_fails_on_another_status_or_a_published_output() {
    // The CLI does not refuse: it exits 0 and writes its output.
    let f = fixture("refusal-cli-wrote", &refused_harness());
    let report = run_described(&f, &with_cli(REFUSAL_CLI));
    for needle in [
        "the counterpart exited 0, not the declared refusal 2",
        "the counterpart left out-a present, not absent as it started",
    ] {
        assert!(has(&report, needle), "{needle}: {:?}", report.failures);
    }

    // Another failure status is not the declared refusal.
    let f = fixture("refusal-cli-other", &refused_harness());
    cat_extra(&f, "exit 3\n");
    let report = run_described(&f, &with_cli(REFUSAL_CLI));
    assert!(
        has(&report, "exited 3, not the declared refusal 2"),
        "{:?}",
        report.failures
    );

    // The call itself did not refuse: nothing is replayed.
    let f = fixture("refusal-call-ok", &equal_harness());
    let report = run_described(&f, &with_cli(REFUSAL_CLI));
    for needle in [
        "the call returned ok, but its counterpart expects a refusal",
        "publication 1 installed out-a",
        "the library left out-a present, not absent as it started",
    ] {
        assert!(has(&report, needle), "{needle}: {:?}", report.failures);
    }
    assert!(cli_log(&f).is_empty());

    // A cancelled call never has a refusal counterpart.
    let cancelled = errored().replacen(
        r#"{"status": "error", "kind": "nofile", "message": "too many open files"}"#,
        r#"{"status": "cancelled"}"#,
        1,
    );
    let f = fixture(
        "refusal-call-cancelled",
        &script(
            &format!("rm \"$R/roles/out-a\"\n{}", documents(SCENARIO_DOC, &cancelled)),
            0,
        ),
    );
    let report = run_described(&f, &with_cli(REFUSAL_CLI));
    assert!(has(&report, "the call was cancelled"), "{:?}", report.failures);
}

#[test]
fn a_refusal_is_whole_call_and_never_a_signal() {
    // The call refused, but left another output role it owns written.
    let described = with_cli(REFUSAL_CLI).replacen(
        r#"{"id": "tmp-a", "kind": "directory", "source": "scratch", "owner": "a"}"#,
        r#"{"id": "tmp-a", "kind": "directory", "source": "scratch", "owner": "a"},
     {"id": "log-a", "kind": "file", "source": "output", "owner": "a"}"#,
        1,
    );
    let f = fixture(
        "refusal-other-output",
        &script(
            &format!(
                "rm \"$R/roles/out-a\"\n: > \"$R/roles/log-a\"\n{}",
                documents(SCENARIO_DOC, &errored())
            ),
            0,
        ),
    );
    let report = run_described(&f, &described);
    assert!(
        has(&report, "the library left log-a present, not absent as it started"),
        "{:?}",
        report.failures
    );
    assert!(cli_log(&f).is_empty(), "nothing replayed");

    // Killed by a signal: never a refusal.
    let f = fixture("refusal-signal", &refused_harness());
    cat_extra(&f, "kill -9 $$\n");
    let report = run_described(&f, &with_cli(REFUSAL_CLI));
    assert!(
        has(&report, "killed by signal 9, which is never a refusal"),
        "{:?}",
        report.failures
    );
}

#[test]
fn a_refusal_replay_that_replaces_roles_is_a_breach_never_read_through() {
    let f = fixture("refusal-roles-link", &refused_harness());
    cat_extra(
        &f,
        "RD=$(dirname \"$(dirname \"$(dirname \"$out\")\")\")\nmv \"$RD/roles\" \"$RD/roles.real\"\nln -s \"$RD/roles.real\" \"$RD/roles\"\nexit 2\n",
    );
    let report = run_described(&f, &with_cli(REFUSAL_CLI));
    assert!(
        has(&report, "after a launch, ") && has(&report, "roles is no longer a directory"),
        "{:?}",
        report.failures
    );
    assert_eq!(cli_result(&report).status, super::Status::Fail);
}

#[test]
fn a_seeded_refusal_leaves_both_destinations_byte_identical_without_overwrite() {
    let cli = REFUSAL_CLI.replacen(r#""exit": 2"#, r#""exit": 2, "overwrite": false"#, 1);
    let f = seed_harness("refusal-seeded", "", &errored());
    // Refuses an existing output unless told to overwrite, as pbfhogg does.
    cat_extra(
        &f,
        "case \"$*\" in *--overwrite*) ;; *) [ -e \"$out\" ] && { echo exists >&2; exit 2; } ;; esac\n",
    );
    let report = run_described(&f, &seeded(&with_cli(&cli)));
    assert!(report.passed, "{:?}", report.failures);
    assert!(!cli_log(&f)[0].contains("--overwrite"), "{:?}", cli_log(&f));

    // Negotiated, the CLI is told to overwrite and replaces the seed.
    let f = seed_harness("refusal-seeded-negotiated", "", &errored());
    cat_extra(
        &f,
        "case \"$*\" in *--overwrite*) ;; *) [ -e \"$out\" ] && { echo exists >&2; exit 2; } ;; esac\n",
    );
    let report = run_described(&f, &seeded(&with_cli(REFUSAL_CLI)));
    assert!(
        has(&report, "exited 0, not the declared refusal 2"),
        "{:?}",
        report.failures
    );
    assert!(cli_log(&f)[0].contains("--overwrite"), "{:?}", cli_log(&f));
}

#[test]
fn a_declared_overwrite_is_passed_and_required() {
    let cli = CAT_CLI.replacen(r#""policy_differences""#, r#""overwrite": true, "policy_differences""#, 1);
    let f = fixture("overwrite-true", &equal_harness());
    let report = run_described(&f, &with_cli(&cli));
    assert!(report.passed, "{:?}", report.failures);
    assert!(cli_log(&f)[0].contains("--overwrite"));
}

#[test]
fn a_later_iteration_reuses_a_refusal_and_launches_nothing() {
    let f = fixture("measured-refusal", &refused_harness());
    cat_extra(&f, "exit 2\n");
    let mut reuse = Default::default();
    let (first, _) = measured(&f, &with_cli(REFUSAL_CLI), 0, None, &mut reuse);
    assert!(first.passed, "{:?}", first.failures);
    let before = cli_log(&f).len();
    let baseline = first.identity.expect("identified");
    let (second, _) = measured(&f, &with_cli(REFUSAL_CLI), 1, Some(&baseline), &mut reuse);
    assert!(second.passed, "{:?}", second.failures);
    assert_eq!(cli_log(&f).len(), before, "nothing launched");
    assert!(cli_result(&second).detail.contains("CLI refusal reused from iteration 0"));
}

#[test]
fn diagnostics_show_post_seal_events_and_setup_s() {
    let call = CALL_DOC.replacen(
        r#"[{"at_us": 12, "kind": "note", "key": "sort.x", "message": "hello", "truncated": false}]"#,
        r#"[{"at_us": 12, "kind": "note", "key": "sort.x", "message": "hello", "truncated": false},
            {"at_us": 30, "kind": "warn", "key": "out.drop", "message": "at seal", "truncated": false},
            {"at_us": 31, "kind": "warn", "key": "out.drop", "message": "late", "truncated": true}]"#,
        1,
    );
    let scenario = SCENARIO_DOC.replacen(
        r#""diagnostics": []"#,
        r#""diagnostics": [{"at_us": 1, "kind": "note", "key": "setup.gen", "message": "made", "truncated": false}]"#,
        1,
    );
    let f = fixture("diagnostics", &script(&documents(&scenario, &call), 0));
    let report = run(&f);
    assert!(report.passed, "{:?}", report.failures);
    assert_eq!(
        report.diagnostics,
        [
            "- note setup.gen: made",
            "a note sort.x: hello",
            "a warn out.drop: at seal",
            "a warn out.drop: late (truncated) (after seal)"
        ]
    );
}

#[test]
fn printable_escapes_controls_but_tabs() {
    assert_eq!(printable("a\tb\nc\x1b[2J"), "a\tb\\nc\\u{1b}[2J");
}

/// One measured process of `describe_doc` as iteration `iteration`, against
/// `baseline` and the shared `reuse` cache: its report and trajectory.
fn measured(
    f: &Fixture,
    describe_doc: &str,
    iteration: usize,
    baseline: Option<&super::Identity>,
    reuse: &mut super::counterpart::Reuse,
) -> (Report, Option<crate::sidecar::SidecarData>) {
    measured_as(f, describe_doc, iteration, baseline, reuse, None)
}

fn measured_as(
    f: &Fixture,
    describe_doc: &str,
    iteration: usize,
    baseline: Option<&super::Identity>,
    reuse: &mut super::counterpart::Reuse,
    profiled: Option<crate::measure::ProfiledMode>,
) -> (Report, Option<crate::sidecar::SidecarData>) {
    let scenario = describe::parse(describe_doc.as_bytes())
        .unwrap()
        .scenarios
        .remove(0);
    let datasets = BTreeMap::from([("input".to_owned(), f.input.clone())]);
    let ids = BTreeMap::from([("input".to_owned(), "xxh128:0".to_owned())]);
    let mut trajectory = None;
    let report = verify(
        &f.harness,
        &f.cli,
        &scenario,
        &datasets,
        &Places {
            library_root: &f.root.join("library"),
            fifo_dir: &f.root.join("state"),
        },
        Some(super::Measured {
            iteration,
            baseline,
            reuse,
            datasets: &ids,
            features: "",
            profiled,
        }),
        &mut trajectory,
    )
    .unwrap();
    (report, trajectory)
}

#[test]
fn a_measured_process_carries_its_iteration_identity_and_trajectory() {
    // The harness refuses any request but iteration 3's.
    let body = format!(
        "grep -q '\"iteration\": 3' \"$2\" || exit 9\n{}",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let f = fixture("measured", &script(&body, 0));
    let (report, trajectory) = measured(&f, DESCRIBE, 3, None, &mut Default::default());
    assert!(report.passed, "{:?}", report.failures);
    let identity = report.identity.expect("a measured process is identified");
    assert_eq!(
        identity.plan.canonical,
        r#"{"calls":{"a":{"command":"cat","input":"input"}}}"#
    );
    assert_eq!(identity.datasets["input"], "xxh128:0");
    assert_eq!(report.spans.len(), 1);
    assert_eq!(
        (report.spans[0].entry_us, report.spans[0].return_us),
        (10, 20)
    );
    assert_eq!(report.snapshots["a"]["writer_blocks"], 3);
    let trajectory = trajectory.expect("the capture is handed back");
    assert_eq!(trajectory.calls, report.spans);
    assert_eq!(trajectory.records.len(), 3);
}

#[test]
fn a_process_disagreeing_with_iteration_0_fails_and_keeps_the_baseline() {
    let f = fixture("measured-0", &script(&documents(SCENARIO_DOC, CALL_DOC), 0));
    let (first, _) = measured(&f, DESCRIBE, 0, None, &mut Default::default());
    let baseline = first.identity.expect("identified");
    let other = SCENARIO_DOC.replacen("\"cat\"", "\"sort\"", 1);
    let f = fixture("measured-1", &script(&documents(&other, CALL_DOC), 0));
    let (report, trajectory) = measured(&f, DESCRIBE, 1, Some(&baseline), &mut Default::default());
    assert!(!report.passed);
    assert!(
        has(
            &report,
            "iteration 1 disagrees with iteration 0: the plan differs"
        ) && has(&report, "first at calls.a.command (\"cat\" vs \"sort\")"),
        "{:?}",
        report.failures
    );
    let kept = std::fs::read_to_string(report.run_dir.join("evidence/baseline.txt")).unwrap();
    assert!(kept.contains("\"command\":\"cat\"") && kept.contains("\"command\":\"sort\""));
    assert!(
        trajectory.is_some(),
        "a failed process's capture is handed back too"
    );
}

#[test]
fn a_later_iteration_reuses_the_cli_digest_and_still_digests_the_library_output() {
    let f = fixture("measured-reuse", &equal_harness());
    let mut reuse = Default::default();
    let (first, _) = measured(&f, &with_cli(CAT_CLI), 0, None, &mut reuse);
    assert!(first.passed, "{:?}", first.failures);
    let before = cli_log(&f).len();
    let baseline = first.identity.expect("identified");
    let (second, _) = measured(&f, &with_cli(CAT_CLI), 1, Some(&baseline), &mut reuse);
    assert!(second.passed, "{:?}", second.failures);
    let log = cli_log(&f);
    let added = &log[before..];
    assert_eq!(added.len(), 1, "only the library digest ran: {added:?}");
    assert!(added[0].starts_with("digest ") && added[0].ends_with("/roles/out-a"));
    let detail = &cli_result(&second).detail;
    assert!(
        detail.contains("CLI digests reused from iteration 0") && detail.contains("out-a: equal"),
        "{detail}"
    );
}

/// A harness that checks the profiling variables point into its run
/// directory and writes `report` there, then its documents.
fn profiling_harness(report: &str) -> String {
    let body = format!(
        r#"[ "$HOTPATH_OUTPUT_PATH" = "$R/hotpath.json" ] || exit 4
[ "$HOTPATH_OUTPUT_FORMAT" = json ] && [ "$HOTPATH_METRICS_SERVER_OFF" = true ] || exit 3
cat > "$HOTPATH_OUTPUT_PATH" <<'EOF'
{report}
EOF
{}"#,
        documents(SCENARIO_DOC, CALL_DOC)
    );
    script(&body, 0)
}

const PROFILE: &str =
    r#"{"functions_timing": {"data": [{"name": "pbfhogg::cat", "calls": 1, "total": "3 ms"}]}}"#;

#[test]
fn a_profiled_process_carries_its_report_and_its_directory_admits_it() {
    let f = fixture("profiled", &profiling_harness(PROFILE));
    let (report, _) = measured_as(
        &f,
        DESCRIBE,
        0,
        None,
        &mut Default::default(),
        Some(crate::measure::ProfiledMode::Hotpath),
    );
    assert!(report.passed, "{:?}", report.failures);
    let profile = report.hotpath.expect("the report is kept");
    assert_eq!(profile.functions[0].name, "pbfhogg::cat");
    assert!(!report.run_dir.exists(), "removed with the run directory");
}

#[test]
fn a_profiled_process_without_a_valid_report_fails_and_keeps_it() {
    let unmeasured = r#"{"functions_timing": {"data": [{"name": "f"}]}}"#;
    let f = fixture("profiled-bad", &profiling_harness(unmeasured));
    let (report, _) = measured_as(
        &f,
        DESCRIBE,
        0,
        None,
        &mut Default::default(),
        Some(crate::measure::ProfiledMode::Hotpath),
    );
    assert!(!report.passed);
    assert!(
        has(&report, "the hotpath report: "),
        "{:?}",
        report.failures
    );
    assert!(
        report.run_dir.join("hotpath.json").is_file(),
        "kept as evidence"
    );

    let f = fixture(
        "profiled-none",
        &script(&documents(SCENARIO_DOC, CALL_DOC), 0),
    );
    let (report, _) = measured_as(
        &f,
        DESCRIBE,
        0,
        None,
        &mut Default::default(),
        Some(crate::measure::ProfiledMode::Alloc),
    );
    assert!(
        has(
            &report,
            "the hotpath report: the harness wrote no hotpath.json"
        ),
        "{:?}",
        report.failures
    );
}

#[test]
fn an_unprofiled_run_sets_no_profiling_variable_and_admits_no_report() {
    // Removal of inherited ones is `spawn_captured_unsetting`'s test.
    let body = format!(
        "[ -z \"${{HOTPATH_OUTPUT_PATH+x}}${{HOTPATH_OUTPUT_FORMAT+x}}${{HOTPATH_METRICS_SERVER_OFF+x}}\" ] || exit 4\necho '{{}}' > \"$R/hotpath.json\"\n{}",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("unprofiled-report", &script(&body, 0));
    assert!(
        has(&report, "an unexpected entry \"hotpath.json\""),
        "{:?}",
        report.failures
    );
}
