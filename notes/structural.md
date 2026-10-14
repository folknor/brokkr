# Structural

Shape and placement, from the design hunt: axes that should be types, and moves,
splits and rewrites, each with the payoff it buys. Siblings: `consolidations.md`
(CON), `bugs.md` (BUG), and the hygiene documents (`hygiene-*.md`), where design
findings about mechanisms those documents already held were added to the
existing entries instead. Entries record what hunters reported; nothing has
been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

## STR-001 - The project root and the build root are the same type, and are already swapped

Reported by: config-cli, measurement.

`Detection.project_root` (the config dir: `.brokkr/`, data, stores) and `Detection.build_root` (the cwd: cargo, git, `target/`) are both `PathBuf`, and call sites rebind them under each other's names. The disk gate no longer reads a root from `LockContext` (it probes the cwd's target itself), so the residue is the measurement and bootstrap plumbing below. `BenchHarness::new_with_lock(paths, project_root, db_root, ..)` is called with `project_root = <effective build root>` and `db_root = Some(project_root)`, so the parameter named `project_root` receives the build root, and git and env are collected from it. `MeasureRequest { project_root, build_root: Option<&Path> }`, `resolve_bootstrap_paths` returning `(paths, effective, db_root)`, `with_worktree(project_root, parent_build_root, ..)` and `bench_cmd::run` destructuring `(project, project_root, build_root, ..)` all carry the same pair. Comments in `bench_cmd/mod.rs` and `context.rs` spend paragraphs explaining which root is which. `run_measured`, the `Verify` arm and the `Sync` arm each recompute the build root from `std::env::current_dir()` and compare it with `project_root` to derive `parent_build_root`, though `detection.build_root` already holds that value.

Proposed: `ProjectRoot(PathBuf)`, which owns `.brokkr/`, every store (`results_db()`, `sidecar_db()`, `bench_home()`) and the lock context, and `BuildRoot { Live(PathBuf) | Worktree(Worktree) }`, which owns git identity, cargo's cwd and the target dir. Both are minted only by `project::detect*`; the no-config case mints both explicitly from the cwd. `LockContext` takes a `BuildRoot` for the disk gate and a separate display label instead of a `&str` path. `BuildRoot::cargo_command()` sets `CARGO_TARGET_DIR` for the worktree variant, so no spawn can forget the isolation rule.

## STR-002 - Dispatch has no model of a command

Reported by: config-cli, process-control.

Every property a command has is decided inline in a roughly 1750-line `run()` match: which projects it applies to, whether it needs a `brokkr.toml`, which root it anchors on, whether it takes the lock and under what label, whether it opens a run log, and which project section it requires. Several of these are decided a second time elsewhere: the help table, the `man` topic table, the handler's own `project::require` and the `[project]` tags in help text. `run()` is two halves with an ordering contract. First comes a chain of `if let Command::X = cli.command { return ...; }` for pre-detection commands, then `detect()`, then a `match` whose first two arms exist only to error if the chain was reordered ("internal error: a pre-detection command reached project dispatch"). Each arm independently chooses `detect_optional()?`, no detection, or `detect()`, and which root to use. Lock policy is likewise per arm, with free-text labels (`"run"` for `Passthrough`, `"verify pmtiles"`, `"regress"`). `Stop`, `Status`, `Query`, `Geocode`, `Results`, `Sidecar`, `Invalidate`, `CompareTiles`, `PmtilesStats`, `List`, `Report`, `Approve` and `Prepare` take no lock. `Approve` and `Prepare` (which runs Node) are the kind of handler the `Visual` arm's comment says was last time "the one cargo-driving command reaching its handler unlocked". `context::invoked_command()` is a third way to name the lock holder. `LockContext { project: &str, command: &str, project_root: &str }` carries the command as a string, and `disk_gate::check` exempts commands by comparing that string with `"clean"` and `"env"`, so a renamed subcommand silently loses its exemption. `env::collect_tools` is a per-project `match` on `Project` listing which tools to probe, another fact about a project stored outside it.

`cli/schema.rs` (about 3250 lines) is one flat `Command` enum of about 85 variants, with project tags in doc strings and per-project arg structs (`PbfArgs`, `VerifyPbfArgs`, `ModeArgs`). `as_pbfhogg()`, which maps CLI variants to `PbfhoggCommand` and returns a 5-tuple under `#[allow(clippy::type_complexity)]`, is a pbfhogg concern living in the CLI module, so the edge points cli -> pbfhogg internals. Project argument plumbing also lives in the dispatcher: `Sync`'s mode resolution and `validate_gate_selection`, `Corpus`'s `--force` refusal, `Service`'s bare-is-an-index branching, `Suite`'s per-name `require`, and `mode.hotpath = Some(1)` written into `Hotpath` to express a per-command default mode.

Proposed: a declarative `CommandSpec` reached through an exhaustive `fn spec(&self) -> &'static CommandSpec`, so a new variant is a compile error rather than a fail-open table miss. It holds `applies: Visibility`, `config: Required | Optional | None`, the lock policy `None | Blocking | NonBlocking` with a label derived from the name, whether the disk gate applies, the default mode and the tools to probe. Dispatch resolves what the spec needs (detection, lock, run log) into a `Ctx` and calls one `handle(self, ctx)` per command family module (`pbfhogg::cli`, `elivagar::cli`, `ratatoskr::cli`, ...), where each project defines its own commands next to its dispatch. The "internal error" arms and the `as_pbfhogg() -> None` safety arm disappear. Help filtering, the `[project]` help prefix and `man` topic visibility all read the spec. The gating half of this question is recorded in the hygiene platform document's project-applicability entry.

