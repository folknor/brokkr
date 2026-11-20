//! Clap command and argument definitions.

use super::validation::{
    validate_env_kv, validate_meta_filter, validate_param_filter, validate_since,
    validate_test_name,
};
use clap::{ArgGroup, Args, Parser, Subcommand};

/// Version banner: semver plus the short git hash (with a `-dirty` suffix for
/// an unclean tree) and the UTC build time, stamped at compile time by
/// `build.rs`. e.g. `0.1.0 (abc123def 2026-07-12 12:34:56 UTC)`. The first
/// line `--version` prints (`crate::version_cmd`), which answers the flag
/// before clap parses; clap keeps it for the flag's place in help.
pub(crate) const LONG_VERSION: &str = env!("BROKKR_LONG_VERSION");

/// `brokkr outline --depth` when the flag is absent; clap renders it in the
/// help text, so the doc comment does not repeat it.
const DEFAULT_OUTLINE_DEPTH: usize = 4;

/// Run count a bare `--bench` (no COUNT) means, for every command that takes
/// one. A string because clap's `default_missing_value` is parsed like argv.
const DEFAULT_BENCH_RUNS: &str = "3";
const DEFAULT_SINGLE_RUNS: &str = "1";

#[derive(Parser)]
#[command(name = "brokkr", about = "Shared development tooling", version = LONG_VERSION)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

// What `check --textlint` / `--script` refuse: everything that shapes the build,
// certifies a run, or runs a phase the selection skips.
const SELECTION_CONFLICTS: [&str; 11] = [
    "features",
    "no_default_features",
    "package",
    "profile",
    "gate",
    "force_rust",
    "json",
    "fix_gremlins",
    "timings",
    "commands",
    "args",
];

// What `check --audit` refuses: it runs no phase, so every flag that acts on a
// run (rewriting, timing, logging, a verdict trailer, a selection). The flags
// that shape a run - profile, gate, features, `-p`, `--force-rust`, forwarded
// args - are accepted: they name the check the audit's plan explains.
const AUDIT_CONFLICTS: [&str; 6] = [
    "json",
    "fix_gremlins",
    "timings",
    "commands",
    "textlint_names",
    "script_names",
];

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Run the convention checks, clippy, rustdoc and tests
    #[command(
        display_order = 0,
        long_about = "\
Phases in order: the convention checks (gremlins, header, textlint,
manifest, script checks, dependency rules, publish cycle), then clippy,
rustdoc and tests - plus coverage, member-check and fixture-lint where the
profile and config ask for them. A failing phase stops the run before the next one
starts. Extra args after `--` go to the test phase: tokens before a second
`--` to `cargo test`, tokens after it to libtest. To run one test without
the other phases, use `brokkr test NAME`.

Output (default text mode, no flags):
  - Gremlins: one line per banned-Unicode hit. `--fix-gremlins`
    rewrites them in place before the scan.
  - Clippy and rustdoc: one line per diagnostic in the form
    `error[CODE] file:line:col message`. Cargo runs with
    `--message-format=json` so every diagnostic carries its lint code,
    not just the first occurrence per rule.
  - Tests: one line per failure on failure, compact summary on pass.
    Never capped - every failing test in every suite is listed.

Which diagnostics are shown (every diagnostic phase):
  1. Everything is an error, except a warning from a dependency outside
     the workspace, which neither fails nor prints.
  2. Errors in files with unstaged changes (git diff, plus untracked
     files) come first; the rest follow. Diagnostics are sorted by
     (lint code, file, line) so every hit of a rule clumps together.
  3. Nothing is capped: every error is printed, every time.

Examples:
  brokkr check                                     # configured checks and test sweeps
  brokkr check --fix-gremlins                      # rewrite banned chars before checking
  brokkr check -- --test read_paths                # run one test file
  brokkr check -- -- --ignored                     # run ignored tests
  brokkr check -- --test read_paths -- --ignored   # one file, ignored only
  brokkr check --no-default-features               # check without default features
  brokkr check --features commands                 # check with specific features
  brokkr check --package pbfhogg-cli               # check only the CLI crate
  brokkr check -p crate-a -p crate-b               # check two crates
  brokkr check --package pbfhogg-cli -- --test cli # one test file in the CLI crate

In depth: `brokkr man check` (one section at a time, e.g. `man check clippy`)."
    )]
    Check(CheckCmd),
    /// Run ONLY the clippy phase against an ad-hoc target (investigative).
    #[command(
        display_order = 0,
        long_about = "\
Run ONLY clippy - no gremlins/style/textlint/manifest/dependency/test.
An investigative probe, not a gate: it allows a real `--all-features` and
free feature sweeps a `[[check]]` gate never would, run under brokkr's env
and toolchain discipline (disable_toolchain honoured, [[check]] env applied,
global lock held) so a differential lint hunt no longer forces raw
`cargo +nightly clippy` (which drops HIGH_PRECISION=1 and the toolchain pin).

Two modes:
  - Ad-hoc: `-p` (repeatable) + `--all-features` / `--features` /
    `--no-default-features`. Env is the union of every [[check]] entry's env
    (a project invariant a probe must not drop); a key two entries set to
    different values is a config error unless `--env` picks one.
  - `--sweep NAME`: borrow one [[check]] or [[lint_sweep]] entry's
    packages/features/env/target verbatim, whatever its `when`.

`--env KEY=VALUE` (repeatable) overrides either env source and wins last.
Output matches `brokkr check`'s clippy phase: one uncapped line per
diagnostic.
Exit 0 iff zero diagnostics; 1 on any lint or build error.

Examples:
  brokkr clippy                              # default selection, default features
  brokkr clippy -p mycrate --all-features    # one crate, all features
  brokkr clippy --features a,b -p mycrate    # a virtual workspace needs -p
  brokkr clippy --sweep ffi                  # replay the 'ffi' [[check]] entry
  brokkr clippy --sweep ffi --env HIGH_PRECISION=0
  brokkr clippy -p mycrate --lib             # lib-only lint surface (cfg(test) dead code)

In depth: `brokkr man clippy`."
    )]
    Clippy(ClippyCmd),
    /// Run `cargo fmt`. All arguments are forwarded raw.
    #[command(display_order = 0)]
    Fmt(FmtCmd),
    /// Run a bin or example target by name (`cargo run`).
    ///
    /// Targets are discovered from cargo metadata across the whole workspace:
    /// every bin and every example is runnable by its target name. The bare
    /// form runs `[bin] default` (or the sole runnable); with several
    /// candidates and no default it lists them. Trailing arguments are
    /// forwarded raw to the program after `--`. Flags for brokkr
    /// (`--debug`/`--release`, `--features`) go before the name.
    ///
    /// In depth: `brokkr man run`.
    #[command(display_order = 0)]
    Run(RunCmd),
    /// Serve a bin or example target in the background, without the lock.
    ///
    /// Bare lists the live serves, all projects. `serve TARGET` builds it as
    /// `run` would (same flags, `--with` included), replaces a serve of the
    /// same name in this project, starts it detached under a brokkr
    /// supervisor and returns; the global lock is released then. Every
    /// measurement (`--bench`, `--hotpath`, `--alloc`, `brokkr bench`, `crit`
    /// sampling, `sync --bench`) refuses while a serve is live; every other
    /// command runs beside it with a warning. `--stop` stops one.
    ///
    /// In depth: `brokkr man serve`.
    #[command(display_order = 0)]
    Serve(ServeCmd),
    /// Build one bin or example target and print a stable path to it.
    ///
    /// Resolves NAME exactly as `run` does, builds it under the lock, copies
    /// the executable to `.brokkr/artifacts/<xxh128>/<name>` and prints that
    /// path - the only line on stdout; all narration goes to stderr. The copy
    /// never changes under a later build, so a script can run it as long as
    /// it likes: `bin=$(brokkr build dellingr --release)`. A name that does
    /// not pick one target is an error, not an index.
    ///
    /// In depth: `brokkr man run`.
    #[command(display_order = 0)]
    Build(BuildCmd),
    /// Install the project's binaries (`cargo install --path ...`).
    ///
    /// Installs the packages named by `[bin] install`, or the workspace's
    /// sole bin-carrying package when the list is absent. `[bin] debug =
    /// true` installs with `--debug` and `[bin] profile` with `--profile`;
    /// `--debug`/`--release`/`--profile` override them.
    ///
    /// Installs with `--locked`, so what ships is what `check` validated.
    /// `--unlocked` restores cargo's default re-resolution.
    ///
    /// Then runs the project's `[[install]]` steps in order: `copy` publishes
    /// a file or directory brokkr owns exclusively, `run` a shell command.
    ///
    /// In depth: `brokkr man run`.
    #[command(display_order = 0)]
    Install(InstallCmd),
    /// List rust source files above a line-count threshold (default 800).
    ///
    /// Scans tracked and untracked-not-ignored `.rs` files and prints those
    /// with more than THRESHOLD lines, largest first.
    #[command(display_order = 0)]
    Wc(WcCmd),
    /// Read the bundled documentation (`brokkr man` lists the topics).
    ///
    /// The `docs/**.md` files are compiled into the binary and rendered to the
    /// terminal. Topics are filtered by the detected project, except the
    /// project-agnostic topics (such as check, clean, crit, config and
    /// measure), which are listed everywhere. A long topic lists its
    /// sections instead of dumping the file: `brokkr man conventions script_check`
    /// reads one, `--full` reads the lot.
    #[command(display_order = 0)]
    Man(ManCmd),
    /// Run a criterion bench target, or compare two stored baselines.
    ///
    /// Bare lists the workspace's bench targets. A measuring run saves a
    /// baseline named for the commit; `--compare A B` diffs two stored
    /// baselines without sampling anything.
    ///
    /// In depth: `brokkr man crit`.
    #[command(display_order = 0)]
    Crit(CritCmd),
    /// Run a script bench (`[bench.NAME]` in brokkr.toml) and store its run.
    ///
    /// Bare lists the benches. brokkr's options go before NAME; everything
    /// after NAME belongs to the bench (`NAME --help` describes it). Every
    /// attempted run is stored in the per-user bench store, which `brokkr
    /// results` reads.
    ///
    /// In depth: `brokkr man bench`.
    #[command(display_order = 0)]
    Bench(BenchCmd),
    /// Audit Cargo.lock for dependency smells (duplicate versions, etc.).
    ///
    /// Phase-based; each phase emits zero or more findings. Duplicate-version
    /// findings include blame attribution (which direct dependency anchors
    /// an old version).
    ///
    /// In depth: `brokkr man deps`.
    #[command(display_order = 0)]
    Deps(DepsCmd),
    /// Show environment information
    #[command(display_order = 1)]
    Env,
    /// Build and run with passthrough args (deprecated - use `run` subcommands instead)
    #[command(name = "passthrough", display_order = 99, hide = true)]
    Passthrough(PassthroughCmd),
    /// Query benchmark results
    #[command(
        display_order = 3,
        long_about = "\
Query benchmark results from .brokkr/results.db, and this project's
script-bench runs from the per-user bench store, listed after the typed rows.

