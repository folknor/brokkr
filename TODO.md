# TODO

## Won't fix

### Inconsistent path-to-string conversion
All paths are constructed from known UTF-8 components, so `.display().to_string()` won't corrupt in practice. 100+ occurrences across 30+ files — not worth the churn.

### Hand-rolled UUID via `/dev/urandom`
10 correct lines in `src/db.rs`. Not worth adding the `uuid` crate as a dependency.

### `#[allow(clippy::too_many_arguments)]` proliferation
Functions genuinely need many parameters. `BenchContext` covers the common case; remaining allows are the pragmatic choice.

---

## Backlog

### Hotpath JSON: emit raw numeric values

`parse_metric()` in `hotpath_fmt.rs` reverse-engineers formatted strings like `"59.2 MB"` and `"3.06 ms"` back into numbers to compute change %. Fragile — silently breaks if the hotpath crate changes formatting (new units, precision changes). The hotpath crate should emit raw numeric values alongside formatted strings in its JSON output so brokkr doesn't need to parse display text.

### RTK double execution

Commands appear to run twice (two "Finished... Running..." blocks in output). The rtk PreToolUse hook may be executing the command in addition to the original — investigate hook configuration.

### `HarnessContext` for no-build commands

7 handlers in `main.rs` manually expand `bootstrap + bootstrap_config + BenchHarness::new` because they don't need a cargo build (allocator, planetiler, bench-all). `BenchContext::new()` always builds. Add a lighter `HarnessContext` (or make the build step optional in `BenchContext`).

### `Worktree` has no `Drop` impl

If the process panics or is killed (SIGKILL/SIGTERM) inside a `--commit` benchmark, the worktree at `.brokkr/worktree/<hash>` is left behind. Mitigated: `Worktree::create` cleans up stale worktrees at the same path before creating a new one. A `Drop` impl would require interior mutability or an `Option` wrapper — probably not worth the complexity.

### Eliminate `cargo_features` duplication

`cargo_features` is specified in two independent places that must stay in sync:

1. **Build time** — `BenchContext::new` in `main.rs` passes features to `BuildConfig`, which feeds `cargo build`.
2. **Record time** — Each benchmark module independently hardcodes the same string into `BenchConfig.cargo_features`, which gets written to SQLite.

These can silently drift. `bench_read.rs` hardcodes `"zlib-ng"` regardless of what was actually compiled. `bench_merge.rs` duplicates the `if uring` conditional from `main.rs`.

**Fix:** `BenchContext` already builds the binary — it knows exactly what features were used. Carry the resolved feature string (from `BuildConfig.features.join(",")`) forward and have the harness automatically attach it to every result. Remove `cargo_features` from `BenchConfig` entirely — no benchmark module should ever set this manually. Eliminates ~15 hardcoded values and the duplicate uring conditional in `bench_merge.rs`.
