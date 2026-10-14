# Hygiene - validation pipeline

Hygiene findings from the hunt for the `check`/`test`/`clippy` pipeline, test execution, the convention engines (gremlins, header, textlint, script_check, manifest, dependency rules) and `brokkr deps`. Siblings: `hygiene-platform.md` (PLT, cross-cutting themes), `hygiene-measurement.md` (MEA), `hygiene-projects.md` (PRJ). Entries record what hunters reported; nothing has been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## VAL-011 - Check/test tunables have no index and no injection point

Reported by: check-core, test-execution, config-cli-bootstrap.

Residue: `PHASE_CLOCK` poison handling, CPU-topology and run-log-retention tunables are untouched (only accounting retention got an injection point).

## VAL-016 - Check run-state policy is per call site

Reported by: check-core.

Residue: the silent `PHASE_CLOCK` poison path in `watchdog.rs` and wall-clock run-log pruning in `run_log.rs` remain.

## VAL-021 - Check/test tests that cannot fail

Reported by: check-core, test-execution.

Residue: the clippy-order and pinned-unification tests in `test_execution_tests.rs` and the other weak tests are unchanged.

## VAL-022 - Tests exercise compositions production doesn't use

Reported by: check-core.

Residue: the merge test in `test_execution_tests.rs` still feeds stderr text through `cargo_filter::parse_clippy` instead of cargo JSON.

## VAL-025 - Stale claims in check/test code and `check.md`

Reported by: check-core, test-execution.

Residue: the `schema.rs` comments are fixed, but the stale test-phase comments the residue mentions were not identified or changed.

## VAL-026 - Misattached doc comments and stale naming in check core

Reported by: check-core.

Residue: the `whole_workspace` residue and the misattached comments in `test_execution.rs` remain.

## VAL-027 - Check/test messages name the wrong thing or say nothing

Reported by: check-core, test-execution.

Residue: none of the listed messages (direct-binary timeout, idle-kill, nextest zero-work, watchdog spawn, `PHASE_CLOCK` poison, run-log open, `test_cmd` output path) was changed.

## VAL-028 - Dead or unreachable code in the check/test pipeline

Reported by: check-core, test-execution.

Residue: `resolve_planned_sweep` is infallible and its callers are migrated, but the "other listed residues in S- and I-owned files" were not identified or verified.

## VAL-038 - Convention-engine and deps errors misclassified

Reported by: convention-engines.

Residue: callers outside the shared conversion still label cargo metadata errors as `Build`: `dependency_rules.rs`, `check_cmd/features.rs`, `check_cmd/nextest_lane.rs`, `check_cmd/prepare.rs`, `check_cmd/direct_runtime.rs`.

## VAL-040 - Convention-engine guards that fail open

Reported by: convention-engines.

Residue: dead-scope checks can still be bypassed by skipped phases or the prose-only shortcut.
