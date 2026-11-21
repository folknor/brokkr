# Architecture

This page is the module map: what each source file owns and why. It opens
with the crate layout, then groups the modules by area: startup and the CLI,
configuration, the check and test pipeline, diagnostics, convention
scanners, cargo and builds, the lock, processes, reservations, measurement,
output, worktrees and install, shared helpers, the results server, serve,
script benches and the project commands.

## Crate layout

brokkr is a single crate with no workspace. It builds the `brokkr` binary
and two self-contained bins beside it, `brokkr-rustc-guard` and
`brokkr-results-server`, which include the shared sources they need by path.

## Startup and the CLI

These modules start the process, parse the command line and decide which
project and command apply:

- `src/main.rs`: module declarations and imports. `main_parts/mod.rs`
  declares the two startup modules and re-exports `bootstrap::main`.
  `main_parts/bootstrap.rs` owns startup and the command context helpers.
  Past the harness shim, startup drops an inherited `BROKKR_MARKER_FIFO` and
  `BROKKR_HARNESS_ARTEFACT_DIR` from brokkr's own environment, so no child
  inherits them. The normal `main_parts/dispatch.rs` module owns dispatch.
  Shared argument resolution lives in `command_args.rs`. Passthrough and
  timing, formatting and visual approval have normal modules
  (`passthrough_cmd.rs`, `fmt_cmd.rs`, `visual_approve_cmd.rs`).
- `src/project.rs`: the `Project` enum (the built-in projects, with `Other`
  for the rest), `Detection` with shared and typed project config, `detect()`
  and `require()` gating. `Detection::load` detects a project from its config
  dir and build root without reading the working directory; it is
  `detect_optional`'s core and how a test detects a fixture. `project.rs`
  also mints the distinct private `ProjectRoot` and `BuildRoot` types, and
  `context.rs` requires those types for measurement, lock and worktree
  setup.
- `src/cli/`: the CLI definition (clap derive). `schema.rs` has `Cli`,
  `Command`, one `#[derive(Args)]` struct per subcommand with arguments
  (named `VariantCmd` after its variant) and the shared `ModeArgs` and
  `ProgramArgs`. `validation.rs` has the value parsers. Both are normal
  private modules re-exported through `cli.rs`.
- `src/cli/visibility.rs`: the exhaustive `CommandId` and `CommandSpec`
  catalogue. `COMMANDS` lists every `CommandId` for registry checks.
  Dispatch, help visibility, project prefixes and man topics consume this
  policy.
- `src/run_invocation.rs`: the brokkr invocation a results row records,
  captured once at the CLI boundary from the parse. `parse_cli` in
  `main_parts/bootstrap.rs` supplies the positional boundary.
- `src/version_cmd.rs`: answers `--version` and `-V` before clap parses.

### Subcommand argument structs

A subcommand's arguments go in its own struct, never inline in a struct
variant. With its own struct, the enum-level functions clap generates only
register and dispatch the subcommands, and each struct's `Args`
implementation builds and extracts its own arguments in its own function.
With inline variants, the one enum-level function builds every argument of
every subcommand. Clippy's `large_unopt_stack_frames` estimated that
unoptimized frame above 2 MB, and building the command tree overflowed a
libtest thread's 2 MiB stack.

### Command policy

Each `CommandSpec` records a command's:

- applicability
- config need
- lock policy
- worktree selection
- disk gate
- default mode
- run log
- tool probes

The lock and the run log are refined per invocation, as worktree selection
is. Dispatch decides `reads_only` once and feeds it to `lock_for`,
`run_log_for` and tree selection. A `brokkr bench --dry-run`, and any
`brokkr bench` that only lists, describes or refuses (as `bench::runner::runs`
decides from the declared benches), takes no lock, writes no log and has no
tree.

### Project-scoped help

`brokkr --help` lists only the commands that apply to the detected project.
A hidden subcommand still parses. Dispatch checks its command specification
and refuses an inapplicable project before entering its handler. Project
prefixes and man topic visibility read the same specifications.

### Invocation identity

`src/run_invocation.rs` records the exact argv and the arm key
`results --compare` pairs by. The arm key is the argv minus brokkr's own
`--commit`, `--verbose` and iteration counts, with the trailing positional's
tokens kept verbatim and JSON-encoded when a token defeats the space join.
It also records the qualified key a row stores when the row has a configured
selection the argv can't show; historical elivagar tilegen rows have their
block flags recorded this way.

### Version banner

`src/version_cmd.rs` prints the banner, the local build stamp and protocol,
and the user-wide results server's, from one quick
`remote::Client::identity_once`.

## Configuration

These modules load, compose and validate `brokkr.toml`:

- `src/config.rs`: the config hub. It declares `bench`,
  `bench_annotations`, `check` and `project_config` as `#[path]` modules
  under `src/config_parts/`, and `include!`s `schema.rs`, `parser.rs`,
  `compose.rs`, `user.rs` and `tests.rs` into one module, so those five
  files share one namespace.
- `src/config_parts/schema.rs`: `DevConfig`, `HarnessConfig`,
  `ResolvedPaths` and the other shared serde schemas, plus the bbox text
  parser clap and dataset deserialization share. A `[[lint_sweep]]` is a
  `LintSweep`, validated and planned as the test-free `[[check]]` entry of
  its shape (`LintSweep::as_check_entry`), so the two tables can't drift on
  what a build-shape key means.
- `src/config_parts/parser.rs`: `load`, `load_user_only` and every table's
  parser and validator: host sections (`is_host_key`,
  `check_host_section`), textlint presets, script checks, `[[check]]`,
  `[test]` profiles and quarantine, install steps and datasets. It also has
  `Claim`, the one statement of what a profile's `certifies` obliges, the
  hostname via libc, path resolution and the captured-env helpers.
- `src/config_parts/check.rs`: lint policy for the test phase: the `-A`
  flags it injects (`test_phase_allow_flags`), which lints reach a test
  build or belong to cargo, and the rustflags target key.
- `src/config_parts/tests.rs`: the config loader's unit tests.
- `src/config_parts/user.rs`: the user-wide layer, described in
  User-wide layer below.
- `src/config_parts/compose.rs`: `include` and layer composition, described
  in Composition below.
- `src/config_parts/bench.rs`: the script-bench schema, described in Bench
  schema below.
- `src/config_parts/project_config/`: `ProjectConfig` and the section
  schemas and parsers for litehtml, sluggrs, ratatoskr and mogwai.
  `[pbfhogg]` is an ordinary host-name section with no project section of
  its own. `load` rejects a known project section when `project` names
  another project, and requires the matching section for litehtml-rs,
  sluggrs, ratatoskr and mogwai. `dellingr.rs` and `piners.rs` only warn
  that a dellingr project's `[dellingr]` or a piners project's `[piners]` is
  retired with its commands: the table loads whatever it holds, unvalidated,
  and stays in the composed config a script sees. `Detection` and
  `MeasureRequest` hold `ProjectConfig`, so handlers match on the variant
  instead of re-checking presence.
- `src/profile.rs`: the validation profile resolver for `[test.profiles.*]`.
  See its module header and `docs/commands/check.md`.

### User-wide layer

`src/config_parts/user.rs` is the user-wide `brokkr.toml` layer, resolved
through XDG and overridden by `BROKKR_USER_CONFIG`. It accepts
`[[textlint]]`, `[textlint_preset.*]`, `[[script_check]]` and the user-only
`results_server`, and rejects everything else by name.

`ResultsServer` is that URL, validated once: scheme, host, no credentials,
path prefix and the `/bench` endpoint. Composition takes it out of the user
table before merging and refuses a value of that name in every project
layer, checked file by file before the merge can hide one.

Composition reads the rest of the user layer as the lowest-precedence raw
table before section parsing. `brokkr env` reports the layer from the
`ConfigSources` startup recorded rather than reading it again.
`config::load_user_only()` composes a normal `DevConfig` for a tree with no
project config, which is how `check`'s no-detection branch runs the user's
textlint and script_check entries.

Unit-test builds compose no user layer (`selected_user_table` returns none
under `cfg(test)`), so a fixture load never sees the developer's file.
`compose_layers_with` takes the user layer as an argument, which is how
tests compose one through the production path.

### Composition

`src/config_parts/compose.rs` owns `include = [...]` and the user-wide layer.
`compose()` folds the user config, then the include graph (post-order), then
the project's `brokkr.toml` into one table before section parsing.

`compose()` returns `ConfigSources`, which records:

- which files define each table
- which file supplied every other value except an array of tables, whose
  entries join across files (`value_origin`)
- each file's canonical directory, the base for `include` entries and a
  bench's `script_file`

A bench's `changelog` is the exception to joining: the layer that sets it
replaces it whole, so it gets an origin (`is_whole_value`). In a
`[bench.NAME]` table, `script` and `script_file` are one setting across
layers (`clear_replaced_alternatives`). Named array entries from higher
layers replace lower entries, and presets resolve over the composed config.

`UserLayerSources` records the user layer's `results_server`, copied onto
`DevConfig::results_server`, and its surviving `[[script_check]]` names.
Those names exempt their entries from the load refusal of a `when = "gate"`
entry in a project with no complete profile. `git::check_clean` exempts
in-tree includes as it exempts `brokkr.toml`.

### Bench schema

`src/config_parts/bench.rs` is the script-bench schema:

- `[bench.*]`: script or script file, located through
  `ConfigSources::value_origin`, build settings and params
- `[bench_params.*]`: composed into each bench through `uses`
- `[data]`: reserved and uninterpreted

`bench_annotations.rs` reads a bench's annotations (`description`,
`version`, `date`, `changelog`) field by field. Each is absent, invalid with
its reason, or valid. That one reader is shared by the load, which refuses
an invalid annotation, and by `brokkr results --compare`, which names it. It
also renders the annotations for `--help`.

Param declarations are parsed strictly, each with exactly one `Domain`:
`from`, `choices`, `validated`, `free`, or the typed `integer`, `number` and
`bbox`. `Domain::admit` judges an item both for a default here and for a
value at bind. A `choices` list and its default are checked here. The `from`
reference graph is judged on each bench's effective params at load, so no
later refusal tightens a loading config. What needs the resolved config is
`src/bench/params.rs`, at run time. Nothing here reads or compiles Lua.

## Check pipeline

These modules plan and run `brokkr check` and the investigative
`brokkr clippy`:

- `src/check_cmd.rs`: the check hub. Only `activation`, `selection`,
  `conventions`, `reporter`, `verdict` and `fixture_lint` are real
  submodules; every other `src/check_cmd/*.rs` file is `include!`d into
  this one module and shares its imports and private items. A file in that
  set can't hide an item from the rest, and an import one file drops may
  still be read by another.
- `src/check_cmd/check_plan.rs`: `plan_check`, the one planner of a check
  run. It decides the profile and claim, the tested sweeps and lint lanes,
  the derived doctest carriers, the skipped phases, the serial-build answers
  and every phase's selection. `cmd_check` consumes the plan to run and
  `cmd_check_audit` to explain. Planning announcements go through a
  `Narrator` at the point they are decided: live on a run, collected by the
  audit. The file also has the audit's own findings: unreferenced
  `[[check]]` entries, package selectors naming no workspace package, and
  absent `allow_exact` paths.
- `src/check_cmd/phase.rs`: `check` orchestration. It runs setup around the
  plan, sequences conventions and builds, hands off preparation, and does
  the accounting and the sequential test resolutions.
- `src/check_cmd/phase_type.rs`: the check-specific facts layered onto
  `cargo_env::Phase`: labels, skip, prose and group facts, ceilings, cargo
  verbs and lint ownership.
- `src/check_cmd/reporter.rs`: the explicit `Check { commands }` and
  `Clippy` output policy used by the shared diagnostics pipeline.
- `src/check_cmd/diagnostics.rs`: clippy and rustdoc cargo runs, diagnostic
  attribution, merging, rendering and failure excerpts.
- `src/check_cmd/clippy_cmd.rs`: the investigative `brokkr clippy` entry
  point over the shared diagnostic pipeline.
- `src/check_cmd/prepare.rs`: the `prepare` phase, described in Prepare
  phase below.
- `src/check_cmd/concurrency.rs`: what check runs concurrently, described
  in Concurrency below.
- `src/check_cmd/test_phase.rs`: the test-phase driver, lane execution,
  fail-fast accounting and per-lane stop records.
- `src/check_cmd/binaries.rs`: `TestBinary`, each test executable with its
  owning package, target and package root, read from the build's artifact
  stream, and the direct `--list` that attributes tests to packages.
- `src/check_cmd/direct_runtime.rs`: `DirectRuntime`, cargo's launch
  envelope reconstructed for running a prebuilt test binary directly, and
  the refusal of a configured target runner, which direct execution would
  bypass. Described in Direct runtime below.
- `src/check_cmd/parallel.rs`: the `parallel = { budget = N }` lane, which
  runs a sweep's binaries concurrently under slices of one test budget
  (`binary_timings::allocate`). Tests within one binary still share a
  process.
- `src/check_cmd/isolate.rs`: the `isolation = "process"` lane, which runs
  each prepared binary once per selected test and keeps going after a
  failure.
- `src/check_cmd/nextest_lane.rs`: the `harness = "nextest"` lane, the
  linked nextest engine as a process-per-test executor under a config
  brokkr synthesizes per run. The project's `.config/nextest.toml` and
  `NEXTEST_PROFILE` are never read. brokkr owns the build, the filters and
  the thread count.
- `src/check_cmd/nextest.rs`: the nextest lane's coverage classification,
  mapping nextest's per-testcase verdict onto the coverage buckets. A
  universe comes from its own unfiltered listing, never from a lane's: a
  package-scoped filterset drops whole binaries from a listing.
- `src/check_cmd/accounting.rs` and `src/check_cmd/accounting/`: execution
  accounting, described in Accounting below.
- `src/check_cmd/coverage.rs`: policy coverage under a complete profile,
  described in Accounting below.
- `src/check_cmd/continuation.rs`: the diagnostic continuation report, the
  executions an early-stopped run left unresolved, built from the plan and
  the journal alone. It certifies nothing and never changes a verdict.
- `src/check_cmd/watchdog.rs`: the run and phase time ceilings of `check`,
  `test` and `clippy`, described in Watchdog below.
- `src/check_cmd/report.rs`: the facts a passing run's grouped green lines
  are built from, streamed per phase, with per-sweep narration sent to the
  run log. It re-exports `run_log`'s `RunLog` and `run_log_kind`.
- `src/check_cmd/test_execution.rs`: sweep shape descriptions, support
  builds, serial harness attribution, the serial cargo-test runner, and the
  build verifier that holds that run's own artifact stream to the plan and
  settles its shim gate. `test_execution_tests.rs` has the extracted
  check-command test coverage.
- `src/check_cmd/activation.rs`: check and clippy sweep activation,
  effective profile selection and the shared ad-hoc environment merge.
  `brokkr clippy --sweep` resolves a `[[lint_sweep]]` name too; the ad-hoc
  env union stays `[[check]]`-only.
