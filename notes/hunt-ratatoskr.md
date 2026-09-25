I found 64 findings, grouped below by the eight questions. Several are live bugs, not predictions; I've put those first. Paths are under `/home/folk/Programs/brokkr/` unless shown in full. I made no edits and ran no builds.

## Bugs I tripped over

- **B1. `brokkr kill` doesn't stop `sync --all`; it grinds through the rest of the cohort.** In `src/ratatoskr_sync/list_smoke.rs`, `run_sync_all` treats `DevError::Interrupted` as an ordinary script failure and moves on to the next script. The SIGTERM flag stays set because the guard is installed only once. So every remaining script still spawns sæhrimnir (whose readiness wait never checks the flag), then its harness is killed at once, and each one leaves a preserved artefact dir. `service --all` stops on the first interrupt (via `?`), so each cohort handles interrupts its own way.
- **B2. `brokkr kill` doesn't stop `sync --gate all` either.** `run_gate_cohort` (`bench_gate.rs`) records `Interrupted` as a gate FAIL and continues. The next gate's sidecar `SigtermGuard::install()` resets `SHUTDOWN_REQUESTED`, so the rest of the sweep runs in full while holding the global lock.
- **B3. Every ratatoskr sync bench row in results.db is labelled Release.** `bench_loop` hard-codes `cargo_profile: CargoProfile::Release`, and `CargoProfile` (`src/build.rs`) has no dev variant, so a debug build can't even be recorded. ratatoskr's own `brokkr.toml` sets `[ratatoskr.harness] debug = true`, so this is already wrong in stored data. `cargo_features: None` is hard-coded too, and neither database records features.
- **B4. The gate never checks build profile.** `evaluate_against_baseline` checks that gate_name, script and fixture match the baseline row, but not `profile`, even though it is stored. A `--debug` run is compared against a release baseline, and the reverse, without any error. `GateEntry` carries `#[allow(dead_code)]` for the columns nobody reads.
- **B5. A short or empty baseline pin can compare a run against itself.** `GateDb::lookup_baseline` (`src/db/gate.rs`) matches `uuid LIKE ?1 || '%'` and takes the newest match. The current row is inserted *before* the lookup. A pin of `""` or a one-character prefix matching the new row makes it its own baseline, so `max_delta = 0` and `equal_to_baseline` always pass. An ambiguous prefix silently picks the newest row, and `%`/`_` in a pin act as wildcards.
- **B6. The bench path has no time limit.** `bench_loop` calls `sidecar::run_sidecar` with no deadline, and the script's `ceiling:` frontmatter is parsed but ignored under `--bench`. A hung harness in a gate sweep holds the global lock until someone kills it, and its stdout/stderr buffers grow in memory with no cap.
- **B7. `docs/commands/sync.md` wrongly says a SIGTERM between bench iterations still cleans up.** It says the mock and in-flight harness "are reaped via their `Drop` impls". SIGTERM with no handler kills the process outright, so no `Drop` runs. sæhrimnir is spawned with `isolate_pg=false` on the bench path, the graceful kill signals only brokkr's PID, and the mock is orphaned with its ports still bound.
- **B8. Sync's `run.toml` can be invalid TOML.** `write_run_toml` (`list_smoke.rs`) builds it with `format!`, inserting paths and the features label into `"..."` without escaping, so a `"` or `\` in a path breaks it. Service's `run.toml` goes through `toml::to_string` on a serde struct. The two writers also use different schemas (`binary` vs `harness_binary`, git fields in one only).
- **B9. Service scripts share one directory namespace with brokkr's own dirs.** Service artefacts go under `.brokkr/ratatoskr/<stem>`, the same level as `sync/`, `mock/` and `gate.db`. A script named `mock.lua` or `sync.lua` writes into those trees. A fixture named `readiness` becomes a dir at the path where mock-serve does `remove_file`, which then fails.

## 1. One value, one owner

- **Protocol list.** The nine protocols are spelled out separately in at least eight places: the `Endpoints` fields, `parse_sentinel`'s nine locals, its match arms, its error text, its missing-list, `endpoint_env_pairs`, `print_endpoints`, the one-line format in `FixtureSession::start`, and the nine `test_endpoint_env_*` config fields. The HTTP-vs-`host:port` rule is written twice (`endpoint_env_pairs` and `print_endpoints`), and `127.0.0.1` appears 18 times.
  - **Already diverged:** comments and docs say "five" (the `READINESS_BUDGET` doc, `ScriptInfo.protocol`, and service.md, sync.md and ratatoskr.md: "five-line sentinel"). service.md also lists only jmap/graph/gmail as HTTP.
  - **Fix, enforceable by type:** a `Protocol` enum carrying name, scheme and config key, `Endpoints` as `[u16; N]`, and the config as a map keyed by protocol. An exhaustive match then makes a forgotten protocol a compile error.
