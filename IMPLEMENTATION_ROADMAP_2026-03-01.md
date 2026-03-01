# Brokkr Refactor Implementation Roadmap (2026-03-01)

## Goals
- Reduce synchronization drift across projects and commands.
- Make command logic easier to extend safely.
- Improve user-facing consistency (CLI behavior, filters, defaults, output).
- Keep rollout low-risk via incremental, testable PRs.

## Guiding Constraints
- Preserve existing functionality unless explicitly redesigned.
- Prefer mechanical extractions before semantic behavior changes.
- Keep each PR reviewable (single responsibility, clear acceptance criteria).

## Sequence Overview
1. PR1: Establish command handler architecture scaffolding (no behavior change)
2. PR2: Move bench/verify dispatch out of `main.rs`
3. PR3: Introduce typed request structs and remove arg-heavy signatures
4. PR4: Consolidate nidhogg HTTP client + shared fixtures
5. PR5: Align results filter semantics and CLI docs/options
6. PR6: Split DB internals into layered modules
7. PR7: Optimize DB child loading and compare path
8. PR8: Normalize output/exit behavior and add output mode foundation
9. PR9: Config v2 (global datasets + host overrides) with migration compatibility
10. PR10: Optional CLI UX redesign (project-aware surface)

## PR Slices

### PR1: Command Handler Scaffolding
Scope:
- Add `src/commands/` module with shared interfaces and stubs.
- Keep existing `main.rs` logic intact; only introduce indirection points.
- Add compile-time checks for handler registration.

Files (expected):
- `src/commands/mod.rs` (new)
- `src/commands/shared.rs` (new)
- `src/commands/pbfhogg.rs` (new)
- `src/commands/elivagar.rs` (new)
- `src/commands/nidhogg.rs` (new)
- minimal edits in `src/main.rs`

Risk: Low
Acceptance criteria:
- `cargo check` passes.
- No CLI behavior or output changes.
- Existing tests pass unchanged.

### PR2: Move Bench/Verify/Hotpath/Profile Routing out of Main
Scope:
- Transfer large dispatch branches from `main.rs` into handlers.
- Keep function bodies same where possible (mechanical move first).
- Keep project gating behavior unchanged.

Risk: Medium
Acceptance criteria:
- `src/main.rs` shrinks significantly and becomes thin router.
- All prior commands still execute with same flags and output patterns.
- No new `project::require` behavior differences.

### PR3: Introduce Typed Request Objects
Scope:
- Add request structs for dataset, bench, profile, hotpath, verify flows.
- Replace `#[allow(clippy::too_many_arguments)]` hotspots in orchestrator and major module entry points.
- Centralize defaulting/validation logic in constructors.

Risk: Medium
Acceptance criteria:
- Most `too_many_arguments` suppressions removed from orchestration layer.
- CLI values map to typed requests with explicit validation errors.
- No behavioral regressions in existing command runs.

### PR4: Consolidate Nidhogg API Client and Fixtures
Scope:
- Add `src/nidhogg/client.rs` with unified GET/POST/timed request helpers.
- Centralize query/geocode fixtures and defaults.
- Replace duplicated curl logic in `query`, `geocode`, `verify_*`, `bench_api`, and health checks where practical.

Risk: Medium
Acceptance criteria:
- One shared path for most HTTP request logic.
- Consistent default geocode/query fixtures across commands.
- Existing nidhogg verify/bench flows still pass.

### PR5: Align Results Filter Semantics (CLI + SQL + Docs)
Scope:
- Decide canonical semantics for command/variant filters (contains vs prefix).
- Implement explicit flags if needed (for example `--variant-prefix`, `--variant-contains`).
- Update CLI help text to match actual query behavior.

Risk: Medium
Acceptance criteria:
- CLI help and DB behavior are consistent.
- Existing compare workflows remain available.
- Add/adjust tests for filter matching semantics.

### PR6: Split DB Internals into Layers
Scope:
- Decompose `src/db/mod.rs` into modules:
  - `schema.rs`
  - `write.rs`
  - `query.rs`
  - `compare.rs`
  - `hotpath.rs`
  - `types.rs`
- Keep public API stable initially (`ResultsDb`, `QueryFilter`, format exports).

Risk: Medium
Acceptance criteria:
- No schema or migration behavior changes in this PR.
- Existing DB tests still pass.
- `db/mod.rs` becomes thin re-export/entry module.

### PR7: DB Query/Load Efficiency Improvements
Scope:
- Remove duplicate KV loads in hotpath child loading.
- Reduce per-row query overhead in compare/details paths.
- Add targeted micro-tests around row hydration paths.

Risk: Low to Medium
Acceptance criteria:
- Fewer SQL queries for compare/details row hydration.
- No output regression in `brokkr results` table/detail/hotpath compare output.
- Existing migration compatibility maintained.

### PR8: Output/Exit Normalization
Scope:
- Ensure only top-level `main()` exits process.
- Convert handler-level `process::exit` into returned status/control signals.
- Route direct `println!` usage through output abstraction where appropriate.
- Optionally add output mode enum (`human` default, future `json`).

Risk: Medium
Acceptance criteria:
- Behavior unchanged for normal users.
- Better unit-testability of command handlers.
- Quiet mode semantics remain stable.

### PR9: Config v2 Model (Global Datasets + Host Overrides)
Scope:
- Extend config parser to support canonical top-level datasets with host overrides.
- Preserve backward compatibility with current host-scoped datasets.
- Add clear conflict/precedence rules and validation.

Risk: High
Acceptance criteria:
- Existing `brokkr.toml` files continue to work unchanged.
- New v2 layout works with deterministic precedence rules.
- Clear error messages for ambiguous/invalid config states.

### PR10 (Optional): Project-Aware CLI Surface Redesign
Scope:
- Redesign CLI to avoid exposing invalid project commands at runtime.
- Options:
  - project namespaces (`brokkr nidhogg ...`) or
  - dynamic gating/help based on detected project.

Risk: High
Acceptance criteria:
- Improved discoverability with fewer runtime-gating surprises.
- Migration notes for users.
- Legacy aliases or compatibility path if needed.

## Dependency Graph
- PR1 is prerequisite for PR2.
- PR2 should land before PR3.
- PR4 can start after PR2 (independent of PR3 but cleaner with PR3).
- PR5 can run after PR2 (or after PR4 if client changes affect query paths).
- PR6 should precede PR7.
- PR8 can follow PR2/PR3.
- PR9 is best after PR3 and PR6 (cleaner typed models + modular config/DB impact handling).
- PR10 should be last.

## Suggested Milestones
- Milestone A (Stabilize architecture): PR1 + PR2 + PR3
- Milestone B (Consolidate business logic): PR4 + PR5
- Milestone C (Data layer hardening): PR6 + PR7
- Milestone D (UX and config evolution): PR8 + PR9 + optional PR10

## Testing Strategy by Phase
- Every PR:
  - `cargo check`
  - targeted unit tests for touched modules
- Milestone A:
  - smoke test key commands across all three projects
- Milestone B:
  - nidhogg command parity tests (`query`, `geocode`, `verify`, `bench api`)
- Milestone C:
  - DB migration/open/query regression tests
  - compare output parity checks
- Milestone D:
  - config compatibility matrix (old vs v2 format)
  - CLI behavior snapshot tests/help text verification

## Rollout Notes
- Prefer merging PR1-PR3 quickly to create a stable foundation.
- Keep PR9 isolated and well documented due to config blast radius.
- If capacity is limited, stop after PR7; that yields most maintainability gains with lower UX migration risk.
