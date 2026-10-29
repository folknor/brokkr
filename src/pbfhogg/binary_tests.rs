#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, Command};

use super::{
    CapabilityStore, OVERWRITE_FLAG, PROBE_PROTOCOL, PbfhoggBinary, STAGING_DIR_FLAG, STORE_CAP,
    TMP_DIR_FLAG, decide, parse_store_line,
};
use crate::pbfhogg::argv::PbfArgv;
use crate::reservations::PRIVATE_DIR_PLACEHOLDER;

// ---------------------------------------------------------------------------
// The decision rule
// ---------------------------------------------------------------------------

/// [`decide`] for `--overwrite`.
fn decide_overwrite(
    help: Option<i32>,
    control: Option<i32>,
    probed: Option<i32>,
) -> Result<bool, String> {
    decide(help, control, probed, OVERWRITE_FLAG)
}

#[test]
fn decide_reads_the_overwrite_probe_only_after_both_controls_pass() {
    assert_eq!(decide_overwrite(Some(0), Some(2), Some(0)), Ok(true));
    assert_eq!(decide_overwrite(Some(0), Some(2), Some(2)), Ok(false));
}

#[test]
fn decide_never_turns_a_broken_probe_into_unsupported() {
    // A binary that exits 2 for everything, or a removed subcommand.
    assert!(decide_overwrite(Some(2), Some(2), Some(2)).is_err());
    // Help works but the parser swallowed the unknown control flag.
    assert!(decide_overwrite(Some(0), Some(0), Some(0)).is_err());
    // Anything but 0 or 2 from the real probe, or a signal anywhere.
    assert!(decide_overwrite(Some(0), Some(2), Some(1)).is_err());
    assert!(decide_overwrite(None, Some(2), Some(0)).is_err());
    assert!(decide_overwrite(Some(0), None, Some(0)).is_err());
    assert!(decide_overwrite(Some(0), Some(2), None).is_err());
}

// ---------------------------------------------------------------------------
// The clap behaviour the probe relies on, checked against real clap
// ---------------------------------------------------------------------------

/// The exit code a clap binary with `cli` would end with for `argv`.
fn exit_code(cli: &Command, argv: &[&str]) -> Option<i32> {
    match cli
        .clone()
        .try_get_matches_from(std::iter::once("pbfhogg").chain(argv.iter().copied()))
    {
        Ok(_) => Some(0),
        Err(e) => Some(e.exit_code()),
    }
}

/// [`decide`] fed with the three probes run against `cli`.
fn probe_clap(cli: &Command, sub: &str) -> Result<bool, String> {
    probe_clap_flag(cli, sub, OVERWRITE_FLAG)
}

/// The same for the probe argument `probe_arg` (a flag, or `--flag=value`).
fn probe_clap_flag(cli: &Command, sub: &str, probe_arg: &str) -> Result<bool, String> {
    decide(
        exit_code(cli, &[sub, "--help"]),
        exit_code(cli, &[sub, "--hel", "--help"]),
        exit_code(cli, &[sub, probe_arg, "--help"]),
        probe_arg,
    )
}

/// A value-taking flag is probed with its value attached: clap then reads
/// the trailing `--help` as help, where a bare flag would take it as its value
/// and fail for the wrong reason.
#[test]
fn clap_value_flags_are_probed_with_an_attached_value() {
    let with = Command::new("pbfhogg").subcommand(
        Command::new("sort")
            .arg(Arg::new("input").required(true))
            .arg(output_arg())
            .arg(Arg::new("staging-dir").long("staging-dir")),
    );
    assert_eq!(probe_clap_flag(&with, "sort", "--staging-dir=."), Ok(true));
    assert_eq!(
        probe_clap_flag(&cli(), "sort", "--staging-dir=."),
        Ok(false)
    );
}

fn output_arg() -> Arg {
    Arg::new("output").short('o').long("output").required(true)
}

