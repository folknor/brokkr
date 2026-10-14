# Design hunt: measurement core

Scope: `src/measure.rs`, `src/harness.rs` + `src/harness_mod/`, `src/sidecar.rs`,
`src/sidecar_cmd.rs`, `src/sidecar_fmt.rs`, `src/hotpath_fmt.rs`, `src/db/`,
`src/request.rs`, `src/results_cmd.rs`, `src/history.rs`, `src/history_cmd.rs`,
`src/output.rs`, `src/build.rs`, `src/context.rs`, `src/worktree.rs`,
`src/worktree_record.rs`, `src/bench_cmd/`, `src/runnables.rs`.

Ordered roughly by payoff. Bugs found along the way are collected at the end
(section 4) and also referenced inline.

---

## 1. Axes that should be types

### 1.1 Wall time is a pair of integers held in step by comments

`BenchResult { elapsed_ms: i64, elapsed_us: Option<i64> }`, `RunRow`/`StoredRow`
(same pair), `Distribution { min_ms.. max_ms, us: Option<DistributionUs> }`, and
`iterations: Vec<i64>` + `iterations_us: Vec<i64>` (parallel vecs). Every one of
these carries a doc comment saying "the ms value is the us value rounded, never
treat them as independent" - the invariant is prose. Enforcement is scattered:
`us_to_ms` / `elapsed_to_ms` (harness), `all_or_none` (loop side), the
`len() == len()` check in `db::write::insert_timings` (store side),
`bench_ordering_key` (`elapsed_us.unwrap_or(ms * 1000)`), `ms_value` (display),
`format_ms_or_dash`/`format_wall` (db formatters).

They already disagree once: `sidecar::run_sidecar_with_deadline` computes
`SidecarSummary.wall_time_ms` with `as_millis()` (floor), while every harness path
rounds to nearest (`elapsed_to_ms`, `us_to_ms`, whose doc explains that flooring
is "the one error worth avoiding"). So the sidecar's stored wall for a run and the
results row's wall for the same run can differ by a millisecond in the flattering
direction.

Type: `enum Wall { Micros(i64), LegacyMillis(i64) }` with `.ms()` (always derived,
nearest) and `.ordering_key()`; `Distribution` holds `Wall`s; iterations become
`Vec<Wall>` (one list, alignment impossible to break). The DB layer splits a
`Wall` into its two columns at the boundary and re-joins on read. The
`all_or_none` rule becomes a constructor on the iteration list, not a convention
two modules repeat.

### 1.2 How a child ended: three bools, an i32 and a sentinel

`SidecarRunResult { stopped_by_marker, stopped_by_signal, stopped_by_deadline:
bool, exit_status }`, `HotpathCapture { stopped_by_marker, stopped_by_deadline,
status_ok, status }`, `DeadlineCapture { killed_on_deadline, output_cut }`,
`PassthroughOutput { code: i32 }`, `exit_code_from_status` (128+signal, -1
fallback), `HistoryRow.exit_status: i32`, `RunInfo.exit_code: Option<i32>`
(and `run_hotpath` records `pid 0, exit 0` as sentinels for "unknown").

The bools are mutually exclusive by construction but nothing says so; each loop
re-derives the classification in its own order (`interrupted` first, then
`!stopped && check_success`). `stopped_by_deadline` is never read by any
`run_external_*` loop - a deadline kill there surfaces as a generic "killed by
signal 9" subprocess error rather than "exceeded its deadline".

Type: `enum Ending { Exited(i32), Signalled(i32), StopMarker(String /*resolved*/),
Deadline(Duration), Interrupted }` produced once by the sidecar/captured runners,
plus one `fn verdict(&Ending, ok_codes) -> IterationVerdict { Counted, Failed(DevError),
Interrupted }`. `exit_code_from_status`, `check_success_or` and the per-loop
branching collapse into that function. `RunInfo` takes `Option<Ending>` instead of
`pid: 0` / `exit_code: Some(0)` sentinels.

### 1.3 Measurement mode is a string after the request

