# brokkr clean

```
brokkr clean [--worktrees] [--cargo [PKG]] [--archives [--keep N]] [--all] [--dry-run]
```

Remove the scratch and temporary files a brokkr project accumulates. Runs in
any project; what it finds is project-shaped, but the rule it follows is not.

Every other locked command refuses to start when the invocation cwd's `target` disk (symlinks
resolved, so a `target` linked elsewhere is measured where it points) has
under 5% of its size available (`src/disk_gate.rs`). The project root's own
disk is not gated. `clean` is exempt, being the remedy - as is `env`, which
shows the free space. Finding config in the parent directory does not move
this probe to the config directory. An environment `CARGO_TARGET_DIR` is
also probed (relative paths resolve from cwd); cargo-config target overrides
are not resolved by this lock-time gate.

## The rule

**`clean` removes only what brokkr created, identified either by a
brokkr-designated directory or by a name brokkr can *construct*.** It never
parses a filename to decide whether it owns it.

That distinction is the whole safety property, and it is load-bearing in the
one place it would be tempting to skip: the durable archive store, where names
look like `<dataset>-<variant>-<commit>.pmtiles`. Dataset names contain
hyphens, so a filename cannot be split back into its parts unambiguously - and
a prefix test alone would let the variant `raw` claim `raw-fast`'s archives and
delete them. `ArchiveStore::prune()` builds each configured `(dataset,
variant)` name shape and matches forward. Anything the constructed matcher does
not claim is preserved unconditionally, because a file brokkr cannot name by
construction is outside the store's ownership.

The same rule is why an explicit `-o` output path is never touched while the
*default* `-o` directory is: one is a location brokkr designated, the other is
your file in a place you chose.

## Permanently out of scope

The measurement stores are never removed, by any flag combination:

| store | why |
| --- | --- |
| `.brokkr/results.db` | the benchmark record `brokkr results` reads |
| `.brokkr/sidecar.db` | the profiler record `brokkr sidecar` reads |
| `.brokkr/litehtml-visual.db` | litehtml visual runs, results, and approvals |
| `.brokkr/sluggrs-visual.db` | sluggrs visual runs, results, and approvals |
| `$XDG_DATA_HOME/brokkr/history.db` | global command history |
| `.brokkr/piners/corpus/runs.db` | the corpus run store, source of truth |
| `.brokkr/ratatoskr/gate.db` | the gate baselines |
| `.brokkr/library-certifications/` | `brokkr library --all` certification records |
| `.brokkr/bench/` | criterion baselines + their environment stamps |
| `$XDG_DATA_HOME/brokkr/sidecar-backups/` | durable profiler backups |

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
Its path is a named permanent store in the ratatoskr layout. Cleanup preserves
that path and any directory containing it, even if the store moves below the
current `.brokkr/ratatoskr/` level.

## Routine clean

Bare `brokkr clean`, in order:

- `<target>/verify` - verify output (pbfhogg).
- `<target>/rustflags-*` - the per-sweep isolated target dirs a
  `rustflags`-carrying `[[check]]` entry builds into. Reproducible caches
  nothing else reaches. Taken from cargo's real resolved target dir, not a
  host `target` override, since that is where the check phase actually built
  them.
- **Scratch**, whose shape differs per project: elivagar's scratch dir (the
  host `scratch`, where tilegen writes before the rename into the durable
  store) is wiped and recreated, and `<data>/tilegen_tmp` (the `--tmp-dir`
  brokkr hands elivagar) is removed. Nidhogg's constructed
  `bench-ingest-output/` directory is removed. Pbfhogg removes its known
  bench/hotpath PBF and OSC outputs, `multi-extract/`, the `geocode-<dataset>/`
  dirs for configured datasets, merged-PBF cache names constructed from
  configured PBF/OSC entries. First it reclaims every free pbfhogg launch
  lease with its private directory, wherever that sits, and names each lease
  it keeps and why (see "Launch reservations" in `docs/projects/pbfhogg.md`).
  Its removals of pbfhogg's verify output and scratch go through the
  reservation gate, so a path a running pbfhogg launch still uses is named,
  kept and skipped while the rest of the clean goes on, and a tree holding a
  `.pbfhogg-backup-*` is never removed; it also removes the fixed-name
  `.brokkr-pbfhogg/` directories one earlier brokkr left in scratch and the
  data dir. The run directories a failed `brokkr library` run kept
  (`<scratch>/library/<scenario>/run-N/`) go each through the gate, so one
  an orphaned harness still holds stays alone, and a scenario directory left
  empty goes with them; only in a clean run by hand, never in the cleanup
  after an interrupted command, which runs after the command released the
  lock that protects a live run between its launches. Other pbfhogg scratch
  files survive.
- **Sidecar marker FIFOs**, every project: in `.brokkr/`, where measured and
  `brokkr library` runs make them, and in the configured scratch dir, where
  an older brokkr made them. A FIFO is removed only when its name has the
  reserved `.sidecar-<token>.fifo` shape, it is a FIFO, and no process holds
  its read end (a non-blocking write open is refused with `ENXIO`); clean
  never parses the token. A live brokkr always holds its FIFO's read end, and
  the window before it opens one is covered by `.brokkr/.sidecar.lock`, which
  each creator holds shared across it and the sweep exclusive; the lock file
  stays. A clean run by hand waits for the lock; the cleanup after an
  interrupted command skips the sweep, with a warning, when it is held. Not
  covered: a FIFO an older brokkr that takes no lock is creating at that
  instant, and elivagar's scratch, wiped whole before this sweep runs.
