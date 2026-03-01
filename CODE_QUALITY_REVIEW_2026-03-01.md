# Brokkr Code Quality Review (2026-03-01)

## Scope
General architecture and code-quality review focused on:
- refactoring targets
- restructuring opportunities
- business logic consolidation
- separation of concerns
- long-term dev UX and user UX improvements

This review assumes broad latitude to change API surfaces, CLI shape, and database schema.

## Summary
The main growth risk is orchestration centralization in `src/main.rs`. As command and project count grows, change coupling increases and synchronization drift becomes likely. There is also clear duplication in nidhogg HTTP client logic, CLI/DB filter semantics drift, and config-model constraints that make multi-host dataset management harder than needed.

`cargo check` passes at time of review.

## Findings (Ordered by Severity)

### 1. Critical: Single orchestrator “god module” is the core maintainability bottleneck
Evidence:
- `src/main.rs:49` routes all command execution through one giant match.
- `src/main.rs:488`, `src/main.rs:912`, `src/main.rs:1018`, `src/main.rs:1133` hold large domain-specific dispatch branches.
- Repeated setup patterns (`bootstrap`, `bootstrap_config`, `BenchContext::new`, `HarnessContext::new`) appear across many handlers (`src/main.rs:220`, `src/main.rs:535`, `src/main.rs:589`, `src/main.rs:766`, etc.).

Why it matters:
- New commands require touching central dispatch plus project modules plus sometimes CLI plus DB formatting.
- Cross-project behavior consistency depends on manual discipline rather than architecture.
- This is the most likely source of “hard to keep things in sync.”

### 2. High: Config model likely mismatches intended global dataset behavior
Evidence:
- Top-level `datasets` key is explicitly excluded during host parsing (`src/config.rs:112`).
- Resolved datasets are sourced from host-only config (`src/config.rs:182`).

Why it matters:
- No first-class global dataset registry with host override layering.
- Unknown/new host fallback can yield empty dataset maps unexpectedly.
- Forces duplicated host-level dataset definitions when datasets should be canonical.

### 3. High: CLI contract and runtime behavior drift
Evidence:
- CLI help says `--variant` is prefix-oriented (`src/cli.rs:73`).
- Query implementation uses substring contains (`LIKE '%'||...||'%'`) in regular query and compare paths (`src/db/mod.rs:784`, `src/db/mod.rs:533`).
- Single global CLI exposes project-specific commands then rejects at runtime via repeated `project::require` checks (`src/main.rs:492` onward).

Why it matters:
- UX inconsistency: docs vs behavior.
- Discoverability penalty: users see commands that are invalid in current project context.
- Project gating is correct functionally but late and noisy from a UX perspective.

### 4. High: Nidhogg client/business logic is duplicated and already drifting
Evidence:
- Curl request logic appears in multiple places:
  - shared helpers: `src/nidhogg/mod.rs:23`, `src/nidhogg/mod.rs:48`
  - benchmark-specific request paths: `src/nidhogg/bench_api.rs:101`, `src/nidhogg/bench_api.rs:145`
  - status health checks with separate curl invocation: `src/nidhogg/server.rs:176`
- Default data/query fixtures are spread out.
- Geocode defaults are inconsistent:
  - CLI default: `København` (`src/cli.rs:279`)
  - verify default in main: `Kobenhavn` (`src/main.rs:1386`)
  - shared constants: `src/nidhogg/mod.rs:18`

Why it matters:
- Behavior differences between `query`, `verify`, `bench`, and server status can accumulate.
- Makes fixture evolution and API-shape evolution risky.

### 5. Medium: DB module has too many responsibilities and avoidable inefficiency
Evidence:
- `src/db/mod.rs` includes schema constants, insert logic, query logic, compare logic, hotpath parsing, UUID generation, and extensive tests in one file.
- Child loading path calls KV loading twice for hotpath rows:
  - `load_children` loads `kv` (`src/db/mod.rs:647`)
  - `load_hotpath` then calls `load_kv` again (`src/db/mod.rs:707`)

Why it matters:
- Harder to evolve schema/query API independently.
- Extra DB round-trips in detailed and compare views.

### 6. Medium: Output/exit paths reduce composability and testability
Evidence:
- Non-`main` command handlers call `process::exit` (`src/main.rs:256`, `src/main.rs:337`).
- Direct `println!` is mixed with output abstraction (`src/main.rs:369`, `src/main.rs:388`, `src/main.rs:472`, `src/harness.rs:473`).

Why it matters:
- Harder to enforce uniform output policy (quiet mode, future machine-readable output modes).
- Harder to test command-level behavior as pure return values.

