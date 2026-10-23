#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, Command};

use super::{
    CapabilityStore, OVERWRITE_FLAG, PROBE_PROTOCOL, PbfhoggBinary, STORE_CAP, Writes, decide,
    parse_store_line,
};

// ---------------------------------------------------------------------------
// The decision rule
// ---------------------------------------------------------------------------

#[test]
fn decide_reads_the_overwrite_probe_only_after_both_controls_pass() {
    assert_eq!(decide(Some(0), Some(2), Some(0)), Ok(true));
    assert_eq!(decide(Some(0), Some(2), Some(2)), Ok(false));
}

#[test]
fn decide_never_turns_a_broken_probe_into_unsupported() {
    // A binary that exits 2 for everything, or a removed subcommand.
    assert!(decide(Some(2), Some(2), Some(2)).is_err());
    // Help works but the parser swallowed the unknown control flag.
    assert!(decide(Some(0), Some(0), Some(0)).is_err());
    // Anything but 0 or 2 from the real probe, or a signal anywhere.
    assert!(decide(Some(0), Some(2), Some(1)).is_err());
    assert!(decide(None, Some(2), Some(0)).is_err());
    assert!(decide(Some(0), None, Some(0)).is_err());
    assert!(decide(Some(0), Some(2), None).is_err());
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
    decide(
        exit_code(cli, &[sub, "--help"]),
        exit_code(cli, &[sub, "--hel", "--help"]),
        exit_code(cli, &[sub, OVERWRITE_FLAG, "--help"]),
    )
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

/// A fake pbfhogg: `sort` accepts `--overwrite`, `cat` does not, any other
/// subcommand is unknown. Each call appends its argv to `<script>.log`. A
/// real run behaves like the refusing pbfhogg: an existing `-o` target
/// without `--overwrite` exits 3, otherwise the target is written.
pub(crate) const FAKE_PBFHOGG: &str = r#"#!/bin/sh
echo "$*" >> "$0.log"
case "$1" in
  sort) ok=0 ;;
  cat) ok=2 ;;
  *) exit 2 ;;
esac
case "$2" in
  --help) exit 0 ;;
  --hel) exit 2 ;;
  --overwrite) exit $ok ;;
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

fn args(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|&a| a.to_owned()).collect()
}