Examples:
  brokkr results                                    # last 20 results
  brokkr results --command read                     # last 20 matching 'read'
  brokkr results 0b74fb6f                           # look up by UUID prefix
  brokkr results --commit a65a                      # filter by commit prefix
  brokkr results --command check                    # substring: check-refs, check-ids, ...
  brokkr results --mode hotpath                     # filter by measurement mode
  brokkr results --dataset europe                   # filter by dataset (substring match on input file)
  brokkr results --command tags-filter --dataset eu # combine filters
  brokkr results --compare a65a 911c                # compare two commits
  brokkr results --compare a65a 911c --mode bench   # compare, filtered
  brokkr results --param dataset=denmark            # script-bench runs with that param
  brokkr results --retire old-cat --reason \"replaced by cat\"  # hide a bench's runs
  brokkr results --unretire old-cat                 # show them again
  brokkr results --invalidate 3f2a --reason \"noisy host\"  # hide one run
  brokkr results --revalidate 3f2a                  # undo that
  brokkr results --rename cat concat                # cat's runs list as concat
  brokkr results --rename concat cat                # undo that
  brokkr results --hidden                           # list hidden runs too

In depth: `brokkr man results`."
    )]
    // A mark is a write, one per command, and no query option means anything
    // to it.
    #[command(group(
        ArgGroup::new("mark")
            .args(["retire", "unretire", "invalidate", "revalidate", "rename"])
            .multiple(false)
            .conflicts_with_all([
                "query", "commit", "compare", "command", "mode", "dataset", "meta", "env",
                "param", "grep", "grep_v", "limit", "top", "hidden", "invocation",
            ])
    ))]
    // The marks that carry a reason.
    #[command(group(ArgGroup::new("reasoned").args(["retire", "invalidate"])))]
    Results(ResultsCmd),
    /// Query sidecar /proc timelines, markers, and phase summaries
    #[command(
        display_order = 4,
        long_about = "\
Query sidecar data captured in .brokkr/sidecar.db during `--bench`,
`--hotpath`, and `--alloc` runs. A UUID prefix is required - use
`brokkr results` to find one. `--run N|all` picks a specific run
within the result (default: best run).

A run on a dirty tree (allowed by `--force`) or one that failed has no
results.db row, but its sidecar data is still stored, under an id of its
own that the run's `[sidecar]` line names even under quiet. The `dirty`
pseudo-UUID resolves to the most recent such run; earlier ones stay
readable by their ids. To A/B a variant and keep every cell, commit it
locally so each cell stores a row (`brokkr man measure`).

Examples:
  brokkr sidecar <uuid>                               # per-phase summary (default view)
  brokkr sidecar dirty                                # the last forced/failed run
  brokkr sidecar <uuid> --human                       # same, as a fixed-width table
  brokkr sidecar <uuid> --samples                     # raw /proc sample stream (JSONL)
  brokkr sidecar <uuid> --samples --phase STAGE2      # samples within a marker phase
  brokkr sidecar <uuid> --markers                     # raw marker events (JSONL)
  brokkr sidecar <uuid> --durations                   # START/END pair timings
  brokkr sidecar <uuid> --counters                    # application counters
  brokkr sidecar <uuid> --stat rss                    # min/max/avg/p50/p95 for a field
  brokkr sidecar --compare a65a 911c                  # two results, phase-aligned

In depth: `brokkr man measure`.",
        group(ArgGroup::new("sample_view").args(&["samples", "stat"]).multiple(false)),
        group(ArgGroup::new("phased_view").args(&["samples", "stat", "counters"]).multiple(false)),
    )]
    Sidecar(SidecarCmd),
    /// Clean build artifacts and scratch data.
    ///
    /// Removes only what brokkr created, identified by a brokkr-designated
    /// directory or a constructed canonical name. The measurement stores
    /// (results.db, sidecar.db, history.db, ratatoskr's
    /// gate.db) are permanently out of scope.
    ///
    /// In depth: `brokkr man clean`.
    #[command(display_order = 5)]
    Clean(CleanCmd),
    /// Show lock status (who holds the benchmark lock)
    #[command(display_order = 6)]
    Lock,
    /// List cargo/rustc/build-script processes running outside brokkr (--kill reaps them)
    #[command(
        display_order = 6,
        long_about = "\
List every cargo-family process (cargo, rustc, rustdoc, clippy-driver,
build scripts) with no brokkr ancestor, attributed to whatever started it.

--kill: SIGKILL them, leaves first, plus rust-analyzer when it is the
starter (it would only re-run the cargo). A shell or editor starter is
never signalled. Every locked brokkr command does this reap on its own
once it holds the lock; this is the by-hand form."
    )]
    Strays(StraysCmd),
    /// Manage the rustc-wrapper fence that blocks cargo outside brokkr while the lock is held
    #[command(
        display_order = 6,
        long_about = "\
Manage the rustc-wrapper fence (`brokkr-rustc-guard`).

Enrolled as `build.rustc-wrapper` in $CARGO_HOME/config.toml, the guard
routes every rustc invocation this user's cargos make through a runtime
check: BROKKR_CARGO=1 passes, and so does a capability the current hold
handed out; otherwise a held brokkr lock refuses the compile with exit 1,
before anything builds. Admitted compilation holds a shared lease on
~/.brokkr/compile.lock, which a new hold drains before it measures.
The stray reap removes offenders after the fact - this prevents them.

The guarantee is conditional: it covers compilation that participates in
the lease protocol. Overrides, and guards that cannot reach compile.lock
at all, fail open and can overlap measured work.

Bare shows status. --install writes the config line (refusing to
overwrite a foreign wrapper such as sccache); --remove unsets it.
Enrollment invalidates cargo's compile fingerprint once per target dir."
    )]
    Guard(GuardCmd),
    /// Stop this project's brokkr commands: the queued ones and the lock holder
    #[command(
        display_order = 6,
        long_about = "\
Stop this project's brokkr commands: cancel the ones queued for the lock,
then ask the lock holder to wrap up cleanly when it is this project's.

The scope is the project key in brokkr.toml, so another checkout or a
--commit worktree of the same project is the same scope. Without a
brokkr.toml the scope is this state root's unconfigured commands; with a
brokkr.toml that does not load, bare kill refuses. --project NAME takes
another project's commands, --all every project's. A holder outside the
scope is left alone, and kill names it and the form that would stop it.

A queued command is cancelled through its queue record, without a
signal: it gives up its wait, never runs, and exits 130. It covers every
command that had printed its wait line before kill started.

Default for the holder: it is asked over its control socket and sends
itself SIGTERM. Brokkr catches it, forwards SIGTERM to the run child
(cargo for plain run, the program with --commit or --with), and waits
up to 1.5s before SIGKILLing that child. Run descendants receive no
direct signal from brokkr. Bench children receive SIGTERM as a process
group; after the leader exits or 1.5s, the group receives SIGKILL.
This gives a directly run program a bounded graceful stop; completion
of shutdown output is not guaranteed.

Brokkr flushes partial sidecar data (reachable via
`brokkr sidecar dirty`), discards the stopped
attempt's own scratch output, and releases the lock. No project
clean follows; diagnostics and kept run directories stay.

--hard: sends SIGKILL to an in-scope holder and its descendants
without any cleanup. Use when the default path is wedged; follow up
with `brokkr clean` manually. Queued commands are cancelled the same
way with or without it.

A `brokkr serve` server is not the lock holder once its start has
returned, so kill does not stop it; it lists the scope's live serves
as left running, each with the `brokkr serve --stop` that stops it
(SIGTERM to its process group, its grace, then SIGKILL). A serve
still starting holds the lock and is stopped like any holder.

Exits 0 when everything in scope was stopped or cancelled, including
when nothing was; 1 when a stop failed, or a holder or queued command
could not be placed in or out of the scope."
    )]
    Kill(KillCmd),
    /// Browse command history
    #[command(
        display_order = 7,
        long_about = "\
Browse the global command history log in brokkr's XDG data directory.

Every brokkr invocation is recorded with timing and exit status. Each line
starts with the entry's id, which `brokkr history <id>` takes. Timestamps
are UTC, and so are the --since/--until dates they are compared against.

Examples:
  brokkr history                        # last 25 entries
  brokkr history 1234                   # detail view for a single entry
  brokkr history -n 50                  # last 50
  brokkr history --all                  # everything
  brokkr history --command bench        # filter by command substring
  brokkr history --project pbfhogg      # filter by project
  brokkr history --failed               # only non-zero exit
  brokkr history --status 130           # filter by exit code (e.g. 130 = interrupt)
  brokkr history --since 2026-03-01     # from date (YYYY-MM-DD)
  brokkr history --until 2026-03-05     # through the end of that day
  brokkr history --slow 10000           # commands that took >10s"
    )]
    History(HistoryCmd),
    /// Delete results and sidecar data by UUID or commit prefix
    #[command(
        display_order = 8,
        long_about = "\
Delete rows from .brokkr/results.db (runs + FK children) and
.brokkr/sidecar.db (samples, markers, summary, counters, meta, latest)
matching a UUID prefix or commit prefix. Useful when a benchmark was
run under wrong pretences and the numbers are garbage.

Dry-run by default: lists the rows that would be deleted. Pass -f
to actually delete.

