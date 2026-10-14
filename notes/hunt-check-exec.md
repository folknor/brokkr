# Design hunt: check-exec (what actually runs tests)

Scope read: `src/test_runner.rs`, `src/test_runner/harness_shim.rs`,
`src/check_cmd/{parallel,nextest_lane,nextest,direct_runtime,watchdog,concurrency,binary_timings}.rs`,
`src/test_cmd.rs`, `src/test_cmd/focused.rs`, `src/cpu_topology.rs`. Followed
out into `src/check_cmd/output.rs` (the serial/cargo-threaded sweep runner),
which is the main caller and where several of the duplicates live.

The short version: the runner already produces a typed fact stream
(`ObsEvent` via `StreamTap`), but it does not act on that stream. Every caller
works from text the runner rebuilds out of the events, plus exit status, plus
argv strings, and each caller reaches its own verdict. The watchdog verdict is
computed in three places, the "a budget was blown, so stop" rule in four, the
"is a runner/target configured" question in two, and the libtest argv contract
in four or more. Most of the findings below come back to that.

---

## 1. Axes that should be types

### 1.1 Sentinel strings standing in for "no test to name"
`NO_COMPLETION`, `IDLE_WEDGE` and `WALL_WEDGE` are `&str` constants passed
around where a test name goes. `TestTracker::timed_out` returns
`Option<(String, Duration)>`, and the caller has to string-compare the name to
work out which clock fired:
- `kill_of`: `TimeoutReason::PerTest { name } if name == NO_COMPLETION`
- `watchdog_loop_with_timing`: `if name == IDLE_WEDGE`
- `harness_shim::Session::watchdog`: `&& name != super::IDLE_WEDGE`
- `TimeoutReason::label()` turns the enum back into the same sentinels.

`TimeoutReason` was introduced to keep "which clock fired" out of the name
field (its doc says so), but `PerTest { name: NO_COMPLETION }` puts it straight
back in. A test that is really named `(no test completed within the budget)`
would be indistinguishable from the sentinel. That is absurd but legal, and it
shows the type is not doing the work. What the code needs instead:
```
enum Expiry { Watched { test, elapsed }, NoCompletion { elapsed }, Idle { elapsed } }
enum TimeoutReason { Watched{name}, NoCompletion, Idle, Wall{blamed}, OneTestWall{name} }
```
`timed_out` should return `Expiry`. `KillCause` then follows from
`TimeoutReason` with no string compare. The `HungTest.test: String` field
(always `reason.label()`) disappears.

### 1.2 Attribution confidence is recomputed, not carried
Whether a kill's name is exact, a suspect, or absent is decided in three
places:
- in the runner, from `WallShape` (which picks `TimeoutReason` and the name);
- in `test_cmd::hung_test_tag`: `one_test && matches!(reason, PerTest)`;
- in `format_hung_test`, which branches on the reason to choose its wording;
  the "stale, best guess" warning is printed from inside the watchdog loop
  (`warned`).

`HungTest` should carry `attribution: Exact | Suspect { stale: bool } | None`,
set once at kill time. The consumers would then only render it.

### 1.3 libtest argv as the carrier of the runner's contract
The runner has two preconditions: it requires `--format json -Z unstable-options`
on every lane, and `--test-threads=1` on the serial runner. It enforces only the
second, by scanning argv (`enforce_single_threaded`). Every caller builds the
argv by hand:
- `check_cmd/output.rs` (serial and cargo-threaded): pushes json, then calls
  `reject_format_override`, which checks both `--format` and `--format=`.
- `parallel::direct_libtest_args`: pushes json and refuses only an exact
  `--format`. It is safe today only because the user args go before brokkr's
  json and libtest takes the last occurrence. This is a different rule
  reaching the same answer by accident of ordering.
- `test_cmd::direct_libtest_args`: a hard-coded array.
- `isolate.rs`, plus the discovery args in `focused.rs`.
- `effective_test_threads` is also parsed again in `parallel.rs` and twice in
  `output.rs`. (A different `effective_test_threads` with the same name lives
  in `config_parts/parser.rs` and works on profiles. That is a second name
  collision to watch.)

What the code needs: a `LibtestInvocation { filters, exact, include_ignored,
nocapture, threads: Threads::One | Threads::N(u32) | Threads::Default,
passthrough: Vec<String> }` that the runner takes and turns into argv itself,
always appending the format flags last. Passthrough tokens would be refused at
construction if they contain `--format` or `--test-threads`. With that, the
serial/parallel runner choice can be typed (`Threads::One` gets the watchdog)
instead of re-derived from argv in `output.rs`.