- `src/check_cmd/selection.rs`: lane selection, described in Lane selection
  below.
- `src/check_cmd/conventions.rs`: convention-phase scans, finding blocks
  and script-check failure rendering, plus selector hygiene, described in
  Selector hygiene below.
- `src/check_cmd/member_check.rs`: the `member_check` phase, one
  package-mode `cargo check --locked` per workspace member over its
  production targets (lib, bins) in the `unify-package` target dir. It owns
  the artifact accounting keyed by package id and target class, the by-name
  bin probe that waives only on cargo's own `required-features` refusal, and
  the candidate lost-features explanation.
- `src/check_cmd/fixture_lint/`: the `fixture_lint` phase, described in
  Fixture lint below.
- `src/check_cmd/verdict.rs`: `Verdict`, stop precedence, the JSON summary
  schema and test timings. `check --json` emits a single schema-versioned
  summary trailer built here (`CheckSummary`).
- `src/check_cmd/binary_timings.rs`: `.brokkr/parallel-timings.toml`,
  described in Binary timings below.
- `src/build_purpose.rs`: the shared build-purpose resolver, described in
  Build purposes below.
- `src/rustdoc_check.rs`: the rustdoc phase's lint-only mode, described in
  Rustdoc check below.
- `src/cpu_topology.rs` and `src/cpu_topology/quota.rs`: the parallel
  budget probes, described in CPU topology below.
- `src/scope.rs`: working-tree scope and the check run's file snapshot,
  described in Scope and snapshot below.

### Prepare phase

`src/check_cmd/prepare.rs` plans the whole invocation before any test runs.
It has two products, kept apart because they are different claims:

- **Execution inventory**: every enumerable lane (serial, parallel,
  isolated, nextest; doctests aren't enumerable) is built, its binaries
  listed under the launch envelope execution uses, its selection recorded
  and every test executable hashed. Every run prepares it, so a kill in one
  binary can name the tests of the binaries after it.
- **Policy universe**: every test of each required shape, its exclusions
  and filter liveness, enumerated only under `certifies = "complete"`.

`LaneEnv` is decided once per lane: the env, `--config` flags, launch
config and target every cargo run of that lane shares, so no run
re-fingerprints the shape. The build step is a warm-up: every lane's
universe, support and shape builds run in target-dir chains, concurrently,
and nothing is hashed. The inventory step then builds each lane again,
alone, and hashes straight after, so one read is both the lane's build
record and its pre-listing observation. A warm-up failure is replayed and
the lane isn't built again.

On a complete claim a universe is built and snapshotted right before the
policy pass lists it. Snapshot reads spread over a process-wide budget of
readers, and every plan hash goes through `test_runner::hash_file_at` with
a `HashSite`. A later build that replaces an accepted lane's file is found
at lane entry. Lanes consume the prepared selection; where an up-front
preparation failed, a lane that needs a selection prepares its own through
the same code.

### Concurrency

`src/check_cmd/concurrency.rs` holds one rule: no compilation overlaps the
execution of a test binary, a listing included, since the per-test cap and
listing deadline are wall-clock limits. Two kinds of work run concurrently
within it:

- background script checks (`stage = "background"`), joined before the
  prepare phase lists anything
- target-dir groups: clippy, rustdoc and prepare builds grouped by the
  directory they write, serial in configured order inside a group, one
  thread per group

`resolve_serial_builds` collapses every group into one chain under
`[test] serial_builds` or a configured cargo build dir, decided once per
run for the phases it schedules.

### Direct runtime

`src/check_cmd/direct_runtime.rs` reconstructs what cargo sets when it
launches a test binary, from the sources cargo derives it from: the package
root as cwd, the dynamic-loader path in cargo's order, config `[env]`, the
`CARGO_PKG_*` and manifest variables, build-script `rustc-env` values,
runtime `CARGO_BIN_EXE_*`, and `CARGO`. Cargo-owned values are applied
last, so a sweep's env can't forge them. The parallel, isolated and
`brokkr test` direct runs use it, and a configured target runner is refused
rather than bypassed.

### Accounting

Accounting and coverage are two claims a check run keeps apart:

- **Execution accounting** (`accounting.rs`, with `accounting/plan.rs`,
  `journal.rs`, `reconcile.rs` and `report_json.rs`): every execution the
  plan expects ends as exactly one outcome (passed, failed, timed out,
  interrupted, ignored, unobserved), journaled as the runner observes it.
  Every run that runs tests keeps the journal; only a complete profile
  gates on it. The reconciliation is pure: it reads the plan and journal,
  spawns nothing, and so still runs after a watchdog kill.
- **Policy coverage** (`coverage.rs`): under a complete profile, every
  (build shape, resolution, binary, test) pair is selected by some lane, or
  is ignored, quarantined or curated. Anything else is orphaned and fails
  the check. Selection comes from the plan, never from what the test phase
  reached, and a `[[quarantine]]` entry justifying no pair fails.

### Watchdog

`src/check_cmd/watchdog.rs` arms the run ceiling (check only) and the
per-phase ceilings after the lock is taken; the durations live in
`test_runner/ceilings.rs`. When one fires it SIGKILLs every
identity-checked descendant found in `/proc`, raises the shutdown flag and
lets the main thread unwind, so history, the run log and the lock's drop
still run and the exit is 124. Only after `UNWIND_GRACE` does it exit the
process itself, restoring the toolchain pin and recording history first.

### Lane selection

`src/check_cmd/selection.rs` owns check and test lane eligibility, package
admission, dedupe, reach accounting and lane repro rendering. It hands the
admitted package sets to the build-purpose request constructors.

`RunSweeps` holds a check run's lanes: the tested sweeps, then the
`[[lint_sweep]]` lanes. `CheckSelections` builds clippy and rustdoc over all
of them and the test phase over the tested prefix, which is all the test
machinery is handed. `derive_doctest_carriers` derives a complete claim's
doctest carriers, keyed on the same effective build its dedupe uses, with
the lint labels reserved.

### Selector hygiene

`selector_findings` in `src/check_cmd/conventions.rs` reports dead globs and
exclusions that excuse everything. It works over the snapshot's present
files (`TreeSnapshot::present_files`, the inventory less paths observed
absent), and only `check --audit` reads it (`cmd_check_audit` in
`check_plan.rs`). The scanners themselves never warn about it.

Its `[[textlint]]` findings come from `textlint::Selection`. The textlint
phase also fails on `Selection` for a rule setting `require_live_paths`, so
the audit and that gate share one definition of dead scope.

### Fixture lint

`src/check_cmd/fixture_lint/` is the `fixture_lint` phase:

- `mod.rs` runs each entry: preflight, the lock transaction, per variant a
  `cargo clean -p` then a warn-level `cargo clippy --no-deps --locked`, and
  the non-fresh artifact requirement.
- `walk.rs` is the strict `syn` module walk. It follows unconditional `mod`
  routes only, deliberately not `test_code`'s over-approximating classifier.
- `manifest.rs` is the expectation manifest and the exact verdict on lint
  plus backticked path.
- `lock.rs` is external identity containment and path identity against the
  main resolve.

### Binary timings

`src/check_cmd/binary_timings.rs` owns `.brokkr/parallel-timings.toml`. One
pure allocator assigns and orders binary slots from test counts and
per-binary history. Serial cost (the sum of a binary's own tests) and the
slowest single test are `Duration`s stored as integer milliseconds.

The allocator uses serial cost, not wall time. Wall time is a function of
the slots granted, so feeding it back oscillates; this was measured as a
two-cycle. The slowest test caps slots where extra threads stop helping.
The file is advisory in every direction: a missing or stale file costs one
warm-up run, never a wrong result.

### Build purposes

`src/build_purpose.rs` is the shared Cargo build-purpose resolver for run,
install, check, clippy, test and crit. Purpose policy owns profile defaults,
feature admission and Cargo profile flags. It also owns a sweep's profile
precedence and eligibility: a `profile_required` pin that a `brokkr test`
flag contradicts leaves the sweep out.

Host `features` never reach the resolver. Measured builds merge them
upstream, and the standalone purposes don't inherit them.

Its submodules:

- `src/build_purpose/requests.rs` owns package and feature request
  construction without check dependencies.
- `src/build_purpose/features.rs` owns feature routing and projection.
- `src/build_purpose/environment.rs` owns sweep environment composition,
  isolated target-dir selection, and placing a sweep's cfg rustflags into
  rustdocflags (`place_sweep_rustdocflags`, for the rustdoc phase and every
  test-lane invocation).

### Rustdoc check

`src/rustdoc_check.rs` is the rustdoc phase's lint-only mode. It probes
whether the rustdoc cargo will run supports `-Z unstable-options --check`,
which renders nothing; rendering is most of a `cargo doc`. It hands the
phase its flags for `rustflags::place`.

After the run, it stamps each documented unit's
`target/doc/CRATE/index.html` with the pre-launch time. Cargo judges doc
freshness by that file, which `--check` never writes, so without the stamp
every run would re-document every member.

### CPU topology

`src/cpu_topology.rs` counts physical cores in one last-level cache domain
intersected with the calling thread's allowed CPUs. It reads procfs
(`Cpus_allowed_list`) and sysfs: `cache/index3/shared_cpu_list`, collapsed
by `physical_cores_at` onto distinct (`topology/physical_package_id`,
`topology/core_id`) pairs, since a core id is unique only within a package;
a missing package id reads as 0. The count is the default in-flight budget
for a `parallel` `[[check]]` sweep and `brokkr env`'s `l3 domain:` line.

The module also computes the `budget = "cores"` count: every allowed
physical core as distinct `topology/core_cpus_list` sibling groups, falling
back to `topology/thread_siblings_list`, and failing closed to
`available_parallelism()`. Both budget probes return a
`CoreProbe` (count, quota cap, fallback flag) for the plan line.

The probes count cores, not logical CPUs: SMT siblings contend for one
core, and cross-chiplet pairs pay fabric latency. On AMD the domain is a
CCX or CCD, on Intel the die: one metric, two names, and `domain_label`
picks the word.

`src/cpu_topology/quota.rs` is read-only cgroup v2 CPU quota discovery
through the calling thread's cgroup and mountinfo. It caps the default test
budget by every visible ancestor's `cpu.max`, without changing the reported
topology count or explicit budgets.

### Scope and snapshot

`src/scope.rs` has the scope helpers:

- `unstaged_files()` is the working tree against the index, plus untracked
  files.
- `dirt()` classifies the working tree (`Unknown`, `Clean`, `ProseOnly`,
  `Code`) for `check`'s markdown-only shortcut, using `git::TreeState` and
  the shared extension policy.
- `prioritize()` orders a phase's errors so the ones in files with unstaged
  changes come first.
- `TreeSnapshot` is one check run's file list, status and cached file bytes,
  shared by the convention scanners.
- `presence()` observes whether a listed path exists, uncached. Only
  `NotFound` is absence, a dangling symlink included; any other metadata
  failure is an error. `present_files()` is the inventory less what it finds
  absent.

The byte cache is keyed by canonical file identity, so every spelling of one
file reads the same bytes. That includes a symlink, `..`, and an absolute
path outside the root that the test-code classifier reaches. Nothing is ever
dropped: `check` has no output cap and no flag to raise one.

## Test runner

These modules run libtest harnesses for `check` and `brokkr test`:

- `src/test_runner.rs`: the stable crate-visible facade for the shared
  libtest runner, described in Runner modules below.
- `src/test_runner/harness_shim.rs`: harness isolation for `check`'s serial
  lane, described in Harness shim below.
- `src/test_cmd.rs` and `src/test_cmd/`: `brokkr test NAME`. Each sweep is
  prebuilt once, every harness lists itself, and only harnesses holding a
  match run, each directly under `check_cmd::DirectRuntime`. `plan.rs` handles sweep and package
  planning, build shapes and harness discovery. `run.rs` handles sweep
  execution and run accounting. `verdict.rs` classifies libtest outcomes and
  reports command verdicts. `display.rs` filters streamed output and repeat
  display. `focused.rs` validates and lists matching harnesses.
- `src/test_scratch.rs`: the scratch allocator for unit tests, described in
  Test scratch below.

### Runner modules

`src/test_runner.rs` re-exports the crate-visible items of its submodules:

- `context.rs` has caller-supplied stop and warning behavior.
- `events.rs` owns stream observations.
- `outcome.rs` reduces those observations into one execution outcome and
  lane verdict.
- `plan.rs` owns strict-plan inputs, executable hashing and the
  `BuildGate`. A cargo-launched run's connections wait on the gate while the
  caller holds that run's own build to its plan.
- `invocation.rs` owns the typed libtest selection (`LibtestArgs`,
  `TestThreads`) and its launch rendering; protocol flags are set here,
  never taken from a caller's passthrough.
- `ceilings.rs` has every time bound from the per-test cap to the check
  ceiling and `phase_ceiling`, with compile-time assertions that each inner
  bound expires before the next outer one.
- `deadline.rs` owns typed expiry, timeout cause and attribution, and the
  shared freeze, snapshot and kill verdict.
- `libtest_json.rs` parses and reconstructs libtest JSON.
- `procgroup.rs` owns process groups, pipes and drains.
- `run.rs` owns the one cargo or direct libtest runner and the separate
  listing runner.
- `hung.rs` owns timeout snapshots and reports.
- `output.rs` parses libtest text output and renders compact failures. It is
  separate from the cargo diagnostic text reader; check and `brokkr test`
  use it for post-run display and failure reporting.
- `harness_shim.rs` isolates each cargo-launched harness and rustdoc
  stream.
- `tests.rs` has the runner unit tests.

Every libtest lane uses the JSON event stream
(`-Z unstable-options --format json`), so nightly is required. The tracker
reports a typed `Expiry`, and `HungTest` records exact, suspect or absent
attribution separately from the timeout cause.

### Harness shim

`src/test_runner/harness_shim.rs` isolates harnesses on `check`'s serial
lane. brokkr installs itself as cargo's host target runner and as rustdoc.
Each harness says what it is in a handshake: its role, and the executable
cargo asked it to run. For rustdoc, the handshake names the package
directory instead, which a derived doctest carrier admits or declines per
library.

The harness then hands brokkr a fresh stdout pipe over a unix socket and
execs itself. brokkr authenticates the socket by `SO_PEERCRED`, a root-cargo
parent and its own executable. brokkr drains each pipe into its own tracker
with a pidfd deciding liveness. A harness that dies mid-test therefore
can't leave the shared stream with an open suite that bills the next
harness.

One `Arc<SessionState>` holds the acceptor, watchdog and drain state. Typed
refusal reasons fail closed on a strict session. Where the caller gave the
shim a build gate, every connection waits for the gate's verdict before it
is answered, and a strict session refuses anything but a clean build.
Harness and cargo drains share `procgroup::join_drains` and its bounded
grace. The module also has the entry point `main` checks first
(`test_runner::maybe_run_harness_shim`).

### Test scratch

`src/test_scratch.rs` is the one scratch-dir allocator for unit tests:
`scratch` for a directory, `scratch_path` for a file path. It asserts that
each `(module, name)` is used once. The allocator deletes before returning,
so a shared name means two tests delete each other's files: invisible
serially, an intermittent failure under concurrency. The assertion exists
because that precondition failed twice, both times a shared helper naming
its own dir rather than a test picking a duplicate.

`cargo_fixture` allocates a project a test builds with cargo. Under
`cfg(test)`, `worktree::isolated_target_dir` answers `cargo_fixture_target`
first, so every cargo brokkr runs on a fixture is pinned to the fixture's
own `target`, never the outer test build's.

## Diagnostics

These modules read compiler and cargo output into one diagnostic model:

- `src/diagnostic.rs`: shared compiler and cargo diagnostic fields,
  rendering and dedupe identity, plus the metadata-backed `OwnerIndex` that
  classifies JSON diagnostics and stderr manifest lints.
  `Diagnostic::is_dependency_warning` is the shared warning-exemption rule.
  `check`'s clippy and rustdoc phases consume `Diagnostic` directly, merging
  on `DiagnosticKey` and rendering at error level. Lint namespace views use
  the borrowed `LintName` from `config::check`.
- `src/cargo_json.rs`: the parser for cargo's `--message-format=json`
  diagnostics (`parse_cargo_diagnostics`), feeding the shared diagnostic
  model. Its `manifest_lints_by_block` reads cargo's own manifest lints
  (`cargo::unused_dependencies` and others), which reach stderr only, as
  text. They are recognized by a `Cargo.toml` arrow plus a lint identity,
  then classified through `OwnerIndex` by check's clippy, test and
  support-build paths.
- `src/cargo_filter.rs`: the legacy cargo text reader for commands that
  still receive rendered diagnostics. It and the cargo JSON reader both
  produce `diagnostic::Diagnostic`, and `filter_clippy_with_owners` uses
  `OwnerIndex` for the same dependency-warning rule. Libtest output parsing
  lives in `src/test_runner/output.rs`. Check's test-build and support-build
  warnings use cargo JSON plus stderr manifest-lint parsing.
- `src/rustc_text.rs`: the one reader of rustc-shaped text, described in
  Rustc text below.

### Rustc text

`src/rustc_text.rs` has:

- `rustc_blocks`, where a column-zero `error` or `warning` header opens a
  block
- the header rule
- `classify_block`, `classify_line` and `classify_diagnostic`, which sort a
  block into finding, failure summary, cargo status (warning tallies,
  `build failed`) or progress

It is shared by `script_check`'s failure excerpts, `cargo_json`'s summary
filter and manifest-lint discovery, `cargo_filter`'s text parser and
`check`'s failed-run reports, so they can't disagree on what a diagnostic
is.

## Convention scanners

These modules implement the convention gates `check` runs:

- `src/gremlins.rs`: the gremlin detector, described in Gremlins below.
- `src/header.rs`: the `[header]` check, a required file header with a
  current-year (`{year}`) requirement via the shared `CivilDate`.
- `src/civil_date.rs`: shared proleptic-Gregorian `CivilDate` conversion
  between calendar dates and Unix time, used by `[header]` and HTTP-date
  parsing.
- `src/textlint.rs`: the `[[textlint]]` engine, described in Textlint
  below.
- `src/lex.rs`: Rust source tokenization over `ra-ap-rustc_lexer`, so
  string, char, comment and raw-string boundaries are the compiler's own.
  It backs textlint's region scoping and `test_code` masking
  (`mask_line_by`).
- `src/script_check.rs`: the `[[script_check]]` logic (`evaluate`,
  `run_one`): run a command under `sh -c` and pass only when its captured
  output matches the configured `expect` sentinel. The exit code is
  ignored; spawn failure and shutdown are errors. Each entry runs under a
  deadline derived from the phase ceiling, and a stream past the 1 MiB
  capture cap fails the entry. Orchestration and failure rendering live in
  `check_cmd/conventions.rs`.
- `src/test_code.rs` and `src/test_code/`: the test-code classifier,
  described in Test-code classifier below.
- `src/globs.rs`: the `globset` wrapper for the path-glob lists shared by
  `header`, `textlint`, `manifest` and `[gremlins].include`, and the
  case-insensitive scanned-extension policy shared by gremlins and the
  prose-only shortcut.
- `src/dependency_rules.rs`: `[[dependency_rule]]` checks over shared Cargo
  metadata and `DependencyKind`. `config_parts/schema.rs` still represents
  config `kinds` tokens as strings, and the phase validates them when it
  runs. `selector_findings` lists every bad `from` or `except` for
  `check --audit` through the same per-rule resolver the phase refuses
  with.
- `src/manifest.rs`: `[manifest]` structural checks and `ManifestDeps`, the
  shared `toml_edit` walker for package, target and workspace dependency
  tables.

### Gremlins

`src/gremlins.rs` scans for invisible or deceptive Unicode. It scans these
files, tracked or untracked but not gitignored:

- `.rs`, `.toml`, `.md`, `.markdown`, `.js`, `.sh`, `.py`, `.yml` and
  `.yaml` files, matched case-insensitively
- whatever `[gremlins].include` globs name

The shared extension policy lives in `src/globs.rs`. `is_candidate`
(extension or include, before exclusion) is shared with `check --audit`'s
gremlins findings. The module exposes `tracked_files()`, the shared file
walk, and `CodepointSet` (allow and ban singletons and ranges).

### Textlint

`src/textlint.rs` is the generic engine for grep-style convention hooks:
declarative forbid-a-regex-on-a-line rules with bounded predicates
(`allow_marker`, `except`, `in_toml_section`, `table_row_only`).

`test_code` works through `lex::mask_line_by` per rule, composing with
`region`. Its `skip` mode blanks test-only bytes, and `only` blanks
everything outside the test selection. The legacy `skip_test` key shares
one preset-merge slot with `test_code` in the config parser and is
rewritten to it on the merged rule. `must_scan` admission counts a file
only at an eligible line holding real content, which under `test_code` is
code in scope. An `only` rule refuses a file whose selection depends on an
unresolved bare `#[test]`.

`Selection` judges a rule's selectors by path. Each `paths` glob's liveness
comes from `GlobSet::matches` indices, the survivors are judged by the
scanner's own `applies`, and presence is asked only of matched paths. It is
the one definition behind the audit's textlint findings and counts and
behind `require_live_paths`, whose failures are `DeadScope` entries beside
the line violations.

A classifier failure or refusal is returned in
`TextlintScan::classifier_error`, not as the scan's error. `Withheld` names
the (rule, file) pairs it left undecidable: every `test_code` rule's Rust
files on a failure, the named pairs on a refusal. The scan judges every
other pair, and `run_textlint` prints the findings and then fails with that
error.

### Test-code classifier

`src/test_code.rs` and `src/test_code/` are the classifier behind
`[[textlint]] test_code`, scoped to the unexpanded source model. The module
doc states the contract. `textlint` runs it only when some rule with a
`test_code` scope applies to a tracked `.rs` file.

`cfg.rs` proves a `cfg` predicate false without `test` and, for the test
selection only, with `test`. It uses strong Kleene logic with `test` fixed
and every other atom Unknown, and the with-`test` proof also reads
`cfg_attr`-introduced `cfg`s. It collects `cfg_attr` path alternatives.

`facts.rs` is the `syn` visitor over one file under one inherited `Context`
(test-only, dead in test builds) and resolution `Entry` (directory plus
pending non-mod-rs stem). It yields:

- module and literal-`include!` edges
- masked spans and test-dead spans
- bare-`#[test]` spans and their shadowing sites
- refusals and tripwire evidence

`graph.rs` walks from `cargo metadata` target roots with per-context visit
keys and intersects production masks across occurrences. Per occurrence it
keeps what it proves test-dead, to derive the test selection. It classifies
a second time with bare `#[test]` honored when a shadowing site leaves it
unresolved, and marks the files whose selection differs. It reads every
source's contents through the caller's snapshot reader, so masks describe
the bytes the scan matches. Module resolution, the ambiguity check and
visit identity still consult the live filesystem.

## Cargo and builds

These modules shape how brokkr launches cargo and builds binaries:

- `src/cargo/`: shared typed Cargo metadata snapshots, memoized once per
  root, query and environment for the invocation, with the indexed
  dependency graph. `load_fresh` bypasses the cache, for a script bench's
  per-arm target resolution. It also has the shared parser and per-root
  cache for `rustc -vV` facts, run from the project's directory so rustup
  selects its toolchain.
- `src/cargo_env.rs`: `[cargo_env]` evaluation, described in Cargo env
  below.
- `src/tree_policy.rs`: the edits brokkr holds on a code tree for the
  locked window, described in Tree policy below.
- `src/toolchain.rs`: `disable_toolchain`. It moves `rust-toolchain.toml`
  or `rust-toolchain` to a `.brokkr-disabled` sidecar and back, adopting a
  sidecar a hard-killed run left behind.
- `src/cargo_config_strip/`: the top-level `strip_rustflags` key, described
  in Rustflags stripping below.
- `src/rustflags.rs`: where a build's extra rustc flags come from, described
  in Rustflags below.
- `src/build_provenance.rs`: `BuildProvenance`, described in Build
  provenance below.
- `src/target_platform.rs`: a `[[check]] target` sweep's triple, described
  in Target platform below.
- `src/build.rs`: the build entry points and profile types, described in
  Builds below.
- `src/executable_artifact.rs`: picks the one executable a
  `cargo build --message-format=json` produced, by the build's own
  selection. The package is matched by the record's full `package_id`
  against the `-p` spec (name, version, source, globs), the target by
  `target.kind` and `target.name`, and test-profile units are skipped. More
  than one surviving record (several configured `build.target`s, or an
  artifact-dependency unit of the same target) is refused, never resolved
  by cargo's unordered output.
- `src/harness_build.rs`: the shared harness build for every orchestration
  command (ratatoskr `sync` and `service`). It builds a `[*.harness]`-shaped
  spec under an already-resolved profile and returns the binary, its
  directory (ratatoskr's `BROKKR_TEST_BIN_DIR`) and the profile cargo built
  under, which is what callers record.
- `src/target_namespace.rs`: the names brokkr and Cargo own directly under
  a Cargo target root: isolated sweep, worktree and fixture targets, `tmp`,
  `verify` and the built-in profile dirs. Every writer of such an entry
  takes its name from here, and a custom profile naming one is refused.

### Cargo env

`src/cargo_env.rs` evaluates `[cargo_env]` and holds the shared `Phase`
identifiers attached to its first failure. Before every cargo brokkr
launches that can run a build script, each entry's command runs. It runs
under `sh -c`, in cargo's launch directory, under cargo's env less every
configured name, and its trimmed one-line stdout is set on that cargo.

An entry with `if_exists` applies only where that path exists under the
launch directory. Elsewhere, such as a `--commit` worktree older than the
script, brokkr removes its name from that cargo's env and runs nothing,
while the cargo-config conflict check still covers it. Composition replaces
an entry whole (`compose::is_atomic`), so a redefined command never
inherits another layer's guard.

The spawn constructors apply it when the caller names the cargo spawn
profile, never by matching the program name. Those constructors are the
captured, sidecar and passthrough runners in `output/child.rs` and the
process-group spawn in `test_runner/procgroup.rs`. brokkr prints the first
failure once and records it process-wide. Every later cargo is refused, and
`main` (`seal`) and `check` fail the run even where a tolerant path
swallowed the error.

### Tree policy

`src/tree_policy.rs` has `TreePolicy`, built from `disable_toolchain` and
`strip_rustflags`, and `HeldTree`, the guard that the lock owns (or
`with_worktree` owns, on a `--commit` checkout). Activation always runs
cargo config recovery first, policy or not, so a crashed run's filtered
file is undone even after its key is gone. `restore_active_hold` is the
watchdog backstop's bounded restore of every registered directory.

### Rustflags stripping

`src/cargo_config_strip/` implements the top-level `strip_rustflags` key.

`matching.rs` deletes physical token sequences from every rustflags value
of the one file cargo reads in `TREE/.cargo`:

- `-C` and `-Z` pairs are grouped into units.
- All matches are found against the original.
- Overlaps and unplaceable matches are refused.
- Values keep their type.
- An unmatched file is never rewritten.

`transaction.rs` has:

- the rename-only install and restore (`renameat2` no-replace) over three
  reserved names
- a self-verifying marker line
- one policy-independent recovery state table shared by activation, drop,
  worktree admission and the watchdog takeover
- the process-wide mutex the backstop takes for good

### Rustflags

`src/rustflags.rs` answers where a build's extra rustc flags come from, and
how to add to them without discarding them. Cargo picks one rustflags
source (encoded env, then env, then matching `target.*`, then
`build.rustflags`). Exporting `RUSTFLAGS` to add one `-A` therefore drops
the project's `-Dwarnings` and link flags without a word.

The module inspects the whole config chain: the build root's ancestors plus
the cargo home, which `cargo_home` resolves as cargo does, down to the
passwd entry. It walks the chain as cargo's merged loader does:

- from the build root's physical path
- each directory's file chosen by cargo's own probe (`selected_config_file`,
  shared by `guard`, `crit`'s stamp and `strip_rustflags`, each of which
  adds its own write or absence check)
- includes by their lexically normalized reached path, never canonicalized
- one include set per root; a repeat within a root is refused, and a file
  reached from two roots is read twice

A small `cfg()` evaluator works over a `Platform`: the host from brokkr's
own `cfg!` values, or a `[[check]] target` sweep's triple from
`target_platform`. Host triples come from `cargo::rustc`, so the selected
project toolchain is used consistently. An unrecognized `cfg(...)` resolves
to the inert direction, never the destructive one, and the decision says
when it guessed (`SinkDecision::certain`).

The module returns the live layer as a `Sink`. The target layer is live
when the merged matching target flags come to something, string or array,
as cargo decides it. It is judged per configured triple (an ambient cross
triple decides only platform-free selectors), and chosen only when live for
all of them. Injection at a config layer goes through `--config`, where
cargo's array merge puts brokkr's entry last. It is refused when a config
file sets that key as a string, which cargo won't merge with an array.

Every decision takes a `FlagKind`, since cargo resolves rustdocflags by the
same rule. `place` puts brokkr's rustdocflags where cargo reads them: the
rustdoc phase's `--check` and a sweep's `--cfg` and `--check-cfg` rustflags
(`cfg_args`). `check`'s test phase uses the module to apply
`[lints] allow`.

The module also owns:

- `EffectiveEnv`, a launch's env pairs over brokkr's own, empty values kept
- `CargoLaunchConfig`, one parse of the config chain for the host runner, a
  target sweep's runner, `build.target`, rustdoc, `build-dir` and `[env]`,
  the last merged per key as cargo merges it; the direct-execution lanes,
  the serial shim's fallback and the build-concurrency decision read it
- `effective_rustflags`, which resolves the flags a host build gets and the
  source that won, by the same precedence, for build provenance

### Build provenance

`src/build_provenance.rs` has `BuildProvenance`: what a measured binary was
built with beyond what its commit records. It is stored as a row's
harness-owned `provenance.*` pairs.

The host side (`host_pairs`) is cached per build root and forgotten whenever
a tree edit is applied or undone. It records the compiler's `-V` line,
`effective_rustflags`, and the codegen keys and env inputs cargo reads. The
unit side (`unit_pairs`) records the measured executable's resolved profile
from cargo's compiler-artifact record. Provenance is part of
`BuildIdentity`, which a session's build sets on its harness
(`BenchHarness::with_build_identity`).

### Target platform

`src/target_platform.rs` resolves a `[[check]] target` sweep's triple
against the toolchain cargo would use (`RUSTC`, `CARGO_BUILD_RUSTC`,
`build.rustc`, `rustc`, under the sweep's env). It has:

- `TargetSpec`, the configured spelling: a literal, or a `{host_arch}`
  template that planning resolves from that compiler's host triple
- `TargetFacts`, the triple's `rustc --print cfg` facts restricted to the
  names the triple alone decides, read without refusing anything; they are
  all the rustdoc phase and the rustflags sink take
- `SweepTarget`, a test lane's target through the installed-target and
  cargo-version preflight, with its target libdir
- `differing_host`, the informational comparison of a triple's execution
  facts with the compiler host's

Its consumers are planning, runner discovery, the rustflags sink, the
rustdoc phase's rustdocflags placement, the direct runtime's per-binary
libdir and nextest's build platforms.

### Builds

`src/build.rs` has `BuildConfig` and the build entry points:

- `cargo_build()` returns the executable path and its resolved profile,
  picked by `src/executable_artifact.rs`.
- `RustflagsInjection` and `cargo_build_with_rustflags()` apply a script
  bench's `rustflags`. The flags are placed through `rustflags::place`,
  refused when the placement is uncertain, and built with target and build
  dir both in a per-recipe `rustflags-HASH` under the checkout's target
  base.
- `project_info()` reads the shared typed cargo metadata snapshot.

It also owns the profile types:

- `RustProfile`: dev, release or a validated custom `ProfileName`, the
  profile a Rust build selects
- `BuildKind`: a Rust profile, or the external Java or CMake build that
  historical rows have, the identity a measurement row stores; custom
  profiles are stored as `cargo:NAME` so no name collides with a legacy
  value
- `configured_profile`: the one `profile`-over-`debug` merge every config
  table uses

## Lock

These modules implement the global brokkr lock, its queue and the compile
capability:

- `src/lockfile.rs`: compatibility re-exports for the lock API and record
  format. Acquisition lives in `src/lock/mod.rs`, its ordered hold and drain
  protocol in `src/lock/hold_protocol.rs`, record I/O in
  `src/lock/record.rs`, the waiter queue in `src/lock/queue.rs`, holder
  uptime and hints in `src/lock/presentation.rs`, and the control service
  in `src/lock/service.rs`. The holder report lives in `observe.rs`,
  `activity.rs`, `collector.rs`, `report.rs` and `cgroup.rs` under
  `src/lock/`.
- `src/lock/mod.rs`: lock state and API, flock acquisition, same-process
  re-entry and release. Under `cfg(test)` the lock and the compile lease
  live under one scratch home per test process (`lock_home`), so no test
  takes, reads or drains the real ones. `test_hold` takes that scratch lock
  with no host effects, which lets a test reach `build::cargo_build`. The
  published workload children are one slot per spawning thread
  (`set_child_pid`, `clear_child_pid`), so concurrent lanes don't overwrite
  each other. Slots whose process has gone are pruned on every update.
- `src/lock/hold_protocol.rs`: fresh-hold ordering, described in Hold
  protocol below.
- `src/lock/queue.rs`: the lock queue, described in Lock queue below.
- `src/lock/record.rs`: conversion between typed lock state and the stable
  lock-file wire record, plus record reads and writes.
- `src/lock/presentation.rs`: verified holder uptime plus the advisory
  holder hint shown by lock status.
- `src/lock/observe.rs`: the per-acquisition observation table, created with
  the control service and shared by the `LockInner`, the service and the
  collector. It has open activities, launch cohorts (the `BROKKR_EXEC`
  sequence to the label and activity it was spawned under, bounded), the
  last sidecar marker and the root narration. It lives in memory only,
  never in the lock file.
- `src/lock/activity.rs`: `Activity`, the scope guard commands open around
  their steps: check phases, parallel lanes and binaries, verify checks and
  tool steps, measured runs, cargo builds and downloads. Activities nest per
  thread, with explicit `ActivityHandle` parents across threads. The module
  also has `narrate` (fed by every `output::status` line),
  `launch_context` (the one snapshot of cohort token and capability the
  spawn boundary stamps) and `note_marker` for the sidecar.
- `src/lock/collector.rs`: the process sampler behind `brokkr lock`,
  described in Collector below.
- `src/lock/report.rs`: the report extension (`x1.*` lines on the `status`
  reply, byte-bounded, legacy keys untouched so older clients still
  decode), its decoder, and the one renderer the socket path and the
  fallback share.
- `src/lock/cgroup.rs`: the per-acquisition cgroup, described in Hold
  cgroup below.
- `src/lock/service.rs`: the lock holder control socket, described in
  Control socket below. `src/lock_service.rs` re-exports the old API path.
- `src/lock_cmd.rs`: `brokkr lock` and `brokkr kill`, described in Lock and
  kill commands below.
- `src/lock_protocol.rs`: the shared lock and guard wire literals:
  environment names, lock path components, the published `auth` key and the
  rustc-guard handshake. The standalone guard includes it through
  `lock_probe.rs`, as does the holder and path resolver.
- `src/lock_probe.rs`: the shared non-blocking `LOCK_SH | LOCK_NB` probe,
  included by the lock implementation and the standalone rustc guard. It
  also defines the parse-and-hash side of protocol tokens (`Nonce`,
  `AuthHash`) and includes `lock_protocol.rs`. `src/lock_tokens.rs` holds
  minting plus `AcqId` and `RunToken`, included only by the lock
  implementation so the guard bin has no dead code.
- `src/lock_record.rs` and `src/lock_record_codec.rs`: the lock-file
  format. `lock_record.rs` is the guard-compatible field split and `auth`
  parser, included by both the lock implementation and the standalone
  guard, using the field name from `lock_protocol.rs`.
  `lock_record_codec.rs` owns the remaining keys, emission order and value
  escaping, and only the lock implementation includes it.
- `src/hold.rs`: the compilation capability, described in Compile
  capability below.
- `src/bin/rustc_guard.rs` and `src/guard.rs`: the rustc-wrapper fence,
  described in Rustc guard below.
- `src/disk_gate.rs`: the free-space gate, described in Disk gate below.
- `src/maintenance.rs`: store maintenance, described in Maintenance below.

### Hold protocol

`src/lock/hold_protocol.rs` orders a fresh hold:

1. The flock wait. The waiter joins the lock queue before the first attempt
   and tries the flock only on its turn; a non-blocking try that would pass
   a waiter refuses.
2. Service setup.
3. The draining-record publish.
4. The queue claim.
5. The compilation-lease drain.
6. Authorization publication.
7. Host effects, the last of which, before the hold goes live, is entering
   the accounting cgroup.

Until the live owner takes the descriptor, the pre-live owner invalidates
any record it published.

### Lock queue

`src/lock/queue.rs` is the lock queue under `~/.brokkr/queue/`. It has one
flock-proven record per waiter, and the rename-against-unlink handshake by
which `brokkr kill` cancels a waiter and a waiter claims its place.

The admission index is sequence-numbered tickets under the `admit.lock`
mutex. Each record has a heartbeat, and a stale waiter's place is forfeited
only by a try that takes the lock. A lock file's queue is the directory
beside it, so a scratch lock has its own.

### Collector

`src/lock/collector.rs` samples the processes behind `brokkr lock`. It takes
two `/proc` scans a quarter second apart, with membership by `BROKKR_EXEC`
token, a descendant walk and the cgroup. A row gets a CPU rate only for a
process seen with the same token in both scans, and an identity check
surrounds each row's reads. The holder runs the collector single-flight on
its own thread behind a rate limit and serves the cached result; the
reader-side fallback calls it directly.

### Hold cgroup

`src/lock/cgroup.rs` is the opportunistic per-acquisition cgroup. brokkr
enters it only on cgroup v2, under a `/`-rooted cgroup2 mount, in a
directory this user owns (delegation). It uses descriptor-held directories,
an explicit restore on release, a stranded flag and a bottom-up sweep of
leftover `brokkr-ID` groups. Its `usage_usec` is the CPU of everything born
under the hold since entry.

### Control socket

`src/lock/service.rs` is the lock holder control socket. Each fresh hold
binds `~/.brokkr/lock/ACQUISITION_ID.sock` before its drain. `status` and
`stop` requests use the public acquisition id, and the holder answers from
itself, so lock status and graceful kill work across PID namespaces.

A `status` reply has the legacy fields plus the report extension, built
from memory and the collector's cache without waiting, so a status query
never delays a stop. The lock record holds typed boot and namespace scope
before it is encoded.

### Lock and kill commands

`src/lock_cmd.rs` is the presentation and orchestration of `brokkr lock`
and `brokkr kill` over `lockfile`, `lock_service` and the lock queue.
`brokkr lock` lists the holder, the queue and the live serves
(`crate::serve::lock_report`).

`KillScope` is kill's scope: a project key, an unconfigured state root, or
every project. Kill cancels the scope's queued commands, then stops the
holder when it is in scope, and lists the scope's serves as left running
(`crate::serve::kill_report`).

### Compile capability

`src/hold.rs` is the compilation capability: the per-acquisition nonce a
hold hands its descendants, its published hash, and `stamp()`. The spawn
choke points apply `stamp()` last, after caller env, so nothing displaces
it. Environment names come from `lock_protocol.rs`.

The capability stands in for the guard's `/proc` ancestry inference, which
is spoofable by naming any binary `brokkr`. Ancestry inference also fails
wherever process shape is invisible, in the worst direction: refusing
brokkr itself. The capability is per acquisition, not per process:
pid+starttime+boot_id names the process, so a child of a released hold
would still match a later one.

### Rustc guard

`brokkr-rustc-guard` (`src/bin/rustc_guard.rs`) is a self-contained bin
target of its own; the package sets `autobins = false` because every bin is
explicit. Cargo invokes it as `build.rustc-wrapper` once
`brokkr guard --install` writes that line into `$CARGO_HOME/config.toml`.

The guard decides per rustc invocation:

1. `BROKKR_CARGO` passes, checked before any lease.
2. Otherwise it takes a shared lease on `~/.brokkr/compile.lock` with
   `FD_CLOEXEC` cleared, so the lease survives `exec` and the compiler
   itself holds it.
3. It reads the published capability hash under that lease: lock first,
   then check. A new hold can't republish without the exclusive lock the
   share excludes.
4. It probes `brokkr.lock` last, as the sole authority on whether a hold
   exists at all. A crashed brokkr leaves a well-formed record behind, and
   deciding on the record refused every compile on the machine.

The guard admission function accepts explicit paths and an environment
lookup for isolated tests. Handshake values and lock literals come from
`lock_protocol.rs`.

Holder and guard fail in opposite directions on purpose. The holder fails
acquisition without a drain barrier, and the guard fails open on broken
lease infrastructure. The guarantee is conditional; see
`docs/commands/check.hygiene.md`.

`src/guard.rs` is the comment-preserving `toml_edit` config editor
(install, remove, status). It refuses to touch a wrapper brokkr didn't
write, such as sccache.

### Disk gate

`src/disk_gate.rs` is the free-space gate, run on every fresh lock hold. It
is a `HoldEffects` field, so lock tests stay inert. It refuses the command
when `target`'s filesystem has under 5% of its size available (`statvfs`
`f_bavail`, df's `Avail`).

The gate is anchored to the command's build root, a `LockContext` field
separate from the config and state root, and probed through
`host::storage::probe_path`. The probe resolves symlinks, so a `target`
symlinked into a shared dir is measured on the shared disk. The project
root's own disk is deliberately not gated: a full `/` with `target` linked
onto a roomy disk must not refuse. `CARGO_TARGET_DIR` is probed too; a
cargo-config `build.target-dir` isn't, since that needs `cargo metadata`.

The gate is checked after the flock wait, before the drain. The command
spec's `disk_gate` field exempts `clean` and `env`. The gate fails open when
a size can't be read. `check_paths` is also called when a `--commit`
worktree is cut, for the worktree container's disk.

### Maintenance

`src/maintenance.rs` removes what killed brokkr commands left: `check` and
`test` harness sessions, nextest-lane generated configs and artifact
staging copies. Each is named by construction by its store and made only
under the brokkr lock.

Removal runs during the `MaintenancePhase`, which only dispatch's own fresh
acquisition (`lockfile::acquire_for_command`) has. The phase is taken once
(`LockGuard::take_maintenance`) and consumed before the command runs, so no
re-entered guard or in-handler hold ever has one. Under `clean --dry-run`
maintenance only reports.

## Processes

These modules identify, spawn, signal and reap processes:

- `src/process/mod.rs`: the shared process layer's facade: boot id,
  namespace inodes and the namespace-sharing rule, identity verification,
  clock ticks, best-effort signalling and process snapshots.
- `src/process/identity.rs`: typed PIDs, start ticks, boot identifiers,
  namespace inodes, process identities and host scope.
- `src/process/table.rs`: the shared `/proc/PID/stat` parser and process and
  sidecar metrics.
- `src/process/inflight.rs`: a live subtree for a report written before a
  kill: each descendant's age and command line, and, from `/proc/locks`,
  the file lock it is blocked on, with the lock's path and holder.
- `src/process/helpers.rs`: best-effort PID and process-group signalling
  plus `/proc` diagnostic snapshots.
- `src/process/reap.rs`: pidfd-backed verified signalling and the common
  reaper exemptions.
- `src/process/pidfd.rs`: an unverified pidfd for the caller's own unreaped
  child and a typed, full-precision exit wait on it (exited, timed out,
  interrupted, failed). The child supervisor waits on it between ticks so a
  wall is taken at the exit, and the test runner's harness liveness wraps
  it.
- `src/process/spawn.rs`: `ChildPolicy`, described in Spawn policy below.
- `src/stray.rs`: the stray-cargo reaper. It scans `/proc` for cargo-family
  processes with no brokkr ancestor, attributes starters, and SIGKILLs
  leaves first (rust-analyzer starters included). `ReaperExemptions` (this
  process, its ancestors and its descendants) is the boundary it shares
  with the orphan reap. Every locked command runs it after acquisition, and
  `brokkr strays` runs it.
- `src/test_orphans.rs`: the orphan reap, described in Orphan reap below.
- `src/shutdown.rs`: cooperative shutdown. `SigtermGuard` covers each
  tracked-child window; its async-signal-safe handler kills registered test
  groups and records a scoped stop, guards nest with only the outermost
  installing, and a second signal takes the default action.
  `is_shutdown_requested` is what runners poll, and `commit_stop` is the
  control socket's sticky stop. Outside a guard, `GroupReaper` SIGKILLs a
  live detached test group and re-raises. It also has
  `ChildIsolation`, `kill_descendants` for `kill --hard` and
  `die_with_parent`.
- `src/oom.rs`: OOM protection. `protect_child` marks a child as the kernel
  OOM killer's preferred target.
- `src/fs_probe.rs`: cancellable polling reads from process output
  descriptors.

### Spawn policy

`src/process/spawn.rs` has `ChildPolicy`, the one subprocess construction
policy behind the spawn choke points. It applies `[cargo_env]`, OOM
marking, parent-death, the orphan-reap token, and the hold capability
stamped last. `spawn` itself, the final boundary, allocates and stamps each
child's `BROKKR_EXEC` launch token together with the capability of the same
hold.

`spawn_exact` spawns a script bench's external program. The child calls
`execve` itself as its last setup step, so libc's `/bin/sh` fallback never
applies. A close-on-exec pipe the child writes before the exec tells the
exec's own refusal apart from the setup's.

The policy's rendered env serves engines that build their own children:
nextest gets one `BROKKR_EXEC` cohort per lane. Plain probes and downloads
with custom command builders use `output::child::command_output` and
`command_status`, which apply the policy after caller configuration and use
the shared supervisor. Managed mocks use service policy. Process-group
isolation is `shutdown::ChildIsolation::ProcessGroup(&guard)`, selectable
only while a `SigtermGuard` is borrowed.

`spawn_detached` is the one spawn meant to outlive the hold: the
`brokkr serve` supervisor. It:

- bypasses the final stamp and any armed reservation
- removes `HOLD_BOUND_ENV`: the capability, `BROKKR_EXEC`, the orphan
  token, the lease marker and the marker FIFO
- keeps one inherited descriptor
- in `pre_exec`, calls `setsid` and moves the child into the hold cgroup's
  parent (`lockfile::current_hold_cgroup_parent`), failing the spawn when
  that move fails

### Orphan reap

`src/test_orphans.rs` is the stray reap's complement, for what a name
can't match. It reaps test processes left running by a brokkr that was
SIGKILLed without a handler (the OOM killer), where `PR_SET_PDEATHSIG` took
only the runner's direct child (cargo on the serial lanes).

Every test-runner child has `BROKKR_TEST_RUN=TOKEN`, one token per brokkr
process. Libtest lanes get it through `test_runner::spawn_process_group`,
and the nextest lane through the private generated cargo config that
`src/check_cmd/nextest_lane.rs` renders from `ChildPolicy::test_process`.
The token's liveness proof is an flock on `~/.brokkr/test-runs/TOKEN.lock`
held for the process's life, which is namespace-proof where a pid isn't.

Every fresh hold claims the files whose flock it can take and SIGKILLs
their token's carriers: an environ scan, newest first, starttime-checked.
It exempts its own ancestors and descendants.

## Reservations

These modules reserve the files and directories a script-bench launch can
use:

- `src/reservations/mod.rs`: launch reservations for script-bench children
  and iterations: the protocol, its scope, the process-wide mutex and the
  store selection.
- `src/reservations/store.rs`: the lease store, described in Lease store
  below.
- `src/reservations/iteration.rs`: a script-bench iteration, described in
  Iteration below and in `reference/bench-resources.md`.
- `src/reservations/launch.rs`: `LaunchSpec` admission: conflict refusal,
  placement checks, reservation-only coverage of a tree around the launch,
  the per-launch private directory, and the descriptor handoff that the
  spawn choke point (`process/spawn.rs`) makes inheritable for that one
  child. It also has `ReadHold` (`hold_reads`): a script-bench run's bound
  inputs, reserved for reading from before their digests to the run's end,
  refused outright without a lease store.
- `src/reservations/guard.rs`: the mutation gate that every deletion,
  rename or replacement of a path a launch can use goes through. It refuses
  reservation overlaps and trees holding a pbfhogg backup.
  `remove_reserved_tree` is the overlap-only form for configured
  directories a script-bench iteration can sit under.
- `src/reservations/paths.rs`: lexical and resolved spellings of a path for
  overlap tests.

### Lease store

`src/reservations/store.rs` is the lease store under
`~/.brokkr/launch-leases/`. Each lease has a flocked lock file and a record
of its resources and tie: none, an iteration owner, or a borrow naming its
owner. Every lease is durable except a borrow, whose files are named apart
and never synced.

A scan classifies every lease before reclaiming any. The one exception is a
free borrow, removed as met. A held borrow therefore pins its free owner,
and an unreadable record, other than a free borrow's, keeps every free
owner. Reclamation of a free lease removes private directories, then the
record, then the lock; a free borrow is reclaimed whatever its record says.
A lease is released after the child exits.

### Iteration

`src/reservations/iteration.rs` is a script-bench iteration. It owns the
owner lease and its directory with `out/`, `scratch/`, `stdout/`, `stderr/`
and `profile/`.

Call admission is a read-only veto, with authority by current resolution
and conflicts with anything but the owner. The module arms the borrow for
the call's child and releases it with an observer between polls
(`Call::finish_observed`). It also owns:

- spools; the stderr spool is kept apart from the call so it is read
  whatever the release did, and each spool's inode is pinned for the
  iteration's life
- a marked call's hotpath report and scratch, made through held directory
  descriptors that a displaced area fails
- the spool preview and tail
- brokkr's own file access (`open_file`, `write_file`, a call's stdin)

brokkr's own file access is admitted as a call's would be, then opened
through an area's descriptor or along the resolved path without following a
link. An output is written by unlinking and creating afresh.
`iteration_tests.rs` runs the module against real children.

## Measurement and stores

These modules run measured commands and keep their results:

- `src/measure.rs`: the shared `MeasureMode` (Run, Bench, Hotpath, Alloc)
  and `MeasureRequest`.
- `src/measure/execution.rs`: one `MeasurementLoop` with a fixed
  `WallSource`, ordered `Iteration` evidence and winner selection.
  `BenchHarness::record` owns row-then-sidecar storage and failure fallback,
  and capture methods use the session lock and loop FIFO.
- `src/measure/argv.rs`: `Invocation` (the program and argv a measured
  spawn executes) and `format_cli_args`. Harness runners record `cli_args`
  from the invocation they execute.
- `src/measure_dispatch.rs`: `dispatch_profiled`, the profiled-versus-measured
  mode split and profiler narration shared by the mogwai and sluggrs
  adapters. Each adapter keeps its own build and harness calls.
- `src/measure/`: the rest of the session. `wall.rs` is timing,
  `capture.rs` measured child capture, `kv.rs` the stderr protocol,
  `record.rs` row building and persistence, and `tests.rs` the unit tests.
- `src/harness.rs`: re-exports only (`BenchHarness`, `BenchConfig`,
  `BuildIdentity`, `WallSource`, `format_cli_args`) from `measure`; it owns
  no code.
- `src/sidecar_backup.rs`: crash-safe rotation of sidecar database backups
  under `$XDG_DATA_HOME/brokkr/sidecar-backups/`, one rotation per project
  root under an ownership-checked `origin` file.
- `src/results_cmd.rs`: `brokkr results` over results.db, a compact table
  or one row's detail block, with script-bench runs delegated to
  `bench/results/`.
- `src/invalidate_cmd.rs`: `brokkr invalidate`, hard-deleting results and
  sidecar rows by uuid or commit prefix through `db::RunStore`. An empty
  prefix is refused, and without `--force` it opens both stores read-only
  and only previews.
- `src/hotpath_fmt.rs`: column-aligned tables of stored hotpath data, used
  by the typed results views, the bench results views and row recording.
- `src/history_cmd.rs`: `brokkr history` queries over `HistoryDb` and the
  best-effort recording of each command.
- `src/context.rs`: `MeasurementSession` (one lock and request provenance,
  with an optional built executable), typed `DirtyPolicy`, bootstrap helpers
  and worktree lifecycle. `bootstrap_config` resolves host paths from config
  alone; `bootstrap` is the cargo query a command needing its target dir
  makes of the tree it acts on.
- `src/request.rs`: the `ResultsQuery` and `SidecarQuery` structs.
- `src/db/`: the results and sidecar databases, described in Databases
  below.
- `src/db/historical_qualifier.rs`: the strict recognizer of historical
  typed pbfhogg read, write and merge argv, kept for the v27 results.db
  migration and stored comparison identities.
- `src/sidecar.rs`: the monitoring sidecar, described in Sidecar below.
- `src/sidecar/namespace.rs`: the marker FIFO namespace, described in
  Marker FIFOs below.
- `src/sidecar_cmd/` and `src/sidecar_fmt.rs`: `brokkr sidecar`, described
  in Sidecar command below.
- `src/history.rs`: `HistoryDb`, the global command history at
  `$XDG_DATA_HOME/brokkr/history.db`.
- `src/datasets/`: bbox geometry (`bbox.rs`), the per-dataset bench element
  ID lists (`element_ids.rs`) and the environment inventory (`status.rs`).
  Measurement DB paths live in `db/paths.rs`.
- `src/crit_cmd/`: `brokkr crit`, described in Criterion runner below.

### Databases

`src/db/` has `ResultsDb` and `SidecarDb`, and:

- `store.rs`'s shared SQLite open policy, busy timeout and versioned
  migration owner
- read-only query opens; an older schema is migrated first and a newer one
  refused
- schemas, migrations, queries, formatting and comparison
- the canonical documents of stored library rows (`library_documents`,
  read-only history)
- the sidecar's call boundaries, sink records, arrival positions and per-run
  clock findings

The v11 upgrade marks an older run's clock findings only where an
epoch-magnitude or negative stamp settles it.

The files split as follows:

- `store.rs`: the shared connection and schema-version policy
- `schema.rs` and `migrate.rs`: results.db DDL, connection lifecycle and
  the migration steps
- `types.rs`, `write.rs`, `query.rs` and `compare.rs`: the row types,
  inserts, queries and comparison queries
- `run_store.rs`: run identity and selection shared by the results and
  sidecar stores
- `sidecar.rs`: sidecar.db, separate from results.db
- `hotpath.rs`: parsing of hotpath's JSON report, without depending on the
  hotpath crate
- `like.rs`: literal `LIKE` prefix and substring patterns, with `%` and `_`
  escaped
- `paths.rs`: where the databases live
- `format/`: table, single-row, detail and compare rendering, with
  dataset-aware short names (`dataset.rs`)

### Sidecar

`src/sidecar.rs` is the monitoring sidecar, always on for measured modes. It
owns:

- `/proc` sampling
- the FIFO marker protocol: process-wide and `%CALL_ID`-tagged call
  streams, whose sink-end records are a call sink's sealing emissions and
  terminal record
- a capture that keeps every record in one arrival-ordered list, with its
  loss split by cause (`CaptureLoss`) and a census of the call ids it saw
- the clock check that counts records stamped outside the run interval
  (`ClockOutliers`, stored and repeated on every query; it never rebases a
  stamp)
- `Timeline`, the per-stream chronological projection every marker view
  reads; a stored library call's stream closes at its return when its
  stored `CallSpan` is known

Each FIFO drain is bounded in bytes, so a child writing without pause can't
starve the supervisor's deadline and shutdown checks. The latest marker goes
to the hold's observation table (`lockfile::activity::note_marker`) for
`brokkr lock`.

A run is supervision (`supervise_sidecar`, ending in an `OpenCapture`) and a
finish that closes the capture. The typed paths call both back to back
(`run_sidecar_with_deadline`). A script-bench call pumps the open capture
while its borrow's release waits, finishing only once no holder is left.

### Marker FIFOs

`src/sidecar/namespace.rs` is the marker FIFO namespace shared by the
creator and `brokkr clean`:

- the `.sidecar-TOKEN.fifo` name, with a fresh random token, never removed
  before creation
- the `.sidecar.lock`, which each creator holds shared from `mkfifo` until
  its read end is open, and the sweep holds exclusive
- the readerless test (`ENXIO` on a non-blocking write open), which alone
  lets the sweep remove a FIFO

### Sidecar command

`src/sidecar_cmd/` and `src/sidecar_fmt.rs` implement `brokkr sidecar`: run
and stream resolution, then view rendering, using `Timeline` for the
chronological marker projections.

- `mod.rs` resolves a prefix across the typed stores and the project's
  script-bench runs at once, with one ambiguity rule over both and the
  typed aliases first. It holds `--compare`'s operands (`UUID/ITER/CHILD`
  for a bench side).
- `source.rs` is the one interface the views read: a typed run through its
  sidecar.db queries, or a bench selection.
- `bench.rs` is that bench selection. `--run` and `--child` are judged whole
  before anything renders. Each selected child's stored evidence is
  converted back (`bench::evidence::child_evidence`), with its iteration as
  the stream's run, and its header and capture warnings. It is tested in
  `bench_tests.rs`.
- `views.rs` has the views, written once.

`sidecar_fmt::Process` holds what is known of the process: a typed run's
sampled end, or a bench child's wall. A bench child's wall closes its
timeline, keeps sampleless phases with absent readings, and adds the
microsecond and boundary-free sink fields. `sidecar_fmt::HostProjection`
places a bench child's iteration host trace on its clock, so the phase
summary and `--compare` show each phase's host window.

`src/sidecar_fmt/compare.rs` is `--compare`: a pure pairing of the two
sides' segment names by ordinal within a name, with every segment in one
row and one-sided rows for what the other side lacks, and its two
renderers.

### Criterion runner

`src/crit_cmd/` is `brokkr crit`, the criterion runner for any Rust repo,
not project-gated. See `docs/commands/crit.md`.

- `mod.rs` has the measure and compare split, baseline naming, and the
  `CRITERION_HOME` store under `.brokkr/bench/`. The store keeps that name,
  from when the command was `brokkr bench`, so recorded baselines keep
  reading.
- `discover.rs` finds bench targets from cargo metadata; the owning package
  is derived, never supplied.
- `stamp.rs` is the build-environment record and the shared-fields-only
  difference rule that gates `--compare`.

## Output

These modules implement the console and child-process channel described in
`reference/output.md`:

- `src/output.rs`: declares the console and child-process modules and
  re-exports both, so `output::run_msg` and `output::run_captured` stay the
  call paths.
- `src/output/console.rs`: prefixed console output (`[build]`, `[bench]`,
  `[verify]`, `[hotpath]`, `[run]`, `[sidecar]`, `[error]`).
- `src/output/paths.rs`: `output::path`, a project path for an operator
  line, relative to the invocation directory (anchor fixed once in `main`)
  and absolute otherwise.
- `src/output/child.rs`: the child runners, described in Child runners
  below.
- `src/run_log.rs`: shared run-log reservation, lock activation and
  retention. `check_cmd/report.rs` re-exports its old paths.
- `src/error.rs`: `DevError`, described in Errors below.

### Child runners

`src/output/child.rs` has the captured and passthrough child runners, the
shared supervisor and its `Ending` verdict.

`Exits` is the one statement of which exits complete a run. It has the full
accepted set, where 0 isn't implied, so an answer command expected to answer
no can fail on 0. It also says whether the exit is the command's answer,
which is what records `meta.exit_code`.

Between ticks the supervisor waits on the child's pidfd, so an exit wakes
it at once and the recorded elapsed is the exit, not the next tick. The
exit wake observes once more as `Wake::Exit` before reaping, so a stop
marker still wins.

Every spawn names its `cargo_env::SpawnProfile`. The plain runners are
`Plain`; `run_cargo` and the profile-taking passthrough, idle and sidecar
spawns use `Cargo { phase }`. A bounded variant caps how much of each stream
a capture keeps. `supervise_group_leader` is the supervisor without capture
for a served process-group leader: every ending SIGKILLs the group while
the leader is still unreaped.

### Errors

`src/error.rs` has `DevError`, one variant per failure kind. Two variants
have a reporting contract:

- **Spawn**: a child that couldn't be started, kept apart from `Subprocess`
  so a missing binary never renders as a signal death
- **Reported**: a failure whose site already printed its detail; `check`'s
  summary prints the message of every other error

## Worktrees and install

These modules manage `--commit` worktrees and `brokkr install`:

- `src/worktree.rs`: persistent git worktrees for `--commit`, described in
  Worktrees below.
- `src/worktree_lfs.rs`: the Git LFS pointers left in a `--commit`
  worktree, described in LFS pointers below.
- `src/worktree_lock.rs`: a `--commit` worktree's root `Cargo.lock`,
  described in Worktree Cargo.lock below.
- `src/worktree_record.rs`: `.brokkr/worktrees.toml`, the last-used
  bookkeeping and LRU retention for `--commit` worktrees (`worktree_keep`,
  default 6). It hooks into `context::with_worktree` rather than a command,
  since `--commit` spans many commands (the measured ones, `crit`, `run`).
  It evicts only when cutting a new worktree, never evicts a dirty one, and
  skips and reports on failure. It is a growth damper, not a bound: the
  count is per project.
- `src/git.rs` and `src/git/tree_state.rs`: git identity and tree state,
  described in Git below.
- `src/runnables.rs`: `brokkr run` and `brokkr install`. Both discover
  their targets from `cargo metadata --no-deps`: every member's bins, and
  for `run` its executable examples. `[bin]` curates what discovery leaves
  ambiguous: a `default`, an `install` package list and a shared profile
  default.
- `src/build_info.rs`: the build stamp, described in Build stamp below.
- `src/install_step/`: `[[install]]` steps, described in Install steps
  below.

### Worktrees

`src/worktree.rs` keeps every `--commit` worktree in one container,
`~/.brokkr/worktrees/CHECKOUT_KEY/SHORT/`. A slot has the checkout, one
symlink per path dep leaving the repo, and `.brokkr-target`, which points
to an isolated target dir under the project's effective target. The
container is outside the project because a pre-existing directory is the
only thing a sandbox's writable roots can grant. It isn't
`PROJECT/.brokkr/`, where cargo's upward workspace search would find the
live manifest.

Every admission, cut or reuse:

1. refuses a checkout that isn't pristine (`is_pristine`), asked only after
   cargo config recovery has undone any filtered file a crashed run left
   there
2. finds the tracked files left as Git LFS pointers (`worktree_lfs.rs`)

A cut is gated on the container's free space. It runs `git worktree add`
with `GIT_LFS_SKIP_DOWNLOAD_ERRORS=1`, so an object the LFS server can't
serve stays a pointer instead of failing the checkout. It holds a
`.brokkr-cutting` mark in the slot until admission passes, so a cut that
failed or was killed partway is replaced as stale rather than reused.

`remove_one` is the one slot removal (target dir, worktree, links), used by
`purge_all`, retention eviction and `create`'s stale replacement. The last
two refuse a dirty worktree first, via `is_dirty`. `purge_all` also sweeps
legacy `.brokkr-worktree-*` siblings. `list` must be given the build root,
since the checkout key derives from it.

### LFS pointers

`src/worktree_lfs.rs` finds the Git LFS pointers in a `--commit` worktree by
content. A pointer is a regular-file blob of at most 1024 bytes in the
commit's tree whose on-disk file parses as a git-lfs pointer, whatever the
attributes say now. The file is opened without following a symlink and read
no further than a pointer reaches.

A tracked file missing from the checkout is an error, since a worktree is a
full checkout. Scope is the snapshot at admission: not submodule contents,
not hydrated payload integrity, not what a build reads later.

### Worktree Cargo.lock

`src/worktree_lock.rs` manages a `--commit` worktree's root `Cargo.lock`. On
every admission, after the pristine check and before any cargo call there,
it replaces a lock the commit doesn't track with the live tree's. The copy
goes through a staging file in the slot and is then renamed, so the
worktree doesn't build against whatever cargo resolved the day the slot was
cut.

A tracked lock is never touched. Only the conventional layout on both sides
is managed, where each root `Cargo.toml` is its own workspace root; anything
else is `Unmanaged` and left to cargo. The module also computes the
post-build digest a script bench stores.

### Git

`src/git.rs` and `src/git/tree_state.rs` have commit identity (`Commit`,
`ShortCommit`), `collect` for run provenance, and `TreeState`.
`TreeState` is one NUL-safe `git status --porcelain=v1 -z` parse with named
policies for:

- measurement cleanliness
- the run record
- worktree reuse
- worktree deletion
- prose-only checks
- unstaged diagnostic priority

Toolchain sidecar names come from `toolchain::moved_paths`. A filtered
cargo config is exempt only when `cargo_config_strip::verifies` it as
brokkr's untouched install over an original equal to `HEAD`. Approved-image
recognition comes from sluggrs' writer-owned layout helper.

### Build stamp

`src/build_info.rs` is the build stamp `brokkr install` writes into every
bin it installs (`docs/commands/run.md`, Build stamp):

- `sample` reads HEAD and `TreeState::for_install` at the package
  directory's git top level.
- `install_root` resolves the cargo home install passes as `--root`, or
  names the `CARGO_INSTALL_ROOT` or `install.root` override that leaves the
  bins unstamped.
- `installable_bins` lists the bins cargo installs by the package's
  default-feature closure, refusing a `dep/feat` requirement or an
  ambiguous implicit-feature reading; `install --names` shares it.
- `stamp_file` adds the `.brokkr.build_info` section with `objcopy` through
  a sibling copy, reads it back natively and renames it over the file.

`src/elf_section.rs` is that native reader: std-only, 64-bit little-endian
ELF sections by name (extended numbering included), and `own_stamp` from
`/proc/self/exe`. `brokkr-results-server` includes it by path for its
identity route's `stamp`.

`src/runnables.rs`'s `cmd_install` calls the stamp after each package's
cargo install. The config recompose against the lock wait is in
`main_parts/bootstrap.rs`'s `run_install`.

### Install steps

`src/install_step/` runs `[[install]]` steps. `brokkr install` runs them
after the last package installs and checks them before the first; a copy
source that an earlier `run` step may generate may be missing at check
time.

`mod.rs` has:

- destination resolution: canonical parent, final component never followed
- admission: overlaps first, then ownership from the registry and existence
  alone, so only this step's own install is ever fingerprinted
- parents created only after every check passed on the uncreated path
- `run` steps, through the plain passthrough spawn

The overlap check puts off limits the home, the project, the checkout,
`~/.brokkr`, the source and every other live managed destination. An
unreadable registry record refuses copy steps.

`tree.rs` has the tree fingerprint that is ownership evidence: SHA-256
through `preflight/digest.rs`'s incremental state over paths, types,
permission bits and bytes. Any other object type is foreign and never
opened. The file also has the staging copy and the resumable removal.

`registry.rs` is `user_dirs::install_registry()`'s `HASH.json`: one record
per destination, holding its owner, the journal of an install in flight and
the identity of the parent directory it runs in.

`transaction.rs` does:

- staging in a private `.brokkr-install-RANDOM` container beside the
  destination
- publication by one `renameat2` (exchange or no-replace)
- verification of the displaced object before anything deletes it
- config-independent recovery of every journaled phase, which acts only
  while the parent path still names the recorded directory

## Shared helpers

These modules serve many commands:

- `src/clean.rs`: `brokkr clean` policy and cleanup through the typed
  layouts below. It reclaims free launch leases, then removes:
  - the `rustflags-HASH` target dirs under cargo's resolved target
  - project scratch: pbfhogg's scratch outputs and its scratch `library/`
    run directories, and nidhogg's ingest outputs
  - sidecar FIFOs, readerless ones only, under `sidecar/namespace.rs`'s
    lock
  - artefact trees and test-hung snapshots
  - the isolated unification and fixture-lint targets and build snapshots
  - litehtml's state and pbfhogg's verify output
  - `--commit` worktrees, only under `--worktrees`

  `--cargo PKG` is a `cargo clean -p` of the project's own package.
- `src/clean/layout.rs`: the project `.brokkr` state root and its
  self-ignoring `.gitignore`, named per-project paths, constructed scratch
  targets and reserved target namespaces.
- `src/env.rs`: `brokkr env` report collection, formatting and command
  orchestration over typed host facts.
- `src/host.rs` and `src/host/`: typed probes of the machine
  (`cpu.rs`, `kernel.rs` with io_uring, `memory.rs`, `storage.rs`) used by
  environment reporting and history, with reusable storage and io_uring
  results.
- `src/wc.rs`: `brokkr wc`, the tracked and untracked-not-ignored `.rs`
  files over a line threshold, largest first, listed through
  `git ls-files`.
- `src/health.rs`: observations of brokkr's own state, described in Health
  below.
- `src/user_dirs.rs`: the per-user directory resolver, described in User
  directories below.
- `src/user_dirs/paths.rs`: the shared per-user home, brokkr state-root and
  lock-path source, included by `user_dirs.rs` and the standalone
  `rustc_guard` binary. Explicit-home path helpers let the guard's decision
  use injected paths.
- `src/atomic_write.rs`: the one file replacement sequence, described in
  Atomic writes below.
- `src/preflight.rs`: the facade for `src/preflight/digest.rs`, described in
  Digests below.
- `src/artefacts.rs`: `ArtefactDir`, the per-run `PARENT/TEST_ID/run-N/`
  allocator with preserve-on-failure semantics, used by ratatoskr
  (`.brokkr/ratatoskr`).
- `src/man.rs`, `src/man/render.rs` and `src/man/sections.rs`:
  `brokkr man`, described in Man pages below.
- `src/utf8_path.rs`: consistent UTF-8 path and filename conversion for
  tool argv.

### Health

`src/health.rs` holds observations of brokkr's own state and their one
evaluator:

- the store inventory, with each remedy or the statement that none exists
- allocated size per store: inodes once, no device crossed, incomplete
  walks marked
- free space per filesystem, probed at each root's nearest existing
  ancestor
- the pressure finding under 10% free

`check` reads free space only, and `env` reads both, over the same store
list, so the two report the same findings. The project's configured trees
(cargo target, scratch, data) aren't brokkr state and never enter the list.
Walking a large build dir or dataset tree took minutes. Their free space is
`env`'s `storage:` block, `statvfs` alone, each tree a row even when its
filesystem can't be read.

### User directories

`src/user_dirs.rs` is the one resolver for per-user directories:

- `home()` treats an empty `$HOME` as unset.
- `brokkr_state_dir()` and typed accessors cover the global lock, compile
  lock, lock sockets, test-run registrations, nextest env, harness sockets,
  worktrees and the `[[install]]` ownership registry.
- `xdg_data_home` and `xdg_config_home` count an empty or relative XDG
  value as unset, per the spec.
- `cargo_home` falls back past an unset `$HOME` to `account_home`, the
  passwd entry, as std and cargo do.

It is shared by `guard`, `crit`'s stamp, `rustflags`, `history`, the bench
store, the sidecar backups and the user-wide config.

### Atomic writes

`src/atomic_write.rs` is the one file replacement sequence: hidden sibling
temp, fsync, rename, directory fsync. The last three go through one
descriptor of the target's directory. A temp the module fills itself is
created exclusively and written through its descriptor. Any temp must be
the inode at its name, with no other link, when published. A symlinked
target is resolved so the link survives, and permissions carry over.

The sequence has two tiers:

- **Atomic**: `replace(path, bytes)`, and `Staged` for streamed writes,
  used for brokkr's own state, config edits and downloads of tools. After a
  crash on a journaling filesystem, the target holds the old bytes or the
  new, never a prefix, or is absent if it was before. The new bytes
  surviving is best effort; a failed directory fsync is warned about, not
  an error.
- **Durable**: `replace_durable`, which publishes at the path as spelled and
  refuses a symlinked target, with `create_dir_all_durable(anchor, dir)`
  syncing every directory from an anchor assumed durable (the home) down,
  on every call. Success means the new bytes survive a crash, and an error
  says whether the target was already replaced.

The durable tier serves the `[[install]]` registry records. The lease store
and the install destination resolver use the directory chain.

### Digests

`src/preflight/digest.rs` has parsed XXH128 and SHA-256 values, the file
and tree digest distinction, and cached hashing. On a cache miss, a file is
read through one descriptor. Its stamp must not move during the read, nor
its path name another file after it, or the digest is refused and not
cached. A FIFO, device or socket is refused, whether it is the path
digested or inside a tree.

### Man pages

`brokkr man` is built from three files:

- `src/man.rs` has the `TOPICS` table: name, summary, `include_str!`'d
  markdown and `Visibility`.
- `src/man/sections.rs` is the heading-slug section addresser: a
  fence-aware `##` and `###` scan, byte ranges, and exact, prefix and
  substring resolution.
- `src/man/render.rs` is a `pulldown_cmark` to ANSI terminal renderer for
  headings, GFM tables, alerts, footnotes and nested lists. Links keep their
  text and drop the URL.

## Results server

`src/bin/results_server/` is `brokkr-results-server`, the bench store
behind HTTP (`reference/results-server.md`, `docs/results-server.md`). It is
a self-contained bin target that includes `src/bench/store/` with `#[path]`:

- `main.rs` has the subcommands (`serve --db --listen`, `deploy`; nothing
  deployment-specific compiled in), `Limits` and startup. Its connection
  deadline is `wire::CONNECTION_DEADLINE`, which brokkr's client outlasts.
- `identity.rs` has the executable digest the identity route reports,
  streamed from the opened `/proc/self/exe` at startup, and the probe it
  echoes.
- `deploy.rs` is the deploy (`docs/results-server.md`, Deploying after an
  install): the host's fixed home-relative declaration, its restart argv
  under one deadline, then fresh probes until the endpoint answers with
  deploy's own digest.
- `owner.rs` resolves the file to one canonical path, refuses hard links
  and dangling links, and takes the `FILE.lock` flock before `Store::open`
  migrates.
- `worker.rs` is the single thread owning the store. It has the bounded job
  queue, the `Queued`, `Running` and `Withdrawn` compare-and-swap between
  worker and handler, the autocommit check after every job, and the guard
  that stops the process when the worker ends.
- `http.rs` is hyper's HTTP/1.1 server on tokio: routing and refusing from
  the head, admission before the body, the deadlines and the advisory
  statuses.
- `tests.rs` drives the server over loopback HTTP, and `deploy_tests.rs`
  drives deploy against it.

## Serve

`src/serve.rs` and `src/serve/` are `brokkr serve`
(`docs/commands/serve.md`): long-lived servers that hold no lock.

`serve.rs` has:

- the start, under dispatch's hold: replace the same identity through the
  verified stop, link the snapshots into the generation directory
  `.brokkr/serve/NAME.TOKEN/`, publish, spawn the supervisor, and wait for
  it and for `--ready`
- the lock-free list and stop
- the measurement gate `refuse_measurement`, which requires the hold
- the once-per-process `warn_if_live` from the fresh-hold hook
- `brokkr lock`'s report
- the reconcile every acting reader runs: a free entry is SIGKILLed down by
  carrier scan and retired once none is left; another namespace's entry is
  unknown and blocks

`refuse_measurement` is called where every measurement begins:
`MeasurementSession::with_request` for a recorded mode, ratatoskr's sync
bench, the script bench before its preflight, and crit sampling.

`registry.rs` is `~/.brokkr/serves/`. Each generation has a `.lock` file,
which is the supervisor's flock and the immutable intent, created, locked
and written whole under `registry.lock` before the supervisor exists. It
also has a `.state` file the supervisor replaces by rename. An entry is
never unlinked while held. `registry::scope_of` judges a recorded boot and
namespace: only a positively earlier boot retires unscanned, and any gap is
unknown, which blocks.

`supervisor.rs` is the detached brokkr process that `main` routes
`--brokkr-serve-supervisor` to before detection. In order, it:

1. sets CLOEXEC on its flock
2. installs stop handling in never-escalate mode before it publishes its
   identity
3. runs the server as its own group leader, with parent-death,
   `BROKKR_SERVE=TOKEN` and no hold variables, under
   `output::child::supervise_group_leader`
4. runs a final carrier sweep, then exits

`carriers.rs` scans `/proc` for `BROKKR_SERVE` carriers, binding each
environ read to its starttime, and reports unknown on a table it can't
read. A reader's scan exempts its own process family from signalling but
still counts it; the supervisor's own sweep exempts nothing.

## Script benches

`src/bench/` implements script benches. This section groups its files by
role: the Lua seam, the run, the store and its clients, and the iteration's
supporting layers.

### Lua seam

`lua.rs` is the one seam to the linked dellingr crate, reached by its
absolute `::dellingr` path; a textlint rule keeps the rest of the tree off
it. It is tested in `lua_tests.rs`. It does these jobs:

- compiles a script and checks its global names, including whether it
  declares `preflight` or `validate` (a bare global assignment anywhere)
- runs one sample in a fresh State
- runs a declared hook (`Hook::Validate` or `Hook::Preflight`) in one
- runs a contract-test file, described in Contract-test files below
- implements `require` and `check_module`, described in Modules below
- marshals every builtin and decides it through its `Host` trait

The phase (top level, validate, preflight, bench) decides which builtins a
call may reach. A chunk's top level and `validate()` launch nothing, make no
handle and ask no `since()`.

Marshalling works as follows:

- Strings cross as bytes.
- A builtin's failure goes back through Lua as an `Abort` and is stored
  after the calling line.
- A stop is asked for before every builtin but `print`, and polled by a
  watcher thread that triggers the State's interrupt, so pure Lua stops at
  its next safepoint.
- Handles are recognized by a protected metatable, and JSON arrays and
  `json.null` by identity through the State's registry (`Marks`).
- The `binary_with()` options, `record` and `json.encode` are read through
  one Lua-to-`Value` conversion.
- A script's line map is applied to its own chunk's frames alone, and every
  error's lines are mapped when the script has a line map.

`Value` is the dellingr-free form of `bench(params, keys)`'s arguments,
`config()`'s table and JSON.

### Contract-test files

`run_suite` in `lua.rs` runs a contract-test file in a State with every
bench builtin removed and `run`, `test`, `assert` and `project_root` added.
`run(spec)` reaches a `Driver`. Each `test` case runs at once from inside
its builtin and its failure is recorded. A stop ends the file.

### Modules

`require` loads a `bench_lib` module into the State on its first call
there. The module's chunk runs at top level whatever phase asked, and that
phase is restored after. Its export is anchored and cached. The loading
chain refuses a cycle and requires exactly one non-nil return. The module's
own error passes through unchanged, and every name asked for is recorded.
`check_module` holds a module to no global writes and no `_G`.

`library.rs` is the `bench_lib` directory as one run sees it
(`docs/commands/bench.md`, Modules). It is read once, and every `.lua` file
with a module name is compiled (`module_name`, the rule the registry check
shares). Symbolic links and unnamed files are passed over.

### JSON and script lines

`json.rs` is JSON over `Value` (`docs/commands/bench.md`, JSON):

- encode with sorted keys and a size cap
- strict decode: duplicate keys refused in the visitor, integer tokens kept
  exact, depth checked before Lua sees it
- `record`'s object root

`script_lines.rs` maps an inline script's Lua lines to its config file's
(`docs/brokkr.toml.bench.md`, Inline line numbers). It rereads the
declaring file with toml_edit's spans and decodes the literal with each
byte's file line. It keeps the map only when that decoding, toml_edit's and
the loaded script agree. `mod.rs`'s `InlineLines` names the chunk and words
the finding either way.

### Resolved view

`view.rs` is the resolved config a script sees (`docs/brokkr.toml.bench.md`,
The resolved view), tested in `view_tests.rs`. It is a pure function of the composed table, the
hostname and the project root:

- the host is selected through `config::is_host_key`
- file paths are absolute
- the primary appears as `snapshot.base`
- every file entry is recorded by segmented path, with where its pin was
  set

`View::host_of` is the one rule that a resolved path under `datasets` or
`host` is this host's. Every refusal of a missing key or table, and
`--help`'s empty listing, words by it.

### Param binding

`params.rs` binds a bench's params (`docs/brokkr.toml.bench.md`, Binding
params), tested in `params_tests.rs`. It handles:

- the tokens after NAME: lists, ranges and `all`, with their key order
- dependency order
- collections bound whole, and every other param's several values fanned
  out into arms resolved per parent
- the run-time refusals, collected across every arm
- the bench's `rustflags` resolved per arm (`Bound::rustflags`), a splice
  replaced by its param's selected array
- the bound files digested in preflight
- the document's `params`, the inputs and the script's arguments

Among the run-time refusals, a `required` param's absence is one refusal
whatever its arms, with the keys under each path its parents lead to as
detail only. A `choices` value outside its list is refused too.

Each bound file's content is digested once, and each entry's shape and pin
on its own. Under `Depth::PinsOnly`, a dry run's depth, every shape is
checked and only a pinned file is hashed.

### Run

`runner/` is `brokkr bench NAME` (`docs/commands/bench.md`). `runner/mod.rs`
is the invocation. Before any arm runs, it:

1. refuses `--hotpath` for a bench without `hotpath_features`, `--alloc`
   for one without `alloc_features`, and the two together
2. parses params and binds them into arms
3. answers `--help` with each param's domain and each `from` param's keys
   per parent path
4. refuses a bench whose `validated` params have no `validate()`, then runs
   every arm's `validate()`
5. under `--dry-run`, resolves the commit, checks every input's shape and
   pinned inputs' pins, names each arm, and stops
6. opens or probes the destination before anything is built
7. holds every arm's inputs for reading, then digests them
8. freezes the script

Each arm then runs as its own run:

1. The build folds in host features, and under `--hotpath` the bench's
   `hotpath_features` (under `--alloc` its `alloc_features`), and records
   cargo's resolved features. A timing build that resolved a hotpath or
   alloc feature, or a hotpath build that resolved an alloc one, fails its
   arm.
2. The `--commit` worktree, if any.
3. The check after the build: the tree built must still be at the
   invocation's commit, or the arm fails. The live tree's dirty state is
   read again and joined with the first reading.
4. The iterations and the outcome.
5. Publication through `publication.rs`.
6. The uuid line naming where the run went.

A failed arm doesn't stop the rest. A stop, a failed publication or,
without `--commit`, the live HEAD found moved before the next arm does stop
the rest. An attempt is `build_into` (the worktree and the build), then
`iterate` against the snapshot once the worktree is released, with
`since()` answers handed in and back.

### Rounds

`runner/rounds/` is `--rounds` (`docs/commands/bench.md`, Rounds):

- `plan.rs` is the grid. Each arm is placed by its competitor (a
  `--versus` param's key, a commit, a bench) and its workload key, and
  every slot holds one recipe.
- `mod.rs` runs the preparation once for every bench and commit, and each
  distinct build once up front. Builds are keyed per cell, so a workload
  param spliced into the bench's `rustflags` gives its workload its own
  build. A failed build is stored as a build-phase run. Then it runs the
  cells, workload by workload and round by round, with the order reversed
  every other round, and publishes each with its `store::Schedule`.

### Iteration host

`runner/host.rs` is an iteration, tested in `host_tests.rs` against real
children (the input in `writes`, substituted outputs and replaced spools
included). The iteration's owner lease covers the bound inputs, which are
its first handles. Every call is first judged by `policy.rs` and admitted
with the access it declared.

The builtins behave as follows:

- `binary()` runs on a borrow, with a fresh marker FIFO per child drained
  while the borrow's release waits.
- `since()` is cached per run.
- `output()` and `scratch()` give handles.
- `binary_with()` takes `env`, `reads`, `writes`, `stdin`, `hotpath`,
  `stop_at`, `deadline`, `program`, `measured`, `cold` and `warm`,
  described in Call options below.
- Each result's spools are given out as `stdout_file` and `stderr_file`.
- `read()`, `write()` and `filesize()` go through the resource layer.

Each child's stderr is snapshotted from its spool: the first and last MiB
for the script, and a 64 KiB tail for the store, under a run-wide stderr
budget. `runner/host.rs` also calls the host trace around each sample.

### Call options

The `binary_with()` options a call can declare:

- **hotpath**: the call's admission makes the report under `profile/` and
  sets the three `HOTPATH_*` variables. The report is judged after every
  reaped launch, whatever its exit, and stored on the child with its stage
  and occurrence, under a run-wide report budget. Under `--hotpath` or
  `--alloc`, a completed iteration fails when a marked call didn't capture
  or none was marked.
- **stop_at**: handed to the sidecar supervisor as an exact marker
  (`sidecar::StopRequest::exact`, a span end or a point marker). On a marked
  call it comes with a SIGTERM grace (`ChildOptions::stop_grace`), so the
  child can write its report. Its `StopMarker` ending is given back as
  `stopped_at`. Any other ending of such a call fails the iteration after
  its child is stored.
- **deadline**: handed to the supervisor as its wall limit, a lifetime
  ceiling that also ends a stop's grace. Its `Deadline` ending fails the
  iteration, or refuses a preflight, through `host::overdue`.
- **program**: resolved through `external.rs`, with its executable stored on
  the child. One that can't be resolved fails the iteration.
- **measured**: marks the one child whose wall is the iteration's result.
- **cold** and **warm**: checked against what the call declares written,
  opened through the resource layer, grouped by inode, and prepared through
  `cache.rs` after the admission and before the launch instant. The account
  is stored on the child, and a failed one fails the iteration with no
  child.

### Preflight and validate

`runner/preflight.rs` is a script's preflight (`docs/commands/bench.md`,
Preflight), tested in `preflight_tests.rs` against real children.
`runner/mod.rs` runs it for every arm after the inputs are held and the
commit resolved, and before they are hashed. Every refusal is collected, so
one refuses the invocation whole and nothing is stored. Its host launches
external programs only, each on a launch lease that reserves nothing and is
inherited by the child. A program that can't be resolved or executed is
given back as `start_error`.

`runner/validate.rs` is a script's `validate()` host
(`docs/commands/bench.md`, Validate): `config()` alone. It refuses every
launch, handle and `since()` again behind the Lua seam's own refusal.

`runner/stored_config.rs` is the configuration text a run stores, with
every module the run's preflight or iterations asked for among it.

### Contract tests

`runner/contract/` is the contract tests (`docs/commands/bench.md`,
Contract tests), run by `brokkr bench --test` and check's `bench` phase. It
is tested in `contract/tests.rs`:

- `spec.rs` reads a `run(spec)` table strictly, every key known.
- `mod.rs` runs one arm through the real binder against `view::build` of the
  composed table, with every host section dropped and the spec's host (held
  to `config::check_host_section`) as the selected one. It runs the real
  `ValidateHost`, then `host.rs`'s virtual preflight and iteration.