Examples:
  brokkr invalidate 0b74fb6f           # preview deletion by UUID prefix
  brokkr invalidate 0b74fb6f -f        # perform deletion
  brokkr invalidate --commit a65a      # preview: every row on commit a65a...
  brokkr invalidate --commit a65a -f   # perform"
    )]
    Invalidate(InvalidateCmd),
    /// Run visual tests against reference artifacts.
    ///
    /// In depth: `brokkr man visual`.
    #[command(display_order = 50)]
    Visual(VisualCmd),
    /// Run the cargo tests matching a name (release by default; --debug for dev)
    ///
    /// Every test harness of the package holding a match runs, each as its
    /// own process after one build, so a failing or crashing harness never
    /// hides the ones after it; every failure is listed, tagged with its
    /// harness.
    /// Always: --include-ignored and --nocapture. Test thread count comes
    /// from the selected profile when set, otherwise it defaults to one.
    /// Adds --release unless dev profile is selected. Profile precedence,
    /// per sweep: `--debug` / `--release` on the CLI, then the sweep's
    /// `[[check]] profile`, then `[test] debug` in brokkr.toml (default
    /// release). A sweep with `profile_required` is left out of a run whose
    /// flag asks for the other profile, rather than overridden.
    /// Sweep selection follows the configured profile or [[check]] entries,
    /// and defaults to one --all-features sweep when none are configured.
    /// Streams the test's own stdout/stderr live and prints a [test]
    /// PASS/FAIL footer with wall time per harness that ran it. Libtest's own framing
    /// (`running N tests`, `test foo ... ok`) is dropped - the test's
    /// output is the signal.
    ///
    /// The package passed to `cargo test -p` is resolved in order:
    ///   1. `-p/--package` on the command line
    ///   2. `[test] default_package` in brokkr.toml
    ///   3. The detected project's built-in package default, if it has one
    ///
    /// Projects without any of these (e.g. a workspace) must pass -p.
    ///
    /// Env vars exported to the test process:
    ///   BROKKR_TEST_BIN_DIR=<target>/{release,debug} - directory of any
    ///     `build_packages` artefacts rebuilt for this sweep. Tests that
    ///     spawn the just-rebuilt binary should read this rather than
    ///     guessing via `cfg!(debug_assertions)` (which lies under
    ///     `[profile.test]` overrides). Tracks --debug.
    ///
    /// Example:
    ///   brokkr test merge_basic_create_modify_delete_uring
    ///   brokkr test -p calendar extract_tag_value_flattens_nested_text
    ///   brokkr test roundtrip_uring_tiny_output -N 5
    ///   brokkr test some_unit_test --debug
    ///
    /// In depth: `brokkr man test`.
    #[command(display_order = 10)]
    Test(TestCmd),
    /// List fixtures/snapshots and approval state
    #[command(display_order = 50)]
    List,
    /// Record current output as accepted baseline (requires clean git tree)
    #[command(display_order = 50)]
    Approve(ApproveCmd),
    /// Show detailed results for a past run
    #[command(display_order = 50)]
    Report(ReportCmd),
    /// Show current state of all fixtures/snapshots
    #[command(name = "visual-status", display_order = 50)]
    VisualStatus,

    // ----- litehtml-only commands (display_order = 51) -----
    /// Normalize raw email HTML into a self-contained fixture
    #[command(display_order = 51)]
    Prepare(PrepareCmd),
    /// Extract a sub-fixture from a prepared HTML file
    #[command(name = "html-extract", display_order = 51)]
    HtmlExtract(HtmlExtractCmd),
    /// Print structural outline of a prepared HTML file
    #[command(display_order = 51)]
    Outline(OutlineCmd),

    // ----- sluggrs-only commands (display_order = 55) -----
    /// Rendering hotpath (defaults to --hotpath 1, use --alloc for allocation tracking)
    ///
    /// Builds a cargo example target and runs it through the bench harness.
    /// `--target hotpath` (the default) builds the `hotpath` example and files
    /// rows under the command name `render`; any other `--target NAME` builds
    /// the `NAME_bench` example and files under `NAME`.
    ///
    /// The mode selects the build: `--hotpath` adds the `hotpath` feature,
    /// `--alloc` adds `hotpath-alloc`, and `--bench` builds the example bare -
    /// the uninstrumented walls, the ones worth comparing across commits. With
    /// no mode flag this stays `--hotpath 1`, which is what a bare
    /// `brokkr hotpath` has always meant.
    ///
    /// In depth: `brokkr man hotpath`.
    #[command(name = "hotpath", display_order = 55)]
    Hotpath(HotpathCmd),

    // ----- mogwai-only commands (display_order = 58) -----
    /// Benchmark a mogwai surface (CLI invocation or harness target)
    ///
    /// Two kinds of surface, one row shape, no layers.
    ///
    /// ARGV-SHAPED surfaces go through the shipped bin and need no
    /// registration: `brokkr mogwai -- gen --type summary --seed 1`. Benching
    /// the release binary measures what ships, startup and argument parsing
    /// included.
    ///
    /// HARNESS-SHAPED surfaces have no command line, so the harness is the
    /// addressable thing: `brokkr mogwai screen_projection -- --cells 4`
    /// resolves `[mogwai.targets.screen_projection]` to a cargo example plus
    /// the features it must be built with. Those features are why `--hotpath`
    /// and `--alloc` produce a profile rather than an empty row.
    ///
    /// Bare `brokkr mogwai` lists both. Invocations are composed at the call
    /// site and captured verbatim, so selecting an arm is a query
    /// (`brokkr results --grep`) rather than a name lookup.
    ///
    /// In depth: `brokkr man mogwai`.
    #[command(name = "mogwai", display_order = 58)]
    Mogwai(MogwaiCmd),

    // ----- ratatoskr-only commands (display_order = 60) -----
    /// List or run Service-subprocess test scripts (deterministic harness)
    ///
    /// One command over the service-harness cohort, in brokkr's usual
    /// bare-is-an-index shape (`sync`, `results`, `man`, `deps`):
    /// `brokkr service` lists the discovered scripts, `brokkr service
    /// <script>` runs one (a directory runs that cohort), and
    /// `brokkr service --all` runs every discovered script against a
    /// single shared harness build. Replaced the former
    /// `service-test`/`service-suite`/`service-list` triple; the old
    /// names are gone, not aliased.
    ///
    /// Running builds the harness binary declared by `[ratatoskr.harness]`
    /// (its own build, independent of the `[[check]]` sweeps), allocates a
    /// per-run artefact dir at `.brokkr/ratatoskr/service/<test>/run-N/`,
    /// then spawns
    /// `<binary> --test-harness <SCRIPT>` with
    /// `BROKKR_HARNESS_ARTEFACT_DIR` and `BROKKR_TEST_BIN_DIR` set in
    /// the env (plus the `RATATOSKR_TEST_*_ENDPOINT` family when the
    /// script's frontmatter declares a `-- fixture:`). Captures
    /// stdout/stderr into the artefact dir alongside `run.toml` and a
    /// copy of the script; preserves the dir on failure, deletes it on
    /// success unless `--keep-artefacts` is set.
    ///
    /// `-N <count>` on a single script repeats it; on a directory or
    /// `--all` it runs the cohort `<count>` times in order (soak).
    /// Default is stop-on-first-failure so the failing artefacts land
    /// fast for triage; `--keep-going` runs everything and the summary
    /// lists the failures.
    ///
    /// In depth: `brokkr man service`.
    #[command(
        name = "service",
        display_order = 60,
        group = clap::ArgGroup::new("service_target").args(["script", "all"])
    )]
    Service(ServiceCmd),

    /// List, run, or bench sync-test scripts against sæhrimnir
    ///
    /// One command over the sync-harness cohort, in brokkr's usual
    /// bare-is-an-index shape (`results`, `man`, `deps`): `brokkr sync`
    /// lists the discovered scripts, `brokkr sync <script>` runs one
    /// (PASS/FAIL), and `brokkr sync <script> --bench [N]` measures it.
    ///
    /// With no SCRIPT, walks `[ratatoskr] sync_script_dir` (default
    /// `crates/app/tests/sync-harness`), parses top-of-file frontmatter
    /// (`description`, `expected`, `fixture`, `protocol`, `ceiling`) and
    /// prints a sorted table. The empty-state output names the expected
    /// directory so a fresh checkout gets a useful response.
    ///
    /// With a SCRIPT, builds the harness binary declared by
    /// `[ratatoskr.harness]`, spawns sæhrimnir against the script's
    /// `-- fixture: <NAME>` frontmatter, parses the per-protocol ports out
    /// of the readiness sentinel, then spawns `<harness binary>
    /// --test-harness <SCRIPT>` with `BROKKR_HARNESS_ARTEFACT_DIR` and
    /// `BROKKR_TEST_BIN_DIR` set plus one `RATATOSKR_TEST_<PROTO>_ENDPOINT`
    /// per protocol whose env-var spelling is configured under
    /// `[ratatoskr]`. Tears sæhrimnir down with a bounded SIGTERM grace period
    /// after the harness exits. PASS/FAIL on the harness binary's exit code; on
    /// FAIL the per-run dir at `.brokkr/ratatoskr/sync/<test>/run-N/` is
    /// preserved with `harness/`, `mock/`, and `run.toml` for triage.
    ///
    /// `--bench` keeps that spawn shape and measures it: sæhrimnir is
    /// spawned once and reused across iterations, and the harness binary
    /// runs N times with `BROKKR_MARKER_FIFO` set so the script can emit
    /// `SYNC_START` / `SYNC_END` markers around the measured region.
    /// Best-of-N is selected on that marker span (falls back to wall-clock
    /// elapsed when the script doesn't emit those markers); the best
    /// iteration's `summary.json` (if the script writes one into
    /// `BROKKR_HARNESS_ARTEFACT_DIR`) gets ingested as `meta.<key>` rows
    /// alongside the result. Stored in `.brokkr/results.db` via the
    /// standard `BenchHarness` so `brokkr results --compare` works.
    ///
    /// In depth: `brokkr man sync` (gates: `brokkr man gate`).
    #[command(name = "sync", display_order = 66)]
    Sync(SyncCmd),

    /// Spawn sæhrimnir against a fixture, print endpoints, run until Ctrl-C
    ///
    /// Manual-exploration tool for local fixture work. Resolves
    /// `[ratatoskr] mock_server_binary` and `[ratatoskr] fixtures_dir`
    /// from `brokkr.toml` (both required), spawns sæhrimnir with
    /// `--fixture <PATH> --readiness-file <PATH>`, waits for the
    /// readiness sentinel, prints the per-protocol listening
    /// endpoints, and runs until SIGINT/SIGTERM. SIGTERMs sæhrimnir on
    /// shutdown before escalating to SIGKILL after a bounded grace period.
    /// Auto-build of sæhrimnir is not yet wired - the binary must
    /// already exist at `mock_server_binary`.
    ///
    /// In depth: `brokkr man sync`.
    #[command(name = "mock-serve", display_order = 65)]
    MockServe(MockServeCmd),
}

// ---------------------------------------------------------------------------
// Per-command arguments: each subcommand's clap builder and extractor run
// in their own frame rather than inline in `Command`'s generated functions.
// ---------------------------------------------------------------------------

// `brokkr check`'s arguments; see `Command::Check`.
#[derive(Args)]
pub(crate) struct CheckCmd {
    /// Cargo features to enable
    #[arg(long, value_delimiter = ',')]
    pub(crate) features: Vec<String>,

    /// Disable default Cargo features
    #[arg(long)]
    pub(crate) no_default_features: bool,

    /// Target specific workspace packages (repeatable). Each sweep runs
    /// the subset its config admits; packages a sweep rules out are
    /// dropped with a note, and a sweep admitting none is skipped.
    #[arg(long, short)]
    pub(crate) package: Vec<String>,

