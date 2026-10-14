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

Residue: `check_cmd/sweep_env.rs::composed_rustflags_env` still reads `CARGO_ENCODED_RUSTFLAGS`/`RUSTFLAGS` directly instead of through `RustflagsEnv`.

## VAL-003 - Failure sentinels and tool labels are strings matched by content

Reported by: check-core, test-execution.

Residue: the test phase still signals red tests through the `TESTS_FAILED` string sentinel matched in `test_phase.rs` and `phase.rs`.

## VAL-009 - Two owners of `-Zfeature-unification` support; package-mode builds in two target dirs

Reported by: test-execution.

Residue: `check_cmd/install_shape.rs` install-feature builds and missing-bin probes still do not run in the package-unification isolated target dir.

## VAL-011 - Check/test tunables have no index and no injection point

Reported by: check-core, test-execution, config-cli-bootstrap.

Residue: `PHASE_CLOCK` poison handling, CPU-topology and run-log-retention tunables are untouched (only accounting retention got an injection point).

## VAL-015 - The same condition is handled differently across test lanes

Reported by: test-execution.

Residue: `prepare.rs::direct_preflight` still refuses all forwarded args for isolated lanes (the doctest-carrier half is stale per `validate_complete_doctest_carriers`).

## VAL-016 - Check run-state policy is per call site

Reported by: check-core.

Residue: the silent `PHASE_CLOCK` poison path in `watchdog.rs` and wall-clock run-log pruning in `run_log.rs` remain.

## VAL-018 - Test lanes read cargo config late and fail open

Reported by: test-execution.

Residue: `rustflags.rs::CargoLaunchConfig::discover` still skips read/parse failures and no validated cargo-config snapshot is carried into lane setup.

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

## VAL-032 - `TEXTLINT_PRESET_FIELDS` mirrors `TextlintRule` by hand

Reported by: convention-engines.

Residue: `TEXTLINT_PRESET_FIELDS` is still hand-maintained with no parity check.

## VAL-034 - The displayed cargo argv is a second copy of the real one

Reported by: convention-engines.

Residue: `check_cmd/conventions.rs` still prints a literal cargo metadata command.

## VAL-035 - Convention-engine config is validated when the phase runs, not at load

Reported by: convention-engines, config-cli-bootstrap, check-core.

Residue: textlint compilation and manifest glob/granularity validation still happen at scan time.

## VAL-037 - Convention-engine output is inconsistent

Reported by: convention-engines.

Residue: the gremlins, header and manifest green lines are still bare `phase_ok`.

## VAL-038 - Convention-engine and deps errors misclassified

Reported by: convention-engines.

Residue: the shared JSON conversion in `error.rs` and cargo metadata failures in `cargo/metadata.rs` still map to `Build`.

## VAL-039 - Convention-engine tests that prove little

Reported by: convention-engines.

Residue: textlint preset-field parity is untested.

## VAL-040 - Convention-engine guards that fail open

Reported by: convention-engines.

Residue: dead-scope checks can still be bypassed by skipped phases or the prose-only shortcut.