- **Elivagar also**: `corpus-calibrands/` (the default `-o` for
  `pmtiles-corpus mutate`). The `ocean-build_tmp`, `.ingest_tmp`, and
  `.tilegen_tmp` names are not brokkr-designated paths and are left alone.
- **Run-artefact trees**: the named `.brokkr/ratatoskr/{service,service-mock,sync,mock}/`
  trees and child directories under `.brokkr/piners/corpus/` (the `gate.db`
  and `runs.db` paths are protected),
  `.brokkr/dellingr/`, `.brokkr/mogwai/`, `.brokkr/test-hung/`, and, for
  litehtml, `.brokkr/prepare-cache/`. `.brokkr/artifacts/` (the `brokkr build`
  snapshots) goes only in a clean run by hand, never in the cleanup after an
  interrupted command, since a script may still be running one. A per-user
  `~/.brokkr/sidecar-status` file an older brokkr left (the last marker now
  lives in the holder's memory) is removed with the project scratch.
  Other directories under `.brokkr/ratatoskr/` are preserved. Hung-test snapshots
  also prune their oldest directories as new diagnostics arrive, keeping a
  bounded recent history even between cleans.
- **Litehtml also**: under `$XDG_CACHE_HOME/brokkr/litehtml-prepare/`, every
  version directory (a constructed 32-hex-digit name) other than the one this
  brokkr's bundled Node scripts hash to. The current one stays, so the next
  `prepare` or `visual` does not reinstall its dependencies. The capture
  script is stored in this same versioned cache, not as `.brokkr/capture.js`.

A routine clean **spares the durable tilegen output archives**
(`<output>/<dataset>-<variant>-<commit>.pmtiles`). They are reproducible but
expensive, retention already bounds their growth, and they are what `regress`
diffs. If persistent worktrees exist, a routine clean run by hand names them
and points at `--worktrees` rather than removing them; the interrupt/kill
cleanup does not print that hint. A routine clean that finds nothing prints
`nothing to clean` unless a cargo sweep already reported its work; a
`--dry-run` reports only what it would remove.

## `--cargo [PKG]`

Additionally runs `cargo clean -p <PKG>`, wiping the package's own build
artifacts across all profiles while keeping dependency caches. `PKG` defaults
to the `brokkr.toml` project name; pass one to clean a different package.

This is the fix for stale-incremental build state - phantom undefined
`anon.*.llvm.*` symbols at link time, or a confident false `E0599` from a
`-p`-scoped build that another shape compiles fine. In both cases the package
to name is the *failing* one, which is usually a workspace member rather than
the project-name default. See `docs/commands/check.test-lanes.md` (`brokkr man
test-lanes build-errors`).

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

Purge every persistent `--commit` worktree of this checkout: each slot under
`~/.brokkr/worktrees/<checkout key>/`, together with its isolated target dir
(`<target>/brokkr-worktrees/<checkout key>/<short hash>`, followed through the
slot's `.brokkr-target` link only when the link still points at that
constructed name). The dependency symlinks in a slot are removed as links; the
live checkouts they point at are untouched. Placement is described under
`--commit` in `brokkr man run`, and in `src/worktree.rs`.

Worktrees cut before the container existed - the sibling
`.brokkr-worktree-<project>-<short hash>` directories next to the checkout -
are found and purged too, so the move orphans nothing. That name is matched by
construction - the prefix followed by a bare hex hash - so a checkout named
`foo` never claims a sibling checkout `foo-bar`'s worktrees.

Unlike retention eviction, the purge does not spare a worktree with
uncommitted work: it is asked for by name, and a purge that quietly kept some
would misreport what was reclaimed.

This is the explicit hammer. Routine growth is damped automatically: cutting a
new `--commit` worktree first evicts the least-recently-used ones beyond
`worktree_keep` (default 6, per host - see `brokkr man config worktree_keep`).
That bound only applies when a *new* worktree is created, so a project that has
stopped growing never shrinks by itself, which is exactly what `--worktrees` is
for. It also cannot promise the disk stays clear, since the count is per
project.

Worktrees are keyed by the **build** root, since that is the git repo they are
cut from and the checkout whose path names their directory. Discovery is
anchored there for the same reason, and the root is threaded in from the
dispatch layer rather than re-derived from cwd here - the two must agree, and a
function that recomputes a root it was not given is stating an invariant it
cannot enforce. Under the config-one-level-up layout the project root would
give the wrong key, so a purge would report zero and reclaim nothing - which is
why the count reads `removed N of M worktrees found` whenever those differ.
"Removed 0" and "looked in the wrong place" are otherwise the same message, and
each worktree carries an isolated target dir: on the nautilus workload, ~1.3G
apiece.

On elivagar this also empties the durable output store - every canonical
archive, not the keep-N pruning of `--archives` (it is `--archives --keep 0`).
The deep clean is the one place the store is treated as what it is,
reproducible; rerun `tilegen` to get an archive back. It still follows the
constructed-name rule: only `<dataset>-<variant>-<commit>.pmtiles` names built
from configured datasets go, so a hand-named file or the ocean artifact
survives even when `output` points at the data dir. Skipped when the output dir
coincides with scratch, which the routine pass already wiped.

## `--all` and `--dry-run`

`--all` is `--worktrees` + `--archives` + `--cargo` (default package).

`--dry-run` lists what would go without deleting anything - the output says
`would remove` / `would clean` where a real run says `removed` / `cleaned`.
Worth a first pass on any project where you have hand-placed files near a
brokkr-designated directory.