`MeasureMode` is a proper enum, but at the harness boundary it becomes
`Option<String>` (`with_measure_mode(req.mode_label())`), stored as
`BenchHarness.measure_mode: Option<String>`, cloned into `RunRow.mode:
Option<String>`, `RunInfo.mode`, and printed. `BenchConfig.mode` is a second,
dead fallback (`effective_mode` = harness.or(config); every writer sets `None`;
the doc comment admits the field caused two bugs). `ratatoskr_sync/bench_gate.rs`
mints its own `Some("bench")`. `pbfhogg/dispatch.rs` has its own
`if alloc { "alloc" } else { "hotpath" }` label. The DB tests insert `"mmap"`,
`"zlib"`, `"release"` into the column, i.e. the type admits anything.

Also `MeasureMode::{Bench,Hotpath,Alloc} { runs: usize }` admits `runs: 0`; four
loops each re-check "benchmark requires at least 1 run" as a `DevError::Config`
at the end. `NonZeroUsize` makes that unreachable.

Type: `enum RecordedMode { Bench, Hotpath, Alloc }` (the DB-writing subset),
`MeasureMode::recorded() -> Option<RecordedMode>`, harness field non-optional
(a harness without a mode cannot be built - today `HarnessContext` without
`with_request` silently writes `mode = NULL`). Delete `BenchConfig.mode` and
`BenchConfig.brokkr_args` (also a never-set fallback).

### 1.4 Profiled mode -> cargo feature is a bool passed around

`hotpath_feature(alloc: bool)`, `hotpath_alloc_note(alloc: bool)`,
`MeasureRequest::is_alloc()`, `hotpath_features()`. Callers in pbfhogg, elivagar,
nidhogg, dellingr, mogwai, sluggrs, piners, main_parts each compute `alloc` and
call these, and several of them also use `hotpath_feature(alloc)` as a *label*
(`let label = harness::hotpath_feature(...)`), conflating feature name and
recorded mode. A `ProfiledMode { Hotpath, Alloc }` with `.feature()`, `.label()`,
`.note()` owned by `measure.rs` removes the bool and the label/feature pun.

### 1.5 Build provenance is restated by every writer instead of read from the build

