# brokkr hotpath

`brokkr hotpath` is the sluggrs rendering benchmark. It builds a cargo
example target and runs it through the standard bench harness, so its rows
are stored in `.brokkr/results.db` alongside every other measured command's,
and you query them with `brokkr results` and `brokkr sidecar` as usual. This
page covers its modes, `--target`, `--commit`, and its output and storage.

These invocations show the common shapes:

```sh
brokkr hotpath                        # --hotpath 1, the default
brokkr hotpath --alloc                # per-function allocations
brokkr hotpath --bench 5              # 5 uninstrumented walls
brokkr hotpath --bench 5 --commit HEAD~20
brokkr hotpath --target email --hotpath 3
```

The command exists only in sluggrs. Running it elsewhere gives the usual
`'brokkr hotpath' is only available in sluggrs projects`.

## Modes

The command is named for its default, not its only mode. It takes the
shared mode flags (`--bench`, `--hotpath`, `--alloc`, `--commit`,
`--features`, `--force`, `--verbose`, `--dry-run`, `--stop`); see `brokkr
man measure`. Each mode builds with different features and times
differently:

| Mode | Cargo features | Timing | `mode` column |
| --- | --- | --- | --- |
| `--hotpath N` | `hotpath` plus host and CLI features | hotpath capture | `hotpath` |
| `--alloc N` | `hotpath-alloc` plus host and CLI features | hotpath capture | `alloc` |
| `--bench N` | host and CLI features only (bare) | self-reported `elapsed_ms=` | `bench` |

`--bench` builds the example bare on purpose. Instrumentation taxes every
call it wraps, so an instrumented wall measures the instrument as much as
the renderer; the bare walls are the ones worth comparing across commits.
dellingr's `--bench` follows the same rule.

A bare `brokkr hotpath` with no mode flag resolves to `--hotpath 1`. Every
other measured command would resolve no mode flag to a plain run, executed
once with nothing stored; this one doesn't.

`--alloc` prints a `NOTE: alloc profiling - wall-clock times are not
meaningful` banner, the same line every project prints, even without
`--verbose`, because the allocator shim dominates the wall.

### Self-reported timing

Unlike every other project's `--bench`, sluggrs' `--bench` doesn't use
brokkr's external wall clock. The timing comes from an `elapsed_ms=` line
the example writes to stderr.

That is deliberate. brokkr's wall times the whole process, which for a
renderer is dominated by device init, shader compilation and font loading:
hundreds of milliseconds of near-constant setup around a measurement worth
single-digit milliseconds. Because the setup cost barely varies, it doesn't
only add noise; it swamps the signal, and two different runs both report the
same flat number. Self-reporting lets the example time only the region that
matters.

Two consequences follow:

- **`elapsed_ms=` on stderr is mandatory**: an example that omits it fails
  the run rather than being silently mistimed. `total_ms=` is an accepted
  alias.
- **The value may be fractional** (`elapsed_ms=6.847`): brokkr stores it as
  exact microseconds in `elapsed_us`, with `elapsed_ms` holding it rounded.
  Best-of-N selection and `--compare`'s delta both use the microsecond
  reading when available, which makes A/B differences of 100 to 200 us
  visible at all; rounded to whole milliseconds they vanish.

Any other stderr `key=value` line whose key is a field name (the grammar is
in `brokkr man output-channels`) is stored in the results.db `kv` column and
shows in `brokkr results`, so metrics like `cold_prepare_us=214` stay with
the row.

## target

`--target NAME` selects the example. The name its rows are filed under
differs from the example name for the default:

| `--target` | cargo example built | `command` column |
| --- | --- | --- |
| `hotpath` (default) | `hotpath` | `render` |
| `gpu` | `gpu-bench` | `gpu` |
| anything else, e.g. `email` | `email_bench` | `email` |

The default target's rows are filed as `render` for continuity with older
rows, which predate `--target`. So the query for the default benchmark is
`brokkr results --command render`, not `--command hotpath`.

The `_bench` suffix on non-default targets is a naming convention in
sluggrs' `examples/` dir, not something brokkr can discover: a `--target`
naming no such example fails in cargo, not in brokkr. `gpu` is the one
exception: sluggrs names that example with a hyphen, so brokkr maps it by
name.

The `hotpath` example exercises two rendering paths:

- **Cache-miss**: the first frame, with a cold glyph cache: outline
  extraction, band building, texture upload.
- **Cache-hit**: later frames reusing cached glyphs, with vertex buffer
  reuse.

## commit

`--commit REF` builds and runs the example from a git worktree at that
commit, through the same worktree lifecycle every other measured command
uses. Worktrees persist across runs, so the cargo `target/` inside survives,
and brokkr reuses one when the same commit is asked for again; `brokkr clean
--worktrees` collects them.

Everything sluggrs measures here is code: the example target and the
renderer it drives both come from the worktree. There is no external asset to
hold fixed, so `--commit` here needs no split-tree rule, unlike dellingr,
which takes its harness from the worktree and its Lua workload from the
current tree. The whole subject is the old commit.

Two consequences follow:

- The example must exist at that commit. `--commit` on a revision predating
  a `--target`'s example fails in cargo with an unknown-target error.
- Data paths and the results DB still resolve against the main tree, so a
  baseline run writes its row to the same `results.db` as a current-tree run
  and the two are directly comparable. Only the build and the git provenance
  come from the worktree.

The usual baseline shape is two runs and a comparison:

```sh
brokkr hotpath --bench 5 --commit REF
brokkr hotpath --bench 5
brokkr results --command render --compare COMMIT_A COMMIT_B
```

Use `--bench`, not `--hotpath`, for that comparison; see Modes.

`--compare` pairs the two rows into one delta row. Pairing keys on the tuple
`brokkr man results compare` gives, including the arm key: the recorded argv
with brokkr's `--commit`, `--verbose` and iteration counts removed. Rows from
before the key existed pair by their stored argv. Without that removal the
older-commit row could never pair with the current-tree row, since
`--commit` is what distinguishes them.

Any other flag difference between the two invocations does split them into
separate rows, on purpose: that is how arm-defining flags avoid being
averaged together. So keep the two invocations identical apart from
`--commit`.

## Output and storage

`--verbose` prints the full build and run output. Without it brokkr runs
quiet, the same as every other measured command; this isn't a
sluggrs-specific choice.

Rows have an empty `input_file` and `n/a` for dataset and variant: sluggrs
has no dataset registry, and the example is the operation. `--force` runs on
a dirty tree, but the harness then refuses to store the result, as
everywhere else.

`--dry-run` resolves the example name and feature set and prints them
without building, taking the lock or running anything.