### 7. Medium: Many `too_many_arguments` suppressions indicate missing request-object boundaries
Evidence:
- Suppressions across orchestrator and project modules, e.g.:
  - `src/main.rs:342`, `src/main.rs:1017`, `src/main.rs:1132`
  - `src/pbfhogg/bench_all.rs:25`
  - `src/elivagar/bench_self.rs:18`

Why it matters:
- High call-site noise and higher chance of parameter mismatch mistakes.
- Harder to evolve APIs safely as new options are added.

## Recommended Restructure Plan

### A. Move from centralized dispatch to per-project command handlers
Create a thin root router and delegate execution to project-owned handlers.

Possible shape:
- `src/commands/mod.rs`: shared traits and routing
- `src/commands/shared.rs`: `check`, `env`, `results`, `clean`, maybe `pmtiles-stats`
- `src/commands/pbfhogg.rs`, `src/commands/elivagar.rs`, `src/commands/nidhogg.rs`
- `main.rs` becomes parse + detect + route only

Target outcome:
- Additions to one project do not require broad edits.
- Shared behavior is explicit and reusable.

### B. Introduce typed request objects for command execution
Replace argument-heavy calls with typed request structs:
- `DatasetRequest { dataset, pbf, osc, bbox, ... }`
- `BenchRequest { runs, features, variant, ... }`
- `ProfileRequest { tool, no_mem_check, ... }`

Target outcome:
- Lower cognitive load and fewer accidental signature mismatches.
- Easier defaulting/validation in one place.

### C. Redesign config model to include global datasets + host overrides
Proposed layered model:
1. Global datasets registry (canonical)
2. Host-specific path settings
3. Optional host-specific dataset overrides

Target outcome:
- Clear config semantics and less duplication.
- Better portability across machines.

### D. Refactor DB into explicit layers and clarify filter semantics
Split `db/mod.rs` into:
- `db/schema.rs`
- `db/write.rs`
- `db/query.rs`
- `db/compare.rs`
- `db/hotpath.rs`

Also:
- Resolve `variant` semantics mismatch (`contains` vs `prefix`) explicitly in CLI options.
- Avoid duplicate KV loading when hotpath data is present.

Target outcome:
- Easier schema evolution and better query performance predictability.

### E. Build a shared nidhogg API client module
Add a dedicated client module used by `query`, `geocode`, `verify_*`, and `bench_api`:
- typed helpers for `query`, `query_batch`, `geocode`, `health`
- shared fixtures/constants in one location
- consistent error mapping and timing behavior

Target outcome:
- One place to adjust API contract and request behavior.
- Eliminates literal drift and duplicated curl execution code.

### F. Normalize output and exit behavior
- Keep `process::exit` only in top-level `main()`.
- Return structured outcomes from handlers.
- Route all user output through `output` module (optionally support output modes later).

Target outcome:
- Better testability and future machine-readable UX extension.

## CLI and UX Improvement Opportunities
- Make CLI project-aware after detection:
  - hide invalid commands for current project in help output, or
  - nest by project namespace (`brokkr pbfhogg bench ...`) if you want explicitness over implicit detection.
- Add explicit filter mode flags:
  - `--variant-prefix`
  - `--variant-contains`
  - potentially align `--command` similarly.
- Consider a stable command taxonomy:
  - `bench`, `verify`, `profile`, `hotpath` with project-specific subcommands generated from per-project modules.

## Data Model / Schema Evolution Ideas
- Introduce normalized `commands` / `variants` dimensions if dataset size grows significantly.
- Keep `run_kv` for extensibility, but elevate common metrics to typed columns when they become critical for compare/sort/filter.
- Add schema version notes/changelog table to improve migration introspection.

## Suggested Rollout (Low-Risk Sequence)
1. Extract per-project command handler modules without changing CLI shape.
2. Introduce typed request structs and remove most `too_many_arguments` suppressions.
3. Consolidate nidhogg client + fixtures.
4. Clarify filter semantics and align CLI/docs/SQL.
5. Refactor DB internals into layered modules with no external API break.
6. Optional CLI surface redesign and config v2 rollout with migration helper.

## Open Questions
1. Should CLI be project-aware at parse/help time, or keep global commands with runtime gating?
2. Do you want config v2 now (global datasets + host overrides), with automatic migration support?
3. Should results DB prioritize analytical queries (more normalized/typed) or local human-friendly history first?

## Appendix: Hotspot Indicators
- File size concentration:
  - `src/main.rs` (~1404 LOC)
  - `src/harness.rs` (~1048 LOC)
  - `src/db/mod.rs` (~1163 LOC)
  - `src/db/format.rs` (~1036 LOC)
- Many argument-heavy functions and repeated setup patterns across command handlers.