#[test]
fn owned_outputs_get_the_flag_exactly_where_the_subcommand_accepts_it() {
    let (root, path) = install("per-subcommand", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::new(&path, &root).unwrap();

    let sort = binary
        .invocation(args(&["sort", "in.pbf", "-o", "out.pbf"]), Writes::OwnedOutput)
        .unwrap();
    assert_eq!(sort.args().last().map(String::as_str), Some(OVERWRITE_FLAG));
    assert_eq!(sort.program(), path.as_path());

    let cat = binary
        .invocation(args(&["cat", "in.pbf", "-o", "out.pbf"]), Writes::OwnedOutput)
        .unwrap();
    assert!(!cat.args().iter().any(|a| a == OVERWRITE_FLAG));
}

#[test]
fn each_subcommand_is_probed_once_per_handle_and_once_per_build() {
    let (root, path) = install("probe-once", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::new(&path, &root).unwrap();
    for _ in 0..3 {
        binary
            .invocation(args(&["sort", "-o", "x"]), Writes::OwnedOutput)
            .unwrap();
        binary
            .invocation(args(&["cat", "-o", "x"]), Writes::OwnedOutput)
            .unwrap();
    }
    // Three probes for each of the two subcommands, no more.
    assert_eq!(spawns(&path), 6);

    // A second handle on the same bytes reads the recorded answers.
    let again = PbfhoggBinary::new(&path, &root).unwrap();
    let sort = again
        .invocation(args(&["sort", "-o", "x"]), Writes::OwnedOutput)
        .unwrap();
    let cat = again
        .invocation(args(&["cat", "-o", "x"]), Writes::OwnedOutput)
        .unwrap();
    assert_eq!(spawns(&path), 6);
    assert!(sort.args().iter().any(|a| a == OVERWRITE_FLAG));
    assert!(!cat.args().iter().any(|a| a == OVERWRITE_FLAG));
}

#[test]
fn different_bytes_never_reuse_a_recorded_answer() {
    let (root, path) = install("rebuilt", FAKE_PBFHOGG);
    PbfhoggBinary::new(&path, &root)
        .unwrap()
        .invocation(args(&["sort", "-o", "x"]), Writes::OwnedOutput)
        .unwrap();
    assert_eq!(spawns(&path), 3);

    // The rebuild no longer accepts the flag on sort.
    publish(&path, &FAKE_PBFHOGG.replace("sort) ok=0", "sort) ok=2"));
    let rebuilt = PbfhoggBinary::new(&path, &root).unwrap();
    let sort = rebuilt
        .invocation(args(&["sort", "-o", "x"]), Writes::OwnedOutput)
        .unwrap();
    assert_eq!(spawns(&path), 6);
    assert!(!sort.args().iter().any(|a| a == OVERWRITE_FLAG));
}

#[test]
fn reads_and_forced_commands_never_probe_or_get_the_flag() {
    let (root, path) = install("no-probe", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::new(&path, &root).unwrap();
    let inspect = binary
        .invocation(args(&["sort", "in.pbf"]), Writes::Nothing)
        .unwrap();
    assert_eq!(inspect.args(), args(&["sort", "in.pbf"]).as_slice());
    assert_eq!(spawns(&path), 0);
}

#[test]
fn a_broken_binary_is_an_error_not_a_cached_unsupported() {
    let (root, path) = install("always-2", "#!/bin/sh\necho \"$*\" >> \"$0.log\"\nexit 2\n");
    let binary = PbfhoggBinary::new(&path, &root).unwrap();
    let err = binary
        .invocation(args(&["sort", "-o", "x"]), Writes::OwnedOutput)
        .unwrap_err()
        .to_string();
    assert!(err.contains("--help"), "got: {err}");
    // Nothing was recorded, so the next handle probes again.
    assert!(CapabilityStore::open(&root).read().is_empty());
}

#[test]
fn an_unknown_subcommand_is_an_error() {
    let (root, path) = install("unknown-sub", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::new(&path, &root).unwrap();
    assert!(
        binary
            .invocation(args(&["no-such", "-o", "x"]), Writes::OwnedOutput)
            .is_err()
    );
}

#[test]
fn a_binary_replaced_under_a_live_handle_is_refused() {
    let (root, path) = install("replaced", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::new(&path, &root).unwrap();
    publish(&path, &format!("{FAKE_PBFHOGG}# rebuilt\n"));
    let err = binary
        .invocation(args(&["sort", "in.pbf"]), Writes::Nothing)
        .unwrap_err()
        .to_string();
    assert!(err.contains("replaced"), "got: {err}");
}

/// Spawn `invocation` the way every pbfhogg call site does.
pub(crate) fn execute(invocation: &crate::measure::Invocation<'_>, cwd: &Path) -> Option<i32> {
    crate::output::run_captured(&invocation.program_string(), &invocation.arg_refs(), cwd)
        .unwrap()
        .code()
}

#[test]
fn a_reused_owned_output_runs_twice_against_a_refusing_binary() {
    // The second iteration is the one the refusal breaks.
    let (root, path) = install("reuse", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::new(&path, &root).unwrap();
    let out = root.join("out.pbf");
    let out_str = out.to_str().unwrap();
    for _ in 0..2 {
        let invocation = binary
            .invocation(args(&["sort", "in.pbf", "-o", out_str]), Writes::OwnedOutput)
            .unwrap();
        assert_eq!(execute(&invocation, &root), Some(0));
    }
    // Declared as writing nothing, the same reuse is refused - the fixture
    // does model the refusal.
    let undeclared = binary
        .invocation(args(&["sort", "in.pbf", "-o", out_str]), Writes::Nothing)
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
    let binary = PbfhoggBinary::new(&path, &root).unwrap();
    let ctx = typed_ctx(&root);

    // A writing command is negotiated and survives a reused output.
    for _ in 0..2 {
        let sort = binary
            .command_invocation(PbfhoggCommand::Sort.build_args(&ctx, ArgMode::Bench).unwrap())
            .unwrap();
        assert_eq!(sort.args().last().map(String::as_str), Some(OVERWRITE_FLAG));
        assert_eq!(execute(&sort, &root), Some(0));
    }
    let probes = spawns(&path);

    // Geocode's own --force replaces its output: no probe, no flag.
    let geocode = binary
        .command_invocation(
            PbfhoggCommand::BuildGeocodeIndex
                .build_args(&ctx, ArgMode::Bench)
                .unwrap(),
        )
        .unwrap();
    assert!(!geocode.args().iter().any(|a| a == OVERWRITE_FLAG));
    // A read-only command: no probe, no flag.
    let check = binary
        .command_invocation(PbfhoggCommand::CheckRefs.build_args(&ctx, ArgMode::Bench).unwrap())
        .unwrap();
    assert!(!check.args().iter().any(|a| a == OVERWRITE_FLAG));
    assert_eq!(spawns(&path), probes);
}

#[test]
fn a_probe_that_cannot_spawn_names_the_subcommand_and_the_probe() {
    let (root, path) = install("unspawnable", FAKE_PBFHOGG);
    // A missing working directory makes the probe's spawn itself fail.
    let binary = PbfhoggBinary {
        project_root: root.join("gone"),
        ..PbfhoggBinary::new(&path, &root).unwrap()
    };
    let err = binary
        .invocation(args(&["sort", "-o", "x"]), Writes::OwnedOutput)
        .unwrap_err()
        .to_string();
    assert!(err.contains("sort"), "got: {err}");
    assert!(err.contains("--help"), "got: {err}");
}

#[test]
fn an_empty_argv_is_refused() {
    let (root, path) = install("empty-argv", FAKE_PBFHOGG);
    let binary = PbfhoggBinary::new(&path, &root).unwrap();
    assert!(binary.invocation(Vec::new(), Writes::Nothing).is_err());
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
    assert_eq!(entries.last().map(|(d, _, _)| d.clone()), Some(format!("d{}", STORE_CAP - 1)));
}