    /// Validation profile from `[test.profiles]` in brokkr.toml.
    /// Selects sweeps, libtest filters (--include-ignored, --skip,
    /// --test-threads, etc.), and env vars for the test phase.
    /// Falls back to `[test] default_profile` if unset, then to
    /// today's single-sweep behaviour. Conflicts with `--features` /
    /// `--no-default-features` (those override the sweep set).
    #[arg(long, conflicts_with_all = ["features", "no_default_features"])]
    pub(crate) profile: Option<String>,

    /// Run the gate profile named by `[test] gate_profile` (validated
    /// at load time to certify "complete"). The stable pre-commit
    /// invocation: docs and hooks can say `brokkr check --gate` and
    /// survive profile renames. Conflicts with every flag that would
    /// narrow the run.
    #[arg(long, conflicts_with_all = ["profile", "features", "no_default_features", "package"])]
    pub(crate) gate: bool,

    /// Run the Rust phases (clippy, tests) even when the working tree
    /// holds nothing but markdown edits. Without it, such a tree runs
    /// the gremlins, textlint, bench and script_check phases only -
    /// documentation cannot change how the code builds, so clippy and
    /// the tests would be re-proving what the last full run already
    /// established on the same code. Implied by `--gate`, by a
    /// profile, and by any flag that shapes the build.
    #[arg(long)]
    pub(crate) force_rust: bool,

    /// Append one machine-readable summary line (a JSON object) as
    /// the last line of stdout: `schema`, `certifies`, `verdict`,
    /// `profile`, `sweeps`, `failed_phase`, `termination`, and under a
    /// complete profile `policy_coverage` and `execution_accounting`,
    /// then `elapsed_ms`. The object is versioned (`schema`) and
    /// additive within a version - consumers must tolerate unknown
    /// fields. Human output is unchanged; parse the final stdout line.
    #[arg(long)]
    pub(crate) json: bool,

    /// Before checking, rewrite banned Unicode in tracked source files
    /// with their ASCII equivalents (em/en dash -> `-`, smart quotes ->
    /// straight, NBSP -> space, zero-width/bidi deleted). Writes files
    /// in place.
    #[arg(long)]
    pub(crate) fix_gremlins: bool,

    /// After the check is otherwise done, print every test that ran in
    /// descending order by wall-clock time. Build time is excluded - timing
    /// starts when libtest emits the per-test start marker.
    #[arg(long)]
    pub(crate) timings: bool,

    /// Log the full `cargo clippy` / `cargo test` command for every
    /// sweep instead of the collapsed `<phase> <name>: <shape>` form.
    /// The default collapses because the command is mostly profile
    /// boilerplate repeated identically per sweep; a *failing* sweep
    /// reprints its command either way.
    #[arg(long)]
    pub(crate) commands: bool,

    /// Run only the named `[[textlint]]` rule (repeatable, or
    /// comma-separated), skipping every other phase. For iterating on
    /// one failing rule - the run is not a gate, so it refuses every
    /// flag that shapes or certifies one. Combines with `--script`.
    #[arg(long = "textlint", value_name = "NAME", value_delimiter = ',',
        conflicts_with_all = SELECTION_CONFLICTS)]
    pub(crate) textlint_names: Vec<String>,

    /// Run only the named `[[script_check]]` entry (repeatable, or
    /// comma-separated), whatever its stage or `when`, skipping every
    /// other phase.
    /// For iterating on one failing script - not a gate. Combines with
    /// `--textlint`.
    #[arg(long = "script", value_name = "NAME", value_delimiter = ',',
        conflicts_with_all = SELECTION_CONFLICTS)]
    pub(crate) script_names: Vec<String>,

    /// Explain the check this invocation would run, and run nothing: its
    /// plan (profile, lanes and their shapes, each phase's disposition,
    /// textlint rule file counts), then configuration findings - dead
    /// selector globs, `[[check]]` entries no profile references,
    /// `[[dependency_rule]]` and `[[quarantine]]` package selectors
    /// naming no workspace package, `allow_exact` paths that are absent.
    /// Accepts the flags that shape a run; builds nothing. Exits 0
    /// whatever it finds.
    #[arg(long, conflicts_with_all = AUDIT_CONFLICTS)]
    pub(crate) audit: bool,

    /// Raw arguments forwarded to the test phase. Tokens before a
    /// literal `--` are passed to `cargo test` (before cargo's own
    /// `--`); tokens after the second `--` are passed to libtest
    /// (after brokkr's default `--test-threads=1`, which a profile's
    /// `test_threads` replaces). Examples:
    /// `brokkr check -- --test read_paths` (cargo-level filter),
    /// `brokkr check -- -- --ignored` (libtest-level flag). Rejected
    /// under a `certifies = "complete"` profile: they narrow the test
    /// run but not the plan, which would then expect tests that never
    /// ran.
    ///
    /// The leading `--` is required: tokens after it are raw values
    /// for this positional either way, so neither `trailing_var_arg`
    /// nor `allow_hyphen_values` is needed - and without them an
    /// unrecognised brokkr flag is a clap error instead of being
    /// silently forwarded to cargo.
    pub(crate) args: Vec<String>,
}

// `brokkr clippy`'s arguments; see `Command::Clippy`.
#[derive(Args)]
pub(crate) struct ClippyCmd {
    /// Target package (repeatable). No `-p` uses cargo's default package
    /// selection: every member of a virtual workspace, or the root package
    /// of a package-rooted one.
    #[arg(long, short, value_name = "PKG")]
    pub(crate) package: Vec<String>,

    /// Enable all Cargo features. Allowed here because clippy is a probe,
    /// not a gate (the `[[check]]` `features = "all"` ban still stands).
    #[arg(long, conflicts_with_all = ["features", "no_default_features", "sweep"])]
    pub(crate) all_features: bool,

    /// Cargo features to enable (comma-separated).
    #[arg(long, value_delimiter = ',', conflicts_with = "sweep")]
    pub(crate) features: Vec<String>,

    /// Disable default Cargo features.
    #[arg(long, conflicts_with = "sweep")]
    pub(crate) no_default_features: bool,

    /// Borrow one `[[check]]` or `[[lint_sweep]]` entry's shape and run
    /// just its clippy invocation. Conflicts with the ad-hoc target flags.
    #[arg(long, value_name = "NAME", conflicts_with = "package")]
    pub(crate) sweep: Option<String>,

    /// Lint the library target alone (`--lib`) instead of the default
    /// `--all-targets`. Reproduces the lint surface of a plain `cargo
    /// build` / `cargo doc`, where an import used only from
    /// `#[cfg(test)]` code is dead - under `--all-targets` the test
    /// target keeps it alive and `unused_imports` never fires, so a
    /// tree the doc build rejects lints clean. Composes with `--sweep`.
    #[arg(long)]
    pub(crate) lib: bool,

    /// Extra env var KEY=VALUE (repeatable, non-empty KEY). Overrides the
    /// merged `[[check]]` env (ad-hoc) or the entry env (`--sweep`).
    #[arg(long, value_parser = validate_env_kv)]
    pub(crate) env: Vec<String>,
}

// `brokkr fmt`'s arguments; see `Command::Fmt`.
#[derive(Args)]
pub(crate) struct FmtCmd {
    /// Raw arguments forwarded to `cargo fmt`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}

// `brokkr run`'s arguments; see `Command::Run`.
#[derive(Args)]
pub(crate) struct RunCmd {
    /// Bin or example target name (bare form: `[bin] default`)
    // `value_terminator`: a `--` reached while NAME is still unfilled ends
    // NAME empty and starts ARGS, so `run -- ARGS` is the bare form and every
    // later `--` is forwarded literally.
    #[arg(value_terminator = "--")]
    pub(crate) name: Option<String>,

    #[command(flatten)]
    pub(crate) program: ProgramArgs,

    /// Raw arguments forwarded to the program (after `--`)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}

// `brokkr serve`'s arguments; see `Command::Serve`.
#[derive(Args)]
pub(crate) struct ServeCmd {
    /// Bin or example target to serve. Omit to list the live serves.
    #[arg(value_name = "TARGET", conflicts_with = "stop")]
    pub(crate) target: Option<String>,

    #[command(flatten)]
    pub(crate) program: ProgramArgs,

    /// The instance name, so one target can be served twice (two
    /// datasets on two ports). Default: the target's name. A start
    /// replaces the live serve of this name in this project.
    #[arg(long, value_name = "NAME", requires = "target")]
    pub(crate) name: Option<String>,

    /// Wait until a line of the server's output (stdout or stderr)
    /// contains TEXT before returning, failing and stopping it if it
    /// exits first or the wait runs out. A server that buffers its
    /// output may print it late.
    #[arg(long, value_name = "TEXT", requires = "target")]
    pub(crate) ready: Option<String>,

    /// How long `--ready` waits, in seconds (default 60)
    #[arg(long, value_name = "SECS", requires = "ready")]
    pub(crate) ready_timeout: Option<u64>,

    /// Seconds the server gets to exit after SIGTERM before its process
    /// group is SIGKILLed, on a stop or a replacement (default 5)
    #[arg(long, value_name = "SECS", requires = "target")]
    pub(crate) grace: Option<u64>,

    /// Stop a live serve: bare, this project's only one; or the one
    /// named WHICH in this project, or whose listed id starts with
    /// WHICH in any project.
    #[arg(long, value_name = "WHICH", num_args = 0..=1, default_missing_value = "")]
    pub(crate) stop: Option<String>,

    /// Raw arguments forwarded to the program, after `--`
    #[arg(last = true, requires = "target")]
    pub(crate) args: Vec<String>,
}

// `brokkr build`'s arguments; see `Command::Build`.
#[derive(Args)]
pub(crate) struct BuildCmd {
    /// Bin or example target name (bare form: `[bin] default`)
    pub(crate) name: Option<String>,

    /// Dev profile (overrides `[bin] debug`)
    #[arg(long, conflicts_with = "release")]
    pub(crate) debug: bool,

    /// Release profile (overrides `[bin] debug = true`)
    #[arg(long)]
    pub(crate) release: bool,

    /// Cargo profile by name: `dev`, `release` or a custom
    /// `[profile.<name>]` (overrides `[bin] profile`/`debug`)
    #[arg(long, value_name = "NAME", value_parser = crate::build::RustProfile::parse,
          conflicts_with_all = ["debug", "release"])]
    pub(crate) profile: Option<crate::build::RustProfile>,

    /// Cargo features to enable. Forwarded verbatim to `cargo build`,
    /// which validates them - comma- or space-separated, repeatable.
    #[arg(long, short = 'F', value_name = "LIST")]
    pub(crate) features: Vec<String>,

    /// Enable all Cargo features.
    #[arg(long, conflicts_with = "features")]
    pub(crate) all_features: bool,

    /// Disable default Cargo features.
    #[arg(long)]
    pub(crate) no_default_features: bool,