`BuildConfig.profile: &'static str` (`"release"` / `"dev"`) while
`CargoProfile` exists as an enum. The recorded profile (`BenchConfig.cargo_profile`)
is set by ~30 writer sites, almost all as a literal `CargoProfile::Release`, two as
`for_debug(debug)` - independently of what `BuildConfig` actually built. The
`CargoProfile::Dev` doc comment records the drift this already caused ("older dev
rows in those series carry the wrong profile"). Same for features:
`BenchContext::with_build_config` joins `build_config.features` into the harness
default, `BenchConfig.cargo_features` can override it, and `HarnessContext` (no
build) leaves it unset.

Type: `cargo_build` returns `BuiltArtifact { path, profile: CargoProfile, features:
FeatureSet, target: TargetKind }`; `BuildConfig.profile: CargoProfile` (plus a
`.dir_name()` - see bug 4.4); the harness takes the artifact (or an explicit
`ExternalBuild::{Java, CMake}`) and stamps the row from it. Writers stop naming
profiles at all. The ~30 literal sites disappear.

### 1.6 Project root vs build root: two `&Path`s that swap silently

`BenchHarness::new_with_lock(paths, project_root, db_root, ..)` is called with
`project_root = effective build root` and `db_root = Some(project_root)` - the
parameter named `project_root` receives the build root, and git/env are collected
from it. `MeasureRequest { project_root, build_root: Option<&Path> }`,
`resolve_bootstrap_paths` returns `(paths, effective, db_root)`,
`with_worktree(project_root, parent_build_root, ..)`, `bench_cmd::run` destructures
`(project, project_root, build_root, ..)`. Comments in `bench_cmd/mod.rs` and
`context.rs` spend paragraphs explaining which root is which.

Type: `ProjectRoot(PathBuf)` (owns `.brokkr/`, all stores, lock context) and
`BuildRoot { Live(PathBuf) | Worktree(Worktree) }` (owns git identity, cargo
cwd, and - see 2.6 - the target dir). Store paths become methods on
`ProjectRoot` (`results_db()`, `sidecar_db()`, `bench_home()`), which also fixes
the duplicated derivation in 2.7.

### 1.7 Run identity is a bare hex string minted in two places

`db::types::generate_uuid` and an inline copy in `BenchHarness::store_sidecar`
(same urandom + version/variant bits). Short form: `db::types::short_uuid` plus
three inline `&uuid[..8.min(len)]` (harness `previous_run_kv`, `store_sidecar`,
`results_cmd`). The sidecar `dirty` alias, uuid prefixes, and full uuids all travel
as `&str`; `SidecarDb::resolve_session` is re-run inside every query
(`query_samples`, `query_markers`, `query_counters`, `query_meta`,
`query_run_info`) on an already-resolved value, and `query_meta` swallows an
ambiguity error into `(0, 1)`.

Type: `RunId` (minted once, `.short()`), `RunRef { Prefix(String) | Alias(String) }`
resolved once per command into a `SessionId` that the query methods take - so
they cannot be handed an unresolved prefix. See 2.4 for the two stores'
disagreeing resolvers.

### 1.8 kv namespaces are string prefixes classified at every reader

`KvPair.key: String` carries five classes encoded as prefixes: `meta.*` (writer
metadata), `env.*` (captured env), `prev.*` (predecessor provenance),
`threads.*` (hotpath thread summary), bare (runtime counters) - plus the special
`peak_rss_kb` that `build_row` promotes to a column. See 2.3 for the readers that
each classify these and already disagree. Type: at load, `StoredRow` carries
`meta: BTreeMap`, `env` (already done - `captured_env`), `predecessor:
Option<PreviousRun>`, `thread_summary`, `counters`; at write, `RunRow` takes the
same split and the prefixing happens only in `db::write`.

### 1.9 Hotpath metrics stored as display strings and parsed back

`HotpathFunction { avg, total, percent_total, p50, p95, p99: Option<String> }`
and `HotpathThread` likewise. `hotpath_fmt::parse_metric` reverse-engineers units
(`ns`/`us`/`ms`/`s`/`B`/`KB`/`%`) to compute diffs. The `db/hotpath.rs` header
admits the three percentile names are hard-coded in parser, schema and formatter
(a decision in three places) and that `p99.9` is silently dropped. Parse once at
ingest into `Quantity { Duration(ns) | Bytes(u64) | Percent(f64) }` and
`percentiles: BTreeMap<Percentile, Quantity>`; the diff and render then never
see a unit string. The schema change the header calls "a real project" is the
right project.

### 1.10 `--stop` is a raw string interpreted per tick, and stored as typed

`stop_marker: Option<&str>` is re-interpreted on every FIFO drain by
`stop_match` (three spellings: verbatim, `-FOO`, `FOO` -> `FOO_END`). The row
stores `stop_marker` as the *configured* spelling, not the marker that fired, and
not whether it fired at all - a `--stop X` run whose marker never appeared
records `stop_marker = X` beside a full-length wall, indistinguishable from a
stopped run. Parse once into `StopSpec { Exact(String) | SpanEnd(String) |
Lenient(String) }`; record `Ending::StopMarker(resolved)` (1.2) on the row.

### 1.11 Query requests: bool flags for an exclusive choice, `Vec` for a pair

`SidecarQuery { samples, markers, durations, counters, stalls: bool, stat:
Option<String>, compare: Option<Vec<String>> }` - "picks exactly one view" per
the doc, enforced only by clap and by `cmd_sidecar`'s `if` order. `ResultsQuery
{ query, commit, compare: Option<Vec<String>> }` (index 0/1 with `map_or("")`),
`BenchArgs.compare: Option<Vec<String>>`. An `enum SidecarView`, `enum
ResultsView { One(RunRef), Compare(Rev, Rev), Table(Filter) }` and `(A, B)`
tuples remove the unreachable branches and make the filter-dropping bug in 4.3 a
compile error (a view that has no use for a filter would not carry it).

### 1.12 Failures that reach callers as prose

- `parse_kv_stderr`'s missing `elapsed_ms` is `DevError::Config` with a 500-char
  stderr preview; a missing self-reported wall is a contract violation by the
  target, not a config error.
- "benchmark requires at least 1 run" is `DevError::Config` (see 1.3).
- `run_variants` ends with `DevError::Reported("N of M variants failed: ...")` -
  the failed variant names exist only in the string; a machine caller cannot ask
  which ones failed.
- `SidecarDb::resolve_session` ambiguity is `DevError::Config` with the candidate
  list formatted into it.

---

## 2. Decisions made in more than one place

### 2.1 "Which iteration counted, which won, and where does its trajectory go"

Sites: `run_internal`, `run_hotpath`, `run_external_inner`,
`run_external_with_kv_raw`, `run_distribution` (all in `types_run.rs`) and the
foreign loop in `ratatoskr_sync/bench_gate.rs` (via `begin_measurement` /
`run_info_for`). Each one independently decides: failure classification (2.2),
best-of-N (`pick_best` + `bench_ordering_key` in three loops, a hand-written
`us < best` in `run_external_inner`, kept "consistent" by comments), the
`best_run_idx` for sidecar, the `measure_start_epoch` stamp (two loops call
`mark_measure_start`, three set it inline, the foreign loop calls a hook), where
a failed iteration's sidecar goes (`store_sidecar(None, ..).ok()` repeated six
times), and how the row and sidecar are committed (three record-then-store
blocks with the same "row is lost; trajectory need not be" fallback, plus the
deferred `PendingSidecar` / `record_with_pending` / `commit_sidecar` protocol
that a caller must follow by hand).

They already disagree: `run_external_with_kv_raw` takes no `ok_codes` and uses
`check_success`, `run_external_inner` takes `ok_codes`; `run_hotpath` parks
failed captures through a thread-local (`FAILED_CAPTURE`) because the closure
contract cannot return them, while the external loops have the data directly.

Single owner: one `MeasurementLoop` engine parameterised by
`WallSource { External, SelfReported, Closure }` and a sidecar on/off, returning
`Measurement { iterations: Vec<Iteration { wall: Wall, ending: Ending, sidecar:
Option<SidecarData>, stderr }>, winner: usize }`. Recording is a separate step:
`harness.record(measurement, post: impl FnOnce(&Iteration) -> Kv)` that always
does row-then-sidecar with the dirty fallback, so the deferred-commit protocol
and the thread-local parking slot both disappear (a failed iteration is a value
in the list, not a side channel). The ratatoskr loop then plugs in as a
`WallSource::MarkerSpan` instead of borrowing private hooks.

### 2.2 "Did this child run succeed?"

Sites: `CapturedOutput::check_success_or`, `HotpathCapture::status_ok` +
`status_error` (which rebuilds a `CapturedOutput` with a zero duration just to call
`check_success_or`), the `!stopped && check_success*` blocks in each loop,
`run_hotpath_capture` (`!c.stopped_by_marker && !c.status_ok`),
`run_passthrough_in` (signal -> `Subprocess`), `cargo_bench` (`out.code == 0`).
Owner: `verdict(&Ending, ok_codes)` from 1.2.

### 2.3 "What kind of kv pair is this?" - already disagreeing

- `db/format/compare.rs::collect_counters`: counters = not `meta.`, not `env.`,
  not `prev.` (so `threads.*` ARE counters).
- `db/format/single.rs::extras_fields` and `db/format/details.rs`: runtime = not
  `meta.`, not `threads.` (so `prev.*` ARE shown as runtime counters, `threads.*`
  are not).
- `db/query.rs::load_children`: strips `env.*` into `captured_env` at load - which
  makes compare's `env.` exclusion dead code and means the two views' notions of
  "counter" differ on exactly `prev.*` and `threads.*`.
- `harness build_row`: `peak_rss_kb` promoted to a column and removed.
- `db::write::insert_hotpath`: `threads.*` written `OR IGNORE` so stderr wins.
- `ratatoskr/gate.rs` and `ratatoskr_sync/bench_gate.rs`, `db/migrate.rs`,
  `config_parts/parser.rs` each format or strip `meta.` / `env.` themselves.

Owner: a `KvClass` classifier in `db/types.rs` used once at load and once at
write (1.8). The views then pick fields, not prefixes.

### 2.4 "Which run does this id name?" - two stores, two policies

`ResultsDb::query_by_uuid(prefix)` returns every matching row (ambiguity is
rendered as a multi-row table). `SidecarDb::resolve_session` refuses ambiguity
with a candidate list. `results_cmd` resolves the `dirty` alias through the
*sidecar* DB before querying the *results* DB. `results_cmd` and
`sidecar_cmd` each open the sidecar DB with their own `open_sidecar_db` (two
identical copies). Owner: a `RunStore` facade over both files with one
`resolve(RunRef) -> Resolved { One(RunId) | Ambiguous(Vec<RunId>) | Missing }`,
consumed by both commands with a deliberate per-command policy.

### 2.5 "Is the tree clean enough?" - five answers

- `git::check_clean` (via `GitInfo.is_clean`): measurement rule - excludes `*.md`,
  `brokkr.toml` and its includes, `approved.png`, `.brokkr/`. Used by the harness
  to refuse/skip storage, by sluggrs/litehtml, and by `history_cmd` to stamp
  `dirty`.
- `git::has_uncommitted`: record rule - excludes only `.brokkr/` and the moved
  toolchain file. Used by piners.
- `bench_cmd::is_dirty`: raw `git status --porcelain`, nothing excluded. It asks
  the *same* question as the harness ("may this measurement be filed under this
  commit") and disagrees: a markdown edit refuses `brokkr bench` but not
  `brokkr <cmd> --bench`.
- `worktree::is_dirty`: raw porcelain (deliberately different question - "would
  deletion destroy work"; fine).
- `worktree::is_pristine`: hard-codes `"rust-toolchain"`/`"rust-toolchain.toml"`
  while `has_uncommitted` iterates `crate::toolchain::FILES` - a second copy of
  the toolchain file list.

Owner: one `TreeStatus` parsed from a single porcelain call (entries classified
as source / prose / brokkr-store / toolchain-move / config), with named policies
`for_measurement()`, `for_record()`, `for_reuse()`, `for_deletion()`. The bench
baseline guard should call `for_measurement()` (or, if the stricter rule is
intended, say so in the policy, not by accident of using raw git).

### 2.6 "Which target dir does a build in this root use?"

`worktree::isolated_target_dir(build_root)` is the answer, but *applying* it is
each cargo spawn site's job: `build::cargo_build_locked` and
`bench_cmd::cargo_bench` each repeat it with near-identical comments explaining
that forgetting it measures the wrong commit's binary under the right name.
`build::resolve_existing_binary` does not apply it at all (4.4). `runnables`
(`run --commit`) goes through `cargo_build`; any future cargo invocation must
remember. Owner: `BuildRoot::cargo_command()` (1.6) sets `CARGO_TARGET_DIR` for
the worktree variant, so a spawn cannot forget.

### 2.7 "Where does store X live?"

Readers use `resolve::results_db_path` / `resolve::sidecar_db_path(project_root)`;
the writer (`BenchHarness`) derives `db_dir.join("results.db")` /
`.join("sidecar.db")` from its own `db_root.unwrap_or(project_root)`.
`bench_cmd::bench_home` is a third private derivation of `.brokkr/<x>`. They agree
today because `db_root` is always the project root. Owner: `ProjectRoot` methods.

### 2.8 "What is a phase / span?" - four interpretations of marker names

- `sidecar::stop_match`: `-FOO` and `FOO` -> `FOO_END`.
- `sidecar_fmt::resolve_phase_range` (`--phase`): exact name -> to next marker;
  `FOO` -> `FOO_START`..`FOO_END`; then substring -> to next marker.
  So `--phase STAGE2_START` and `--phase STAGE2` give different ranges.
- `sidecar_fmt::print_marker_durations` (`--durations`): pairs `X_START` with the
  next `X_END`.
- `sidecar_fmt::build_phases` (default summary and `--compare`): every marker is a
  boundary, no pairing. The comment at its call site in `print_phase_summary`
  says it "pairs `*_START`/`*_END` into a single phase" - it does not; the
  comment or the code is wrong.
- `stall_totals_ns`: `*_wait_ns` counter suffix convention.

Owner: one `Timeline` model in `sidecar.rs` built once from markers (spans with
start/end, standalone points), queried by every view and by the stop matcher.

### 2.9 "Which `brokkr` CLI tokens define an arm?" - a second CLI grammar

`db/format/compare.rs::normalize_brokkr_args` re-parses the stored invocation
string with hand-kept knowledge of `--commit`, `--verbose`/`-v`,
`--bench/--hotpath/--alloc [N]`, `=N`. `runnables::RUN_VALUE_FLAGS` hand-keeps
which `run` flags take a value. Both must agree with clap and nothing checks.
For compare, the root fix is recording the arm structurally at write time (the
harness knows the measure mode, the commit, the runs count): store an
`arm_key` (normalized argv minus provenance flags, computed from the parsed
`Cli`, not from the joined string) and pair on it. The "a value that happens to be
`--commit` eats the next token" caveat in the doc then goes away.

### 2.10 Host conditions collected twice

`env::collect` (results rows: kernel, governor, memory) and `history_cmd` (reads
`/proc/version` itself for kernel, `env::read_memory` for memory, `git::collect`
for dirty). One `HostSnapshot` type used by both.

### 2.11 SQLite store lifecycle - one per store

`ResultsDb::open`, `SidecarDb::open`, `GateDb::open`, `HistoryDb::open_typed`,
plus `litehtml/db.rs`, `sluggrs/db.rs`, `piners/lint/db.rs`, `piners/corpus_db`:
each does its own WAL pragma, migration runner and `user_version` stamp. Only
history sets a `busy_timeout`; `brokkr results` / `brokkr sidecar` open (and
migrate, and write `user_version`) without the lock and without a busy timeout,
racing a running bench's writes. Worse, `results.db` has three schema owners
(`ResultsDb`, litehtml `MechanicalDb`, sluggrs) sharing one `user_version`; the
migrate tests carry regression cases for litehtml having stamped it. Owner: a
`Store` trait / helper owning open + pragmas + busy timeout + versioned
migrations, and one `user_version` owner per file - give litehtml and sluggrs
their own files rather than guests in `results.db`.

---

## 3. Structure

### 3.1 `harness.rs` is four `include!`s; the split follows nothing

`harness_mod/format_sidecar.rs` holds result-line formatting, `run_variants`,
`hotpath_feature`, `elapsed_to_ms/us`, the hotpath capture runner and its
thread-local parking slot, percentile math, kv-stderr parsing, and sidecar DB
backup rotation. `types_run.rs` holds the harness and all loops.
`foreign_loop.rs` is two hooks. `include!` also means no module privacy between
them. Proposed modules: `measure/wall.rs` (Wall, rounding, distribution
summary), `measure/loop.rs` (2.1 engine), `measure/record.rs` (row building,
`prev.*`, result line), `measure/capture.rs` (hotpath child runner),
`measure/kv.rs` (stderr counter protocol), `sidecar/backup.rs`,
`bench_variants.rs` (`run_variants` is dispatch helper, not harness).

### 3.2 `measure.rs` owns pbfhogg's argument surface

`CommandParams` (direct_io, io_uring, compression, snapshot, osc_range,
inject_prepass, force_altw, merged-cache observations as `Option<String>`
seconds/MB...) and `CommandContext` (pbf_path, osc_paths, pbf_b_path, bbox) are
pbfhogg's dispatch types living in the shared core. The runtime observations
(`merged_cache_age_s: Option<String>`, `to_snapshot_file_mb: Option<String>`)
are numbers formatted early. Move both into `src/pbfhogg/`; `measure.rs` keeps
`MeasureMode`/`MeasureRequest` only.

### 3.3 `output.rs` is a console renderer and a process supervisor

Lines 1-500: prefixed console output, status line, run log, quiet flag, the
pbfhogg-specific verify buffer, and per-project `*_msg` functions (litehtml,
sluggrs, ratatoskr, corpus, lint, deps, wc). Lines 500+: `CapturedOutput`,
deadline/idle-limited runners, SIGTERM forwarding, pipe drains with settle,
`spawn_captured`, passthrough. Split into `console` and `child` (process
supervision). The child half should also absorb the sidecar's own spawn/kill/
drain loop (3.4) so there is one supervisor with one drain implementation.

### 3.4 Two child supervisors with divergent safety

`output::run_captured_limited` and `sidecar::run_sidecar_with_deadline` each
implement: try_wait polling, shutdown-flag check, deadline kill, PG SIGKILL,
stdout/stderr drains. They have already diverged: the output runner bounds the
drain join (`settle_drains`, `DRAIN_SETTLE`, with a comment about the hang the
unbounded join caused), the sidecar one joins unboundedly (bug 4.2); the output
runner forwards SIGTERM with a budget, the sidecar SIGKILLs immediately; the
output runner has an idle limit, the sidecar a thread-local `DeadlineScope`.
One supervisor with a pluggable per-tick observer (the sidecar's `/proc` sampler
+ FIFO drain + stop matcher) would carry the fixes to both.

### 3.5 Dependency edges pointing into project modules

- `sidecar.rs` and `output.rs` call `crate::ratatoskr::process::send_signal_pgrp`
  (core -> ratatoskr). Move process-group signalling into the core (beside
  `lockfile`'s pidfd helpers).
- `output.rs::drain_stamped` calls `crate::test_runner::read_unless_cancelled`
  (console/process core -> test runner).
- `bench_cmd` calls `crate::check_cmd::RunLog` (bench -> check). `RunLog` is
  shared infrastructure; move it out of `check_cmd`.
- `db/gate.rs` is ratatoskr's `gate.db` inside the shared `db` module, reaching
  into `db::types::generate_uuid`. It belongs in `ratatoskr_sync/`.
- `db/format/details.rs` hard-codes `row.project != "pbfhogg"` as the default
  project to hide.

### 3.6 `BenchContext` / `HarnessContext` duplicate the lifecycle

Both acquire the lock with the same `LockContext` literal, call
`resolve_bootstrap_paths`, build a harness, and have byte-identical
`with_request`. The real difference is "build or not". One `MeasureSession`
builder (`lock -> paths -> optional build -> harness(request)`) taking the
`MeasureRequest` at construction, so a harness without request provenance cannot
exist (fixes the `mode = NULL` hole in 1.3).

### 3.7 `BenchConfig` mixes per-row identity with dead fallbacks

`BenchConfig` carries `command`, `input_file`, `input_mb`, `cli_args`,
`metadata` (real per-row inputs), `runs` (loop control), `cargo_profile` /
`cargo_features` (build provenance - 1.5), and `mode` / `brokkr_args` (dead
fallbacks). Split into `RowSubject { command, input, cli_args, metadata }` and
let runs come from the request and provenance from the artifact.

### 3.8 Tests

`harness_mod/tests.rs` is `include!`d into the same namespace, so tests reach
private helpers freely; that is what keeps the five loops "consistent" (each
consistency is pinned by a test of the specific loop, not by shared code). With
the engine in 2.1, one loop test suite replaces per-loop pins. `db/query.rs`
(1724 lines) and `db/migrate.rs` (2184) are mostly test fixtures building full
`RunRow` literals by hand (13+ copies of every field); a `RunRow::fixture()`
builder would shrink them and stop each schema addition touching every test.

---

## 4. Bugs and smells found along the way

4.1 **`ExitStatus::default()` is success.** `sidecar::run_sidecar_with_deadline`,
when `try_wait` errored and the final `wait` also fails, synthesises
`ExitStatus::default()` with the comment "synthesize a failure status" - on Unix
that is exit 0. A child the sidecar lost track of is recorded as a clean success
and its wall is stored.

4.2 **Unbounded pipe join in the sidecar.** `sidecar::drain_pipe` is
`read_to_end` on a thread joined unconditionally after exit. A measured binary
that leaves a grandchild holding stdout/stderr (a daemon, a helper) and exits
normally hangs brokkr with the lock held - the exact hang `output::settle_drains`
was written to fix. The PG SIGKILL only happens on stop/deadline/shutdown, not on
natural exit.

4.3 **`results --compare` silently drops `--meta` and `--env`.**
`CompareFilter` has no `meta`/`env`; `results_cmd` builds it without them, and
clap does not conflict them with `--compare`. An A/B selected by `--env K=1`
compares unfiltered rows. Likewise a uuid query silently ignores `--command`,
`--mode`, `--dataset`, `--grep`. (The doc comment on `query_compare` records that
`--grep` was once dropped the same way - the filter is defined twice.)

4.4 **`resolve_existing_binary` builds `<target>/<profile>/<bin>`.** For
`profile = "dev"` cargo's directory is `debug`, not `dev`; it also ignores the
worktree's isolated target dir and the `examples/` subdir. Only reached from
`main_parts/commands.rs` today; latent.

4.5 **`run_external_with_kv_raw` has no `ok_codes`.** A self-reporting target
that uses a non-zero "success" code cannot be benched on the kv path, while the
external path supports it.

4.6 **`stop_marker` recorded as configured, not as fired** (1.10).

4.7 **Readers write.** `brokkr results` / `brokkr sidecar` run migrations and
`pragma user_version` writes on open, without the lock and without
`busy_timeout`, concurrently with a bench writing the same files (2.11).

4.8 **Stale comment** in `sidecar_fmt::print_phase_summary` claims `build_phases`
pairs `_START`/`_END`; it does not (2.8).

4.9 `run_hotpath` records sidecar `RunInfo` with `pid: 0` and `exit_code: Some(0)`
as sentinels; `brokkr sidecar` then prints them as if real.

4.10 `details.rs` hides `project` when it equals the literal `"pbfhogg"` - a
display default tied to one project in the shared formatter.