- **`.brokkr/ratatoskr` root.** Hard-coded separately in `service_test.rs` (`ARTEFACT_PARENT`), `saehrimnir.rs` (`MOCK_DIR`), `list_smoke.rs` (`SYNC_ARTEFACT_PARENT`), `bench_gate.rs` (the `gate.db` path and its error text) and `main_parts/commands.rs` (clean). Fix: one `RatatoskrLayout` type, with clean asking it for the removable names; a test can then pin that clean spares `layout.gate_db()`.
- **Build profile.** One `debug: bool` has three spellings: `"dev"` in `ratatoskr/build.rs` and `BuildConfig::for_harness`, `"debug"` in the gate row, and a hard-coded `Release` in results.db (B3). Fix: one enum with `as_cargo()` and `as_label()`.
- **`BROKKR_HARNESS_ARTEFACT_DIR`, `BROKKR_TEST_BIN_DIR` and `--test-harness`.** Spelled at three ratatoskr call sites plus two in piners. Fix: one `harness_env(artefact_dir, bin_dir)` constructor.
- **Relative-path rule** ("absolute is kept, relative joins project root"). Implemented four times: `require_path`, `sync_script_dir`, and the gate script in both `run_gate_cohort` and `run_gate_hook`.
- **Config validation and its error messages are repeated.**
  - The "no `[ratatoskr]` section" message has three copies; the "no `[ratatoskr.harness]`" message has four, worded two ways.
  - "sæhrimnir binary not found" has four copies with three different remedies. They say `cargo build --release` in sæhrimnir's repo, but the real config points at `/home/folk/.cargo/bin/saehrimnir`, which is a `cargo install` layout.
  - `run_sync_bench` repeats `validate_sync_config` inline instead of calling it.
- **Lock identity.** `LockContext { project: "ratatoskr", ... }` is a string literal at six sites, while `BenchHarness` uses `Project::Ratatoskr.name()`.
- **Duplicated timing constants.** `SIGTERM_FORWARD_BUDGET` (`output.rs`) copies `SHUTDOWN_BUDGET` "in spirit" at the same 1500 ms. The "50 ms, matching `ServiceClient`" poll interval is defined twice (`process.rs`, `output.rs`) and written as a bare literal twice more in `saehrimnir.rs`, alongside a 25 ms literal. Docs restate 50 ms, 10 s and 1.5 s.
- **Duration grammar.** `discover::parse_duration` and `service_test::format_duration` are inverse halves of one grammar in two files, with no round-trip test.
- **Duplicated JSON helpers.** `value_kind` (`bench_gate.rs`) and `kind_of` (`gate.rs`) are identical.
- **Two projections of `summary.json` with different rules.** `summary_to_kv` drops bools; `meta_to_json` keeps them. `meta_to_json`'s own doc says "Same scalar-only rule as `summary_to_kv`: bools pass through", which contradicts itself.
- **Hard-coded counts of another repo's gates.** Code says "14 zero-drift rules" (`run_gate_cohort` doc and a test comment). `ratatoskr-gate.md` says "14 of the 16 gates", and names the two exceptions. ratatoskr's `brokkr.toml` today has 17 gates, 15 of them with `max_delta = 0` or `equal_to_baseline`, so both claims are stale and they already disagree with each other (rules vs gates). Per CLAUDE.md, reword so no count is hard-coded.

## 2. Values nobody can find, change, or trust

- **Tunables are scattered.** `READINESS_BUDGET` (10 s), `SHUTDOWN_BUDGET` (1.5 s), `DEFAULT_CEILING` (60 s), both poll intervals, the 5-line stderr tail, and the default `--bench 3` (a clap string) are spread over five files. None of them can be injected, which is why the process tests use real `sleep` calls and wall-clock bounds.
- **`[ratatoskr]` config is checked only when a command uses it.** Examples:
  - A gate named `all` is refused only inside `run_sync_bench`, once per gate in a sweep.
  - A `--gate <typo>` is checked after `with_worktree` has already cut the `--commit` worktree.
  - `fixtures_dir` and `mock_server_binary` existence are checked per command.
  - Service cohorts check that the config keys exist up front, but not that fixture names resolve. A typo'd fixture fails mid-soak after the build.
  - `service <SCRIPT>` doesn't check the mock config before building at all, although `service --all` does.

  All of this belongs in one validation pass at `parse_ratatoskr`.