    /// Build this commit in a persistent worktree instead of the live
    /// tree. Remove the worktrees with `brokkr clean --worktrees`.
    #[arg(long, value_name = "REF")]
    pub(crate) commit: Option<String>,
}

// `brokkr install`'s arguments; see `Command::Install`.
#[derive(Args)]
pub(crate) struct InstallCmd {
    /// Install with `--debug` (overrides `[bin] debug`)
    #[arg(long, conflicts_with = "release")]
    pub(crate) debug: bool,

    /// Install release (overrides `[bin] debug = true`)
    #[arg(long)]
    pub(crate) release: bool,

    /// Cargo profile by name: `dev`, `release` or a custom
    /// `[profile.<name>]` (overrides `[bin] profile`/`debug`)
    #[arg(long, value_name = "NAME", value_parser = crate::build::RustProfile::parse,
          conflicts_with_all = ["debug", "release"])]
    pub(crate) profile: Option<crate::build::RustProfile>,

    /// Re-resolve dependencies instead of honouring Cargo.lock
    #[arg(long)]
    pub(crate) unlocked: bool,

    /// Print the bin names install would install, one per line, and stop:
    /// no build, no lock
    #[arg(long, conflicts_with_all = ["debug", "release", "profile", "unlocked"])]
    pub(crate) names: bool,
}

// `brokkr wc`'s arguments; see `Command::Wc`.
#[derive(Args)]
pub(crate) struct WcCmd {
    /// Only list files with more than this many lines.
    #[arg(default_value_t = crate::wc::DEFAULT_THRESHOLD)]
    pub(crate) threshold: usize,
}

// `brokkr man`'s arguments; see `Command::Man`.
#[derive(Args)]
pub(crate) struct ManCmd {
    /// Topic to read; omit to list the topics available here.
    pub(crate) topic: Option<String>,
    /// Sections of the topic to read; omit to list the topic's sections.
    /// Each is matched exactly, then by prefix, then as a substring.
    /// Several are rendered together in document order.
    pub(crate) sections: Vec<String>,
    /// Render the whole topic even when it is long enough to be indexed.
    #[arg(long)]
    pub(crate) full: bool,
}

// `brokkr crit`'s arguments; see `Command::Crit`.
#[derive(Args)]
pub(crate) struct CritCmd {
    /// Bench target to run. Omit for the index.
    #[arg(value_name = "TARGET")]
    pub(crate) target: Option<String>,

    /// Measure this commit in a persistent worktree rather than the
    /// working tree.
    #[arg(long, value_name = "REF")]
    pub(crate) commit: Option<String>,

    /// Compare two stored baselines, reading B against A. Samples
    /// nothing - both sides come from disk.
    #[arg(long, num_args = 2, value_names = ["A", "B"])]
    pub(crate) compare: Option<Vec<String>>,

    /// List the baselines recorded for this project.
    #[arg(long)]
    pub(crate) baselines: bool,

    /// Name for the baseline this run saves. Required when the tree is
    /// dirty, since no commit identifies what was measured.
    #[arg(long, value_name = "LABEL")]
    pub(crate) name: Option<String>,

    /// Compare baselines whose build environments differ, warning
    /// instead of refusing.
    #[arg(long)]
    pub(crate) lenient: bool,

    /// Arguments forwarded verbatim to the criterion harness
    /// (`--sample-size`, `--measurement-time`, `--noise-threshold`, ...).
    #[arg(last = true)]
    pub(crate) args: Vec<String>,
}

// `brokkr bench`'s arguments; see `Command::Bench`.
#[derive(Args)]
pub(crate) struct BenchCmd {
    /// Build and run this commit, in a persistent worktree, with the
    /// working tree's benches. With --rounds, given more than once, the
    /// commits are the competitors.
    #[arg(long, value_name = "REF")]
    pub(crate) commit: Vec<String>,

    /// Iterations to run; defaults to the bench setting, otherwise 3.
    /// The result is the fastest. With --rounds, per cell, default 1.
    #[arg(long, value_name = "N")]
    pub(crate) iterations: Option<std::num::NonZeroU32>,

    /// A/B: run the competitors N times as interleaved rounds, the order
    /// reversed every other round, each cell its own run. The
    /// competitors are --versus's values, several --commit, or NAME as
    /// a comma list of benches.
    #[arg(long, value_name = "N")]
    pub(crate) rounds: Option<std::num::NonZeroU32>,

    /// With --rounds: the param whose values are the competitors; any
    /// other param that fans out is the workload.
    #[arg(long, value_name = "PARAM", requires = "rounds")]
    pub(crate) versus: Option<String>,

    /// Profile: build with the bench's hotpath_features and require
    /// every call marked hotpath to capture its report.
    #[arg(long, conflicts_with = "alloc")]
    pub(crate) hotpath: bool,

    /// Profile allocations: build with the bench's alloc_features and
    /// require every call marked hotpath to capture its report.
    #[arg(long)]
    pub(crate) alloc: bool,

    /// Check the invocation and stop: bind and validate every arm,
    /// resolve the commit, check the inputs and their pins, then print
    /// each arm. Builds, preflights, holds and stores nothing, and takes
    /// no lock, so a queue of benches can be checked while one runs.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Run the project's contract tests, the Lua files under
    /// bench_tests, against scripted fixtures: no build, no data, no
    /// lock. `brokkr check`'s bench phase runs them too.
    #[arg(long, conflicts_with_all = ["dry_run", "hotpath", "alloc", "commit", "rounds"])]
    pub(crate) test: bool,

    /// The bench to run, then its own params; clap reads nothing after
    /// NAME, `--help` included. Omit for the index.
    #[arg(value_name = "NAME [PARAMS]", trailing_var_arg = true)]
    pub(crate) bench: Vec<String>,
}

// `brokkr deps`'s arguments; see `Command::Deps`.
#[derive(Args)]
pub(crate) struct DepsCmd {
    /// Emit NDJSON events on stdout (one JSON object per line). No
    /// prefixed log output, no terminal colors.
    #[arg(long)]
    pub(crate) json: bool,

    /// Always exit 0, even when findings exist. Useful for
    /// report-only invocations in CI.
    #[arg(long)]
    pub(crate) no_fail: bool,

    /// Optional package spec (`name` or `name@version`). When set,
    /// suppress the other phases and print every Normal-kind chain
    /// from a workspace member down to the named package. Useful
    /// for answering "who is pulling in this crate?" and for
    /// disambiguating one specific version of a duplicate.
    #[arg(value_name = "PKG")]
    pub(crate) focus: Option<String>,
}

// `brokkr passthrough`'s arguments; see `Command::Passthrough`.
#[derive(Args)]
pub(crate) struct PassthroughCmd {
    /// Cargo features to enable (e.g. linux-io-uring)
    #[arg(long, value_delimiter = ',')]
    pub(crate) features: Vec<String>,
    /// Print machine-readable timing line (key=value pairs)
    #[arg(long)]
    pub(crate) time: bool,
    /// Print machine-readable JSON timing summary
    #[arg(long)]
    pub(crate) json: bool,
    /// Number of times to run the command (build happens once)
    #[arg(long, default_value_t = std::num::NonZeroUsize::MIN)]
    pub(crate) runs: std::num::NonZeroUsize,
    /// Skip build step and run existing release binary
    #[arg(long)]
    pub(crate) no_build: bool,
    /// Arguments passed to the binary
    #[arg(last = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}

// `brokkr results`'s arguments; see `Command::Results`.
#[derive(Args)]
pub(crate) struct ResultsCmd {
    /// UUID prefix to look up specific result(s)
    #[arg(conflicts_with_all = ["commit", "compare"])]
    pub(crate) query: Option<String>,

    /// Show results for a specific commit (prefix match)
    #[arg(long, conflicts_with = "compare")]
    pub(crate) commit: Option<String>,

    /// Compare two commits side-by-side
    #[arg(long, num_args = 2, value_names = ["COMMIT_A", "COMMIT_B"])]
    pub(crate) compare: Option<Vec<String>>,

    /// Summarize one `brokkr bench --rounds` invocation by its id
    /// prefix: each competitor's median and per-round sign, and every
    /// planned cell without a result. Takes no filter.
    #[arg(long, value_name = "ID", conflicts_with_all = [
        "query", "commit", "compare", "command", "mode", "dataset", "meta", "env",
        "param", "grep", "grep_v",
    ])]
    pub(crate) invocation: Option<String>,

    /// Filter by command (substring match, e.g. "check" matches "check-refs")
    #[arg(long)]
    pub(crate) command: Option<String>,

    /// Filter by measurement mode (substring match; exact values are
    /// `bench`, `hotpath`, `alloc`). `--variant` accepted as a legacy
    /// alias for muscle memory from the pre-rename days.
    #[arg(long, alias = "variant")]
    pub(crate) mode: Option<String>,

    /// Filter by dataset (substring match on input filename, e.g. "europe"
    /// or "eu" matches "europe-20260301-seq4714-with-indexdata.osm")
    #[arg(long)]
    pub(crate) dataset: Option<String>,

    /// Filter by metadata key=value (multiple allowed, AND semantics).
    /// The key is the user-facing name without the `meta.` prefix
    /// (e.g. `--meta format=osc` matches rows with `meta.format = "osc"`).
    /// Rows missing the key are silently excluded.
    #[arg(long, value_parser = validate_meta_filter)]
    pub(crate) meta: Vec<String>,

    /// Filter by captured env var KEY=VALUE (multiple allowed, AND
    /// semantics). Keys are bare env var names (no `env.` prefix),
    /// e.g. `--env PBFHOGG_USE_NEW_PATH=1`. Rows without the key
    /// are excluded - use an explicit baseline value (e.g. `=0`)
    /// on the off runs rather than relying on absence.
    #[arg(long, value_parser = validate_meta_filter)]
    pub(crate) env: Vec<String>,

    /// Filter script-bench runs by a bound param, NAME=VALUE (multiple
    /// allowed, AND semantics). VALUE is the param's value as the table
    /// lists it: a collection's keys joined by `,` in bound order.
    /// Typed rows carry no params, so this excludes all of them.
    #[arg(long, value_parser = validate_param_filter)]
    pub(crate) param: Vec<String>,

    /// Substring match against the run's invocation: the subprocess
    /// invocation (`cli_args`), the brokkr invocation (`brokkr_args`), or a
    /// captured env var as `NAME=VALUE`. Repeatable - each `--grep` must
    /// match (AND). `git log --grep` style. E.g. `--grep apply-changes
    /// --grep zstd:1 --grep uring` to find apply-changes runs that used
    /// both zstd:1 and io_uring, or `--grep LAYER_STATS=1` for an
    /// env-gated arm. Use `--env` for an exact key=value match.
    #[arg(long)]
    pub(crate) grep: Vec<String>,

    /// Inverse of `--grep`: exclude rows whose invocation contains the
    /// term, across the same three sources (`cli_args`, `brokkr_args`,
    /// captured env). Repeatable - a row is excluded if it matches ANY term.
    /// Composes with `--grep`. The A/B case: `--grep apply-changes
    /// --grep-v uring` selects the arm distinguished only by an absent
    /// flag, which `--grep` alone cannot express.
    #[arg(long = "grep-v")]
    pub(crate) grep_v: Vec<String>,

    /// Maximum number of results to show
    #[arg(long, short = 'n', default_value = "20")]
    pub(crate) limit: usize,

    /// Maximum number of functions shown in hotpath reports (0 = all)
    #[arg(long, default_value_t = crate::hotpath_fmt::DEFAULT_TOP)]
    pub(crate) top: usize,

    /// List and compare script-bench runs that marks hide (a retired
    /// bench's) too, each marked. A uuid lookup shows them regardless.
    #[arg(long)]
    pub(crate) hidden: bool,

    /// Retire a script bench: its runs, and any published under the
    /// name later, are left out of listings and --compare. Stored in the
    /// bench store, so the bench may then leave brokkr.toml. Needs
    /// --reason.
    #[arg(long, value_name = "NAME", requires = "reason")]
    pub(crate) retire: Option<String>,

    /// Undo --retire NAME.
    #[arg(long, value_name = "NAME")]
    pub(crate) unretire: Option<String>,

    /// Invalidate one script-bench run, named by a uuid prefix that
    /// names exactly one run of this project: it is left out of listings
    /// and --compare. Needs --reason.
    #[arg(long, value_name = "UUID", requires = "reason")]
    pub(crate) invalidate: Option<String>,

    /// Undo --invalidate UUID.
    #[arg(long, value_name = "UUID")]
    pub(crate) revalidate: Option<String>,

    /// Rename a script bench: runs stored under OLD, or under any name
    /// renamed to it, list, filter, pair and compare as NEW, so the
    /// bench's table may be renamed in brokkr.toml. OLD is the name
    /// runs resolve to now, the end of its chain; NEW must be a name the
    /// store does not know yet, or one renamed into OLD, which undoes
    /// that path: renaming back is the undo.
    #[arg(long, num_args = 2, value_names = ["OLD", "NEW"])]
    pub(crate) rename: Option<Vec<String>>,

    /// Why, for --retire or --invalidate.
    #[arg(long, requires = "reasoned")]
    pub(crate) reason: Option<String>,
}