fn cli() -> Command {
    Command::new("pbfhogg")
        .subcommand(
            Command::new("sort")
                .arg(Arg::new("input").required(true))
                .arg(output_arg())
                .arg(
                    Arg::new("overwrite")
                        .short('O')
                        .long("overwrite")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("renumber")
                .arg(Arg::new("input").required(true))
                .arg(output_arg())
                .arg(
                    Arg::new("overwrite")
                        .long("overwrite")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("cat")
                .arg(Arg::new("input").required(true))
                .arg(output_arg())
                .arg(
                    Arg::new("overwrite-backup")
                        .long("overwrite-backup")
                        .action(ArgAction::SetTrue),
                ),
        )
}

#[test]
fn clap_declaring_the_flag_is_supported_even_with_required_arguments_missing() {
    let cli = cli();
    assert_eq!(probe_clap(&cli, "sort"), Ok(true));
    assert_eq!(probe_clap(&cli, "renumber"), Ok(true));
}

#[test]
fn clap_with_only_a_longer_similar_flag_is_unsupported() {
    assert_eq!(probe_clap(&cli(), "cat"), Ok(false));
}

#[test]
fn clap_unknown_subcommand_is_an_error_not_unsupported() {
    assert!(probe_clap(&cli(), "no-such-command").is_err());
}

#[test]
fn clap_long_option_inference_makes_the_probe_inconclusive() {
    // With inference on, `--overwrite` would resolve to `--overwrite-backup`
    // and read as supported. The `--hel` control resolves to help instead
    // of failing, so the probe refuses to answer.
    let cli = Command::new("pbfhogg").infer_long_args(true).subcommand(
        Command::new("cat").arg(output_arg()).arg(
            Arg::new("overwrite-backup")
                .long("overwrite-backup")
                .action(ArgAction::SetTrue),
        ),
    );
    assert_eq!(exit_code(&cli, &["cat", OVERWRITE_FLAG, "--help"]), Some(0));
    assert!(probe_clap(&cli, "cat").is_err());
}

#[test]
fn clap_positional_taking_hyphen_values_makes_the_probe_inconclusive() {
    let cli = Command::new("pbfhogg").subcommand(
        Command::new("tags-filter").arg(
            Arg::new("expressions")
                .num_args(1..)
                .allow_hyphen_values(true)
                .trailing_var_arg(true),
        ),
    );
    assert!(probe_clap(&cli, "tags-filter").is_err());
}

// ---------------------------------------------------------------------------
// The handle, against fixture executables
// ---------------------------------------------------------------------------

/// A fake pbfhogg: `sort` accepts `--overwrite`, `--staging-dir` and
/// `--tmp-dir`, `cat` accepts none, `build-geocode-index` only the two
/// directories, any other subcommand is unknown. Each call appends its argv
/// to `<script>.log`. A real run behaves like the refusing pbfhogg: an
/// existing `-o` target without `--overwrite` exits 3, otherwise the target
/// is written.
pub(crate) const FAKE_PBFHOGG: &str = r#"#!/bin/sh
echo "$*" >> "$0.log"
case "$1" in
  sort) ok=0 dirs=0 ;;
  cat) ok=2 dirs=2 ;;
  build-geocode-index) ok=2 dirs=0 ;;
  *) exit 2 ;;
esac
case "$2" in
  --help) exit 0 ;;
  --hel) exit 2 ;;
  --overwrite) exit $ok ;;
  --staging-dir=.|--tmp-dir=.) exit $dirs ;;
esac
out=""
force=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then out="$a"; fi
  if [ "$a" = "--overwrite" ]; then force=1; fi
  prev="$a"
done
if [ -n "$out" ]; then
  if [ -e "$out" ] && [ -z "$force" ]; then exit 3; fi
  : > "$out"
fi
exit 0
"#;

/// Install `script` as an executable named `pbfhogg` under a fresh scratch
/// project root, published as cargo does: written beside, then renamed in.
pub(crate) fn install(name: &str, script: &str) -> (PathBuf, PathBuf) {
    let root = crate::test_scratch::scratch("pbfhogg-binary", name);
    let path = root.join("pbfhogg");
    publish(&path, script);
    (root, path)
}