- `host.rs` has the virtual hosts. They apply `policy.rs` and answer every
  effect from the spec. A fixture fault is kept in `Shared::fault`, apart from the script's
  outcome, and raised in the test. A declared response no call asked for is
  refused.

### Run tests

`runner/run_tests.rs` drives `run` itself end to end over a fixture crate
that cargo builds in the test, in a git repository of its own. It runs
against the scratch bench store or a loopback results server, and covers
every rule of the arm loop:

- a failed iteration or build continuing
- the summaries
- HEAD moving from a child and, through a build script some fixtures get,
  during the build
- the tree dirtied or cleaned by the build
- a stop inside an iteration and during a publication, the last arm's
  included
- a refused publication

### Static checks

`src/bench/mod.rs` has the static checks check's `bench` phase runs
(`run_bench` in `check_cmd/conventions.rs`):

- `check_script`, run on every bench: read the script, compile it and
  resolve its global names through `lua.rs`'s `check`, executing nothing
- `check_library`, run once on the `bench_lib` every bench shares and, with
  `bench_scripts` set, on the one the registry has
- `check_registry`, which with `bench_scripts` set runs `registry.rs`

`check` compares the global names dellingr records while compiling
(`Program::globals`) against a fresh State built by the same setup a sample
uses. The script's own assignments count as definitions, and a read of `_G`
leaves the names unchecked with a warning.