## STR-003 - Project and project config section are independent axes

Reported by: config-cli.

`DevConfig` holds `litehtml`, `sluggrs`, `ratatoskr`, `piners`, `dellingr` and `mogwai` as six `Option`s, parsed for every project. A handler therefore checks both "is the project litehtml" and "is `[litehtml]` present". It does the second inline with a repeated `.ok_or_else(|| DevError::Config("no [litehtml] section in brokkr.toml"))`: seven copies across `Visual`, `List`, `VisualStatus`, `Prepare`, `cmd_approve`, and `MockServe` with a different wording. Meanwhile a pbfhogg tree can carry a `[litehtml]` section that is parsed, validated and ignored. The per-project section types (`LitehtmlConfig`, `RatatoskrConfig`, `PinersConfig`, `DellingrConfig`, `MogwaiConfig`, `TilegenConfig`) are defined in the shared schema rather than in their project modules. Proposed: a `ProjectConfig` enum (`Litehtml(LitehtmlConfig)`, `Sluggrs(..)`, `Ratatoskr(..)`, ..., `Generic`) built by `load` from the `project` key, with each project module owning its section type and parser. A section for a different project is a load error, and a missing required section is reported once at load rather than once per command. The visual-command arms collapse to `match detection.project_config { Litehtml(cfg) => .., Sluggrs(cfg) => .. }`.

## STR-004 - The config module holds check's behaviour, and `profile.rs` sits outside `check_cmd`

Reported by: config-cli, check-plan.

`config_parts/schema.rs` defines config types and also `test_phase_allow_flags`, `reaches_test_build`, `is_cargo_lint`, `rustflags_target_key`, `shape_target_key`, `EffectiveUnification::cargo_args/describe/is_per_package`, `SweepProfile::target_subdir/cargo_args` and `ParallelBinaries::resolved_budget`. These are `check_cmd`'s decisions (what flags a build gets, which target dir a shape uses), so `config` depends on knowledge of cargo invocation shape, and `check_cmd` must reach into `config` for its own rules. `parser.rs` holds `captured_env_pairs`/`matches_capture` (a measurement-DB concern), `resolve_paths`/`host_features`/`all_dataset_keys` (path resolution, which `src/resolve.rs` exists for) and `hostname()` (an OS query). It also holds about 600 lines of cross-section validation of `[[check]]`, `[test]` and `[[quarantine]]` (profiles, complete-universe rules, the doctest ledger), which is `check`'s certification policy.

`src/profile.rs` is top-level but its only clients are `check_cmd` and `test_cmd`. It does four jobs: the `extends`-chain merge, the `ResolvedSweep` struct and its build identity, `FeatureConfig` argv rendering, and `resolve_unification`. The *validation* of those same profiles lives in `parser.rs`, which reimplements the merge.