// `brokkr sidecar`'s arguments; see `Command::Sidecar`.
#[derive(Args)]
pub(crate) struct SidecarCmd {
    /// UUID prefix to look up (required; use `brokkr results` to find one)
    #[arg(required_unless_present = "compare")]
    pub(crate) query: Option<String>,

    /// Raw /proc samples as JSONL (one record per 100ms sample)
    #[arg(long, conflicts_with_all = ["markers", "durations", "counters", "stat", "compare"])]
    pub(crate) samples: bool,

    /// Raw marker events as JSONL
    #[arg(long, conflicts_with_all = ["samples", "durations", "counters", "stat", "compare"])]
    pub(crate) markers: bool,

    /// START/END marker-pair durations
    #[arg(long, conflicts_with_all = ["samples", "markers", "counters", "stat", "compare"])]
    pub(crate) durations: bool,

    /// Application-level counters
    #[arg(long, conflicts_with_all = ["samples", "markers", "durations", "stat", "compare", "stalls"])]
    pub(crate) counters: bool,

    /// Roll up cumulative `*_wait_ns` counters by category.
    ///
    /// Convention: a target accumulates blocking time per category into a
    /// strictly-monotonic counter named `<category>_wait_ns` (one atomic add
    /// per blocking event). This view takes the max per name, strips the
    /// `_wait_ns` suffix for the category, and reports total stall time as a
    /// fraction of run wall-clock - which can exceed 100% for waits summed
    /// across concurrent threads (it's the avg threads parked in that
    /// category). Runs with no `*_wait_ns` counters produce an informative
    /// empty result.
    #[arg(long, conflicts_with_all = ["samples", "markers", "durations", "counters", "stat", "compare"])]
    pub(crate) stalls: bool,

    /// Compute min/max/avg/p50/p95 for a /proc field (e.g. `--stat rss`)
    #[arg(long, conflicts_with_all = ["samples", "markers", "durations", "counters", "compare"])]
    pub(crate) stat: Option<String>,

    /// Compare two results phase-by-phase (no UUID argument). A
    /// script-bench side may name its iteration and child,
    /// `UUID/ITER/CHILD`; bare, it is the result's iteration and its only
    /// child.
    #[arg(long, num_args = 2, value_names = ["UUID_A", "UUID_B"],
          conflicts_with_all = ["query", "samples", "markers", "durations", "counters", "stat"])]
    pub(crate) compare: Option<Vec<String>>,

    /// Render as a fixed-width table instead of JSONL. Applies to the
    /// default view, --durations, --counters, and --compare. No-op for
    /// --samples / --markers / --stat (always JSONL).
    #[arg(long)]
    pub(crate) human: bool,

    /// Show a specific run index (0-based), or "all" for all runs. Defaults to the best run.
    /// For a script-bench run, the iteration: by default its result's, or
    /// the last one for a run without a result.
    #[arg(long)]
    pub(crate) run: Option<String>,

    /// For a script-bench run, the child (0-based, launch order) of each
    /// selected iteration. Defaults to the iteration's only child; needed
    /// when it launched several. Refused for a typed run.
    #[arg(long, value_name = "N", conflicts_with = "compare")]
    pub(crate) child: Option<usize>,

    /// Filter to a marker phase (e.g. "STAGE2"). Requires --samples,
    /// --stat, or --counters.
    #[arg(long, requires = "phased_view")]
    pub(crate) phase: Option<String>,

    /// Keep only counters whose name contains this substring (e.g.
    /// `--grep s1a_`). Requires --counters. A run emitting a progress
    /// counter every 64 blobs otherwise buries the ~30 lines that
    /// matter under a full dump.
    #[arg(long, requires = "counters", value_name = "SUBSTR")]
    pub(crate) grep: Option<String>,

    /// Restrict the view to one call stream of a library-harness run
    /// (the `%<call_id>` tag on its FIFO lines). With --samples or
    /// --stat it needs --phase, resolved in that call's markers; the
    /// samples stay process-wide.
    #[arg(long, value_name = "CALL_ID", conflicts_with = "compare")]
    pub(crate) call: Option<String>,

    /// Filter samples by time range in seconds (e.g. "10.0..82.0"). Requires --samples or --stat.
    #[arg(long, requires = "sample_view")]
    pub(crate) range: Option<String>,

    /// Filter samples where a field meets a condition (e.g. "majflt>0"). Requires --samples or --stat.
    #[arg(long, name = "COND", requires = "sample_view")]
    pub(crate) r#where: Option<String>,

    /// Output only these fields (comma-separated, e.g. "t,rss,anon,majflt"). Only with --samples.
    #[arg(
        long,
        value_delimiter = ',',
        requires = "samples",
        conflicts_with = "stat"
    )]
    pub(crate) fields: Vec<String>,

    /// Output every Nth sample (downsample). Requires --samples or --stat.
    #[arg(long, requires = "sample_view")]
    pub(crate) every: Option<usize>,

    /// Output only the first N samples. Requires --samples or --stat.
    #[arg(long, requires = "sample_view")]
    pub(crate) head: Option<usize>,

    /// Output only the last N samples. Requires --samples or --stat.
    #[arg(long, requires = "sample_view")]
    pub(crate) tail: Option<usize>,
}

// `brokkr clean`'s arguments; see `Command::Clean`.
#[derive(Args)]
pub(crate) struct CleanCmd {
    /// Also remove all persistent benchmark worktrees
    /// (sibling `.brokkr-worktree-<project>-*` dirs created by --commit).
    #[arg(long)]
    pub(crate) worktrees: bool,

    /// Also run `cargo clean -p <PKG>` - wipe this project's own build
    /// artifacts (all profiles) while keeping dependency artifacts
    /// cached. The fix for stale-incremental linker failures. PKG
    /// defaults to the brokkr.toml project name; pass a value to clean
    /// a different package (e.g. `--cargo pbfhogg-cli`).
    #[arg(long, num_args = 0..=1, value_name = "PKG")]
    pub(crate) cargo: Option<Option<String>>,

    /// Retired with elivagar's commands: refused with a pointer to
    /// elivagar-devkit, which prunes its archive store now.
    #[arg(long, hide = true)]
    pub(crate) archives: bool,

    /// Retired with --archives, refused the same way.
    #[arg(long, hide = true)]
    pub(crate) keep: Option<usize>,

    /// Everything: --worktrees + --cargo (default package).
    #[arg(long)]
    pub(crate) all: bool,

    /// List what would be removed without deleting anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

// `brokkr strays`'s arguments; see `Command::Strays`.
#[derive(Args)]
pub(crate) struct StraysCmd {
    /// SIGKILL the strays instead of only listing them.
    #[arg(long)]
    pub(crate) kill: bool,
}

// `brokkr guard`'s arguments; see `Command::Guard`.
#[derive(Args)]
pub(crate) struct GuardCmd {
    /// Write build.rustc-wrapper to $CARGO_HOME/config.toml.
    #[arg(long)]
    pub(crate) install: bool,

    /// Remove the guard's build.rustc-wrapper entry.
    #[arg(long)]
    pub(crate) remove: bool,
}

// `brokkr kill`'s arguments; see `Command::Kill`.
#[derive(Args)]
pub(crate) struct KillCmd {
    /// SIGKILL an in-scope holder immediately (brokkr + child) without graceful cleanup.
    #[arg(long)]
    pub(crate) hard: bool,
    /// Stop the commands of the project with this brokkr.toml key instead.
    #[arg(long, value_name = "NAME", conflicts_with = "all")]
    pub(crate) project: Option<String>,
    /// Stop every project's commands.
    #[arg(long)]
    pub(crate) all: bool,
}

// `brokkr history`'s arguments; see `Command::History`.
#[derive(Args)]
pub(crate) struct HistoryCmd {
    /// Look up a single history entry by id (shown in the leftmost column of the default view)
    pub(crate) id: Option<i64>,

    /// Filter by command (substring match)
    #[arg(long, conflicts_with = "id")]
    pub(crate) command: Option<String>,

    /// Filter by project name
    #[arg(long, conflicts_with = "id")]
    pub(crate) project: Option<String>,

    /// Show only failed commands (non-zero exit)
    #[arg(long, conflicts_with_all = ["id", "status"])]
    pub(crate) failed: bool,