### Script registry

`registry.rs` judges every bench's script by its check-time `Location`: its
directory resolved then, and its name as spelled. Registration and
`test_overlaps` compare the same identity. An inline script, or a
`script_file` located outside the root the walk resolved, is a finding
owned by the bench.

The registry walks the scripts directory through held directory
descriptors, descending real directories only and opening nothing else. It
judges each `.lua` entry by its location (canonical directory plus name as
spelled) against:

- the bench locations
- the exemptions, as path prefixes
- for a regular file under `bench_lib`, `library.rs`'s `module_name`

Every failure to list or inspect is a finding. The files `discover_tests`
finds under `bench_tests` by the same walk are excused, and `test_overlaps`
refuses a test file that is a script or a module. A finding's
`FindingOwner` names a bench, the library, the registry or a test.

### Bench store

`store/` is the bench store (`reference/bench-store.md`). It is
self-contained so `brokkr-results-server` includes it by path. It has:

- the run document, which includes a marked child's hotpath capture, a
  `program` child's executable and each spooled child's stderr tail
- its validation, derived result and pairing key with its measurement class
- a `--rounds` run's schedule, its validation and the stored-history decode
  that reads a legacy per-competitor build list as the per-workload matrix
  (`schedule.rs`)
- the frozen migration chain
- publication with the run uuid as request id, and load
- marks (`marks.rs`), described below
- the reader's side (`list.rs`), described below
- the wire contract (`wire.rs`): requests, answers, `execute` and `serve`,
  the failure envelope and its classification, the protocol version and its
  frozen fingerprints; tested in `wire_tests.rs`

