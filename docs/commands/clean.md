# brokkr clean

```
brokkr clean [--worktrees] [--cargo [PKG]] [--archives [--keep N]] [--all] [--dry-run]
```

Remove the scratch and temporary files a brokkr project accumulates. Runs in
any project; what it finds is project-shaped, but the rule it follows is not.

## The rule

**`clean` removes only what brokkr created, identified either by a
brokkr-designated directory or by a name brokkr can *construct*.** It never
parses a filename to decide whether it owns it.

That distinction is the whole safety property, and it is load-bearing in the
one place it would be tempting to skip: the durable archive store, where names
look like `<dataset>-<variant>-<commit>.pmtiles`. Dataset names contain
hyphens, so a filename cannot be split back into its parts unambiguously - and
a prefix test alone would let the variant `raw` claim `raw-fast`'s archives and
delete them. So `--archives` builds each known `(dataset, variant)` prefix from
config and matches forward (`resolve::pmtiles_archive_matches`). Anything the
constructed matcher does not claim is preserved unconditionally, because a file
brokkr cannot name by construction is self-evidently not brokkr's to delete.

The same rule is why an explicit `-o` output path is never touched while the
*default* `-o` directory is: one is a location brokkr designated, the other is
your file in a place you chose.

## Permanently out of scope

The measurement stores are never removed, by any flag combination:

| store | why |
| --- | --- |
| `.brokkr/results.db` | the benchmark record `brokkr results` reads |
| `.brokkr/sidecar.db` | the profiler record `brokkr sidecar` reads |
| `$XDG_DATA_HOME/brokkr/history.db` | global command history |
| `.brokkr/piners/corpus/runs.db` | the corpus run store, source of truth |
| `.brokkr/ratatoskr/gate.db` | the gate baselines |
| `.brokkr/bench/` | criterion baselines + their environment stamps |

`.brokkr/bench/` is why `brokkr bench` points `CRITERION_HOME` there instead of
leaving criterion in its default `target/criterion`. A baseline costs minutes to
produce and cannot be reconstructed from the source tree, so it is a result, not
an artifact - and a result must not live in a directory whose whole purpose is
being safe to delete. Parking it in a brokkr-designated directory means the
existing rule spares it, no special case required, and a user's own
`cargo clean` cannot reach it either.

`gate.db` is the sharpest case. A gate baseline is pinned **by UUID** in
`brokkr.toml`; deleting the row that UUID points at breaks the gate with no way
back short of re-recording, which silently rebases the reference onto current
numbers - a gate that passes because it forgot what it was measuring against.
So on ratatoskr projects `clean` removes the *directories* under
`.brokkr/ratatoskr/` and spares every file at that level.

## Routine clean

Bare `brokkr clean`, in order:

- `<target>/verify` - verify output (pbfhogg).
- `<target>/rustflags-*` - the per-sweep isolated target dirs a
  `rustflags`-carrying `[[check]]` entry builds into. Reproducible caches
  nothing else reaches. Taken from cargo's real resolved target dir, not a
  host `target` override, since that is where the check phase actually built
  them.
- **Scratch**, whose shape differs per project: elivagar's `tilegen_tmp` is
  wiped and recreated; nidhogg's `.ingest_tmp` and `.tilegen_tmp` go; every
  other project sweeps loose `.pbf` files, `geocode-<dataset>/` output dirs,
  and orphaned `.pbfhogg-external-join-<pid>` dirs. Those last survive OOM
  kills (SIGKILL runs no destructor), so each is removed **only after checking
  the PID is dead** - a live sibling run's scratch is left alone.
- **Elivagar also**: `corpus-calibrands/` (the default `-o` for
  `pmtiles-corpus mutate`) and `ocean-build_tmp`.
- **Run-artefact trees**: `.brokkr/ratatoskr/` directories (run-N dirs left by
  failed runs, plus `mock/` dirs from `mock-serve`), `.brokkr/dellingr/`
  wholesale (the harness's hotpath report and marker FIFO), and
  `.brokkr/piners/corpus/run-*/`.

A routine clean **spares the durable tilegen output archives**
(`<output>/<dataset>-<variant>-<commit>.pmtiles`). They are reproducible but
expensive, retention already bounds their growth, and they are what `regress`
diffs. If persistent worktrees exist, a routine clean names them and points at
`--worktrees` rather than removing them.

## `--cargo [PKG]`

Additionally runs `cargo clean -p <PKG>`, wiping the package's own build
artifacts across all profiles while keeping dependency caches. `PKG` defaults
to the `brokkr.toml` project name; pass one to clean a different package.

This is the fix for stale-incremental build state - phantom undefined
`anon.*.llvm.*` symbols at link time, or a confident false `E0599` from a
`-p`-scoped build that another shape compiles fine. In both cases the package
to name is the *failing* one, which is usually a workspace member rather than
the project-name default. See `docs/commands/check.md` (`brokkr man check
test-build-errors`).

## `--archives [--keep N]`

**[elivagar]** Prune the canonical archives in the durable output store to the
newest `N` (default 2) **per `(dataset, variant)` pair**. Scoping to the pair
means building one variant can never evict another's archives at the same
commit.

The default of 2 is the tier-3 workflow's shape: the current build plus one
comparand. Group membership follows the constructed-name rule above, so
hand-named files, the toml-contract ocean artifact `data/ocean-tiles.pmtiles`,
and pre-rename `<dataset>-<commit>` archives all survive untouched.

## `--worktrees`

Purge every persistent benchmark worktree (the sibling
`.brokkr-worktree-<project>-*` dirs that `--commit` creates).

Worktrees are siblings of the **build** root, since that is the git repo they
are cut from and the directory name that goes into their prefix. Discovery is
anchored there for the same reason. Under the config-one-level-up layout the
project root would give both the wrong parent directory *and* the wrong prefix,
so a purge would report zero and reclaim nothing - which is why the count now
reads `removed N of M worktree(s) found` whenever those differ. "Removed 0" and
"looked in the wrong place" are otherwise the same message, and these
directories carry an isolated `target/` each: on the nautilus workload, ~1.3G
apiece.

On elivagar this also wipes the durable output store **wholesale** - every
`*.pmtiles` in the output dir, not the keep-N pruning of `--archives`. The deep
clean is the one place the store is treated as what it is, reproducible; rerun
`tilegen` to get an archive back. Skipped when the output dir coincides with
scratch, which the routine pass already wiped.

## `--all` and `--dry-run`

`--all` is `--worktrees` + `--archives` + `--cargo` (default package).

`--dry-run` lists what would go without deleting anything - the output says
`would remove` / `would clean` where a real run says `removed` / `cleaned`.
Worth a first pass on any project where you have hand-placed files near a
brokkr-designated directory.
