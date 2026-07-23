# Sweep 3 - consolidated review findings

Range reviewed: `aec3ee5..335e563` (32 commits, 2026-07-19 → 2026-07-22, ~8.7k insertions across 36 files).
Prior sweep: `aec3ee5` "repair three regressions found by the sweep-2 self-review".

## Provenance

Six independent reviewers, five scoped by subsystem plus one unscoped breadth pass.

| ID | Reviewer | Scope |
|---|---|---|
| L1 | Opus, scoped | `check_cmd/isolate.rs`, `check_cmd/binaries.rs`, `test_cmd.rs`, `context.rs`, `lockfile.rs` |
| L2 | Opus, scoped | `check_cmd/coverage.rs`, `scripts/smoke-certifies.sh`, the `--gate` path |
| L3 | Opus, scoped | `config_parts/{parser,schema,tests}.rs`, `profile.rs`, `toolchain.rs` |
| L4 | Opus, scoped | `cli.rs`, `cli/*`, `main_parts/*`, `man.rs`, `man/render.rs` |
| L5 | Opus, scoped | `check_cmd/output.rs`, `script_check.rs`, `style.rs`, `textlint.rs` |
| CX | codex `gpt-5.6-sol`, xhigh, unscoped | the whole range |

L1, L2 and L5 additionally read `src/check_cmd/phase.rs` as shared context.

CX ran verification against the tree: `cargo test --all-targets` passed all 1,177 tests, `cargo clippy --all-targets --all-features` passed, HEAD unchanged at `335e563`, no tracked changes made.

## How to read this

42 reported items consolidated to **36 distinct findings**. Grouping is by subsystem; within each subsystem the reporting lane's own ranking is preserved. **No cross-lane re-ranking and no adjudication has been applied** - severity labels, confidence levels and "the code is wrong" / "the doc is wrong" calls are each reviewer's own. Where two reviewers found the same defect, both descriptions are merged and both are credited; where they characterised it differently, both characterisations are kept.

Findings are numbered `S3-01`..`S3-36` for reference.

---

# Resolution status

Worked in waves of Opus agents (max 3 at a time), each wave partitioned so agents never share a file. Integration/build/test/commit happens in the main conversation between waves.