Proposed: keep `schema.rs` as data plus validation, and move build-shape derivation into `check_cmd` (for example `check_cmd/shape.rs`). Give planning a home, `src/check_cmd/plan/`, with `profile.rs` (the merge only, used by both the parser's validation and the runtime), `sweep.rs`, `lane.rs` (lane kind and filters) and selection/features under it. Validation that needs merged profiles (threads, qualified skips, the doc carrier) runs on resolved sweeps at load time instead of on raw definitions, and the config module stops being the thing that must change whenever check's certification rules change.

## STR-005 - `include!`-spliced fragments stand in for modules

Reported by: config-cli, measurement, map-projects.

`main.rs` `include!`s `main_parts/bootstrap.rs` and `main_parts/commands.rs`, and `cli.rs` `include!`s three files (which is why `visibility.rs` must use `//` comments). `harness.rs` is four `include!`s: `harness_mod/format_sidecar.rs` holds result-line formatting, `run_variants`, `hotpath_feature`, `elapsed_to_ms/us`, the hotpath capture runner and its thread-local parking slot, percentile maths, kv-stderr parsing and sidecar DB backup rotation, while `types_run.rs` holds the harness and all of its loops, and `foreign_loop.rs` is two hooks. `resolve.rs` splices `resolve_parts/schema.rs`, which contains no schema: it is the dataset entry resolver plus the PMTiles archive store, the nidhogg data dir and `file_size_mb`. It also splices `resolve_parts/runtime.rs`, which is three DB path functions, two of them piners-only, with `results_db_path` stranded at the bottom of `schema.rs`. `pbfhogg/download.rs` splices `pbfhogg_mod/*`, which is a directory of fragments rather than a module, holding generic pieces (dates, the snapshot promotion `dispatch.rs` calls) beside the download CLI. None of these has a privacy boundary, `use` hygiene or a module doc; everything shares one namespace. The ratatoskr shims are recorded in the hygiene projects document's ratatoskr module-shape entry.

Proposed splits: real modules for `main_parts` and `cli`. Measurement: `measure/wall.rs`, `measure/loop.rs`, `measure/record.rs` (row building, `prev.*`, result line), `measure/capture.rs` (hotpath child runner), `measure/kv.rs` (stderr counter protocol), `sidecar/backup.rs`, and `bench_variants.rs` (`run_variants` is a dispatch helper, not harness). Resolve: `datasets::resolve` (entry resolution, `SnapshotRef`, `select_one`, typed errors), `elivagar::archive_store`, DB paths next to their DBs, `nidhogg_data_dir_path` into `nidhogg`. Download: a real `pbfhogg::download` with submodules `source` (Geofabrik/planet, aliases, URLs), `layout` (`raw_pbf_path`, `osc_filename`), `snapshot` (promotion, collision preflight, toml writes) and `modes`.

## STR-006 - `main_parts/commands.rs` still holds unrelated handlers

Reported by: config-cli, process-control.

`clean` moved to `src/clean.rs`, `lock`/`kill` to `src/lock_cmd.rs` and `env` into `src/env.rs`. Residue: passthrough run and timing stats, `pmtiles-stats`, `verify` routing and the generic hotpath still live in `main_parts/commands.rs`, and `clean.rs` still branches on every project rather than asking each project module for its clean targets.

## STR-007 - `ResolvedSweep` is two types glued together by a mutable field

Reported by: check-plan.

`profile.rs` builds `ResolvedSweep` with `effective_unification: EffectiveUnification::Ambient` as a placeholder (`build_resolved_sweep`, `sweep_from_check_entry`), and `phase.rs::resolve_sweep_unification` later overwrites it in place. Everything keyed on it (`build_shape_key`, `needs_isolated_target_dir`, `Resolutions::for_selection`, `shape_id`) is silently wrong if read before that write, and nothing in the type says which state a sweep is in. The struct derives `Default`, so tests (and potentially production) mint sweeps holding the placeholder. Many tests build `ResolvedSweep { .., ..Default::default() }` and then mutate `effective_unification` (the `selection.rs` test helper `package_mode`), constructing states production never reaches. Overall the sweep is a 25-field bag of primitives and argv fragments that every downstream module re-interprets; the hunter traces most of check-plan's duplicated decisions to that.

Proposed: `SweepDecl`, the pure result of profile resolution (what the user wrote, with provenance), and `PlannedSweep`, resolved per invocation (effective unification, isolated target dir, lane kind, the selection `Attempt`s). Only `PlannedSweep` has `build_shape_key`/`shape_id`. The resolution function (`resolve_unification` plus the promotion eligibility now in `phase.rs`) is the only constructor of `PlannedSweep`, and tests go through it.

## STR-008 - A lane's execution kind is five loose fields and a priority ladder

Reported by: check-plan.

`harness: Harness`, `doc_only: bool`, `parallel_budget: Option<u32>`, `process_isolation: bool` and `test_threads: Option<u32>` together encode one fact: how the lane executes. `prepare::lane_kind` recovers it with a priority ladder (nextest > doc_only > parallel > isolated > serial). Illegal combinations are refused in four different places, or not at all. `lane_kind` silently picks `Parallel` when both `parallel_budget` and `process_isolation` are set, so correctness depends on `phase.rs::reject_conflicting_lanes` having run first. The two docs already disagree: `profile.rs`'s doc on `parallel_budget` says the exclusion is "enforced at resolve time", while `reject_conflicting_lanes` says it is "refused here rather than at resolve time". `LaneKind` is defined in `accounting.rs`, derived in `prepare.rs`, and consumed by `phase.rs`, `continuation.rs` and the doctest predicate, so the type's dependency points accounting <- prepare and the derivation's points prepare <- accounting.

Proposed: `enum LaneExec { Serial { threads }, Parallel { budget }, Isolated, Nextest { threads }, DocOnly }`, built once at resolution, with the conflicts as construction errors and the value carried on the planned sweep. `LaneKind` becomes a projection of it.

## STR-009 - Plan wire records are still primitives

Reported by: check-plan.

`ShapeId`, `LaneIdx`, `Resolution`, `ContentHash`, `TargetKind`, typed refusals and typed anomaly details now exist, serialised transparently. Residue: `prepare.rs` still builds `PairId`, `PlannedArtifact`, `ShapeRecord` and `LaneRecord` from strings and `usize` (nine fields), `AccountingPlan.incomplete` is `Vec<String>`, `DeadFilterRecord` keeps three string fields, the universe maps are keyed `(String, Option<String>)`, and `artifact_drift` keeps a tuple-taking adapter around its typed core.

## STR-010 - A lane's inventory state is three overlapping fields, and continuation is stringly typed

Reported by: check-plan.

`LaneRecord` carries `prepared: bool`, `unavailable: Option<String>` and `skipped: Option<String>`, plus whether `unlisted` is non-empty. Their meanings collide. A skipped lane is recorded with `prepared = true` (the "not attempted" branch in prepare.rs), so `prepared` really means "its expectation is known". `continuation::lane_inventory` reconstructs a disposition from these fields with an ordered if-ladder (superseded > skipped > unavailable > doc-only > unlisted > complete) and emits it as a `&'static str`. `prepare_profile` reads `prepared || kind == DocOnly` for `plan.complete`; DocOnly lanes already get `prepared = lane_ok = true`, so either the special case is dead or the invariant is not what it appears. `phase.rs` reads `unavailable.is_none()` separately. `LaneInventoryOut.availability` and `InventoryOut.availability` are `&'static str` compared with `matches!(l.availability, "complete" | "partial" | ...)`. `is_fail_fast_casualty` compares `c.outcome == Outcome::Unobserved.as_str()` after the enum has already been flattened to a string in `CandidateOut`, and `SuspectOut.cause` is flattened the same way. A test asserts on `.contains("one stream")`.

Proposed: `enum Inventory { Prepared { executions, .., unlisted }, Skipped(reason), Unavailable(UnavailableReason), DocTests }`, with a typed `UnavailableReason` (`CargoThreaded`, `NoShim(reason)`, `PrepareFailed(msg)`, `Stopped`). Enums stay in the model and serialize with `rename_all`; the JSON DTOs stop flattening early.

## STR-012 - `prepare.rs` interleaves building, listing and policy in one loop

Reported by: check-plan.

`prepare_profile` is a loop of about 300 lines. It interleaves lane preparation, stop handling, universe enumeration (policy), filter liveness (policy, coverage's function called from prepare), pair minting and shape-record assembly. The module header says inventory and policy universe "are kept apart in code because they are different claims", but they share this one loop and its mutable `plan`/`ledger`/`universes`. Proposed: two passes over the prepared lanes, `inventory` (per lane, always) and `policy` (per shape, certifying only), with filter liveness and the ledger moving wholly into `coverage.rs`. Today coverage owns `FilterLedger` but prepare drives it.

## STR-013 - Lanes are identified by position in several parallel vectors

Reported by: check-plan.

"The sweep index is the lane identity everywhere." `PhaseSelection.entries`, `LaneRecord.lane` and `ReachLedger`'s `Vec<bool>`s are all aligned to the `sweeps` slice by position, and `nothing_reached`, `reached_labels` and `feature_drop_lines` zip with `sweeps` again. A misaligned zip silently truncates. Proposed: a `LaneIdx` newtype and an index-aligned `Lanes<T>` container that can be constructed only from the sweeps slice, so a misalignment becomes a type error.

## STR-014 - Selection decisions live outside the selection module

Reported by: check-plan, check-exec.

`selection.rs` claims "One decision, made once" for which runs exist. But `effective_unification`, the input that decides between `Resolutions::Combined` and `PerPackage`, is decided in `phase.rs::resolve_sweep_unification`, with its own promotion eligibility and metadata fetch, and written back onto the sweep. `parallel.rs` holds `unify_workspace`, `narrows_selection`, `reject_forwarded_selectors` and `target_selector`, which are selection and unification decisions that `profile.rs` and `selection.rs` call. They live in the parallel lane file only because the parallel lane was their first consumer; the file's own comment says the pin "now lives on the sweep". `parallel::repro_cargo_line` and `output.rs`'s `full_command` build the "failing command" line in two different ways. Proposed: promotion and `resolve_unification` move into selection, which already holds the feature oracle and the CLI scope. The argv classifiers move there too, as one typed forwarded-argv parser. One `ReproLine` builder works from the planned request. The parallel lane file keeps only budgeting, fan-out and per-binary reporting.

## STR-017 - The test runner reaches into process globals

Reported by: check-exec.

`crate::shutdown::is_shutdown_requested()` is polled from inside `wait_parallel`, `run_listing`, `spawn_process_group` and the end of `streaming_run_libtest`. `crate::output::warn` is called from inside the watchdog thread and `join_drains`. `concurrency::SERIAL_BUILDS` is a process static set once per run and read by `run_grouped`. In each case the state sits apart from the behaviour that acts on it. Proposed: a `RunContext { stop: StopSignal, warn: &dyn Fn }` passed in, which makes the runner testable without the process-global flag, and `SERIAL_BUILDS` as a parameter of `run_grouped`.

## STR-018 - Parallel budget allocation is buried in the run function, in mixed time units

Reported by: check-exec.

`run_parallel_sweep` mixes timing load, cost estimation, slot claims, ordering, announcement, fan-out, abort, timing record and reporting. `claim_slots`, `useful_slot_cap`, `timing_weight_for` and `mean_cost_per_test` are pure but composed inline, so they are tested piecewise, while the oscillation and collapse bugs described in `binary_timings.rs`'s header lived in their composition. `BinaryCost { serial: f64, slowest: f64 }` holds seconds as floats, `claim_slots` works in `u64` ms, `weight_ms` and `weight_ms_ceil` convert between them, and everything else uses `Duration`. The "is this weight finite and non-negative" guarding exists only because the store uses `f64`. Proposed: one pure `fn allocate(binaries: &[(Key, count)], history, budget) -> Vec<(Key, slots)>` next to `binary_timings.rs`, tested whole, and `Duration` in the store (serialized as ms), which removes `weight_ms`, `weight_ms_ceil` and the NaN reasoning.

## STR-020 - The harness-shim session

Reported by: check-exec.

`Session` holds about ten `Arc` fields, cloned field by field into `Shared<'a>` for each accept; one `Arc<SessionState>` would do. `finish()` joins drains with no grace bound and relies on each drain noticing `done` within a poll, while the unisolated path uses `join_drains` with `DRAIN_GRACE` and a warning: two drain-shutdown policies for one concern. `Refusal::Unplanned(String)` carries its cause as prose, and accounting receives `ObsEvent::AttributionError { detail: String }`. The cases (unplanned executable, rebuilt since the plan, rustdoc with no carrier, unknown role, handshake I/O) are a closed set and should be a typed enum on the observation.

## STR-021 - The check pipeline does not yet consume the shared Diagnostic

Reported by: check-report.

`src/diagnostic.rs` holds one `Diagnostic` (typed severity, site, `LintName` view, field-derived dedupe key, one renderer), produced by both the cargo JSON and the text readers. Residue: `check_cmd/phase.rs` still converts through `event_to_clippy`, dedupes on the string `DiagKey`, and recovers code and location with `extract_lint_code`/`parse_location`; it should carry `Diagnostic` in `SweepResult`, sort and dedupe from `code` and `site`, and render through the shared renderer, rustdoc included. The test-build warning path still reads text rather than `--message-format=json`, and `cargo_filter` still holds the libtest output parser.

## STR-022 - `check_cmd/phase.rs` is still most of the command

Reported by: check-report.

`activation.rs`, `conventions.rs` and `verdict.rs` came out of it, and header/textlint/manifest findings share one renderer. Residue in `phase.rs`: the diagnostic pipeline and rendering (`run_per_build_shape`, allow announcement and staleness, manifest-lint attribution, merge and format, failed-run stream excerpting), `cmd_clippy`, the test phase driver (`run_test_phase`, `run_test_lane`, `record_phase_stop`) and build/audit orchestration. `finish_check` still takes its verdict inputs as loose arguments, and `Verdict`, `Phase` and `Reporter` are not yet types.

## STR-023 - `check_cmd/output.rs` is not output

Reported by: check-report.

It holds test env construction (`build_test_env`, `sweep_cargo_env`, `sweep_runtime_env`, rustflags composition), isolated target-dir naming, support-artifact pre-builds and fingerprinting, the serial shim fallback, and `run_one_test_sweep`, the serial lane (about 400 lines). That is 3079 lines, about 1800 of them tests, and nothing in it is about printing. Its name also collides with `crate::output`, the real console module, and both are imported side by side. Proposed: `check_cmd/sweep_env.rs` (env and target dir, the owner of the isolated-target-dir decision), with the serial lane joining the execution files. The name `output` goes away entirely.

## STR-033 - Query requests still use bool flags and `Vec`s at the CLI boundary

Reported by: measurement.

Results and sidecar dispatch use exhaustive view enums and compare operands are pairs after validation, but `SidecarQuery` still carries bool view selectors, compare arguments still arrive as `Vec<String>`, and `BenchArgs.compare` is still a `Vec`. Converting them at parse time needs `main_parts/bootstrap.rs`, `request.rs` and `bench_cmd/mod.rs`.

## STR-024 - "Who is the audience for this line?" is a global plus ad-hoc bools

Reported by: check-report.

`run_per_build_shape` both schedules cargo runs and decides what to print for skipped, deduped and dropped lanes (`note`), switching between stdout and the run log on `report_active()`, a process global set by `check`. `announce_allows` takes two bools (`to_stdout`, `blanket_on_stdout`) and also consults `report_active()`. `run_clippy_phase` passes `commands || !report_active()` and `!echo_after`. Proposed: a `Reporter` value (`Check { commands } | Clippy`) passed explicitly, with methods `config_note`, `allow_notice` and `green_line`. The difference between `check` and `brokkr clippy` then becomes one type instead of a global and four flags.

## STR-027 - Lock acquisition fails only as prose

Reported by: process-control.

`lockfile::acquire` fails as `DevError::Lock(String)` for at least seven distinct, actionable reasons: the nested-descendant refusal, the inside-compilation refusal, drain timeout, flock infrastructure failure, metadata publish failure, no entropy and socket bind failure. The disk gate uses `Preflight(Vec<String>)`. A machine caller (`check --json`, an agent) cannot tell "your ancestor holds it, run it elsewhere" from "the kernel is out of lock records". The drain timeout says in its own text that it "is not a transient busy signal", exactly the fact a retry loop needs as data. Carrying a typed reason needs a `DevError` change in `src/error.rs` (`stray::kill`'s counts are already named fields). `cargo_env::FAILED: Mutex<Option<(String, Option<&'static str>)>>` stores the phase as a string from `check_cmd::current_phase()`, so it cannot be matched on. Proposed: an `AcquireRefusal` enum carried in `DevError::Lock`, with prose from `Display`.

## STR-029 - `HoldEffects` is a test seam that has become the protocol's spine

Reported by: process-control.

`HoldEffects` is a struct of six function pointers and two bools, explained as "a seam, so unit tests can take real flocks". In practice it is also the only place that lists everything a fresh hold does: drain, toolchain, capability, reaps, guard warning, disk gate, service. That list is the protocol, and its order matters (the disk gate before the drain; the toolchain after the drain and before the capability). The order lives in the bodies of `acquire_at` and `drain_and_authorize`, under long comments. `INERT` in the tests switches every effect off together, so no test exercises any ordering between them, although ordering is the thing with a history of bugs (the capability published before fallible toolchain work, the `OnceLock` nonce). `hold::stamp_is_a_no_op_without_a_capability` asserts almost nothing, because the capability is a process global. The guard's `decide()` has no test because it reads env and `$HOME` directly, and `decide` is the security-relevant half. `host_after_fresh_hold` calls `crate::check_cmd::activate_run_log()` from `host_drain`, so the lock reaches up into `check_cmd`.

Proposed: a typed hold state machine, `Flocked -> Gated -> Drained -> Authorized -> Live`, where each transition consumes the previous state. "Nothing fallible between publishing the capability and constructing `LockInner`" then becomes a property of the `Authorized -> Live` transition, and the order holds by construction. Effects stay injectable per transition, so a test can enable one effect (just the disk gate, say) instead of all or nothing. The command opens its run log before it acquires. The guard's `decide` takes injected `ProtocolPaths` and an env lookup.

## STR-030 - `cargo_env` is a build-input feature filed as process control

Reported by: process-control.

`cargo_env` evaluates a `[cargo_env]` config section on every cargo spawn. Its process-control content is a bounded `sh -c` evaluator (deadline, drains, group kill) that duplicates `output::run_captured_with_env_and_deadline`, which `guard.rs` already uses for its probe. It reaches into `check_cmd::current_phase()` to label its failure, another upward edge. `cargo_env::apply` decides "is this program cargo?" by `file_name() == "cargo"`, which breaks for rustup's `+toolchain` proxy variants or a wrapper. Proposed: evaluation goes through the shared bounded runner, the phase is passed in typed, and the module becomes the pre-spawn step of a caller-chosen `Cargo` spawn profile, so no name sniffing is needed. The spawn-policy half is recorded in the hygiene platform document's spawning entry.

## STR-031 - How a measured child ended is three bools, an i32 and sentinels

Reported by: measurement.

`SidecarRunResult { stopped_by_marker, stopped_by_signal, stopped_by_deadline: bool, exit_status }`, `HotpathCapture { stopped_by_marker, stopped_by_deadline, status_ok, status }`, `DeadlineCapture { killed_on_deadline, output_cut }`, `PassthroughOutput { code: i32 }`, `exit_code_from_status` (128+signal, with a -1 fallback), `HistoryRow.exit_status: i32` and `RunInfo.exit_code: Option<i32>`. `run_hotpath` records `pid 0, exit 0` as sentinels for "unknown", which `brokkr sidecar` then prints as if real. The bools are mutually exclusive by construction, but nothing says so, and each loop re-derives the classification in its own order (`interrupted` first, then `!stopped && check_success`). No `run_external_*` loop ever reads `stopped_by_deadline`, so a deadline kill there surfaces as a generic "killed by signal 9" subprocess error rather than "exceeded its deadline". "Did this child run succeed?" is answered at `CapturedOutput::check_success_or`, `HotpathCapture::status_ok` + `status_error` (which rebuilds a `CapturedOutput` with a zero duration just to call `check_success_or`), the `!stopped && check_success*` blocks in each loop, `run_hotpath_capture` (`!c.stopped_by_marker && !c.status_ok`), `run_passthrough_in` (signal -> `Subprocess`) and `cargo_bench` (`out.code == 0`). `DevError::Subprocess { code, stderr }` sometimes holds "killed by signal 9" in `stderr`, and `Display` sniffs `starts_with("killed by signal")`.

Proposed: `enum Ending { Exited(i32), Signalled(i32), StopMarker(String /* resolved */), Deadline(Duration), Interrupted }`, produced once by the sidecar and captured runners, plus one `fn verdict(&Ending, ok_codes) -> IterationVerdict { Counted, Failed(DevError), Interrupted }`. `exit_code_from_status`, `check_success_or` and the per-loop branching collapse into that function. `RunInfo` takes `Option<Ending>`, and `Subprocess` carries `ended: Code(i32) | Signal(i32)`.

## STR-032 - Hotpath metrics are stored as display strings and parsed back

Reported by: measurement.

Still current: `HotpathFunction`/`HotpathThread` carry six optional metric strings each, stored as `TEXT`, and `hotpath_fmt::parse_metric` re-derives units to diff them; only p50/p95/p99 survive. A worked design exists: parse at ingest into a `Quantity { DurationNs(i64) | Bytes(i64) | Percent(f64) }` (signed, since `mem_diff` can be negative) and a `Percentile` key that orders fractional values such as 99.9; a schema migration adds child tables `hotpath_function_metrics`, `hotpath_function_percentiles` and `hotpath_thread_metrics` with a `quantity_kind` check, keyed by the parent row with `ON DELETE CASCADE`; new writes go to the typed tables and reads fall back to parsing the old text columns, so no backfill is needed; diffs require matching quantity kinds. The fixer needs `src/db/**` (types, schema, write, query, migrate) and `src/hotpath_fmt.rs` together, and `docs/commands/measure.md`'s hotpath contract.

## STR-034 - The measurement session is two contexts and a config with dead fallbacks

Reported by: measurement, map-projects.

`DirtyPolicy` and `RowSubject` now exist (`src/measure/runner.rs`, `context.rs`). Residue: `BenchContext` and `HarnessContext` are still two contexts with duplicated lock, path and `with_request` setup; `BenchConfig` still mixes row identity, loop control, build provenance and the dead `mode`/`brokkr_args` fallbacks; the db tests still build full `RunRow` literals by hand rather than through a fixture builder.

## STR-035 - The shared core owns pbfhogg's types, and pbfhogg's command inputs are checked at argv time

Reported by: measurement, map-projects.

`CommandContext` (pbf_path, osc_path, osc_paths, pbf_b_path, bbox) and `CommandParams` (direct_io, io_uring, compression, snapshot, osc_range, inject_prepass, force_altw, index_type, merged-cache observations, to_snapshot_file, as_snapshot, ...) live in `crate::measure`, the shared core. Their fields are pbfhogg's vocabulary, so the edge points the wrong way.

`build_body` unwraps the context with `ctx.osc_str()?`, `ctx.pbf_b_str()?` and "extract requires a bbox", so the invariant "this command's inputs are present" is checked at argv-build time by `Result`, not by construction. `InputKind` already lists the shapes (`Pbf`, `PbfAndOsc`, `PbfAndBbox`, `PbfAndMerged`, `OscOnly`) but is not the type of anything. It is also wrong for `DiffSnapshots`, which it labels `Pbf` though the command needs two PBFs; dispatch short-circuits it. `needs_osc()` returns true for `PbfAndMerged` because the merge needs one, which mixes "what the command consumes" with "what setup needs".

`CommandParams` doubles as a side channel. `merged_cache_state: Option<String>` ("hit"/"miss", though `MergedCacheState` exists as an enum), `merged_cache_age_s: Option<String>` and `to_snapshot_file_mb: Option<String>` (a float formatted early, so the results DB gets a rounded text value where `input_mb` is a float) are written by dispatch into a cloned `params` so that `PbfhoggCommand::metadata` can read them back.

`PbfhoggCommand` field shapes admit states their docs forbid. `Inspect { nodes: bool, tags: bool, .. }` has two flags documented as mutually exclusive, and `extended` is a documented no-op in nodes and tags mode. `TagsFilter { input_kind_osc: bool, omit_referenced, invert_match, remove_tags }`: osc input "rejects -R, -i, -t", which `build_body` enforces by silently dropping them and clap enforces with `conflicts_with`, two decisions. `Diff` and `DiffSnapshots` have identical `build_body` arms apart from where the two inputs come from. `resolve_io_flags` returns `(Vec<&str>, Vec<&str>)` = `(features, args)`.

`commands.rs`'s header says every measurable command "is defined here exactly once". In practice a command's facts are spread across `commands.rs` (argv), `cli/schema.rs` (allowed values, conflicts), `cli_adapter.rs` (a 400-line identity match from CLI to enum), `main.rs` (strategy fan-out), `bench_commands.rs` (suite presets by string name, `preset_to_command`), `bench_extract.rs` (a second extract argv) and `dispatch.rs` (input special cases for `DiffSnapshots`, output special cases for `MultiExtract`, promotion routing for `Degrade`). The `commands.rs` tests assert argv token by token for each flag combination, mirroring the implementation, while the decisions that actually drifted (the two extract argv builders, bench against verify multi-extract, output kind against the actual dir) have no test, because each copy is tested on its own.

Proposed: both types move into `src/pbfhogg/`, and `measure.rs` keeps only `MeasureMode`/`MeasureRequest`. Resolved inputs become an enum mirroring `InputKind` (`ResolvedInputs::PbfAndOsc { pbf, osc }`, `OscRange { oscs }`, `TwoPbfs { a, b, origin: Merged | Snapshot }`, ...), so `build_body` pattern-matches and cannot ask for a missing input. Resolution returns runtime observations as typed values (`MergedCacheState`, `SnapshotSide { file, mb }`), and `metadata()` takes those rather than `&CommandContext`. The command fields become `InspectMode::{Default { extended }, Nodes, Tags { type_filter }}`, `TagsFilterInput::{Pbf { omit_referenced, invert, remove_tags }, Osc}` and `IoMode { direct_io, io_uring }` with `features()`/`args()`. Clap derive goes on the command enum itself (the variants are mostly the CLI already), so `cli_adapter.rs` disappears and allowed values live on the types. A per-variant descriptor holds promotion variant, `OutputKind` and io_uring support next to argv. The valuable test is a property over a catalogue of every variant: `build_args` is pure, every path it names is in the plan's artefact list, cleanup removes exactly those paths, and `supports_io_uring` agrees with the error text.

## STR-036 - Two child-process supervisors, and a console module that is also a supervisor

Reported by: measurement, process-control.

Lines 1-500 of `output.rs` are a console renderer: prefixed output, the status line, the run log, the quiet flag, the pbfhogg-specific verify buffer, and per-project `*_msg` functions (litehtml, sluggrs, ratatoskr, corpus, lint, deps, wc). Lines 500 onwards are a process supervisor: `CapturedOutput`, deadline- and idle-limited runners, SIGTERM forwarding, pipe drains with settle, `spawn_captured`, passthrough. `output::run_captured_limited` and `sidecar::run_sidecar_with_deadline` each implement `try_wait` polling, a shutdown-flag check, a deadline kill, a process-group SIGKILL and stdout/stderr drains, and they have already diverged. The output runner bounds its drain join (`settle_drains`, `DRAIN_SETTLE`, with a comment about the hang the unbounded join caused); the sidecar joins unboundedly (filed in `bugs.md`). The output runner forwards SIGTERM with a budget; the sidecar SIGKILLs immediately. The output runner has an idle limit; the sidecar has a thread-local `DeadlineScope`. `cargo_env`'s bounded `sh -c` evaluator is a third copy. Proposed: split `output.rs` into `console` and `child`, and give the child half one supervisor with a pluggable per-tick observer (the sidecar's `/proc` sampler, FIFO drain and stop matcher), so fixes reach every caller.

## STR-037 - Remaining dependency edges pointing up

Reported by: measurement, process-control, deps-misc.

`RunLog` moved to `src/run_log.rs`, the cancellable pipe reader to `src/fs_probe.rs`, and the gate store under ratatoskr (`ratatoskr/gate_store.rs`). Residue: `cargo_env.rs` still calls `check_cmd::current_phase()` (implemented in `check_cmd/watchdog.rs`); `sidecar.rs` still uses the test runner's pipe reader; the `"pbfhogg"` default project is still hidden by name in `db/format/details.rs` and `db/format/single.rs`; `disk_gate`/`env` storage probing should sit below both.

## STR-038 - pbfhogg and elivagar still have their own dispatchers

Reported by: map-projects.

A shared command-plan runner exists in `src/measure/runner.rs`, and nidhogg ingest uses it in all four modes (dry-run now supported, fresh scratch per sample, scratch kept on failure). Residue: pbfhogg's dispatcher, elivagar's dispatcher, elivagar's external tools and nidhogg's api/tiles server lifecycle still run their own skeletons, each deciding dry-run purity, run-mode recording and cleanup-on-failure itself; elivagar's argv construction still creates directories during dry-run. Migrating them onto the runner (external tools and server lifecycles as plan kinds) is the remaining work.

## STR-039 - Digest typing residue

Reported by: map-projects, config-cli, deps-misc.

`Xxh128`/`Sha256` and `FileDigest`/`TreeDigest`/`PathDigest` exist in `preflight/digest.rs`, config fields deserialize into them (a sha256 in an xxh128 field fails at load), and `preflight` is split into `digest` and `host_checks`. Residue: `env.rs` dataset status still compares and holds digest strings; config tests still construct digests from strings.

## STR-040 - The PMTiles archive store has no owner

Reported by: map-projects.

`resolve.rs` owns `pmtiles_archive_name`, `pmtiles_archive_prefix`, `pmtiles_archive_matches`, `legacy_archive_token` and `resolve_pmtiles_by_commit`. The rename into the store lives in `elivagar/dispatch.rs`, retention in two places, and `clean --archives` in `main_parts`. `pmtiles_archive_name`'s comment says it is "the single place this name is constructed". That holds for the name, but retention, enumeration (prefix match plus a hyphen-free remainder plus legacy hex tokens) and the "wiped dir" guard are spread across three modules. `resolve.rs`, the generic dataset-entry resolver, is the wrong home. `pmtiles_archive_name(output_dir, dataset, variant, commit)` takes four same-typed strings. `resolve_pmtiles_by_commit` falls back to the typed string as the hash token for an unresolvable revision (`Err(_) => c.to_owned()`), so a typo gives "no build for <typo>" and tells the operator to run tilegen for a commit that does not exist (deliberate according to its comment). With `--file` it skips hash verification and dataset scoping entirely, without recording that the input was unpinned. Proposed: `elivagar::archive_store::ArchiveStore { dir }` with `path_for(ArchiveKey { dataset, variant, commit })`, `resolve(dataset, variant, rev)`, `admit(scratch_file, key)`, `prune(group, keep)` and `groups()`.

## STR-042 - Commit identity is a string everywhere

Reported by: deps-misc.

`git::CommitId { full: String, short: String }` has public string fields, and `short_of` returns a non-hash input unchanged, so anything can be a "short hash". `GitInfo.commit` is a short hash in a field named `commit`. Results rows store a `String`. Sites abbreviate independently of `SHORT_HASH_LEN`: `env.rs::read_git_rev` (filed in `bugs.md`), `invalidate_cmd.rs::preview` (`chars().take(8)`), `ratatoskr_sync/bench_gate.rs` (`take(8)`), and `db/gate.rs` slicing `&uuid[..8]` (a uuid, but the same display-width decision). Proposed: a `Commit` newtype (full hash, validated hex length) with `short()` as the one abbreviation, and `ShortCommit` as a distinct type for stored keys.

## STR-043 - `env.rs` is five modules

Reported by: deps-misc.

It is a host-facts collector (kernel, governor, CPU, memory, io_uring, memlock), a storage prober (mount-table parsing, statvfs, block-device model and rotational flag, used by `disk_gate`), a tool-version prober, a dataset verifier, a user-config reporter, and the printer for all of them. `history_cmd` calls `env::read_memory`. `disk_gate` calls `env::probe_path` and `env::format_bytes`, while `env` calls back into `disk_gate`. `bench_cmd::stamp` collects its own CPU facts. Proposed: `host/` facts (`memory`, `cpu`, `storage`) as libraries with typed results (`StorageInfo` already is one), consumed by `disk_gate`, `history_cmd`, `preflight`, the sidecar and `bench_cmd::stamp`. Dataset status moves next to dataset resolution, since `env` is only one renderer of it. `env.rs` becomes the report (collect and print), and `format_bytes` moves to `output`.
