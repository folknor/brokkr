# TODO

## Won't fix

### Inconsistent path-to-string conversion
All paths are constructed from known UTF-8 components, so `.display().to_string()` won't corrupt in practice. 100+ occurrences across 30+ files — not worth the churn.

### Hand-rolled UUID via `/dev/urandom`
10 correct lines in `src/db.rs`. Not worth adding the `uuid` crate as a dependency.

### `#[allow(clippy::too_many_arguments)]` proliferation
Functions genuinely need many parameters. `BenchContext` and `HarnessContext` cover the common cases; remaining allows are the pragmatic choice.

---

## Backlog

### Hotpath JSON: emit raw numeric values

`parse_metric()` in `hotpath_fmt.rs` reverse-engineers formatted strings like `"59.2 MB"` and `"3.06 ms"` back into numbers to compute change %. Fragile — silently breaks if the hotpath crate changes formatting (new units, precision changes). The hotpath crate should emit raw numeric values alongside formatted strings in its JSON output so brokkr doesn't need to parse display text.

### RTK double execution

Commands appear to run twice (two "Finished... Running..." blocks in output). The rtk PreToolUse hook may be executing the command in addition to the original — investigate hook configuration.

### Make default binary configurable per-project in brokkr.toml

Currently `find_executable` infers the expected binary name from `BuildConfig.bin` or `BuildConfig.package`. This should be configurable in `brokkr.toml` (e.g. a `default_bin` field per project) so projects with multiple binaries can declare which one brokkr should run by default.

### `Worktree` has no `Drop` impl

If the process panics or is killed (SIGKILL/SIGTERM) inside a `--commit` benchmark, the worktree at `.brokkr/worktree/<hash>` is left behind. Mitigated: `Worktree::create` cleans up stale worktrees at the same path before creating a new one. A `Drop` impl would require interior mutability or an `Option` wrapper — probably not worth the complexity.