| Finding | Wave | Status |
|---|---|---|
| S3-01 | 1 | **fixed** - `--tests` append now guarded by `has_target_selector` in both enumeration (`binaries.rs`) and execution (`isolate.rs`) |
| S3-02 | 1 | **fixed** - process-isolated sweep now emits an `output::warn` announcing that `doctests = true` has no effect (doctests can't be enumerated per binary) |
| S3-03 | 1 | **refuted (adjudicated)** - `zero_test_run` flags on `total == 0 && filtered_out > 0`, and `total` *includes* `ignored`; an all-ignored run has `total > 0` so the non-isolated sibling also reads green. The isolated path already mirrors it. L1/L5 misread the guard's direction. |
| S3-04 | 1 | **fixed** - `enumerate_isolated` tracks a live-name set and removes live names from the global `ignored` set, so binary A's `#[ignore]` no longer suppresses B's live same-named test |
| S3-05 | 1 | **fixed** - `loader_path` takes the effective `LD_LIBRARY_PATH` (sweep env when set, else inherited) so a sweep's value is honored during the direct `--list` exec |
| S3-06 | 4 | pending |
| S3-07 | 1 | **fixed** - pipeline reordered to `decide_sweeps → select_sweep → dedupe_sweeps`; lane-qualified `--sweep` labels resolve before build-shape collapse |
| S3-08 | 1 | **fixed** - `dedupe_sweeps` key extended with `test_exclude_packages` so a sweep permitting `-p PKG` is never discarded for an order-earlier excluding one |
| S3-09 | 1 | **fixed** - `plan_runnable`'s stale `raw` param renamed to `all` (pure rename; caller already passed `all`) |
| S3-10 | 1 | **fixed** - isolated success path now echoes a passing test's captured output under `--raw`, mirroring the non-isolated path |
| S3-11 | 2 | **fixed** - `BenchContext`/`HarnessContext` acquire the lock (activating the worktree pin-disable) before their first `cargo metadata`; restores pre-rework ordering |
| S3-12 | 2 | **fixed (breaking)** - new load-time `validate_complete_universe`: a `complete` profile that leaves any `[[check]]` entry referenced by no sweep/lane is a resolve-time error. Configs carrying unreferenced entries under a complete claim now fail to load until reconciled. |
| S3-13 | 2 | **fixed** - `reject_extra_args_complete`: trailing `-- …` test args rejected under any `complete` claim (they narrow the run but not the audit) |
| S3-14 | 2 | **refuted (adjudicated)** - accountability for `test_exclude_packages` (a host-capability fact) belongs to the unbuilt `requires` feature; a required quarantine issue would be a never-closing "graveyard". Behaviour + docs already agree. |
| S3-15 | 3 | pending |
| S3-16 | 2 | **fixed** - quarantine attribution changed from first-match to most-specific-wins (longest pattern), so a narrower nested entry isn't starved and falsely reported stale |
| S3-17 | 2 | **fixed** - orphan worksheet and stale-entry report both print before the phase fails |
| S3-18 | 2 | **fixed** - an `executed: &[bool]` set threaded from `run_test_phase` so lanes the fail-fast loop never reached don't contribute to the audit's ran-set |
| S3-19 | 4 | pending |
| S3-20 | 4 | pending |
| S3-21 | 2 | **fixed** - `deps` (and a `compare-tiles` sibling) now take the global lock so the disabled toolchain is activated before their `cargo metadata`/`ccu` calls |
| S3-22 | 2 | **fixed** - `fmt` arms-then-locks when `disable_toolchain` is set (rides the lock's activation) instead of a racy bare `DisabledToolchain::activate` guard; stays lock-free otherwise |
| S3-23 | 4 | pending |
| S3-24 | 4 | pending |
| S3-25 | 4 | pending |
| S3-26 | 2 | **fixed** (done with the toolchain wave) - `docs/brokkr.toml.md` corrected: suppression lasts "for the duration of the global lock" (not "every command"), and worktree `--commit` pins are disabled, not honored |
| S3-27 | 3 | pending |
| S3-28 | 1 | **fixed** - emphasis/strong/strikethrough/sub/superscript open+close arms route into `current_cell` when `in_table_cell` (via new `push_marker` helper) instead of leaking to `out` |
| S3-29 | 1 | **fixed** - `Command::Man` arm moved above the `detect_optional()?` call so a malformed `brokkr.toml` no longer aborts `brokkr man` before its own fail-open fallback |
| S3-30 | 1 | **fixed** - in-cell inline code now carries backticks + `theme.code`; header cells styled via `theme.table_header`; column widths measured ANSI-aware (`visible_len`) |
| S3-31 | 1 | **fixed** - `man` listing header drops the `for X` clause (and its dangling space) when no project name is present |
| S3-32 | 2 | **fixed** - `skip_after` wired through into `scan_use_statements` for join rules; `region`/`table_row_only`/`in_toml_section` + `join_wrapped_use` rejected at compile time (a reconstructed `use` is always code) |
| S3-33 | 3 | pending |
| S3-34 | 2 | **fixed** - `keyword_structural_exempt` treats a line-final `=>` as arm-position, so a single-pattern match arm body is no longer flagged |
| S3-35 | 3 | pending |
| S3-36 | 3 | pending |

---

# Test isolation, binaries, sweep selection

## S3-01 - Unconditional `--tests` broadens a sweep that already carries `--test <target>`

**Reported by:** L1 (#1, CONFIRMED), CX (#2, P1)
**Files:** `src/check_cmd/isolate.rs:288`, `src/check_cmd/binaries.rs:42`; contrast `src/check_cmd/output.rs:416`

Both the enumeration build and every per-test execution append `--tests` with no `has_target_selector` guard, while the sibling non-isolated path deliberately does not. `docs/commands/check.md:125-129` states the contract explicitly: "`--tests` is not appended on top of one."

Failure scenario: `[[check]] name = "live", tests = ["serial_tests"]` under a profile with `isolation = "process"`. `sweep_selection_args` emits `--test serial_tests`; `test_binaries` then runs `cargo test --no-run --test serial_tests --tests`, and cargo *unions* target selectors, so it builds every lib/bin/integration test target in the selection instead of just `serial_tests`. `filter_binaries` (`binaries.rs:203`) correctly narrows *enumeration* back to the `serial_tests` binary, so the plan looks right - but `run_one_isolated_test` reuses the same broadened argv, so `cargo test --test serial_tests --tests -- --exact <name>` also runs any identically-pathed test in the lib unit-test harness, in a binary the lane deliberately excluded. The target filter is honoured for listing and silently discarded for execution.

CX frames the consequence as: a complete isolated profile can certify work it did not run.

L1: the code is wrong, not the doc. Traced `sweep_selection_args` → `test_binaries` → `run_one_isolated_test`, cross-checked against the guard the non-isolated path applies for exactly this reason.

## S3-02 - `[test] doctests = true` is silently ignored on a process-isolated sweep

**Reported by:** L1 (#2, CONFIRMED), CX (#2, P1)
**Files:** `src/check_cmd/phase.rs:1793`, `src/check_cmd/isolate.rs:288`

`run_test_phase` passes `doctests` to `run_one_test_sweep` but not to `run_isolated_sweep`, which hardcodes `--tests`. A project that opted back into doctests (`docs/commands/check.md:131`: "There is no per-sweep or CLI override - doctest inclusion is a project-wide, CI-parity property") gets doctests on every sweep except its process-isolated one, with no log line saying so.

L1 notes doctests genuinely cannot be enumerated per-binary, so the right fix is probably an explicit error or an announced omission rather than silence.

## S3-03 - An all-`#[ignore]`d process-isolated sweep prints a green "0 tests passed"

**Reported by:** L1 (#4, CONFIRMED), L5 (#1, CONFIRMED)
**Files:** `src/check_cmd/isolate.rs:67-74`, `:102-118`, `:180-187`

`plan_runnable` only rejects an *empty* runnable list. Tests that are present but `#[ignore]`d are filtered out later, inside the run loop, and counted into `ignored` - so `ran = runnable_count - ignored` can be 0 while `failed == 0`, and the function prints a success line and returns `Ok(true)`.

L1's scenario: profile `serial` with `only = ["serial::"]`, no `include_ignored`; someone marks the last two `serial::` tests `#[ignore]` while debugging. `brokkr check` reports the sweep passed having executed zero test processes.

L5's scenario: a sweep with `isolation = "process"`, no `include_ignored`, selecting a test binary whose tests are all `#[ignore]`d (a hardware/network lane). Output: `[test] <label>: 0 tests process-isolated passed, 12 ignored`, sweep succeeds, `check passed`, exit 0, `--json` says `verdict: "passed"`. Nothing was executed.

Both note every sibling path guards exactly this: `zero_test_run` (`output.rs:648-654`) counts `ignored` toward `total` precisely so this shape cannot read as green; `ran_any` (`phase.rs:1826`); `results.is_empty()` (`phase.rs:1179`).

## S3-04 - The `ignored` set is global across binaries, so one binary's `#[ignore]` suppresses another binary's live test

**Reported by:** L1 (#5, CONFIRMED)
**Files:** `src/check_cmd/isolate.rs:239`, `:260-263`, loop at `:68`

`ignored` is a single `BTreeSet<String>` keyed on bare test name, while `names` is deduped across binaries by the same key. If package A's lib has `util::tests::roundtrip` marked `#[ignore]` and package B's lib has a live `util::tests::roundtrip`, the name lands in `ignored` from A and the loop skips it entirely - B's real test never runs and is counted as ignored.

Requires a duplicated test path across two binaries, which the module's own dedup comment says is expected to happen.

## S3-05 - A sweep-declared `LD_LIBRARY_PATH` is silently discarded during listing

**Reported by:** L1 (#8, CONFIRMED on ordering / PLAUSIBLE that a real project does this), CX (#6, P2)
**Files:** `src/check_cmd/binaries.rs:149`, `:182-183`

`loader_path` builds its tail from `std::env::var("LD_LIBRARY_PATH")` - brokkr's own environment - then the pair is pushed *after* `env_refs`, and `run_captured_with_env` applies pairs in order via `Command::env`, so last wins. A `[[check]] env = { LD_LIBRARY_PATH = "/opt/foo/lib" }` applies to the cargo build and to the cargo-mediated test run but not to the direct `--list` exec, so enumeration fails to load a shared object the tests link against and the sweep dies at `binary_list` with a loader error.

CX frames it as: native-library test binaries can run normally through Cargo but fail during isolation/coverage enumeration.

## S3-06 - Isolated target dirs grow without bound and no clean path reaches them

**Reported by:** L1 (#6, CONFIRMED)
**Files:** `src/check_cmd/output.rs:39-42`, `src/main_parts/commands.rs:406-408`

`isolated_target_dir` mints `<project_root>/target/rustflags-<fnv64>` per distinct flag *string*. Nothing ever removes these: `brokkr clean --cargo PKG` runs `cargo clean -p PKG` in the default target dir only, and the `--worktrees` deep clean does not know about them either. Editing `rustflags = ["--cfg","madsim"]` to add one flag orphans a multi-GB tree permanently, and the documented fix for stale-incremental linker failures (CLAUDE.md, `clean --cargo`) cannot clear a sim sweep's cache at all.

See also S3-20, which reports the same function ignoring cargo's resolved `target_directory`.

## S3-07 - Build-shape dedupe runs before `--sweep` selection, making documented lane-qualified labels unreachable

**Reported by:** L1 (#7, CONFIRMED)
**Files:** `src/test_cmd.rs:509-510`, `:120`

`decide_sweeps` retains one sweep per `build_shape_key` and only *then* does `run` call `select_sweep`. With `lanes = ["tier1","serial"]` over one `[[check]]` entry, only `tier1/all` survives, so `brokkr test foo --sweep serial/all` errors with "matches no sweep in the resolved profile; available: tier1/all" - while `docs/commands/check.md:545` says "`--sweep` labels under a lanes profile are the lane-qualified form".

L1: the doc and the code disagree; I'd call the code wrong (dedupe should happen after selection, or the label check should tolerate any lane whose shape survived).

## S3-08 - `brokkr test` dedupes sweeps with different package exclusions

**Reported by:** CX (#4, P2)
**Files:** `src/test_cmd.rs:504`

The dedupe uses a build-shape key that deliberately omits `test_exclude_packages`. If the retained sweep excludes the requested package while a discarded sweep permits it, `brokkr test -p PKG NAME` incorrectly reports no match; behavior depends on declaration order.

Same function as S3-07, distinct defect.

## S3-09 - `plan_runnable`'s flag is named and documented `raw` but the caller passes `all`

**Reported by:** L1 (minor), L5 (#7, CONFIRMED)
**Files:** `src/check_cmd/isolate.rs:151` vs `:60`

Signature is `fn plan_runnable(plan, label, raw: bool)` with the doc "Under `--raw` the plan is announced before the run"; the sole caller passes `all`. Behaviour matches the surrounding `--all`-restores-the-roll-call design (`:71`, `:353-358`) and `check.md:111-113`, so the *call* is right and the name/doc are stale - but as written, a future edit that trusts the name will introduce a real bug.

## S3-10 - Under `--raw` the isolated path prints nothing for a passing test

**Reported by:** L1 (minor)
**Files:** `src/check_cmd/isolate.rs:349-359` vs `src/check_cmd/output.rs:591-597`

`run_one_test_sweep` prints the full stdout/stderr for a passing test under `--raw`; the isolated path prints nothing. `--raw` is documented as "disable all filtering"; on a process-isolated sweep it disables nothing on the success path.

## S3-11 - The worktree's toolchain pin is live during `cargo metadata`

**Reported by:** L1 (#3, CONFIRMED on the ordering / PLAUSIBLE on how loudly rustup fails)
**Files:** `src/context.rs:290-297`, `:192`, `:196`, `src/lockfile.rs:229`

The old code called `DisabledToolchain::activate(&wt.path)` *before* invoking `f`, so everything in the closure ran with the pin moved aside. The new code only `arm`s the dir; activation moved into `lockfile::acquire`. But the closure's first act is `BenchContext::with_build_config` → `resolve_bootstrap_paths` → `build::project_info` → `cargo metadata`, and the lock is not taken until `context.rs:196`.

Failure scenario: `disable_toolchain = true`, `brokkr <bench-cmd> --commit <old-hash>` where that commit's tree contains `rust-toolchain.toml` pinning a channel that isn't installed - the exact case `disable_toolchain` exists for. `cargo metadata` runs in the worktree under rustup with the pin still in place: it either fails outright (offline / unavailable toolchain) or resolves `target_directory` under a different toolchain than the subsequent build uses. Pre-change, that call was covered.

---

# Coverage accounting and the gate

## S3-12 - The audit's universe is the profile's own sweep list, not every `[[check]]` entry

**Reported by:** L2 (#1, CONFIRMED)
**Files:** `src/check_cmd/coverage.rs:213-226`, `src/check_cmd/phase.rs:596-643`, `src/config_parts/parser.rs:1207-1237`, `:1136-1196`

`enumerate_shapes` groups only the `sweeps` it is handed, fed from `cmd_check`'s `audit_coverage(project_root, &active_sweeps, …)`; `active_sweeps` comes from `decide_active_sweeps`, which for a profile returns exactly `profile::resolve(...)` - the lanes' sweeps and nothing else.

TIERED-CHECK.md:527-535 states the rule explicitly and gives the reason:

> **The universe is every `[[check]]` entry, not the profile's own sweep list.** … If the universe were defined by the sweeps the profile's lanes happen to reference, deleting a sweep from a lane would shrink the universe and the audit with it … So: universe = all `[[check]]` entries x their enumerated tests. **An unconditional entry referenced by no lane of a `complete` profile is a resolve-time error.**

Neither half is implemented. `validate_complete_profile` checks only `extends` and the doctests category; `validate_lanes_profile` checks composition rules. Nothing anywhere cross-references `[[check]]` names against the certifying profile's lanes.

Concrete failure: a workspace with `[[check]]` entries `default`, `ffi`, `live`, `sim`, and

```toml
[test.profiles.gate]
certifies = "complete"
lanes = ["tier1", "serial"]     # both reference sweeps = ["default", "ffi"]
```

`live` and `sim` are never enumerated. The audit prints `coverage: 2 shapes, N pairs - N run, 0 orphaned`, the run prints `check complete`, exit 0 - while two entire build shapes' worth of tests were neither run nor accounted. Worse, it is silent under *edit*: removing `"live"` from a lane's `sweeps` list shrinks the universe by exactly the pairs it removes, so the audit stays at `0 orphaned` and the count drop is the only trace. That is problem 3 reproduced inside the fix for problem 3.

`docs/commands/check.md:199-215` documents the implemented behaviour, so the divergence is TIERED-CHECK.md vs code+check.md. L2: I think the code is wrong - the design's argument for the rule is sound and unrebutted (the "landed" note at TIERED-CHECK.md:493-507 records exactly one deviation, the `enforce` key, and this is not it).

## S3-13 - `brokkr check --gate -- …` narrows the real test run but not the audit

**Reported by:** L2 (#2, CONFIRMED), CX (#1, P1)
**Files:** `src/check_cmd/coverage.rs:271-298`, `src/check_cmd/output.rs:402-408`, `:457-459`, `src/cli/schema.rs:102`, `:159-160`, `src/check_cmd/phase.rs:80`

The audit builds each lane's ran-set from `sweep.name_filters` + `sweep.libtest_args` only. The actual invocation appends `cargo_extra` and `libtest_extra` from `split_extra_args(extra_args)`. `--gate` conflicts with `profile`/`features`/`no_default_features`/`package` - but not with the trailing `args`. (CX: `cmd_check` rejects `-p` for complete profiles but not `extra_args`.)

Concrete failure, in a config whose complete profile has no process-isolated lane:

```
brokkr check --gate -- -- --skip expensive_
```

libtest skips every matching test; the audit enumerates without that `--skip`, so those pairs land in `ran`. Output: `0 orphaned`, `check complete`, exit 0, `"verdict":"complete"` in the JSON. Same with the cargo-level form `brokkr check --gate -- --lib` (drops every integration binary from the run; `test_binaries` in coverage still enumerates them all via its own `--tests` selection).

Partial mitigation that does not close it: `run_isolated_sweep` (`isolate.rs:39-45`) hard-errors on non-empty `extra_args`, so a complete profile that happens to include an `isolation = "process"` lane fails instead. A complete profile composed only of ordinary lanes has no such guard.

L2's fix shape: either reject trailing args under a `complete` claim (same table as `-p`, TIERED-CHECK.md:296-302 - "The table governs flags, not just keys"), or fold `extra_args` into the enumeration argv.

## S3-14 - `test_exclude_packages` removes whole packages from the certified universe with no ledger entry

**Reported by:** L2 (#3, CONFIRMED on behaviour; design-judgement on whether it should fail)
**Files:** `src/check_cmd/coverage.rs:312-328`, `:143-157`; contrast `src/config_parts/parser.rs:1223-1235`

`shape_selection_args` emits `--workspace --exclude <pkg>`, so those binaries are never built or listed, and the exclusion is reported with `output::run_msg` - informational, never fatal, requiring no `[[quarantine]]` entry.

Contrast the sibling suppression channel: `doctests = false` under a complete profile is a **load-time error** unless a `[[quarantine]] category = "doctests"` entry exists, on the stated grounds that doctests are "invisible to the coverage enumeration". `test_exclude_packages` is invisible in exactly the same way and gets a log line instead.

Concrete failure: `test_exclude_packages = ["nautilus-pyo3", "nautilus-cli"]` on the gate's sweep. Everything in those two packages is outside `pairs`, so `pairs` / `run` / `orphaned` are all computed over a universe that silently shrank by two packages, and the run prints `check complete`. Adding a third name to that list is a one-word, uncounted coverage reduction inside the feature whose purpose is to make coverage reduction countable.

The behaviour is documented (check.md:213-215, TIERED-CHECK.md:504), so L2 calls this a **designed** hole rather than an implementation slip - but the largest remaining "report full coverage when tests never ran" surface, and the doctests precedent shows the ledger already has the right shape for it (a `category = "excluded_packages"` entry, or a per-package requirement).

## S3-15 - `scripts/smoke-certifies.sh`: the two coverage-failure scenarios pass vacuously

**Reported by:** L2 (#4, CONFIRMED)
**Files:** `scripts/smoke-certifies.sh:208-214`, `:205-206`

```sh
brokkr check --profile gate-orphan --json
expect "orphaned pair = exit 1" 1 $?
```

Exit 1 is brokkr's *universal* failure code: a config error, a gremlin hit, a clippy diagnostic, a build failure, a test failure and a stale-quarantine failure all produce it (`finish_check`'s `Err(_)` arm → `DevError::ExitCode(1)`). So the "unjustified skip fails coverage" assertion holds whenever the run fails *for any reason at all* - including for reasons that mean the coverage phase never ran. The same is true of the `gate-stale` assertion, and the two would not distinguish a build that fails to compile the generated crate from a working orphan detector.

The script requests `--json` on precisely these invocations and never reads a byte of it, even though the summary carries the discriminating fields: `"failed_phase":"coverage"` and `coverage.orphaned > 0` / the stale-entry path (`orphaned == 0` with `failed_phase == "coverage"`). Likewise `--gate = complete` asserts exit 0 but not that the audit ran - a build where the coverage phase silently no-ops passes that assertion.

Minor, same file: the header comment claims the script asserts "the 0/10/1 contract", which is accurate for the three verdict scenarios; it is the two *coverage* scenarios whose claim outruns what is asserted.

L2 explicitly cleared: no pipeline/exit-code-masking bug - `set -u` with `expect … $?` captures the right status, `git init -q; git add -A` gives the gremlin phase tracked files, and `project = "brokkr"` is valid.

## S3-16 - First-matching quarantine entry takes all the credit, so a narrower entry is reported stale

**Reported by:** L2 (#5, CONFIRMED)
**Files:** `src/check_cmd/coverage.rs:367-370`, `:159-175`

`quarantine.iter().position(...)` - first match wins - and any `pattern` entry with zero pairs fails the check.

Concrete failure: `[[quarantine]] pattern = "test_bar" issue = "B41"` declared before `[[quarantine]] pattern = "test_bar_roundtrip" issue = "B50"`. Every `test_bar_roundtrip` pair is credited to B41; B50 counts 0, is declared stale, and the gate fails with "delete the entries". Deleting B50 leaves those pairs permanently attributed to the wrong issue - and closing B41 then un-suppresses tests that still need B50's suppression.

This is the `test_quote_tick`/B50 finding recorded at TIERED-CHECK.md:96-100 as a *hand-found* defect; the classifier's ordering rule is what makes it mechanically unavoidable rather than merely possible. A more-specific-pattern-wins rule (longest match), or an error on overlapping entries, would make it detectable. Matches the existing unit test `first_matching_entry_gets_the_credit`, which pins the behaviour without pricing this consequence.

## S3-17 - The stale-entry check returns before the orphan worksheet

**Reported by:** L2 (#6, CONFIRMED)
**Files:** `src/check_cmd/coverage.rs:159-196`

A run with both stale entries and orphans never prints the orphan worksheet. Same exit code, but the worksheet is the phase's stated reason for running on unhealthy runs (TIERED-CHECK.md:46-53). Print both, then fail.

## S3-18 - The best-effort audit credits lanes that never executed

**Reported by:** L2 (#6, CONFIRMED)
**Files:** `src/check_cmd/coverage.rs:271-298` vs `src/check_cmd/phase.rs:1819-1821`

`run_test_phase` fails fast on the first failing sweep, so later lanes never execute, yet the audit credits every lane's ran-set. On a failed gate the printed `N run` overstates what actually ran. Cosmetic while the run still exits 1, but the JSON `coverage` object is explicitly sold as the failed run's worksheet.

---

# Config parsing, profiles, toolchain

## S3-19 - Profile `env` can override per-check `rustflags` and target isolation; the parse-time guard inspects only `[[check]].env`

**Reported by:** L3 (#1, CONFIRMED), CX (#3, P1)
**Files:** `src/config_parts/parser.rs:897-909`, `src/profile.rs:349-352`, `src/check_cmd/output.rs:660-672`

The guard is `if !entry.rustflags.is_empty() { for banned in ["RUSTFLAGS","CARGO_TARGET_DIR"] { if entry.env.contains_key(banned) … } }` - it only looks at the `[[check]]` entry. But `build_resolved_sweep` merges the *profile's* `env` into the same `sweep.env` map, and `merged_env` puts `sweep_env` first and only appends a `project_env` key when it is **absent**. `sweep_runtime_env`'s `CARGO_TARGET_DIR`/`RUSTFLAGS` live in `project_env`, so the profile's value wins. CX adds `CARGO_ENCODED_RUSTFLAGS` to the missed set.

Failure scenario:

```toml
[[check]]
name = "sim"
rustflags = ["--cfg", "madsim"]      # accepted: entry.env is empty

[test.profiles.sim]
sweeps = ["sim"]
env = { CARGO_TARGET_DIR = "target/mine" }
```

Load succeeds. The sweep's isolated dir `target/rustflags-<hash>` is computed, put in `project_env`, then dropped by `merged_env` because `sweep.env` already has the key. Cargo builds into `target/mine`, while `BROKKR_TEST_BIN_DIR` (also from `sweep_runtime_env`, and *not* overridden since it isn't a key the profile set) still points at `<meta_target>/debug`. So `build_packages` artefacts are written to one tree and `tests/cli_*.rs` is told to look in another - the exact silent-wrong-binary shape `BROKKR_TEST_BIN_DIR` exists to prevent. Same for `RUSTFLAGS`: the profile's value replaces the composed `--cfg madsim`, so the "sim gate" compiles without its cfg and reports green.

The same hole exists one level down for a plain `[[check]]` entry: `env = { CARGO_TARGET_DIR = "target/foo" }` with **no** `rustflags` is accepted (the guard is conditioned on `rustflags`), and again `BROKKR_TEST_BIN_DIR` is computed from `meta_target_dir`. L3: the guard is conditioned on the wrong thing - the hazard is "someone set CARGO_TARGET_DIR by hand", not "someone set it *and* rustflags".

CX adds the cross-phase consequence: tests can use different flags/target directories from clippy and coverage.

## S3-20 - `rustflags` target-dir isolation hardcodes `<project_root>/target`, ignoring cargo's resolved `target_directory`

**Reported by:** L3 (#2, CONFIRMED)
**Files:** `src/check_cmd/output.rs:39-42`, `:98`

```rust
fn isolated_target_dir(sweep: &ResolvedSweep, project_root: &Path) -> Option<PathBuf> {
    crate::config::rustflags_target_key(&sweep.rustflags)
        .map(|key| project_root.join("target").join(format!("rustflags-{key}")))
}
```

`build_test_env` two functions above carries an explicit comment that "workspaces can place [the target dir] outside the project root, so the caller passes it in rather than us assuming `<project_root>/target`" - and `sweep_runtime_env` *has* `meta_target_dir` in hand at line 98. `isolated_target_dir` ignores it.

Failure scenario: a repo with `.cargo/config.toml` containing `[build] target-dir = "/media/folk/Banan/cargo"` (this exact layout appears in the repo's own test fixture, `src/test_cmd.rs:1100`). Plain sweeps build into `/media/folk/Banan/cargo`; the moment a `[[check]]` entry gains `rustflags`, that sweep silently starts writing a fresh full build tree into `<repo>/target/rustflags-<hash>` - wrong drive, wrong tree, invisible in the collapsed log beyond a `(isolated target)` tag that doesn't say *where*. It also means `brokkr clean --cargo` never reclaims it.

L3: fix is one line - thread `meta_target_dir` into `isolated_target_dir` and join the key onto it. See also S3-06.

## S3-21 - `brokkr deps` is an unpatched `disable_toolchain` call site

**Reported by:** L3 (#3; CONFIRMED for `deps`, PLAUSIBLE for `compare-tiles`)
**Files:** `src/main_parts/bootstrap.rs:253-283`, `:221`, `src/toolchain.rs:33-40`, `src/deps/mod.rs:254-255`

Since the serialisation rework, activation is driven entirely by `lockfile::acquire` ("Commands that never take the lock never touch the file"). The `Deps` arm calls `toolchain::arm(...)` and then `deps::run` **without acquiring the lock** - so nothing is ever moved aside. `deps::run` immediately calls `load_metadata` / `load_metadata_host_filtered` (`cargo metadata`) and `ccu::run` (itself cargo/rustup-mediated).

Failure scenario: the documented `disable_toolchain` use case - a foreign checkout pinned to `rust-toolchain.toml { channel = "1.xx" }` you don't have installed, driven via the one-level-up `brokkr.toml`. `brokkr check` works (takes the lock, pin disabled). `brokkr deps` in the same tree fails with rustup's `toolchain '1.xx-x86_64-unknown-linux-gnu' is not installed`. Two prior commits patched `fmt` and `--commit` worktrees individually; this is the same class, still open.

`Command::CompareTiles` (`main_parts/bootstrap.rs:1046`) also runs unlocked against `build_root` - PLAUSIBLE as a fourth site; L3 did not read `elivagar::cmd::compare_tiles` to confirm it builds.

## S3-22 - `fmt` races locked toolchain suppression

**Reported by:** CX (#5, P2)
**Files:** `src/main_parts/bootstrap.rs:195`

The formatter activates `DisabledToolchain` without taking the global lock. If another command already moved the toolchain file aside, `fmt` adopts that sidecar and restores it while the locked command is still running, potentially switching toolchains mid-command and breaking the original guard's restoration.

## S3-23 - `skip_phases = ["coverage"]` parses, validates, and is a guaranteed no-op - twice over

**Reported by:** L3 (#4, CONFIRMED), L2 (#6, CONFIRMED)
**Files:** `src/config_parts/schema.rs:808-819`, `:874-877`, `src/config_parts/parser.rs:1070-1087`, `src/check_cmd/phase.rs:152`

`PHASE_NAMES` is doing double duty: it is both the `failed_phase` universe for the `--json` trailer (where `coverage` belongs) and the validation universe for `skip_phases` (where it does not). Two independent reasons it can never do anything:

- `skip_phases` requires `certifies = "partial"`, while the coverage phase only runs under `certifies = "complete"`;
- the coverage phase is not gated on `skip(...)` at all - `phase.rs:152` is a bare `if certifies == Some(Certifies::Complete)`, unlike every other phase.

Failure scenario:

```toml
[test.profiles.edit]
certifies = "partial"
skip_phases = ["clippy", "coverage"]
sweeps = ["all"]
```

Loads clean. brokkr prints `skipping phases: clippy, coverage (certifies partial)` - announcing the omission of a phase that was never going to run. Under the "reject silently-broken config at load" principle this should be rejected, and the schema doc comment on `skip_phases` already lists the correct nine names *without* `coverage`. L3: the doc comment is right and `PHASE_NAMES` is the wrong list to validate against - split it, or exclude `"coverage"` in the parser loop.

## S3-24 - A `[textlint_preset.<name>]` block that no rule references is silently accepted

**Reported by:** L3 (#6)
**Files:** `src/config_parts/parser.rs:198-232`

Field names inside a preset are validated (and tested), and a rule naming a missing preset is rejected. But the reverse - a preset nothing uses - is dead config that loads clean. Rename a rule's `preset = "dst-scope"` to `preset = "dst_scope"` and you get a loud error; delete the *rule* and `[textlint_preset.dst-scope]` sits there forever looking load-bearing. Given the commit principle this is the one remaining silent hole in the preset surface. Low severity, cheap to close (`out` is already keyed by name; collect the referenced set in `parse_textlint` and diff).

## S3-25 - Multi-preset list merging is reverse-declaration order, and the test enshrines it

**Reported by:** L3 (#7)
**Files:** `src/config_parts/parser.rs:237-254`, `src/config_parts/tests.rs` (`textlint_multiple_presets_apply_nearest_first`)

`apply_textlint_preset` is applied once per preset in declaration order, and each application prepends the preset's list to whatever the rule table currently holds. With `preset = ["a", "b"]` and neither preset's list overlapping, scalars resolve to **a** (first-listed wins, as documented) but lists come out `["b/**", "a/**"]` - second-listed first. The test asserts exactly that, so a future "fix" to make list order match scalar precedence would look like a regression.

`docs/brokkr.toml.md:271-276` says lists concatenate "preset entries first" and that multiple presets are "applied left to right … Lists take entries from all of them" - it never pins the *inter-preset* order, so this isn't strictly a doc violation. But the test's name ("apply nearest first") describes scalar precedence while its list assertion documents the opposite ordering. Either make list order follow declaration order, or say so in the doc. Order is usually immaterial for `paths`/`exclude` globsets; it is not for `except`, where an earlier regex can shadow reporting.

## S3-26 - `docs/brokkr.toml.md` states the opposite of the code on worktree toolchain pins

**Reported by:** L3 (#5 - the doc is wrong)
**Files:** `docs/brokkr.toml.md:83`, `:70-72`; `src/context.rs:286-297`, `src/toolchain.rs:25-29`, `:33-40`

Doc: *"Worktree builds (`--commit`) are a separate checkout and keep their own pin."* The code says the reverse, deliberately and with a rationale: `context.rs` re-arms the disable dir at the worktree path precisely so the commit's own committed pin is **not** honoured there.

Related, same section: the doc says the file is moved aside "for the duration of every command". Since the serialisation rework it is moved aside only for the **locked window**, and commands that take no lock never touch it - which is exactly how S3-21 became possible. The doc should say "for the duration of the global lock", so the `deps`-shaped gap is visible from the docs rather than hidden by them.

---

# CLI surface and `man`

## S3-27 - `pmtiles-stats` is hidden in projects where it works; `visibility.rs` has become a de-facto gate

**Reported by:** L4 (#1, CONFIRMED)
**Files:** `src/cli/visibility.rs:118-121`, `src/main_parts/bootstrap.rs:1168`, `src/main_parts/commands.rs:594-599`

`TABLE` has `("pmtiles-stats", Visibility::Only(&[Project::Elivagar, Project::Nidhogg]))`, but `cmd_pmtiles_stats` takes no `Project` and calls no `project::require`. The table claims a two-project scope that no handler enforces, so the presentation layer is the only thing scoping the command - the inversion the module header forbids.

Failure scenario: in a pbfhogg (or piners, or `Other(_)`) checkout, `brokkr --help` omits `pmtiles-stats` entirely, yet `brokkr pmtiles-stats data/foo.pmtiles` runs and prints stats successfully - functional but undiscoverable. Conversely, if the table's claim is the intent, the missing `project::require` means the documented restriction (CLAUDE.md: "`pmtiles-stats` - **[elivagar, nidhogg]**") is unenforced. Either way the two disagree. Commit `2d73e19` changed only the table.

L4 cross-checked every other `TABLE` row against the `project::require` call sites and the inline `match project { … other => Err(…) }` arms in `bootstrap.rs` (`test`, `visual`, `list`, `approve`, `report`, `visual-status`, `hotpath`, the ratatoskr/piners block, `suite`, `verify`, the elivagar/nidhogg/pbfhogg dispatchers, `prepare`/`html-extract`/`outline` via `litehtml/cmd.rs:773/853/918`) - those all agree. `results`/`sidecar`/`invalidate` are ungated but `Except(Litehtml)`; deliberate and documented, not counted.

## S3-28 - Bold/italic/strikethrough inside a GFM table cell leaks its markers outside the table

**Reported by:** L4 (#2, CONFIRMED)
**Files:** `src/man/render.rs:410-429`, `:559-563`, `:490-500`; contrast the `in_table_cell` branches at `:168`, `:178`, `:188`, `:215`, `:246`, `:254`

Every other inline event checks `self.in_table_cell` and routes to `push_table_text` (which appends to `current_cell`), but the inline-markup arms (`Tag::Subscript`/`Superscript`/`Emphasis`/`Strong`/`Strikethrough`) unconditionally write to `out`. Since a table is buffered and only emitted at `TagEnd::Table`, the markers are flushed into the output stream *before* the table body.

Failure scenario: `brokkr man lint-corpus`. `docs/commands/lint-corpus.md:21-23` has `| **piners** | …`, `| **pine-lint** | …`, `| **TV** … |`. `Tag::Strong` pushes `**` to `out`, the cell text goes into `current_cell`, `TagEnd::Strong` pushes another `**`. Because `TagEnd::TableRow` sets `end_newline = true`, the next row's `Tag::TableRow` suppresses its newline, so all six markers concatenate onto one line. Output is a stray line reading `************` immediately above the table, and the three cells render as plain unemphasised text. Same defect fires in `brokkr man measure` (`docs/commands/measure.md:207-208`), `brokkr man output-channels` (`:72,74,75`) and `brokkr man check` (`:240`).

## S3-29 - `brokkr man` dies on a malformed `brokkr.toml`, contradicting its own fail-open contract

**Reported by:** L4 (#3, CONFIRMED)
**Files:** `src/main_parts/bootstrap.rs:217`, `:234`, `:239-242`, `src/project.rs:145-158`

`let disable_dir = match project::detect_optional()? {` executes *before* the `Command::Man` arm. The Man arm defensively uses `.ok().flatten().map_or(Project::Other(""), …)`, and its comment promises "Reading the docs must work in a tree brokkr knows nothing about" - but `detect_optional` swallows *only* file-not-found; a parse failure from `config::load` is an `Err`, and the `?` at line 217 propagates it. So the Man arm's `.ok()` guard is dead for the exact case it exists to cover.

Failure scenario: introduce a typo in `brokkr.toml` (say an unclosed string). `brokkr --help` still lists `man` (`parse_cli()` at `:81-84` fails open). `brokkr man config` - the topic that documents the very file you broke - exits 1 with `TOML parse error …` instead of rendering the doc. Same for `brokkr man check`, and identically for `wc`/`deps`, though only `man` advertises the guarantee.

## S3-30 - Inline code and header styling are lost inside tables

**Reported by:** L4 (#4, CONFIRMED - mis-render, not a panic)
**Files:** `src/man/render.rs:177-186`, `:589-590`, `:308-311`, `:75`, `:90`, `:628-672`

`Event::Code` in a cell calls `push_table_text(&text)` with no backticks and no `theme.code`, while the non-table path emits `` ` `` + styled text + `` ` ``. Separately, the `in_table_head` branch of `push_text` is unreachable: header cells set `in_table_cell = true`, so all header text goes through `push_table_text`. `Theme::table_header` is therefore dead, and `render_table` applies no styling at all.

Failure scenario: `brokkr man measure` - the tables in `docs/commands/measure.md` are dense with `` `--markers` ``, `` `--durations` `` etc.; these render as bare words, visually indistinguishable from prose, while the same tokens outside a table are yellow and backticked. Table headers render unbolded despite a theme entry for them.

## S3-31 - `man` listing header degenerates when no project is detected

**Reported by:** L4 (#5, CONFIRMED - cosmetic)
**Files:** `src/man.rs:190-193`, fallback set at `src/main_parts/bootstrap.rs:242`

`brokkr man` in a directory with no `brokkr.toml` prints ``Bundled docs for . Run `brokkr man <topic>` to read one.`` - a stray space before the period.

---

# Output rendering, JSON trailer, convention engines

L5's headline on the highest-risk item: **no path was found where the quiet-gate rework suppresses or truncates a real failure.** Every collapsed-log change is `if commands { full } else { shape }` on the *announcement* line only; no diagnostic rendering was removed, `--limit` still caps only unscoped hits (`scope::partition` puts scoped hits ahead of the cap unconditionally), and the failure paths now print strictly *more* (`failing command: cargo …`).

## S3-32 - `skip_after` is a silent no-op for `join_wrapped_use` textlint rules

**Reported by:** L5 (#2, CONFIRMED)
**Files:** `src/textlint.rs:284-292`, `:350-394`

In the per-line loop the join-rule `continue` at `:284` sits *above* the latch-arming at `:290`, so `skipping[ri]` is never set for a join rule - and `scan_use_statements` is not passed `skipping` at all and never consults it. `region`, `table_row_only` and `in_toml_section` are likewise silently dropped for join rules.

Scenario:

```toml
[[textlint]]
name = "no-tracing-warn-import"
pattern = "use tracing::.*warn"
paths = ["**/*.rs"]
join_wrapped_use = true
skip_after = "^#\\[cfg\\(test\\)\\]"
```

A file with `use tracing::warn;` inside `#[cfg(test)] mod tests { … }` is still flagged, despite the configured escape hatch - the author's only remedy is an `allow_marker`, which *is* wired through the join pass. This is precisely the "a predicate that does not bound what it claims" class, and the same commit added a compile-time guard for the sibling footgun (`except` + `join_wrapped_use`), so the class is clearly in scope. Either wire `skipping`/`region` into `scan_use_statements`, or reject the combination at compile time the way `except` now is.

## S3-33 - `--json` `sweeps` over-reports: it lists sweeps that never ran

**Reported by:** L5 (#3, CONFIRMED)
**Files:** `src/check_cmd/phase.rs:495` (field), `:525` (population), `:1095-1101`, `:1103-1109`, `:1779-1782`

`emit_json_summary` fills `sweeps` from `active_sweeps` - the *selected* set, before the clippy phase's `cli_package_scope` skips and build-shape dedupe, and before the test phase's skips.

Scenario: profile `tier1` with sweeps `default, ffi, live`; `brokkr check -p nautilus-core --json` where `ffi` and `live` both declare `packages = ["nautilus-adapters"]`. Both are skipped with a log line; only `default` runs. The trailer still reports `"sweeps":["default","ffi","live"]` alongside `"verdict":"passed"`. A CI consumer reads that as "all three sweeps green". This is the same over-claim the `package` field was added to prevent (`phase.rs:496-498`, doc `check.md:153`) - the human log is honest here and the machine trailer is not.

## S3-34 - `[style]`: a single-pattern match arm ending in `=>` on its own line is still flagged

**Reported by:** L5 (#5, CONFIRMED on the code path; frequency of the shape is the uncertainty)
**Files:** `src/style.rs:325-348` (`is_match_guard`), `:280-303`

The `=>` fix is complete for the *guard* shape it targets (`opens_match_guard` correctly reads the masked view; L5 could not construct a masked-`=>`-in-a-string or a `{`/`;`-before-`=>` escape, and the `{`-before-`=>` ordering is right). But the older `is_match_guard` exemption still requires a literal `|` between two alphanumerics, so it only recognises *or*-patterns. An arm with a single pattern that rustfmt leaves on its own line is not exempt:

```rust
match v {
    A =>
        match w {   // flagged: "missing blank line above `match`"
            ...
        }
}
```

Trace: `prev_end = "A =>"` → not `{`, not a comment, `ends_with_assign` false (last char `>`), `ends_with_any(&[',','(','|'])` false, `is_match_guard` finds no `|` → falls through to the shared-identifier check, which a bare pattern shares nothing with. Identical for `if`. A cheap fix is to treat any `prev_end.ends_with("=>")` as arm-position for the `if`/`match` arms.

## S3-35 - `describe_sweep` renders `--test <name>` filters as two comma-separated fragments

**Reported by:** L5 (#6, CONFIRMED - low severity)
**Files:** `src/check_cmd/output.rs:218-220`, `src/profile.rs:125-129`

`cargo_test_filters` is stored flattened as `["--test", "cli_sort"]` and the loop pushes each element as its own `parts` entry. A sweep with one test filter prints `test tier1: workspace, --test, cli_sort, serial` - reads as two items, one of them a bare flag. `docs/commands/check.md:283` promises "any `--test <name>` filters". Join the pairs (or push `format!("--test {name}")`).

## S3-36 - `docs/commands/check.md` still documents the removed NDJSON per-event mode

**Reported by:** L5 (#4, CONFIRMED - the doc is wrong)
**Files:** `docs/commands/check.md:361`, `:367-368`, `:395-396`, `:408`, `:443-444`; contrast `:176-178`

Five phase paragraphs still say "JSON mode emits `style` and `style_summary` events", "`header`/`header_summary`", "`textlint`/`textlint_summary`", "`manifest`/`manifest_summary`", "`dependency_violation` and `dependency_summary` events". The same file at `:176-178` says the NDJSON per-event mode is gone and `--json` is a single summary trailer, and there is no per-event emission anywhere in `phase.rs`. The file contradicts itself - a consumer wiring up `--json` against those phase paragraphs will get nothing. Delete the five sentences.

---

# Noted in passing (raised as observations, not findings)

- **L4** - `visible_in` takes the first `TABLE` match and nothing would catch a second, contradictory row. The two TABLE-honesty tests do not cover a *duplicate* name. Low risk; no duplicate exists today.
- **L4** - `Tag::BlockQuote` (`render.rs:312-347`) only emits the `|` bar on the *first* line, so a multi-paragraph quote would lose the bar on continuation lines. Nothing in the bundled corpus triggers it. Alert (`> [!NOTE]`) and footnote (`[^n]`) paths are likewise unexercised - no such constructs exist in `docs/**`.
- **L5** - textlint's four context-window gates OR across gates (documented at `check.md:391`), which means two `require_*` gates on one rule are satisfied by *either*, not both.
- **L2** - `PHASE_NAMES` doing double duty is the root of S3-23; the `failed_phase` universe and the `skip_phases` universe are not the same set.

---

# Cleared by review

Recorded so a later sweep does not re-tread them. Each was checked by the named reviewer and found sound.

**L1** - test-name escaping into `--exact` (names are separate argv elements, never shell-quoted); prefix-name collisions (`--exact` is exact, `name_filters` correctly not re-passed at execution); signal-killed children (`status.success()` false → `IsolatedOutcome::Failed`); per-child exit aggregation (all tests still run after a failure, `Err` only on genuine spawn failure); `package_name_from_id` across all three cargo id formats; `parse_list_output`'s benchmark/summary-line rejection; the shadowed-borrow in `enumerate_isolated` (shadowing does not drop); `LockGuard::drop` ordering (toolchain restored before `LOCK_UN`); `lockfile::acquire` error paths (the `OwnedFd` is constructed before `activate_for_lock()?`, so an activation failure releases the flock rather than leaking it).

**L2** - the ignored-set derivation (`--list --ignored` for the universe, `!inc && ignored.contains(pair)` subtraction per lane) matches `isolate.rs`'s execution-time routing; package-qualified skips are subtracted from the ran-set in both the audit and the isolated runner, with the collision guard erroring rather than half-obeying; a quarantine entry naming a nonexistent package is caught by the stale check; package-scoped entries do not absorb other packages (`is_none_or` on `package`); `skip_phases = ["test"]` cannot coexist with `certifies = "complete"` (`parser.rs:1070-1078`); the `CoverageOutcome` stats/verdict split delivers counts on both failing audit paths and `None` is genuinely reserved for pre-classification enumeration failure - **that fix is complete**; shape grouping cannot double-count (one `ShapeCoverage` per `build_shape_key`).

**L3** - `--gate` cannot degrade to an ad-hoc run (`cli/schema.rs:102` `conflicts_with_all`); typo'd profile/`[test]` field names error at load (`deny_unknown_fields` on `TestConfig` and `ProfileDef`); `brokkr test` does call `sweep_runtime_env`, so a sim sweep gets its cfg and isolated dir (modulo S3-19); `parse_check`'s reworked duplicate-name guard is behaviour-preserving; `SkipSpec` untagged + `QualifiedSkip` `deny_unknown_fields` does fail deserialization on a typo'd key (poor message, but loud); `resolve_single`'s qualified-skip/isolation guard is applied per lane.

**L4** - the two TABLE-honesty tests are **not** vacuous (clap's derived `command()` returns an unbuilt `Command`, so `get_subcommands()` yields exactly the 87 derived variants and not the auto-injected `help`; every `#[command(name = …)]` override in `schema.rs` was hand-verified against `TABLE`, no name drift); `parse_cli` fail-open behaviour on absent/malformed/unknown-project config; hiding does not disable (hidden subcommands still parse, `brokkr help <hidden>` and `<hidden> --help` still resolve, `from_arg_matches` vs `from_arg_matches_mut` is benign, the `e.exit()` branch unreachable for user input); `Project::Saehrimnir`'s absence from `TABLE` is consistent; `render.rs` panic surface is clean (no byte-index slicing - `strip_ansi` and `pad` iterate `chars()`; `render_table` sizes `widths` from every row before indexing, so ragged rows and an empty header are safe; `usize::try_from(start).unwrap_or(1)`; no recursion; no `unwrap`/`expect` on user data); `TOPICS` covers all 21 `docs/` files exactly once with no orphans either direction; topic lookup is exact-match only, the unknown-topic error points at `brokkr man`, and a wrong-project topic reports itself rather than vanishing.

**L5** - `[[script_check]]` has no vacuous-pass hole (empty `expect` rejected at parse time, `parser.rs:386-389`; empty output cannot match any of the three modes; `sh -c` makes a missing/non-executable script an exit-127-with-stderr failure; the ignored exit code is deliberate and documented - a stubbed `exit 0` still fails for want of the sentinel); `--json` trailer validity (every prior line goes to stdout via `println!`, so the trailer always starts on a fresh line; serialization failure is caught, not panicked; `schema: 1` consistent with the doc; verdict/exit-code agree on all four arms - passed/complete → 0, partial → 10, failed → 1; the missing-trailer case for resolve-time config errors is documented at `check.md:188-190` as intended); counts (`total` computed pre-partition, displayed with the post-partition list plus a trailer in every native phase - no off-by-one; the trailer/`+N more` arithmetic is right in `run_dependency_rules` and `emit_timings`); clippy build-shape dedupe (`BuildShapeKey`, `profile.rs:92-113`) includes packages, features, rustflags, env and build_packages, so it cannot dedupe away a sweep whose clippy invocation or environment actually differs; textlint's `require_*`/`except_*` symmetry is correct by construction.