The remaining files split that work:

- `mod.rs`: the `Store` and its open
- `document.rs`: the run document, as the store takes it and gives it back
- `write.rs`: publication, one validated document applied in the caller's
  transaction, every row or none
- `read.rs`: loading a stored run back into its document
- `schema.rs`: the frozen migration chain and the open policy
- `error.rs`: `StoreError`, split by what a caller may conclude about a
  write: refused and failed writes are unapplied, an errored commit may
  have applied

`store_tests.rs` and `wire_tests.rs` in `src/bench/` test the store and
the wire contract.

`marks.rs` is the append-only log of retire, invalidate and rename requests
and their undos, with a mark uuid as request id and admission against the
folded state. Its fold, `MarkView`, resolves rename chains and hides runs,
and `Revision` is the two-bound revision a read is pinned to. It is tested
in `marks_tests.rs`.

`list.rs` has the read-only open that never creates the file, `RunFilter`
and the listing queries in one total order. A listing is resolved and
hidden against the marks in one snapshot, and returns each run with what
the store derived from it.

### Store clients

These files are brokkr's side of the store:

- `evidence.rs`: the store's path (one scratch store per test process under
  `cfg(test)`), a sidecar capture and a supervisor ending as document
  evidence, and credential redaction. For `brokkr sidecar` it does the
  reverse: a stored child's evidence as the sidecar's types, with the
  store's derived positions.
