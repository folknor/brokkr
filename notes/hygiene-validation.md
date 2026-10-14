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

## VAL-021 - Check/test tests that cannot fail

Reported by: check-core, test-execution.

Residue: the clippy-order and pinned-unification tests in `test_execution_tests.rs` and the other weak tests are unchanged.

## VAL-025 - Stale claims in check/test code and `check.md`

Reported by: check-core, test-execution.

Residue: the `schema.rs` comments are fixed, but the stale test-phase comments the residue mentions were not identified or changed.

## VAL-028 - Dead or unreachable code in the check/test pipeline

Reported by: check-core, test-execution.

Residue: `resolve_planned_sweep` is infallible and its callers are migrated, but the "other listed residues in S- and I-owned files" were not identified or verified.