    /// Filter by exact exit status code
    #[arg(long, conflicts_with = "id")]
    pub(crate) status: Option<i32>,

    /// Show entries from this UTC date onward (YYYY-MM-DD or YYYY-MM-DD HH:MM:SS)
    #[arg(long, value_parser = validate_since, conflicts_with = "id")]
    pub(crate) since: Option<String>,

    /// Show entries up to this UTC date (YYYY-MM-DD, inclusive of that
    /// whole day, or YYYY-MM-DD HH:MM:SS)
    #[arg(long, value_parser = validate_since, conflicts_with = "id")]
    pub(crate) until: Option<String>,

    /// Show commands that took at least this many milliseconds
    #[arg(long, conflicts_with = "id")]
    pub(crate) slow: Option<i64>,

    /// Maximum number of entries to show
    #[arg(long, short = 'n', default_value = "25", conflicts_with_all = ["all", "id"])]
    pub(crate) limit: usize,

    /// Show all entries (ignores -n)
    #[arg(long, conflicts_with = "id")]
    pub(crate) all: bool,
}

// `brokkr invalidate`'s arguments; see `Command::Invalidate`.
#[derive(Args)]
pub(crate) struct InvalidateCmd {
    /// UUID prefix to invalidate (mutually exclusive with --commit)
    #[arg(
        value_name = "UUID",
        conflicts_with = "commit",
        required_unless_present = "commit"
    )]
    pub(crate) uuid: Option<String>,

    /// Commit hash prefix - invalidate every result rooted at matching commits
    #[arg(long)]
    pub(crate) commit: Option<String>,

    /// Actually delete (without this flag, the command is a dry-run)
    #[arg(short = 'f', long)]
    pub(crate) force: bool,
}

// `brokkr visual`'s arguments; see `Command::Visual`.
#[derive(Args)]
pub(crate) struct VisualCmd {
    #[command(flatten)]
    pub(crate) args: VisualArgs,
    #[command(flatten)]
    pub(crate) litehtml: LitehtmlVisualArgs,
}

// `brokkr test`'s arguments; see `Command::Test`.
#[derive(Args)]
pub(crate) struct TestCmd {
    /// Exact test name to run (substring filter, case-sensitive).
    /// A name matching every test in the package (blank, `_`, ...) is
    /// refused - run the whole suite with `brokkr check -p`.
    #[arg(value_parser = validate_test_name)]
    pub(crate) name: String,
    /// Cargo package to test (`cargo test -p <pkg>`). Overrides the
    /// project default and any `[test] default_package` in brokkr.toml.
    #[arg(short = 'p', long = "package")]
    pub(crate) package: Option<String>,
    /// Repeat the test this many times per sweep (flaky-test hunting)
    #[arg(short = 'N', long = "repeat", default_value_t = std::num::NonZeroU32::MIN)]
    pub(crate) repeat: std::num::NonZeroU32,
    /// Parallel cargo compile jobs (`cargo test -j N`)
    #[arg(short = 'j', long = "jobs")]
    pub(crate) jobs: Option<u32>,
    /// Build and run the test in dev profile instead of release.
    /// BROKKR_TEST_BIN_DIR points at <target>/debug accordingly.
    /// Overrides `[test] debug` and a sweep's `profile` from brokkr.toml;
    /// a sweep requiring release (`profile_required`) is skipped.
    #[arg(long, conflicts_with = "release")]
    pub(crate) debug: bool,
    /// Force the release profile, overriding `[test] debug = true` and a
    /// sweep's `profile` from brokkr.toml; a sweep requiring dev
    /// (`profile_required`) is skipped. Mutually exclusive with --debug.
    #[arg(long)]
    pub(crate) release: bool,
    /// Override the per-test watchdog ceiling, in seconds. Without this
    /// flag the ceiling is the standard per-test cap.
    ///
    /// This is the ONLY exception to brokkr's per-test cap, and the
    /// parser enforces the runner's supported maximum. Exceeding the cap - raised
    /// or not - fails the run and stops it. The flag also makes
    /// attribution exact: `<name>` is resolved to every full test name
    /// it matches, and each runs as its own process with libtest
    /// `--exact`, so the process is the unit of one test. Note it bounds
    /// that whole process - startup, the test, teardown - not the test
    /// body alone.
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..=crate::test_runner::ceilings::MAX_TEST_TIMEOUT.as_secs()))]
    pub(crate) timeout: Option<u64>,
    /// Run only the named sweep from the resolved `default_profile`
    /// (e.g. `--sweep all` or `--sweep consumer`), instead of every
    /// sweep the profile lists. Errors if no sweep carries that label.
    #[arg(long, value_name = "LABEL")]
    pub(crate) sweep: Option<String>,
    /// After the run, print every test that ran in descending order by
    /// wall-clock time, as `brokkr check --timings` does. Build time is
    /// excluded - timing starts when libtest emits the per-test start
    /// marker. Doctests are not timed.
    #[arg(long)]
    pub(crate) timings: bool,
}

// `brokkr approve`'s arguments; see `Command::Approve`.
#[derive(Args)]
pub(crate) struct ApproveCmd {
    /// Fixture/snapshot IDs (or unique prefixes). Repeatable.
    pub(crate) fixture: Vec<String>,

    /// Approve every configured fixture/snapshot
    #[arg(long, conflicts_with = "fixture")]
    pub(crate) all: bool,
}

// `brokkr report`'s arguments; see `Command::Report`.
#[derive(Args)]
pub(crate) struct ReportCmd {
    /// Run ID (or prefix)
    pub(crate) run_id: String,
}

// `brokkr prepare`'s arguments; see `Command::Prepare`.
#[derive(Args)]
pub(crate) struct PrepareCmd {
    /// Input HTML file (raw email)
    pub(crate) input: String,
    /// Output HTML file (self-contained fixture)
    pub(crate) output: String,
}

// `brokkr html-extract`'s arguments; see `Command::HtmlExtract`.
#[derive(Args)]
pub(crate) struct HtmlExtractCmd {
    /// Input HTML file (already prepared)
    pub(crate) input: String,
    /// CSS selector to extract (single element)
    #[arg(long, conflicts_with_all = ["from", "to"])]
    pub(crate) selector: Option<String>,
    /// Start of sibling range to extract (inclusive)
    #[arg(long, requires = "to", conflicts_with = "selector")]
    pub(crate) from: Option<String>,
    /// End of sibling range to extract (inclusive)
    #[arg(long, requires = "from", conflicts_with = "selector")]
    pub(crate) to: Option<String>,
    /// Output HTML file (extracted sub-fixture)
    pub(crate) output: String,
}

// `brokkr outline`'s arguments; see `Command::Outline`.
#[derive(Args)]
pub(crate) struct OutlineCmd {
    /// Input HTML file (prepared)
    pub(crate) input: String,
    /// Maximum nesting depth before collapsing
    #[arg(long, default_value_t = DEFAULT_OUTLINE_DEPTH)]
    pub(crate) depth: usize,
    /// Show full tree with no depth limit
    #[arg(long)]
    pub(crate) full: bool,
    /// Print suggested CSS selectors for top-level sections
    #[arg(long)]
    pub(crate) selectors: bool,
}

// `brokkr hotpath`'s arguments; see `Command::Hotpath`.
#[derive(Args)]
pub(crate) struct HotpathCmd {
    #[command(flatten)]
    pub(crate) mode: ModeArgs,

    /// Example binary to build and run (default: hotpath)
    #[arg(long, default_value = "hotpath")]
    pub(crate) target: String,
}

// `brokkr mogwai`'s arguments; see `Command::Mogwai`.
#[derive(Args)]
pub(crate) struct MogwaiCmd {
    #[command(flatten)]
    pub(crate) mode: ModeArgs,

    /// Harness target from `[mogwai.targets.*]`; omit for the CLI (bare lists both)
    // `value_terminator`: as `RunCmd::name`, a `--` before any TARGET leaves
    // it empty, so `mogwai -- ARGS` reaches the CLI surface.
    #[arg(value_terminator = "--")]
    pub(crate) target: Option<String>,

    /// Arguments forwarded to the binary or harness (after `--`)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}

// `brokkr service`'s arguments; see `Command::Service`.
#[derive(Args)]
pub(crate) struct ServiceCmd {
    /// Path to a Lua test script, or a directory under
    /// `crates/app/tests/service-harness/` to run as a cohort.
    /// Omit to list the discovered scripts.
    pub(crate) script: Option<String>,

    /// Run every discovered script in sequence against a single
    /// shared harness build. Mutually exclusive with SCRIPT.
    #[arg(long, conflicts_with = "script")]
    pub(crate) all: bool,

    /// Substring filter against the script's relative name (e.g.
    /// `t1/` to run only the T1 cohort). Only meaningful with
    /// `--all`; a SCRIPT directory scopes the same way.
    #[arg(long, value_name = "SUBSTRING", requires = "all")]
    pub(crate) filter: Option<String>,

    /// Include scripts marked `expected = ignored` in the
    /// frontmatter. By default these are skipped (they reproduce
    /// known-broken Service behaviour and would block clean runs).
    #[arg(long, requires = "all")]
    pub(crate) include_ignored: bool,

    /// Preserve the artefact directory even on success
    #[arg(long, requires = "service_target")]
    pub(crate) keep_artefacts: bool,

    /// Build the harness binary with the dev profile (`<target>/debug/`).
    /// Default is release for parity with `brokkr test` and to match
    /// what users will run in production. Overrides
    /// `[ratatoskr.harness] debug` from `brokkr.toml`.
    #[arg(long, conflicts_with = "release", requires = "service_target")]
    pub(crate) debug: bool,

    /// Force the release profile, overriding `[ratatoskr.harness] debug`
    /// from `brokkr.toml`. Mutually exclusive with `--debug`.
    #[arg(long, requires = "service_target")]
    pub(crate) release: bool,

    /// Build the harness under this cargo profile: `dev`, `release` or
    /// a custom `[profile.<name>]` (`<target>/<name>/`). Overrides
    /// `[ratatoskr.harness] profile`/`debug`.
    #[arg(long, value_name = "NAME", value_parser = crate::build::RustProfile::parse,
          conflicts_with_all = ["debug", "release"], requires = "service_target")]
    pub(crate) profile: Option<crate::build::RustProfile>,

    /// Repeat count. For a single script: number of iterations
    /// (each gets its own `run-N/`). For a directory or `--all`:
    /// cohort cycles, where each cycle invokes every selected
    /// script once. Build is shared across iterations.
    #[arg(
        short = 'N',
        long = "repeat",
        default_value_t = std::num::NonZeroU32::MIN,
        value_name = "COUNT",
        requires = "service_target"
    )]
    pub(crate) repeat: std::num::NonZeroU32,

    /// Keep going after a failed iteration. Default is to stop on
    /// the first failure so the artefact dir lands fast for triage.
    #[arg(long, requires = "service_target")]
    pub(crate) keep_going: bool,
}