- `remote.rs`: the results server client, described in Remote client
  below.
- `publication.rs`: what both destinations share, the retry admission and
  the uncertainty latch, described in Publication below.
- `marking.rs`: `brokkr results`' mark flags (`docs/commands/results.md`,
  Marks). A run's uuid prefix is first resolved, through `results/`'s
  reader, to the one run of the project it names, hidden or not. Then one
  `MarkRequest` is minted per command (a fresh uuid, now, this host) and
  appended through the latch, to an existing local store opened without
  creating it, or through `remote.rs`. It is tested in `marking_tests.rs`.

In the `brokkr` binary, `runner/` is the one publisher of runs and
`marking.rs` the one writer of marks, each locally or through `remote.rs`.
`brokkr-results-server` stores what HTTP clients send it.

### Remote client

`remote.rs` is brokkr's client of a results server, used in place of the
local store when the user-wide `results_server` names one
(`reference/bench-store.md`, What a client owes the contract). It is tested
in `remote_tests.rs` against a scripted loopback server, with a real store
behind `wire::serve` for the round trip. It uses:

- blocking ureq with rustls, one connection per attempt, no redirects
- an attempt timeout outlasting `wire::CONNECTION_DEADLINE`
- answer classification by the envelope, else by the status alone
- a transport error proved unsent only for a connection that never existed
- the destination probe, `GET /bench`, with the protocol checked
- retried reads
- a publication encoded once, with a 200 taken as acknowledgement
- a mark sent the same way, with its conflict read under the mark's uuid

