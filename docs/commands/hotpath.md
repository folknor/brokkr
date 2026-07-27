# `brokkr hotpath` (sluggrs)

The sluggrs rendering benchmark. Builds a cargo **example** target and runs it
through the standard bench harness, so its rows land in `.brokkr/results.db`
alongside every other measured command and are queried with `brokkr results` /
`brokkr sidecar` in the usual way.

```
brokkr hotpath                       # --hotpath 1 (the default)
brokkr hotpath --alloc               # per-function allocations
brokkr hotpath --bench 5             # 5 uninstrumented walls
brokkr hotpath --bench 5 --commit HEAD~20
brokkr hotpath --target email --hotpath 3
```

Sluggrs-only. Running it elsewhere gives the usual
`'brokkr hotpath' is only available in sluggrs projects`.

## Modes

The command is named for its default, not its only mode. It takes the shared
`ModeArgs` (`--bench`, `--hotpath`, `--alloc`, `--commit`, `--features`,
`--force`, `--verbose`, `--dry-run`, `--stop`) - see `brokkr man measure`.

| Mode | Cargo features | Harness path | `mode` column |
| --- | --- | --- | --- |
| `--hotpath N` | `hotpath` + host/CLI features | `run_hotpath_capture` | `hotpath` |
| `--alloc N` | `hotpath-alloc` + host/CLI features | `run_hotpath_capture` | `alloc` |
| `--bench N` | host/CLI features only (**bare**) | `run_external_with_kv` | `bench` |

`--bench` builds the example **bare** on purpose. Instrumentation taxes every
call it wraps, so an instrumented wall measures the instrument as much as the
renderer; the bare walls are the ones worth comparing across commits. This is
the same rule dellingr's `--bench` follows.

A bare `brokkr hotpath` with no mode flag resolves to `--hotpath 1`, which is
what it has always meant. Every other measured command would resolve that to
`Run` (execute once, store nothing); this one does not.

`--alloc` prints a `NOTE: alloc profiling -- wall-clock times are not
meaningful` banner, because the allocator shim dominates the wall.

### `--bench` is on the self-reported timing path

Unlike every other project's `--bench`, sluggrs does **not** use brokkr's
external wall clock. It uses `run_external_with_kv`, so the timing comes from
an `elapsed_ms=` line the example writes to stderr.

That is deliberate. Brokkr's wall times the whole process, which for a
renderer is dominated by device init, shader compilation and font loading -
hundreds of milliseconds of near-constant setup wrapped around a measurement
worth single-digit milliseconds. Because the setup cost barely varies, it
doesn't merely add noise; it swamps the signal, and two genuinely different
runs both report the same flat number. Self-reporting lets the example time
only the region that matters.

Two consequences:

- **`elapsed_ms=` on stderr is mandatory.** An example that omits it fails the
  run rather than being silently mistimed. `total_ms=` is an accepted alias.
- **The value may be fractional** (`elapsed_ms=6.847`). It is stored as exact
  microseconds in `elapsed_us`, with `elapsed_ms` holding it rounded. Best-of-N
  selection and `--compare`'s delta both use the microsecond reading when
  available, which is what makes 100-200us A/B differences visible at all -
  rounded to whole milliseconds they vanish.

Any other stderr `key=value` line lands in the results.db `kv` column and
shows up in `brokkr results`, so metrics like `cold_prepare_us=214` travel
with the row.

## `--target` and the two names it picks

`--target NAME` selects the example, and the name it is filed under differs
from the example name for the default:

| `--target` | cargo example built | `command` column |
| --- | --- | --- |
| `hotpath` (default) | `hotpath` | `render` |
| anything else, e.g. `email` | `email_bench` | `email` |

The default target's rows are filed as `render` for continuity with historical
rows, which predate `--target`. So the query for the default benchmark is
`brokkr results --command render`, not `--command hotpath`. The `_bench` suffix
on non-default targets is a naming convention in sluggrs's `examples/` dir, not
something brokkr can discover - a `--target` naming no such example fails in
cargo, not in brokkr.

The `hotpath` example exercises two rendering paths: **cache-miss** (first frame,
cold glyph cache - outline extraction, band building, texture upload) and
**cache-hit** (subsequent frames reusing cached glyphs - vertex buffer reuse).

## `--commit`

`--commit REF` builds and runs the example from a git worktree at that commit,
via the shared `context::with_worktree` lifecycle - the same machinery every
other measured command uses. Worktrees persist across runs (so the cargo
`target/` inside survives) and are reused when the same commit is asked for
again; `brokkr clean --worktrees` collects them.

Everything sluggrs measures here **is code**: the example target and the
renderer it drives both come from the worktree. There is no external asset to
hold fixed, so unlike dellingr - which deliberately takes its harness from the
worktree and its Lua workload from the current tree - `--commit` here needs no
split-tree rule. The whole subject is the old commit.

Two consequences worth knowing:

- The example must exist *at that commit*. `--commit` on a revision predating
  a `--target`'s example fails in cargo with an unknown-target error.
- Data paths and the results DB still resolve against the main tree, so a
  baseline run writes its row to the same `results.db` as a current-tree run
  and the two are directly comparable. Only the build and the git provenance
  come from the worktree.

The usual baseline shape:

```
brokkr hotpath --bench 5 --commit v0.3.0
brokkr hotpath --bench 5
brokkr results --command render --compare <old-uuid> <new-uuid>
```

Use `--bench`, not `--hotpath`, for that comparison - see the modes table above.

`--compare` pairs the two rows into a single delta row. Pairing keys on
`(command, mode, input_file, brokkr_args, env_fingerprint)`, with `--commit`
and `--verbose` stripped from `brokkr_args` first - otherwise the retro row
could never pair with the current-tree row, since `--commit` is precisely what
distinguishes them. Any *other* flag difference between the two invocations
does split them into separate rows, on purpose: that is how arm-defining flags
avoid being averaged together. So keep the two invocations identical apart from
`--commit`.

## Output and storage

`--verbose` prints the full build/run output; without it brokkr runs quiet, the
same as every other measured command (`output::set_quiet(!verbose)` in
`run_measured`). This is a behaviour the command inherited when it moved onto
`ModeArgs`; it is not a sluggrs-specific choice.

Rows carry an empty `input_file` and `n/a` for dataset/variant - sluggrs has no
dataset registry, the example is the operation. `--force` runs on a dirty tree
but the harness then refuses to store the result, as everywhere else.

`--dry-run` resolves the example name and feature set and prints them without
building, acquiring the lock, or running anything.

## Where it lives

- `src/cli/schema.rs` - `Command::Hotpath { mode: ModeArgs, target }`
- `src/main_parts/bootstrap.rs` - the dispatch arm; forces `--hotpath 1` when no
  mode flag is set, then hands off to `run_measured`
- `src/sluggrs/hotpath.rs` - `cmd()` (target naming, feature selection,
  `BenchContext::with_build_config`) and `run()` (the harness driver)
