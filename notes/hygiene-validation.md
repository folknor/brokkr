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

## VAL-001 - The rustflags env resolution rule has three copies that already disagree

Reported by: check-core.

Residue: `check_cmd/sweep_env.rs` still reads the rustflags environment directly instead of through `RustflagsEnv`.

## VAL-003 - Failure sentinels and tool labels are strings matched by content

Reported by: check-core, test-execution.

Residue: the test phase still signals failure through the `TESTS_FAILED` string sentinel, `test_cmd/plan.rs` relabels clippy output with `replacen`, and the legacy `filter_clippy` string wrapper remains.

## VAL-009 - Two owners of `-Zfeature-unification` support; package-mode builds in two target dirs

Reported by: test-execution.

Residue: install-feature checks and missing-bin probes still do not run in the isolated package-unification target dir that package-mode test sweeps use.

## VAL-011 - Check/test tunables have no index and no injection point

Reported by: check-core, test-execution, config-cli-bootstrap.

Residue: `PHASE_CLOCK` poison handling, CPU-topology and run-log-retention parts remain.

## VAL-015 - The same condition is handled differently across test lanes

Reported by: test-execution.

Residue: `prepare.rs::direct_preflight` still refuses all forwarded args for isolated lanes, and complete-profile admission does not require a doctest carrier for process-isolated lanes.

## VAL-016 - Check run-state policy is per call site

Reported by: check-core.

Residue: the silent `PHASE_CLOCK` poison path and wall-clock run-log pruning remain.

## VAL-018 - Test lanes read cargo config late and fail open

Reported by: test-execution.

Residue: `rustflags.rs::CargoLaunchConfig::discover` still skips read/parse failures, and no validated cargo-config snapshot is acquired before builds and carried into lane setup.

## VAL-019 - Fixed shared files rewritten non-atomically every run

Reported by: test-execution, process-control.

Residue: the user-wide nextest env file is now replaced atomically, but one fixed path is still shared by capability-less writers that are not serialized by the lock.

## VAL-021 - Check/test tests that cannot fail

Reported by: check-core, test-execution.

Residue: the clippy-order and pinned-unification tests in `test_execution_tests.rs` and the other listed weak tests outside A's files remain.

## VAL-022 - Tests exercise compositions production doesn't use

Reported by: check-core.

Residue: the merge test still feeds the legacy text parser instead of cargo JSON.

## VAL-025 - Stale claims in check/test code and `check.md`

Reported by: check-core, test-execution.

Residue: stale `CheckEntry.harness` and `Certifies` comments in `config_parts/schema.rs` and some test-phase comments remain.

## VAL-026 - Misattached doc comments and stale naming in check core

Reported by: check-core.

Residue: the `whole_workspace` residue and misattached comments in `test_execution.rs` and `sweep_env.rs` remain.

## VAL-027 - Check/test messages name the wrong thing or say nothing

Reported by: check-core, test-execution.

Residue: direct-binary timeout, idle-kill and nextest zero-work wording, watchdog spawn and `PHASE_CLOCK` poison reporting, run-log open failures and `test_cmd` output-path messages remain.

## VAL-028 - Dead or unreachable code in the check/test pipeline

Reported by: check-core, test-execution.

Residue: `resolve_planned_sweep` still returns a `Result` with no error path, and other listed residues in S- and I-owned files remain.

## VAL-032 - `TEXTLINT_PRESET_FIELDS` mirrors `TextlintRule` by hand

Reported by: convention-engines.

Residue: `TEXTLINT_PRESET_FIELDS` is still hand-maintained with no parity check.

## VAL-034 - The displayed cargo argv is a second copy of the real one

Reported by: convention-engines.

Residue: `conventions.rs` still prints a literal cargo metadata command instead of the real invocation.

## VAL-035 - Convention-engine config is validated when the phase runs, not at load

Reported by: convention-engines, config-cli-bootstrap, check-core.

Residue: textlint compilation and manifest glob/granularity validation still happen at scan time.

## VAL-037 - Convention-engine output is inconsistent

Reported by: convention-engines.

Residue: the gremlins, header and manifest green lines are still bare.

## VAL-038 - Convention-engine and deps errors misclassified

Reported by: convention-engines.

Residue: the shared JSON conversion and cargo metadata failures still map to `Build`.

## VAL-039 - Convention-engine tests that prove little

Reported by: convention-engines.

Residue: textlint preset-field parity is untested.

## VAL-040 - Convention-engine guards that fail open

Reported by: convention-engines.

Residue: tree-dependent dead-scope checks can still be bypassed by skipped phases or the prose-only shortcut.

## VAL-042 - Convention-engine policy invented per site

Reported by: convention-engines.

Residue: the schema docs for the convention selectors are not consolidated.