- **Frontmatter typos are silently ignored** (`discover.rs`):
  - `expected: ignore` becomes Pass, `ceiling: 5y` becomes 60 s, and `preserve_data_dir: maybe` becomes the default.
  - `fixtures:` is an unknown key and is ignored, so the script runs with no mock and no warning.
  - The test `malformed_ceiling_falls_back_to_default` locks this behaviour in.
  - Fix: reject unknown or unparseable keys, or at least warn; a test can then pin the refusal.
- **Build roots differ between shapes.** `sync --bench` builds in the cwd when `brokkr.toml` is one level up, but sync smoke/all and service always build in `project_root`.
- **`gate_runs.script` stores the canonical absolute path.** The doc says "absolute or repo-relative". Moving or renaming the checkout breaks every pinned baseline through the script-identity check.

## 3. One channel, one implementation

- **`ratatoskr_msg` bypasses the output system.** It uses `println!`, not `emit`, so its lines skip the run log, the status line and quiet mode. Multi-line messages lose the prefix on every line after the first (the `--as-baseline` paste block, the suite soak summary). The prefix isn't padded like the others (`[ratatoskr] ` vs `[corpus]  `).
- **Warnings are written as fake tags.** `"  [warn] ..."` appears inside `ratatoskr_msg` five times, and inside an error message in `missing_baseline_error`, instead of going through `output::warn`. A textlint rule could ban `\[warn\]` string literals outside `output.rs`.
- **Plural hedges instead of `output::count`.** "`gate(s)`", "`script(s)`", "`iteration(s)`", "`row(s)`" appear where `output::count` exists; `service_suite` hand-rolls its plurals. A textlint rule `\(s\)` over `src/**` string regions would catch these.
- **One flow, several prefixes.** The bench loop mixes `[bench]` and `[ratatoskr]`, and the build line is `[harness]`, although the comment on `feature_summary` calls it "the `[ratatoskr] building ...` log line".
- **Two duration formats for the same kind of line**: `412ms` in service vs `0.4s` in sync.
- **Endpoints are rendered twice**: as a table in `print_endpoints` and as one line in `FixtureSession::start`.
- **`artefacts::emit_clean_hint`** lives in the shared module but prints with the ratatoskr prefix.

## 4. Errors

- **Runtime and test failures are reported as config errors.** "harness binary exited", "gate FAILED", readiness timeouts and "no successful iterations" all use `DevError::Config`, so users see `config: gate ... FAILED`. The example output in `sync.md` shows the `config:` prefix. Service instead returns `ExitCode(1)`, so there are two policies for "a test failed".
- **Swallowed errors:**
  - `run_gate_hook` canonicalize: `.unwrap_or(configured_script.clone())`.
  - The sidecar/meta blobs: `serde_json::to_string(..).unwrap_or_else(|_| "{}")`, then parsed back with `unwrap_or_default()` in `evaluate_against_baseline`, a round-trip through a string built in the same function.
  - `missing_baseline_error` DB queries: `unwrap_or_default()` / `unwrap_or(0)`.
  - `MockOutcome` maps a wait I/O error to `killed_after_budget: true`, and counts any SIGKILL (e.g. from `--hard`) as budget overrun.
- **Errors that drop their subject:** "canonicalize script: {e}" names no path, and `run_sync_bench` canonicalizes *before* its `is_file` check, so the friendlier message can't be reached.
- **An invariant held only by call order.** `run_gate_hook`'s `.expect("gate existence already validated upstream")` depends on the order of two functions. `clippy::expect_used` isn't denied, only `unwrap_used`.

## 5. Tests that prove nothing