### 1.4 Cargo-argv classification by string head
`parallel.rs` holds four hand-rolled classifiers over `cargo_extra`:
`partition_target_selectors`, `narrows_selection`, `reject_forwarded_selectors`
and `reject_unsupported_forwarded`. `output.rs` has `has_target_selector` and
the `--config`/`--target` scan in `serial_shim_fallback`. The package-selector
set is already written twice, and the two copies differ:
`narrows_selection` uses `-p | --package | --exclude`, while
`reject_forwarded_selectors` adds `--workspace`. Both also have to remember the
`-pfoo` attached form. What the code needs: one `ForwardedCargoArgs::parse` that
yields typed groups (`package_selectors`, `target_selectors`, `config`,
`target`, `manifest_path`, `target_dir`, `no_run`, `rest`), with every lane and
the shim check reading fields. This also belongs to planning/selection, not to
`parallel.rs` (see 3.4).

### 1.5 Phase identity is `&'static str`
`watchdog::phase_ceiling(phase: &str)` matches on string literals and has a
`_ => 2` catch-all. If a phase is misspelled, or a new one is added (the
`prepare` phase was), it silently gets 2 minutes. `PHASE_CLOCK`,
`current_phase`, `begin_phase` and `failing_phase` all hold `&'static str`. A
`Phase` enum with a `ceiling()` method would make the table exhaustive, and the
`--json` `failed_phase` string would come from `Phase::name()`.

### 1.6 Binary identity as `format!("{}/{}", package, target)`
`parallel.rs` builds the label twice (`BinaryRun.label`,
`run_parallel_sweep::label_of`) and uses it as the `binary_timings` store key.
It drops the target kind, so `[[bin]] foo` and `tests/foo.rs` in one package
both key as `pkg/foo`. Each then reads and overwrites the other's cost (a
lateral bug, low severity: advisory allocation only). `TestBinary::label()`
exists in `direct_runtime` use, so there are already two label spellings. The
store key should be a typed `BinaryKey { package, kind, target }` (or the
planning `unit`), with the label derived from it.

### 1.7 Mixed time units
`BinaryCost { serial: f64, slowest: f64 }` holds seconds as floats.
`claim_slots` works in `u64` ms, `weight_ms` and `weight_ms_ceil` convert, and
everything else uses `Duration`. The "is this weight finite and non-negative"
guarding exists only because the store uses `f64`. A `Duration` (serde as ms)
in the store would remove `weight_ms`, `weight_ms_ceil` and the NaN reasoning.

### 1.8 `ParallelWait::Exited { timed_out: bool, cancelled: bool }`
These two bools are mutually exclusive by construction
(`cancelled && !overtime`), so they belong in one enum
(`Exit::Natural | Wall | Cancelled`), which `KillCause` already almost is.
`ParallelRun.timed_out` and `ListingRun.timed_out` are the same bool again.

### 1.9 Failures that reach callers only as prose
- `DevError::Reported("... exceeded its time budget ... - stopping")` is built
  in three places (`output.rs` twice, `parallel::report_runs`) plus
  `test_cmd::stop_for_budget`. A caller (the `--json` trailer, the exit-code
  mapper) cannot tell "budget blown, stop" from any other already-reported
  error without reading the message. This needs a typed
  `DevError::BudgetBlown { sweep, kind }`, or a `LaneVerdict` (see 2.2).
- `Refusal::Unplanned(String)` in the shim carries its cause as prose, while
  accounting receives `ObsEvent::AttributionError { detail: String }`. The
  cases (unplanned executable, rebuilt since plan, rustdoc with no carrier,
  unknown role, handshake I/O) are a closed set and should be a typed enum on
  the observation.
- `run_libtest_parallel`'s `forward_stdout_line` / `on_build_finished`
  callbacks are passed `|_| {}` by both parallel callers. That says the
  signature is shaped for `streaming_run_libtest`, not for this function.

---

## 2. Decisions made in more than one place