// `brokkr sync`'s arguments; see `Command::Sync`.
#[derive(Args)]
pub(crate) struct SyncCmd {
    /// Path to a sync-test `.lua` script (frontmatter must declare
    /// `-- fixture: <NAME>`). Omit to list the discovered scripts.
    pub(crate) script: Option<String>,

    /// Run every discovered script instead of one, in discovery
    /// order, reporting each. Keeps going past a failure and exits
    /// non-zero if any failed. Mutually exclusive with SCRIPT.
    #[arg(long, conflicts_with = "script")]
    pub(crate) all: bool,

    /// Substring filter against the script's discovered name, e.g.
    /// `jmap` or `t1/`. Only meaningful with `--all`.
    #[arg(long, value_name = "SUBSTRING", requires = "all")]
    pub(crate) filter: Option<String>,

    /// Include scripts whose frontmatter says `expected: ignored`.
    /// They reproduce known-broken behaviour, so `--all` skips them
    /// by default.
    #[arg(long, requires = "all")]
    pub(crate) include_ignored: bool,

    /// Measure instead of just running: N iterations, best-of-N on the
    /// `SYNC_START`/`SYNC_END` marker span, recorded in `results.db`.
    /// Bare `--bench` means the default run count. Needs a SCRIPT, or
    /// `--gate all`.
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = DEFAULT_BENCH_RUNS,
        value_name = "COUNT",
        conflicts_with = "all"
    )]
    pub(crate) bench: Option<std::num::NonZeroUsize>,

    /// Allow recording on a dirty git tree (a gated run that records a
    /// row tags it `dirty`; `results.db` still skips it). Requires
    /// `--bench`.
    #[arg(long, requires = "bench")]
    pub(crate) force: bool,

    /// Preserve the artefact directory even on success
    #[arg(long)]
    pub(crate) keep_artefacts: bool,

    /// Build the harness binary with the dev profile (`<target>/debug/`).
    /// Default is release for parity with what users will run.
    /// Overrides `[ratatoskr.harness] debug` from `brokkr.toml`.
    #[arg(long, conflicts_with = "release")]
    pub(crate) debug: bool,

    /// Force the release profile, overriding `[ratatoskr.harness] debug`
    /// from `brokkr.toml`. Mutually exclusive with `--debug`.
    #[arg(long)]
    pub(crate) release: bool,

    /// Build the harness under this cargo profile: `dev`, `release` or
    /// a custom `[profile.<name>]` (`<target>/<name>/`). Overrides
    /// `[ratatoskr.harness] profile`/`debug`.
    #[arg(long, value_name = "NAME", value_parser = crate::build::RustProfile::parse,
          conflicts_with_all = ["debug", "release"])]
    pub(crate) profile: Option<crate::build::RustProfile>,

    /// Run the named `[ratatoskr.gate.<name>]` gate after the bench
    /// completes. Records a row in `.brokkr/ratatoskr/gate.db`, looks
    /// up the per-hostname baseline, and exits non-zero if any metric
    /// rule fails. Requires `--bench` - every rule compares measured
    /// numbers.
    ///
    /// The reserved value `all` runs every configured gate in turn
    /// (each supplies its own script, so pass no SCRIPT). The sweep
    /// keeps going past a breach and reports all of them.
    /// In depth: `brokkr man gate`.
    #[arg(long, value_name = "NAME|all", requires = "bench")]
    pub(crate) gate: Option<String>,

    /// Record this run as a baseline candidate for `--gate <name>`,
    /// suppress gate evaluation, and print the new UUID plus the TOML
    /// line to paste under `[ratatoskr.gate.<name>.baseline]`.
    /// Requires `--gate`, and refused with `--gate all`: rebasing a
    /// whole cohort at once re-anchors every baseline-relative rule
    /// with no per-gate moment to notice a blessed regression.
    #[arg(long, requires = "gate")]
    pub(crate) as_baseline: bool,

    /// Build and measure the harness from a git worktree at REF
    /// instead of the current tree. The script, the fixture and
    /// sæhrimnir still come from the current tree, so the run varies
    /// the sync code and nothing else. Worktrees persist; remove them
    /// with `brokkr clean --worktrees`. Requires `--bench`.
    #[arg(long, value_name = "REF", requires = "bench")]
    pub(crate) commit: Option<String>,
}

// `brokkr mock-serve`'s arguments; see `Command::MockServe`.
#[derive(Args)]
pub(crate) struct MockServeCmd {
    /// Fixture name. Resolves to `<fixtures_dir>/<NAME>.toml` or
    /// `<fixtures_dir>/<NAME>.lua` (whichever exists; both is an
    /// error). To disambiguate when both exist, pass the name with
    /// its extension (e.g. `--fixture jmap-small.lua`).
    #[arg(long, value_name = "NAME")]
    pub(crate) fixture: String,
}

// ---------------------------------------------------------------------------
// Shared mode args (measurement/build flags for all measurable commands)
// ---------------------------------------------------------------------------

#[derive(Args, Clone)]
pub(crate) struct ModeArgs {
    /// Full benchmark: lockfile, N runs (bare `--bench` means the default
    /// run count), DB storage
    #[arg(long, conflicts_with_all = ["hotpath", "alloc"], num_args = 0..=1, default_missing_value = DEFAULT_BENCH_RUNS)]
    pub(crate) bench: Option<std::num::NonZeroUsize>,

    /// Function-level timing via hotpath feature (optional run count, default 1)
    #[arg(
        long,
        conflicts_with_all = ["bench", "alloc"],
        num_args = 0..=1,
        default_missing_value = DEFAULT_SINGLE_RUNS
    )]
    pub(crate) hotpath: Option<std::num::NonZeroUsize>,

    /// Per-function allocation tracking via hotpath-alloc feature (optional run count, default 1)
    #[arg(
        long,
        conflicts_with_all = ["bench", "hotpath"],
        num_args = 0..=1,
        default_missing_value = DEFAULT_SINGLE_RUNS
    )]
    pub(crate) alloc: Option<std::num::NonZeroUsize>,

    /// Print full build/bench/result output
    #[arg(short, long)]
    pub(crate) verbose: bool,

    /// Build and benchmark an old commit via git worktree
    #[arg(long)]
    pub(crate) commit: Option<String>,

    /// Cargo features to enable (e.g. linux-io-uring)
    #[arg(long, value_delimiter = ',')]
    pub(crate) features: Vec<String>,

    /// Run even if the measured tree is dirty (a dirty tree's results are not
    /// stored; a clean tree, such as a --commit worktree, still stores)
    #[arg(long)]
    pub(crate) force: bool,

    /// Validate argv, config, and path resolution without building or running.
    /// Short-circuits after path/arg-vector construction. Skips cargo build and
    /// process execution; the command's configured lock policy still applies.
    /// Useful for sanity-checking a script of queued benches before leaving it
    /// overnight.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Kill the child process when this marker is emitted via the sidecar FIFO.
    /// Useful for benchmarking only a specific phase of execution.
    #[arg(long)]
    pub(crate) stop: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared program arguments
// ---------------------------------------------------------------------------

/// How `run` and `serve` build their program, and an optional second target
/// handed to it.
#[derive(Args, Clone, Default)]
pub(crate) struct ProgramArgs {
    /// Dev profile (overrides `[bin] debug`)
    #[arg(long, conflicts_with = "release")]
    pub(crate) debug: bool,

    /// Release profile (overrides `[bin] debug = true`)
    #[arg(long)]
    pub(crate) release: bool,

    /// Cargo profile by name: `dev`, `release` or a custom
    /// `[profile.<name>]` (overrides `[bin] profile`/`debug`)
    #[arg(long, value_name = "NAME", value_parser = crate::build::RustProfile::parse,
          conflicts_with_all = ["debug", "release"])]
    pub(crate) profile: Option<crate::build::RustProfile>,

    /// Cargo features to enable. Forwarded verbatim to cargo, which
    /// validates them - comma- or space-separated, repeatable.
    #[arg(long, short = 'F', value_name = "LIST")]
    pub(crate) features: Vec<String>,

    /// Enable all Cargo features.
    #[arg(long, conflicts_with = "features")]
    pub(crate) all_features: bool,

    /// Disable default Cargo features.
    #[arg(long)]
    pub(crate) no_default_features: bool,

    /// Build this commit in a persistent worktree, then run it from the
    /// current directory - the A/B partner of a plain `run`. Remove the
    /// worktrees with `brokkr clean --worktrees`.
    #[arg(long, value_name = "REF")]
    pub(crate) commit: Option<String>,

    /// Also build this target, as `brokkr build` does, and hand the
    /// program its path in `BROKKR_WITH_EXE`. The program is then built and
    /// executed directly, as under `--commit`.
    #[arg(long, value_name = "TARGET")]
    pub(crate) with: Option<String>,

    /// Build the `--with` target at this commit (the program's own
    /// `--commit` is separate)
    #[arg(long, value_name = "REF", requires = "with")]
    pub(crate) with_commit: Option<String>,

    /// Cargo features for the `--with` target, forwarded verbatim
    #[arg(long, value_name = "LIST", requires = "with")]
    pub(crate) with_features: Vec<String>,

    /// Enable all Cargo features of the `--with` target
    #[arg(long, requires = "with", conflicts_with = "with_features")]
    pub(crate) with_all_features: bool,

    /// Disable default Cargo features of the `--with` target
    #[arg(long, requires = "with")]
    pub(crate) with_no_default_features: bool,
}

impl ProgramArgs {
    /// Whether any flag was given: `serve --stop` and bare `serve` take none.
    pub(crate) fn is_empty(&self) -> bool {
        !self.debug
            && !self.release
            && self.profile.is_none()
            && self.features.is_empty()
            && !self.all_features
            && !self.no_default_features
            && self.commit.is_none()
            && self.with.is_none()
    }
}

/// Arguments shared by the litehtml and sluggrs visual runners.
#[derive(Args)]
pub(crate) struct VisualArgs {
    /// Fixture or snapshot ID (or unique prefix)
    #[arg(value_name = "ID")]
    pub(crate) fixture: Option<String>,
    /// Run all fixtures/snapshots
    #[arg(long)]
    pub(crate) all: bool,
}

/// Flags accepted only by the litehtml visual runner.
#[derive(Args)]
pub(crate) struct LitehtmlVisualArgs {
    /// Run all fixtures tagged with this suite name
    #[arg(long, conflicts_with = "all")]
    pub(crate) suite: Option<String>,
    /// Force-regenerate Chrome reference artifacts before comparing
    #[arg(long)]
    pub(crate) recapture: bool,
}