- **`capturing_true_succeeds` / `capturing_false_reports_nonzero_code`** (`service_suite.rs`) test `std::process::Command` and coreutils `/bin/true` and `/bin/false`, not any brokkr code. Their comment claims they exercise "success vs failure routing through the artefact dir". Delete them, or drive `spawn_and_capture` for real.
- **`process.rs` tests** depend on `sleep` being on PATH, on wall-clock timing (150 ms windows), and on "kernel does not reuse PIDs immediately". Three of them test `wait_for_sentinel`, which production never calls.
- **`write_artefacts_omits_git_keys_when_collection_fails`** depends on the host's git and a gitlink trick.
- **Endpoint tests** (which live in `bench_gate.rs` but test `saehrimnir.rs`) check the scheme for only jmap, gmail and imap. Nothing enumerates protocols, so a protocol missing from `endpoint_env_pairs` can't be caught.
- **`GateDb` tests** build the database through `open_mem`, a hand copy of the DDL, so `open`, `run_migrations` and WAL setup are never exercised.
- **Live gate rules that cannot fail.** Every one of the 17 gates in ratatoskr's config has `success equal = 1` and `exit_code equal = 0`: 34 rules. The gate hook is reached only after every iteration succeeded, and the row hard-codes `exit_code: 0, success: true`. These bare metrics are constants.

## 6. Guards and claims that have stopped holding