### Publication

`publication.rs` has the retry admission, `Policy`: a retry-start window
and an attempt cap, and a backoff a stop ends. A publication whose
uncertainty is latched gets one attempt past either limit. The latch itself
is worded for a run or a mark, and a local `StoreError` is classified by its
wire kind.

### Results views

`results/` is `brokkr results` over the store (`docs/commands/results.md`,
Script benches). `results_cmd.rs` calls it beside the typed views.

- `mod.rs` maps the filters and looks runs up on either backend. A remote
  run answer must have read the children it was asked for. `resolve` is
  `brokkr sidecar`'s lookup, one run read whole.
- `table.rs`, `single.rs` and `compare.rs` render; `host.rs` has the
  host-state lines the last two both use.
- `rounds.rs` summarizes a `--rounds` invocation from every run of it, read
  at one revision.
- `config_diff.rs` lists the config paths that changed.
- `annotations.rs` shows how the compared bench's annotations differ.
- `tests.rs` has the views' unit tests.

Each view reads runs against the marks they were selected under; a
`Listing`, a `Lookup` or a `Comparison` holds them. A hidden run is marked
with what hid it, and every run is shown under the name its rename chain
resolves to, with a differing stored name disclosed. The comparison pairs
on the resolved name and `MarkView::comparison_key`, never rewriting the
stored key, and names what it leaves out, including the external
executables each side ran.

For the comparison, each side's stored config is read on its own, under the
name that side was stored as, with the JSON fallback's datetime wrapper
decoded; an unreadable side is named. Every compared run is read with its
children, so captures diff by stage and occurrence and each child's
executable and page-cache preparation are seen whatever the run's class.
Every captured report and `record.hotpath` goes through the typed rows'
hotpath printer.

### Page cache

`cache.rs` is the page-cache preparation of a call's inputs
(`docs/commands/bench.md`, Page cache), all on descriptors. It is tested in
`cache_tests.rs`:

- Observability is asked of the descriptor (`faccessat2` W_OK, effective
  credentials).
- `cold` is write-back then `DONTNEED`.
- `warm` is positional reads through the size observed.
- Every file is prepared before any is scanned.
- A windowed `mincore` scan is counted in bytes to the file's end.
- Observability and size are checked again.
- A failure accounts for every file.

`prerequisites` is what a preflight's `page_cache()` checks of a bound
input (`preflight.rs`, opening it through `reservations::open_input`).

### Host trace

`host_trace.rs` is each iteration's host trace (`reference/bench-store.md`,
The run document):

- the block device holding the iteration container, resolved through sysfs
  (a partition to its disk)
- `/proc/meminfo` `Dirty` and `Writeback` and the device's cumulative
  `stat` counters, read on `CLOCK_BOOTTIME`
- a sampler thread started before `bench()` and joined after it, under a
  run-wide reading budget
- the cell cursor beside the global lock: an in-progress entry written at
  an iteration's start, an ended one after its release
- the run's previous history row and `fstrim.timer` stamp
- the reader's derivations (`window`, `gap`) that `results/host.rs` and the
  sidecar phase views share

### Resources and policy

`resources.rs` is the handle layer over `reservations::Iteration`
(`reference/bench-resources.md`):

- origins and default access
- the `reads`, `writes` and `stdin` override and its refusals
- which handles the file builtins take (`Handle::openable`, `rewritable`,
  `preparable`)
- a finished call's report, read under its budget

`policy.rs` is the call policy every host applies before any effect, shared
by `runner/host.rs`, `runner/preflight.rs` and the contract tests' virtual
hosts:

- the handle table
- a `bench()` call judged into a `Declared`: measured once per iteration,
  cache options only on inputs the call passes and never one path in both
  modes, and the access `call_access` declares, which admission is then
  asked for
- a preflight call's refusals and NUL check
- `page_cache()`'s bound-input rule
- the one postcondition both hosts judge after a call, `Declared::ended`; a
  `stop_at` call whose stop didn't fire fails the iteration

Output and scratch names are `reservations::Names`, which the real
iteration and the virtual one share.

### External programs

`external.rs` is what a `binary_with({program = ...})` call runs in place of
the snapshot (`docs/commands/bench.md`, binary_with). The name is
resolved on the child's own `PATH`, or as a path, to the absolute path the
child is launched by. That file is observed as its xxh3-128 digest, or as
why it couldn't be read. It is opened without blocking and refused unless
regular.

## Project commands

These modules implement commands that apply to particular projects:

- `src/litehtml/`: the litehtml visual subject, described in Litehtml
  below.
- `src/sluggrs/`: the sluggrs visual subject and hotpath runner. `cmd.rs`
  dispatches the snapshot commands, `db.rs` adapts the shared visual store
  to sluggrs' snapshot tables, and `hotpath.rs` builds the `hotpath`
  example with its feature and runs it through the bench harness. Shared
  visual mechanics live under `src/visual/`. See
  `docs/projects/sluggrs.md`.
- `src/visual/`: shared visual mechanics. `status.rs` is the status policy,
  tally and ratchet slack; `compare.rs` pixel comparison and artifact ids;
  `engine.rs` the subject runner and prefix resolution; `db.rs` the
  versioned visual stores (`.brokkr/litehtml-visual.db`,
  `.brokkr/sluggrs-visual.db`). The stores are read-write for `visual` and
  `approve`, and read-only for the query commands. The module does the
  one-time import of each subject's legacy tables from `results.db`, which
  are left in place.
- `src/visual_approve_cmd.rs`: `brokkr approve` over either subject.
- `src/ratatoskr/`: harness orchestration, described in Ratatoskr below.
- `src/mogwai/`: `brokkr mogwai`, described in Mogwai below.
- `src/deps/`: `brokkr deps`, described in Dependency audit below.
- `scripts/litehtml-prepare/`: Node.js fixture preprocessing (cheerio,
  image-size and pngjs) and the pinned Puppeteer the reference capture
  resolves.

### Litehtml

`src/litehtml/` is the litehtml visual subject: Chrome capture, pipeline
execution and element scoring. See `docs/projects/litehtml.md` and
`docs/commands/visual.md`. Shared visual mechanics live under
`src/visual/`.

Its files:

- `cmd.rs`: command dispatch and pipeline execution, and the bundled
  capture script's path in the per-user runtime cache
- `capture.js`: the Chrome capture script, bundled into the binary and
  written to that cache
- `compare.rs`: element matching and scoring over the shared pixel
  comparison and status thresholds
- `db.rs`: the adapter from the shared visual store to litehtml's tables
- `reference.rs`: the published reference, described below

One resolver, `LitehtmlFixture::resolved_viewport_width`, feeds both the
capture viewport and the pipeline's `--width`. `reference.rs` owns the
published reference: staged publication with `chrome.meta.json` as commit
marker, and width and id checks.

Scored results record the width and the reference id they measured;
approvals record only the width. An approval therefore ratchets only
results at its own width, and deliberately survives a same-width
`--recapture`. One shared predicate decides a result whose reference has
since been replaced: `approve` refuses it, and `visual-status` shows it as
`STALE`.

### Ratatoskr

`src/ratatoskr/` is harness orchestration. See `docs/projects/ratatoskr.md`.
`mod.rs` states the split: brokkr owns project gating, harness builds from
`[ratatoskr.harness]`, the lock, artefact dirs and sync-bench history,
while the Lua VM and the `ServiceClient` bindings live in ratatoskr.

- `discover.rs` finds service and sync harness scripts (`**/*.lua`) and
  parses their frontmatter.
- `gate.rs` evaluates the sync-bench regression gate: configured rules
  compiled to typed metric selectors and predicates, judged per rule over
  a current and a baseline `gate_runs` row.
- The protocol registry lives in
  `src/config_parts/project_config/ratatoskr.rs`: the one list of protocols
  with their sentinel names, endpoint shapes and `test_endpoint_env_*`
  keys. `saehrimnir.rs` adds sentinel parsing, endpoints and the
  mock-server lifecycle on top of it.
- `service/mod.rs` declares `service/list.rs`, `single.rs` and `suite.rs`.
  `cmd.rs` re-exports the service entry points.
- `sync.rs` declares `sync/list.rs`, `all.rs`, `smoke.rs`, `runner.rs`,
  `bench.rs` and `gate.rs`, which hold sync indexing, execution,
  measurement and gate evaluation.
- `gate_store.rs` owns the ratatoskr `gate.db` schema and queries.
- `timing.rs` owns lifecycle timing budgets and the frontmatter duration
  grammar. Operator duration displays use `output::format_duration`.

### Mogwai

`src/mogwai/` is `brokkr mogwai`. See `docs/projects/mogwai.md`. It has two
surface kinds, one row shape and no layers:

- **Argv-shaped** surfaces go through the registered bin and need no
  registration (`brokkr mogwai -- gen ...`).
- **Harness-shaped** surfaces resolve a name against `[mogwai.targets.*]`
  to a cargo example plus the features it must be built with
  (`targets.rs`).

Features live in the registry because `--hotpath` and `--alloc` are inert
without them; without the registry, rows would record no profile. A
call-site `-F` appends rather than replaces.

`mod.rs` declares `cmd.rs` and `targets.rs`. `cmd.rs` branches on mode. `--hotpath` and `--alloc` use `run_hotpath` plus
`capture_hotpath`: the report is a file the child is told about, so
`run_external` would record a row with no profile in it. Other modes use
`run_external`. Rows are filed under the target or bin name, with the
invocation captured verbatim in `cli_args` and `brokkr_args`. Pairing is a
query (`--grep` selects an arm, including one defined by an absent flag),
not a name lookup.

Both spawn sites run the child with the project root as cwd, under
`--commit` too, so only the binary comes from the worktree. Mogwai's
defaulted inputs are repo-relative and its corpus and cache are gitignored,
so a worktree checkout doesn't populate them.

The project's `[mogwai.workloads.*]` registry, `meta.timing`,
`meta.identity_counters` and `[HOST.corpus.*]` no longer exist; see the
doc's removal table. A `[HOST.datasets.*]` entry is visible to `brokkr env`
but never read by `brokkr mogwai`, whose rows have no dataset or input
file.

### Dependency audit

`src/deps/` is `brokkr deps`, the dependency audit for any Rust and git
repo, not project-gated. See `docs/commands/deps.md`. It is phase-based
like `check`:

- `mod.rs`: the `DepsEvent` enum and `run()`, which loads one resolving
  Cargo snapshot and projects its graph to the host
- `phase.rs`: the `Phase` list and each phase's outcome and summary
- `render.rs`: the text and NDJSON renderers
- `duplicate_version.rs`: blame-aware duplicate detection with structured
  pickers
- `git_dependency.rs` and `path_dependency.rs`
- `publish_cycle.rs`: publication-cycle detection over declared manifests
- `workspace_dep.rs`, which shares `ManifestDeps`
- `native_code.rs`
- `ccu.rs`: the `ccu --json` shell-out feeding the outdated and stale
  phases
- `focus.rs`: the `brokkr deps PKG` chain trace

Shared typed metadata and its indexed graph live in `src/cargo/metadata.rs`.