fn publish(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let staged = path.with_extension("new");
    std::fs::write(&staged, script).unwrap();
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&staged, path).unwrap();
}

/// How many times the fake binary was spawned so far.
fn spawns(path: &Path) -> usize {
    std::fs::read_to_string(path.with_extension("log"))
        .map(|log| log.lines().count())
        .unwrap_or(0)
}

/// `SUB in.pbf -o OUT`.
fn writing(sub: &str, out: impl AsRef<Path>) -> PbfArgv {
    PbfArgv::new(sub)
        .input("in.pbf")
        .unwrap()
        .output(out)
        .unwrap()
}

/// `SUB in.pbf`.
fn reading(sub: &str) -> PbfArgv {
    PbfArgv::new(sub).input("in.pbf").unwrap()
}

#[test]
fn outputs_get_the_flags_exactly_where_the_subcommand_accepts_them() {
    let (root, path) = install("per-subcommand", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();

    let sort = binary.invocation(writing("sort", "out.pbf")).unwrap();
    assert!(has(sort.args_for_test(), OVERWRITE_FLAG));
    assert_eq!(
        value_of(sort.args_for_test(), STAGING_DIR_FLAG),
        Some(PRIVATE_DIR_PLACEHOLDER)
    );
    assert!(
        !has(sort.args_for_test(), TMP_DIR_FLAG),
        "sort declares no scratch"
    );
    assert_eq!(sort.program_for_test(), path.as_path());

    let cat = binary.invocation(writing("cat", "out.pbf")).unwrap();
    assert!(!has(cat.args_for_test(), OVERWRITE_FLAG));
    assert!(!has(cat.args_for_test(), STAGING_DIR_FLAG));
    assert!(!has(cat.args_for_test(), TMP_DIR_FLAG));
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// The value following `flag` in `args`.
fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Each launch gets its own private directory beside the output, named in
/// the argv the child actually received, gone again once the launch is
/// finished; the recorded `cli_args` keep the placeholder.
#[test]
fn every_launch_gets_a_fresh_private_directory_beside_its_output() {
    let (root, path) = install("private-dir", FAKE_PBFHOGG);
    let outdir = root.join("out");
    std::fs::create_dir_all(&outdir).unwrap();
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    let sort = binary
        .invocation(writing("sort", outdir.join("out.pbf")))
        .unwrap();
    assert!(sort.cli_args().contains(PRIVATE_DIR_PLACEHOLDER));
    let spec = sort.spec_for_test().unwrap();
    assert_eq!(spec.private_in(), Some(outdir.as_path()));
    assert_eq!(spec.writes(), [outdir.join("out.pbf")]);
    assert_eq!(spec.reads(), [PathBuf::from("in.pbf")]);

    for _ in 0..2 {
        assert_eq!(execute(&sort, &root), Some(0));
    }
    let log = std::fs::read_to_string(path.with_extension("log")).unwrap();
    let dirs: Vec<&str> = log
        .lines()
        .filter(|line| line.starts_with("sort in.pbf"))
        .filter_map(|line| {
            line.split_whitespace()
                .skip_while(|a| *a != STAGING_DIR_FLAG)
                .nth(1)
        })
        .collect();
    assert_eq!(dirs.len(), 2, "{log}");
    assert_ne!(dirs[0], dirs[1], "each launch has its own directory");
    for dir in &dirs {
        let dir = Path::new(dir);
        assert_eq!(dir.parent(), Some(outdir.as_path()));
        assert!(
            !dir.exists(),
            "released after its launch: {}",
            dir.display()
        );
    }
}

/// Scratch with no output goes to the root the argv names; a build that
/// accepts neither flag runs with the lease alone.
#[test]
fn scratch_without_an_output_goes_to_its_root() {
    let (root, path) = install("scratch-root", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    let scratch = root.join("scratch");
    let diff = binary
        .invocation(reading("sort").scratch(&scratch))
        .unwrap();
    assert_eq!(
        value_of(diff.args_for_test(), TMP_DIR_FLAG),
        Some(PRIVATE_DIR_PLACEHOLDER)
    );
    assert!(!has(diff.args_for_test(), STAGING_DIR_FLAG));
    assert_eq!(
        diff.spec_for_test().unwrap().private_in(),
        Some(scratch.as_path())
    );

    let old = binary.invocation(writing("cat", "out.pbf")).unwrap();
    assert_eq!(old.spec_for_test().unwrap().private_in(), None);
}

#[test]
fn each_subcommand_is_probed_once_per_handle_and_once_per_build() {
    let (root, path) = install("probe-once", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    for _ in 0..3 {
        binary.invocation(writing("sort", "x")).unwrap();
        binary.invocation(writing("cat", "x")).unwrap();
    }
    // Five probes for each of the two subcommands - two controls and one per
    // negotiated flag - no more.
    assert_eq!(spawns(&path), 10);

    // A second handle on the same bytes reads the recorded answers.
    let again = PbfhoggBinary::fixture(&path, &root).unwrap();
    let sort = again.invocation(writing("sort", "x")).unwrap();
    let cat = again.invocation(writing("cat", "x")).unwrap();
    assert_eq!(spawns(&path), 10);
    assert!(sort.args_for_test().iter().any(|a| a == OVERWRITE_FLAG));
    assert!(!cat.args_for_test().iter().any(|a| a == OVERWRITE_FLAG));
}

#[test]
fn different_bytes_never_reuse_a_recorded_answer() {
    let (root, path) = install("rebuilt", FAKE_PBFHOGG);
    PbfhoggBinary::fixture(&path, &root)
        .unwrap()
        .invocation(writing("sort", "x"))
        .unwrap();
    assert_eq!(spawns(&path), 5);

    // The rebuild no longer accepts the flag on sort.
    publish(&path, &FAKE_PBFHOGG.replace("sort) ok=0", "sort) ok=2"));
    let rebuilt = PbfhoggBinary::fixture(&path, &root).unwrap();
    let sort = rebuilt.invocation(writing("sort", "x")).unwrap();
    assert_eq!(spawns(&path), 10);
    assert!(!sort.args_for_test().iter().any(|a| a == OVERWRITE_FLAG));
}

#[test]
fn reads_and_forced_commands_never_probe_or_get_the_flag() {
    let (root, path) = install("no-probe", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    let inspect = binary.invocation(reading("sort")).unwrap();
    assert_eq!(inspect.args_for_test(), ["sort", "in.pbf"]);
    assert_eq!(spawns(&path), 0);
    assert_eq!(
        inspect.spec_for_test().unwrap().reads(),
        [PathBuf::from("in.pbf")],
        "reads are reserved"
    );
}

#[test]
fn a_broken_binary_is_an_error_not_a_cached_unsupported() {
    let (root, path) = install("always-2", "#!/bin/sh\necho \"$*\" >> \"$0.log\"\nexit 2\n");
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    let err = binary
        .invocation(writing("sort", "x"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("--help"), "got: {err}");
    // Nothing was recorded, so the next handle probes again.
    assert!(CapabilityStore::open(&root).read().is_empty());
}

#[test]
fn an_unknown_subcommand_is_an_error() {
    let (root, path) = install("unknown-sub", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    assert!(binary.invocation(writing("no-such", "x")).is_err());
}

#[test]
fn a_binary_replaced_under_a_live_handle_is_refused() {
    let (root, path) = install("replaced", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    publish(&path, &format!("{FAKE_PBFHOGG}# rebuilt\n"));
    let err = binary.invocation(reading("sort")).unwrap_err().to_string();
    assert!(err.contains("replaced"), "got: {err}");
}

/// Spawn `invocation` the way every pbfhogg call site does.
pub(crate) fn execute(invocation: &crate::measure::Invocation<'_>, cwd: &Path) -> Option<i32> {
    invocation.capture_in(cwd).unwrap().code()
}

#[test]
fn a_reused_owned_output_runs_twice_against_a_refusing_binary() {
    // The second iteration is the one the refusal breaks.
    let (root, path) = install("reuse", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    let out = root.join("out.pbf");
    for _ in 0..2 {
        let invocation = binary.invocation(writing("sort", &out)).unwrap();
        assert_eq!(execute(&invocation, &root), Some(0));
    }
    // With the output passed as a bare argument, undeclared, the same reuse
    // is refused - the fixture does model the refusal.
    let undeclared = binary
        .invocation(reading("sort").arg("-o").arg(out.to_str().unwrap()))
        .unwrap();
    assert_eq!(execute(&undeclared, &root), Some(3));
}

pub(crate) fn typed_ctx(root: &Path) -> crate::pbfhogg::context::CommandContext {
    use crate::pbfhogg::context::{CommandObservations, CommandParams, ResolvedInputs};
    let scratch = root.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    crate::pbfhogg::context::CommandContext {
        inputs: ResolvedInputs::Pbf {
            pbf: root.join("in.osm.pbf"),
        },
        scratch_dir: scratch,
        dataset: "denmark".into(),
        params: CommandParams::default(),
        observations: CommandObservations::default(),
    }
}

#[test]
fn typed_commands_resolve_through_the_negotiation_their_arm_chose() {
    use crate::pbfhogg::commands::{ArgMode, PbfhoggCommand};
    let (root, path) = install("typed", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    let ctx = typed_ctx(&root);

    // A writing command is negotiated and survives a reused output.
    for _ in 0..2 {
        let sort = binary
            .invocation(
                PbfhoggCommand::Sort
                    .build_args(&ctx, ArgMode::Bench)
                    .unwrap(),
            )
            .unwrap();
        assert!(has(sort.args_for_test(), OVERWRITE_FLAG));
        assert_eq!(execute(&sort, &root), Some(0));
    }

    // Geocode's own --force replaces its output, so it never gets
    // --overwrite, but it still writes one: its private directory sits
    // beside the index directory, never inside it.
    let geocode = binary
        .invocation(
            PbfhoggCommand::BuildGeocodeIndex
                .build_args(&ctx, ArgMode::Bench)
                .unwrap(),
        )
        .unwrap();
    assert!(!has(geocode.args_for_test(), OVERWRITE_FLAG));
    assert!(
        has(geocode.args_for_test(), STAGING_DIR_FLAG)
            && has(geocode.args_for_test(), TMP_DIR_FLAG)
    );
    assert_eq!(
        geocode.spec_for_test().unwrap().private_in(),
        Some(ctx.scratch_dir.as_path()),
        "beside the index directory, never inside it"
    );
    let probes = spawns(&path);
    // A read-only command: no probe, no flag.
    let check = binary
        .invocation(
            PbfhoggCommand::CheckRefs
                .build_args(&ctx, ArgMode::Bench)
                .unwrap(),
        )
        .unwrap();
    assert!(!check.args_for_test().iter().any(|a| a == OVERWRITE_FLAG));
    assert_eq!(spawns(&path), probes);
}

#[test]
fn a_probe_that_cannot_spawn_names_the_subcommand_and_the_probe() {
    let (root, path) = install("unspawnable", FAKE_PBFHOGG);
    // A missing working directory makes the probe's spawn itself fail.
    let binary = PbfhoggBinary {
        project_root: root.join("gone"),
        ..PbfhoggBinary::fixture(&path, &root).unwrap()
    };
    let err = binary
        .invocation(writing("sort", "x"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("sort"), "got: {err}");
    assert!(err.contains("--help"), "got: {err}");
}

#[test]
fn a_live_handle_refuses_a_build_and_releases_on_drop() {
    // The handle registers before it reads its executable; while it lives,
    // no cargo may start through the build seam.
    let (root, path) = install("refuses-build", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    assert!(crate::build::executable_guard::begin_build().is_err());
    drop(binary);
    drop(crate::build::executable_guard::begin_build().unwrap());
}

#[test]
fn a_handle_cannot_be_made_while_a_build_runs() {
    let (root, path) = install("during-build", FAKE_PBFHOGG);
    let build = crate::build::executable_guard::begin_build().unwrap();
    assert!(PbfhoggBinary::fixture(&path, &root).is_err());
    drop(build);
    // The failed construction left no registration behind.
    drop(crate::build::executable_guard::begin_build().unwrap());
}

/// A run-scoped launch runs in the cwd it is given, inside the run
/// directory, with its private directory beside its output, and is killed
/// at its deadline; both come back released.
#[test]
fn a_run_scoped_launch_runs_in_its_cwd_and_is_killed_at_its_deadline() {
    use crate::measure::Supervised;
    let script = r#"#!/bin/sh
case "$2" in
  --help) exit 0 ;;
  --hel) exit 2 ;;
  --overwrite|--staging-dir=.) exit 0 ;;
  --tmp-dir=.) exit 2 ;;
esac
case "$1" in
  sort) pwd -P > "$3"; echo "$6" >> "$3" ;;
  slow) sleep 5 ;;
esac
"#;
    let (root, path) = install("run-scoped", script);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    let run = root.join("run-1");
    let cwd = run.join("replay/cwd");
    let outputs = run.join("replay/outputs");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&outputs).unwrap();
    let scope = super::RunScope {
        run_dir: &run,
        cwd: &cwd,
        deadline: std::time::Duration::from_secs(1),
        stdout_kept: 64,
    };
    let out = outputs.join("out");
    let argv = PbfArgv::new("sort").arg("-o").written(&out).unwrap();
    let Supervised::Ran(capture) = binary.run_in(argv, &scope) else {
        panic!("the launch did not run");
    };
    assert!(capture.captured.success());
    let seen = std::fs::read_to_string(&out).unwrap();
    let mut lines = seen.lines();
    assert_eq!(
        lines.next().map(PathBuf::from),
        Some(cwd.canonicalize().unwrap())
    );
    let private = PathBuf::from(lines.next().unwrap());
    assert_eq!(private.parent(), Some(outputs.as_path()), "{seen}");
    assert!(!private.exists(), "released with the launch");

    let Supervised::Ran(capture) = binary.run_in(
        PbfArgv::new("slow")
            .arg("-o")
            .written(outputs.join("slow"))
            .unwrap(),
        &scope,
    ) else {
        panic!("the launch did not run");
    };
    assert!(capture.captured.ending.is_deadline());
}

#[test]
fn an_empty_argv_is_refused() {
    let (root, path) = install("empty-argv", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::fixture(&path, &root).unwrap();
    assert!(binary.invocation(PbfArgv::new("")).is_err());
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

#[test]
fn store_lines_from_another_protocol_are_ignored() {
    assert_eq!(
        parse_store_line(&format!("{PROBE_PROTOCOL}\tabc\tsort\t1")),
        Some(("abc".to_owned(), "sort".to_owned(), true))
    );
    assert_eq!(
        parse_store_line(&format!("{}\tabc\tsort\t1", PROBE_PROTOCOL + 1)),
        None
    );
    assert_eq!(parse_store_line("1\tabc\tsort\tyes"), None);
    assert_eq!(parse_store_line("1\t\tsort\t1"), None);
    assert_eq!(parse_store_line("1\tabc\tsort\t1\textra"), None);
}

#[test]
fn store_keeps_the_newest_answers_and_replaces_a_repeated_key() {
    let root = crate::test_scratch::scratch("pbfhogg-binary", "store-cap");
    let store = CapabilityStore::open(&root);
    store.record("old", "sort", false);
    store.record("old", "sort", true);
    assert_eq!(store.read(), vec![("old".into(), "sort".into(), true)]);

    for i in 0..STORE_CAP {
        store.record(&format!("d{i}"), "cat", true);
    }
    let entries = store.read();
    assert_eq!(entries.len(), STORE_CAP);
    assert!(!entries.iter().any(|(d, _, _)| d == "old"));
    assert_eq!(
        entries.last().map(|(d, _, _)| d.clone()),
        Some(format!("d{}", STORE_CAP - 1))
    );
}