- **Clean spares `gate.db` only because of where it happens to sit.** Clean keeps "every file directly under `.brokkr/ratatoskr`" and deletes every directory. That is not the constructed-name rule CLAUDE.md claims, and moving `gate.db` into a subdirectory would delete the baselines with no warning. The fix is the layout type from question 1 plus a test.
- **A rule can check nothing and still pass.** The empty-rule-set refusal checks only that `metrics` is non-empty. A metric table with no predicates, or `equal_to_baseline = false`, produces zero outcomes, and the gate reports PASSED. Require at least one predicate per rule at parse time.
- **`hold::stamp` claims to cover "every child brokkr starts, by construction".** It's false for sæhrimnir: both spawn sites (`spawn_observed`, `run_mock_serve`) use a raw `Command::new` with no `hold::stamp` and no `oom::protect_child`. This could be enforced with clippy `disallowed_methods` on `Command::new`, allowed only in the choke-point module.
- **`isolate_pg: true` depends on a comment.** About a dozen call sites carry `true, // isolate_pg: caller's SigtermGuard active`, and nothing checks it. A signature like `Isolation::Pg(&SigtermGuard)` would make the invariant a type.
- **Doc claims that are false today:**
  - Code comments and help text (the `service` help in `cli/schema.rs`, `service_test`'s doc, `ratatoskr/mod.rs`) and service.md still say the build goes via the `[[check]]` sweep. It was decoupled; sync.md also still says "harness sweep" and lists "sweep" as a `run.toml` field.
  - `--force` is described three incompatible ways. `SyncBenchRequest.force` and sync.md say rows "land under the dirty alias"; the harness actually refuses to store them in results.db (`harness_mod/types_run.rs`), which is what the CLI help says.
  - Gate docs say "Every `--gate` invocation writes a row" and "gate rows are always written so a failure stays inspectable". A harness failure never reaches the gate hook, so only rule breaches get rows.
  - Gate docs say missing keys are "never silently treated as zero". `sidecar_to_json` writes 0 for io/cs/fault counters when there are no samples.
  - Gate docs say "Numeric scalars only"; bools are accepted.
  - Gate docs say `--as-baseline` with `--gate all` is refused "at parse time". It's refused at dispatch (`validate_gate_selection`).
  - service.md says single-script mock artefacts go under `.brokkr/ratatoskr/<test>/mock/`. The code uses `.brokkr/ratatoskr/mock/<fixture>/`.
  - `BenchConfig`'s comment says "no best-of-N loop"; it is a best-of-N loop.
  - The `Expected` doc says it can flip Fail to "expected failure". It is only used to skip scripts.
  - `ScriptInfo.fixture` says "service scripts leave it None"; service uses it.
  - The `saehrimnir.rs` header says brokkr's "existing `wait_for_sentinel` waits for presence"; that function is unused.
  - The `process.rs` header says `wait_for_sentinel` is "required for manual-matrix items 4 and 5"; nothing uses it.
  - `gate.rs` says the plumbing lives in `src/ratatoskr/sync.rs`, and sync.md says the helpers do. That file is an `include!` shim; the code is in `src/ratatoskr_sync/`.
  - `RatatoskrConfig.test_endpoint_env_*` says the fields are "consumed by `sync`"; service consumes them too.
- **The config docs point in a circle.** service, sync and gate docs point to `docs/brokkr.toml.md` for `[ratatoskr]`; that file points to `docs/projects/ratatoskr.md`; that file points to rustdoc. No doc lists `mock_server_binary`, `fixtures_dir`, `test_endpoint_env_*` and `sync_script_dir` together.
- **Durable code cites transient plans:** "plan 2", "plan 3", "Phase 8", "(plan 3 follow-up)" in a user-facing error, plus an absolute `/home/folk/Programs/sæhrimnir/` path in the `saehrimnir.rs` header. The existing `comments-never-cite-notes` rule could be extended to `\bplan \d|Phase \d` and `/home/`.

## 7. Policy invented per call site

- **Interrupt handling differs by cohort:** stop (service), continue (sync --all, B1), continue with the flag reset (gate sweep, B2). `SigtermGuard` can't nest (its `Drop` restores `SIG_DFL`), which is why the bench path runs unguarded. `MockServeSignalGuard` is a second signal-flag implementation. A re-entrant, counted guard, like the lockfile, would give one policy.
- **Validate-before-build differs per shape**: gate sweep up front, sync --all per script after the build, service suite keys-only, service single not at all.
- **Artefact placement and lifecycle differ:** sync gets a per-run `mock/`; service shares `.brokkr/ratatoskr/mock/<fixture>`. Mock-pid bookkeeping is `remove` then `clear_mock_pids` in service, `clear` only in sync.
- **The process-group decision differs per spawn:** `true` in smoke/service, `false` in bench, each justified in a paragraph-long comment.
- **Shared code depends on the ratatoskr module.** `output.rs`, `sidecar.rs`, `test_runner.rs` and `nidhogg/bench_tiles.rs` import from `crate::ratatoskr::process`. `test_runner::signal_process_group` duplicates `send_signal_pgrp`, and there are seven other raw `libc::kill` sites. `main_parts/commands.rs` checks liveness with `kill(pid,0) == 0`, treating EPERM as dead, the opposite of `pid_is_alive`. Move `process.rs` to a shared module and add a textlint rule against `crate::ratatoskr::` outside `src/ratatoskr*`.
- **Ambient dependencies are reached directly:** `unix_now()` in `db/gate.rs`, `Instant::now` everywhere, the hostname in `run_gate_hook`, and a cwd-relative `canonicalize` of the script.

## 8. Code that is no longer load-bearing

- **`pid_is_alive`, `wait_for_sentinel` and `SentinelOutcome`** (`process.rs`): only tests call them (checked with grep), and `#[allow(dead_code)]` hides that. `send_signal`, `send_signal_pgrp` and `snapshot_proc` carry the same stale `#[allow(dead_code)]` although they are used, so it would also hide them going dead.
- **`write_run_toml`'s `mock_dir` parameter** is an unused placeholder (`let _mock_dir_anchor = mock_dir; // future`).
- **`db/gate.rs::run_migrations`** is a no-op scaffold ("Future migrations go here"); the schema is still version 1.
- **The `exit_code` and `success` gate columns and bare metrics** always hold one value (question 5).
- **`MetricRule.equal_to_baseline: Option<bool>`**: `false` means nothing.
- **The `[ratatoskr.harness].sweep` migration refusal** (`config_parts/parser.rs`): the only consumer's config no longer has `sweep`.
- **Module naming.** `cmd.rs` and `sync.rs` are `include!` shims. `service_suite.rs` depends on imports declared in `service_test.rs` and can't carry `//!` docs. The file names (`service_test`, `service_suite`, `list_smoke`, `bench_gate`) are the retired command names, and `src/ratatoskr_sync/` isn't a module at all.
- **Waived lints.** `too_many_arguments` is denied but waived five times in this scope, and `too_many_lines` four times; request structs would remove the waivers.

## Other smells

- `gate.db` is described as committed but uses WAL, so its completeness depends on a clean connection close.
- The dirty-tree check runs once per gate inside `--gate all`, so a dirty tree gives 17 identical failures.
- Best-of-N can compare a marker-span iteration against a wall-clock one, and `meta.timing_source` records only the winner's source.
- The ratatoskr config in use hard-codes `mock_server_binary = "/home/folk/.cargo/bin/saehrimnir"`.

The rules `brokkr.toml` enforces today are three textlint entries plus Cargo `[lints.clippy]`. Nearly every consolidation above can be held with one of: a textlint rule (fake `[warn]` tags, `(s)` hedges, plan/Phase/`/home/` citations, `crate::ratatoskr::` imports from shared code), clippy `disallowed_methods` (`Command::new` outside the spawn choke point), `expect_used`, an exhaustive enum or type (protocols, build profile, process-group isolation proof), or a parse-time refusal in `parse_ratatoskr` (non-empty rules, a gate named `all`, the `baseline` UUID format, unknown frontmatter keys). Nothing that runs today checks the copies-must-agree rules I listed above.