### 2.1 "Has a ceiling expired, and what is the kill called?" - three implementations
- `test_runner::watchdog_loop_with_timing` (serial, unisolated, and Direct).
- `harness_shim::Session::watchdog` (serial with shim). This is a separate
  re-implementation, and it already disagrees with the first:
  - it ignores `ceilings.shape`. A wall expiry is always `SweepWall`, never
    `OneTest -> PerTest { caller's name }`. That is unreachable today because
    test_cmd's one-test runs are Direct, but nothing prevents the combination;
  - `SweepWall { blamed: Vec::new() }` always, where the other loop reports
    the in-flight set;
  - its idle rule is run-wide (`!executing` across harnesses) instead of
    per-tracker, which is deliberate, but the expiry ordering (per-test, then
    idle, then wall) is copied by hand;
  - no "no event ever observed" warning;
  - on the shared-stream tracker it accepts any `timed_out` name, including
    `IDLE_WEDGE` filed as `PerTest`, which the other branch filters out.
    Unreachable while NO_COMPLETION (20s) always beats IDLE (300s), but the
    two branches of one function already filter differently.
- `wait_parallel` (the parallel runner's own wall clock in a `try_wait` loop),
  running beside a `watchdog_loop` that was deliberately given `wall: None`.
  The parallel lane therefore has two kill paths with two outcome shapes
  (`HungTest` vs `timed_out: bool`), and every caller has to handle both.

The freeze-snapshot-kill sequence (`stop_process_group`, emit `Killed`,
`capture_hung_test`, `kill_process_group`) is also written twice.

Single owner: a `Deadlines` value (per-test, idle, wall, shape) with one pure
`fn verdict(&self, trackers: &[&TestTracker], run_started) -> Option<Verdict>`
covering both the one-tracker and many-tracker cases (the shim case is "many
trackers plus a run-level idle clock"), and one `enforce(verdict)` that does
the SIGSTOP/snapshot/SIGKILL. The parallel runner's wall then becomes
`Ceilings.wall`, and `wait_parallel` keeps only abort/shutdown polling.

### 2.2 "Did this run pass, fail, or blow its budget, and does it stop brokkr?" - four lane-local answers
- `output.rs` serial/cargo-threaded: `timed_out` -> `Err(Reported)`; `hung` ->
  `Err(Reported)`; `!status.success()` -> `Ok(false)`; then completeness from
  `cargo_filter::parse_test_output` over the reconstructed text.
- `parallel::report_runs`: same intent, rebuilt per binary, with its own
  `Incomplete` check by re-parsing reconstructed text and `passed` counted from
  the tracker's `completed` instead of libtest's summary.
- `test_cmd::run_one`: a third parse (`parse_test_output_with_stderr`), a
  `starts_with("test result:")` sniff, a discovery-count comparison, and its
  own `stop_for_budget`.
- `nextest_lane::run_nextest_engine`: a timed-out test is `TestResult::TimedOut`
  in the journal, yet the lane returns `Ok(false)`, an ordinary red sweep.
  **This already disagrees with the contract the docs and every libtest lane
  enforce** ("exceeding it fails the run and STOPS it"): after a nextest-lane
  timeout, `run_test_phase` carries on to the next sweep, while a libtest-lane
  timeout ends the run. The engine's fail-fast stops only the lane.

Meanwhile `accounting.rs` (sibling scope) reconciles the same runs from the
typed `ObsEvent` stream. So there are two independent evidence bases for one
verdict: reconstructed human text plus exit status for the lane's pass/fail,
and typed observations for the audit. They can disagree.

Single owner: an `ExecutionOutcome` reduced from the observation stream
(suite open/closed, terminals per test, kills, stream ends, exit), produced
by the runner (or by a pure reducer shared with accounting) and returned in
`LibtestRun`. Then:
- one `fn lane_verdict(&ExecutionOutcome) -> Pass | Fail | Incomplete | BudgetBlown | Stopped`,
- the nextest lane maps `TestResult::TimedOut` into the same `BudgetBlown`,
- the reconstructed text becomes display-only, which is how the JSON
  reconstructor already describes itself. "Completeness" stops being
  inferred by `cargo_filter` from text brokkr itself synthesized.

The current round trip is: libtest emits JSON, brokkr renders it to libtest's
human format, then `cargo_filter` parses that human format back. This is the
structural core of the problem. It survives because the parsers predate the
JSON lane.

### 2.3 "Is a target runner / foreign build target / rustdoc override configured?" - two answers
- `output.rs::serial_shim_fallback` (whether the shim may override the
  runner), and
- `direct_runtime::refuse_configured_runner` (whether direct execution would
  bypass one).

They already differ:
- config chain: `config_paths(root)` vs `config_paths_under(root, env)`. The
  serial check ignores a sweep-env `CARGO_HOME`, so a runner configured there
  is invisible to it, and the shim would then silently override that runner.
  This is the exact bypass the check exists to prevent;
- empty values: `var_os(..).is_some()` (empty counts as set) vs
  `.filter(|v| !v.is_empty())`;
- cfg evaluation is copy-pasted (`strip_prefix("cfg(")`, `eval_cfg != Some(false)`).

Also, `CARGO_TARGET_<HOST>_RUNNER` is spelled by `format!` in three places
(`streaming_run_libtest`, `serial_shim_fallback`, `refuse_configured_runner`).

Single owner: a `CargoLaunchConfig::discover(root, env)` in `rustflags.rs`
(which already owns the config chain and the cfg evaluator), returning
`{ runner: Option<Source>, build_target: Option<Source>, rustdoc: Option<Source>, build_dir: bool, env: Vec<ConfigEnvEntry> }`.
`serial_shim_fallback`, `refuse_configured_runner`, `concurrency::build_dir_configured`
(a fourth hand-rolled config-chain TOML read) and `direct_runtime::cargo_config_env`
(a fifth) would all read from it.

### 2.4 "env value as cargo will see it" - several ad-hoc lookups
`sweep env over process env` is computed separately by `envelope`'s `has`,
`cargo_program` (PATH), `refuse_configured_runner::effective`,
`serial_shim_fallback::env_has` (which looks at whether the key is present,
not at its value), and `concurrency::set_serial_builds`. They disagree on
whether an empty value counts. This belongs on one `EffectiveEnv` type.

### 2.5 Ceiling constants
`SWEEP_WALL_TIMEOUT` and `PARALLEL_SWEEP_TIMEOUT` are both 1800s, with two
docs describing the same role. Both are dead for `check` and `test`, whose 15
min phase ceiling fires first (`SWEEP_WALL_TIMEOUT`'s own doc admits this).
Yet `output.rs` and `parallel.rs` print "exceeded the parallel test timeout
(1800s)" as a user-facing verdict that cannot occur under those commands. The
ceiling hierarchy (per-test 20s, idle 300s, wall 1800s, phase 5/15/2 min, run
25 min, nextest synthesized slow-timeout, listing deadline = TEST_TIMEOUT in
`binaries.rs` and `focused.rs`, prebuild = IDLE_TIMEOUT in `test_cmd`) is
spread over `test_runner.rs`, `watchdog.rs`, `nextest_lane.rs`,
`binaries.rs`, `focused.rs` and `test_cmd.rs`. Nothing states, or checks, that
the inner bounds sit below the outer ones. A single `ceilings.rs` table
would fix that, holding every bound with its role, plus a const assertion
for the ordering.

---

## 3. Structure

### 3.1 `test_runner.rs` (3100 lines) does six jobs
1. the observation vocabulary (`ObsEvent`, `StreamTap`, `StrictPlan`, `ShimMode`);
2. the libtest JSON protocol (`split_trailing_event`, `JsonReconstructor`);
3. the deadline model (`TestTracker`, `Ceilings`, `TimeoutReason`, constants);
4. process-group mechanics (spawn, stop, kill, `/proc` child scan, drains,
   `read_chunk`, `join_drains`);
5. three runners (`streaming_run_libtest`, `run_libtest_parallel`, `run_listing`);
6. reporting (`format_hung_test`, `is_bare_status_line` for test_cmd's
   condenser, `effective_test_threads`, `hash_file`).

Suggested split: `test_runner/{events.rs, libtest_json.rs, deadline.rs,
procgroup.rs, run.rs, hung.rs}`. `hash_file` belongs with the plan/accounting
(it is the plan's artifact identity, re-checked by the shim), not in the
runner. `StrictPlan` is a planning artifact the runner merely consumes.

### 3.2 Two runners that should be one
`streaming_run_libtest` and `run_libtest_parallel` share about 80% of their
body: spawn group, `GroupReaper`, the stdout JSON drain, the stderr drain,
tracker, watchdog thread, `join_drains`, the buffer clones. They diverge only
in accidental ways:
- parallel emits `ObsEvent::Exited`; serial never does, even for
  `Launch::Direct` (test_cmd's direct runs), so accounting sees no exit for
  them;
- parallel hard-codes `Leader::TestBinary`, but `output.rs` calls it with
  `"cargo"` for `test_threads >= 2` sweeps. **A hang snapshot on a
  cargo-threaded sweep therefore snapshots cargo itself, not the test
  harness.** `Leader` exists precisely to prevent this, and the
  serial path gets it right via `Launch`. This is a real bug;
- parallel has a separate wall loop (2.1) and an `abort` flag; serial has the
  shim and `enforce_single_threaded`;
- serial returns `in_flight`; parallel does not.

One runner: `run_libtest(Launch, LibtestInvocation, Ceilings, Observe, abort: Option<&AtomicBool>, callbacks)`,
with the shim permitted only for `Launch::Cargo` and the watchdog enabled
according to `Threads`. Cargo-threaded parallelism then stops being a separate
code path. It is just a run whose per-test clock ages many in-flight tests,
which `TestTracker` already supports.

### 3.3 The runner reaches out to global state
`crate::shutdown::is_shutdown_requested()` is polled from inside
`wait_parallel`, `run_listing`, `spawn_process_group` and the end of
`streaming_run_libtest`. `crate::output::warn` is called from inside the
watchdog thread and `join_drains`. `concurrency::SERIAL_BUILDS` is a process
static set once per run and read by `run_grouped`. In each case the state
sits away from the behaviour that acts on it. A `RunContext { stop: StopSignal, warn: &dyn Fn }`
passed in would make the runner testable without the process-global flag.
`SERIAL_BUILDS` should be a parameter of `run_grouped`.

### 3.4 `parallel.rs` holds planning policy
`unify_workspace`, `narrows_selection`, `reject_forwarded_selectors` and
`target_selector` are selection/unification decisions that `profile.rs` and
`selection.rs` call. They live in the parallel lane file only because the lane
was their first consumer (the file's own comment says the pin "now lives on
the sweep"). They should move to selection, alongside the
`ForwardedCargoArgs` type from 1.4. The lane file would then hold only
budgeting, fan-out and per-binary reporting. `repro_cargo_line` and
`output.rs`'s `full_command` build the "failing command" line two different
ways; one `ReproLine` builder from the planned request belongs to planning too.

### 3.5 Budget allocation is buried in the run function
`run_parallel_sweep` mixes timing load, cost estimation, slot claims, ordering,
announcement, the fan-out, the abort, timing record and reporting.
`claim_slots`, `useful_slot_cap`, `timing_weight_for` and `mean_cost_per_test`
are pure, but they are composed inline. Moving them into one pure
`fn allocate(binaries: &[(Key, count)], history, budget) -> Vec<(Key, slots)>`
next to `binary_timings.rs` would let the allocation be tested whole, which is
where the oscillation and collapse bugs described in its header actually
lived. Today they are tested piecewise.

### 3.6 `test_cmd.rs` (4000 lines)
It holds a whole second lane: planning (`plan_focused`, `prebuild_targets`,
`BuildShape`, `cargo_head`), its own verdict logic (2.2), display filtering
(`StdoutCondenser`, `keep_stdout_line`, `is_test_phase_noise`,
`stderr_indicates_compile_error`, and `test_runner::is_bare_status_line`
exported just for it), repeat bookkeeping, and package resolution. The
display condenser exists because brokkr renders JSON events back to human
text and then has to filter that text. With 2.2's typed outcome, test_cmd
could render footers from events directly. Splitting it into
`test_cmd/{plan.rs, run.rs, verdict.rs, display.rs}` follows the real seams.

### 3.7 The shim session
`Session` holds about ten `Arc` fields cloned field-by-field into `Shared<'a>`
for each accept. One `Arc<SessionState>` would do. `finish()` joins drains
with no grace bound (relying on each drain to notice `done` within a poll),
while the unisolated path uses `join_drains` with `DRAIN_GRACE` and a warning.
Two drain-shutdown policies for one concern.

### 3.8 Small items
- `LibtestRun.completed` doc still describes "the deferred-observe state machine
  ... `println!("ok")` will have its duration inflated". That machine is
  deleted (see the tombstone comment), so the doc is stale.
- The `run_libtest_parallel` doc says "Parallelism forecloses the serial
  runner's partial-marker watchdog". Also stale.
- The `Ceilings` doc block is glued onto `WallShape` (two `///` paragraphs
  stacked, so `Ceilings` itself has no doc).
- `binary_timings.rs` opens with "Per-binary wall times", but the file's whole
  argument is that it stores serial cost and not wall time.
- `nextest.rs`'s `Disposition::is_terminal` is `cfg(test)`-only: "pins the
  policy" only for tests, and production consumers decide terminality
  elsewhere (coverage). The policy should have one live owner.
- `nextest_lane` always records `StreamEnded { Eof }` with
  `Unattributed { "the engine lane's stream" }`, including after an engine
  error. "Eof" there is asserted, not observed.
- `cpu_topology` is fine as is: a single job, total fallbacks. The one note is
  that it reads only cpu0's L3 domain, so on a heterogeneous or asymmetric
  part the budget default is cpu0's island, not the smallest island.
