# brokkr clean

```
brokkr clean [--worktrees] [--cargo [PKG]] [--archives [--keep N]] [--all] [--dry-run]
brokkr clean --verify-reference [--dry-run]
```

`--verify-reference` removes pbfhogg's verify reference cache and nothing
else. The verify outputs placed from it are hard links that stay, so it frees
less than the cache's logical size while they exist. To keep the cache
bounded instead, set `verify_reference_budget` (`brokkr man config`).

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
| `.brokkr/bench/` | `brokkr crit`'s criterion baselines + their environment stamps |
| `$XDG_DATA_HOME/brokkr/sidecar-backups/` | durable profiler backups |

`.brokkr/bench/` is why `brokkr crit` points `CRITERION_HOME` there instead of
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

## Left alone

No flag removes these either. What keeps each in check, where anything does:

| state | what limits it |
| --- | --- |
| `.brokkr/check-logs/` | each new log prunes its kind to the newest 10, best effort; a single log's size is not capped. The `bench-` logs `brokkr crit` wrote under its old name are no longer any kind's, so nothing prunes them |
| `.brokkr/accounting/` | each new check run prunes to the newest 10, itself among them, best effort; a plan or journal's size is not capped |
| `.brokkr/verify-reference/` | `verify_reference_budget` per host (`brokkr man config`), applied when each verify ends; absent, nothing; `--verify-reference` removes it whole |
| `$XDG_DATA_HOME/brokkr/history.db` | nothing: every invocation, never pruned |
| `$XDG_DATA_HOME/brokkr/sidecar-backups/` | nothing: three whole copies of each project root's sidecar.db, in a directory per root whose `origin` file names it, never pruned; a moved checkout starts a new one. The kind-keyed `<kind>-sidecar.db`, `.1`, `.2` an earlier brokkr wrote at the top level stay too, never written again; delete either by hand |
| `.brokkr/hash_cache`, `hash_cache.lock` | a save drops entries for files that no longer exist; a run that only hits the cache does not save |
| `.brokkr/pbfhogg-capabilities` | capped at 512 lines |
| `.brokkr/worktrees.toml` | cutting a new `--commit` worktree drops the vanished records of that checkout only; nothing else does |
| `.brokkr/parallel-timings.toml` | nothing: measured costs merge in per sweep and binary, and a sweep or binary that stops existing keeps its entries; advisory, and safe to delete |
| `.brokkr/nidhogg.pid` | removed when brokkr stops the server; a server that exits on its own leaves it |
| `.brokkr/.sidecar.lock`, `.gitignore` | one small file each; the lock persists, and an existing `.gitignore` is never rewritten |

Some files exist only while their writer runs, and a writer killed with
SIGKILL leaves them behind. These, made only by commands that hold the brokkr
lock, are removed by store maintenance (`src/maintenance.rs`) the next time
dispatch takes a fresh hold for a command whose spec locks (`check`, `test`,
`build`, `run`, `clean`, `verify` and the measured commands; not `crit`,
`fmt` or `env`, which lock inside their own handler), before that command
does anything, with one line saying how many went and the allocated bytes
observed. `clean --dry-run` only lists them, and `clean --verify-reference`
leaves them alone:

- `~/.brokkr/harness/<pid>-<nanos>-<n>/`, a `check` or `test` harness session.
- `.brokkr/nextest-synth.<pid>-<n>.toml` and
  `.brokkr/nextest-sweep-env.<pid>-<n>.toml`, one private config file per
  nextest-lane read.
- `.brokkr/corpus-sort/brokkr-corpus-sort-<pid>-<id>/`, the spill directory of
  one elivagar corpus sort, which can be large.
- `.brokkr/artifacts/.staging-<pid>-<name>`, a `brokkr build` snapshot's
  partial copy.

Nothing sweeps these, and they can accumulate:

- `.<name>.tmp-<pid>-<n>` beside a file brokkr replaces atomically (the hash
  cache, the capabilities store, `worktrees.toml` and others), removed once
  the replacement is published. Their writers do not all share one
  exclusion, and a lockless `brokkr env` writes the hash cache, so no sweep
  can prove one dead.
- `.brokkr/hotpath-report.json`, at most one: each hotpath capture removes it
  before and after the run. `.brokkr/hotpath-report-<token>.json`, a reserved
  launch's report, goes with that launch's lease instead.

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
  bench/hotpath PBF, OSC and diff text outputs, `multi-extract/`, the `geocode-<dataset>/`
  dirs for configured datasets, merged-PBF cache names constructed from
  configured PBF/OSC entries. First it reclaims every free pbfhogg launch
  lease with its private directory, wherever that sits, and names each lease
  it keeps and why (see "Launch reservations" in `docs/projects/pbfhogg.md`).
  Its removals of pbfhogg's verify output and scratch go through the
  reservation gate, so a path a running pbfhogg launch still uses is named,
  kept and skipped while the rest of the clean goes on, and a tree holding a
  `.pbfhogg-backup-*` is never removed; it also removes the fixed-name
  `.brokkr-pbfhogg/` directories an older brokkr left in scratch and the
  data dir, where a killed child's staging can be a whole PBF. The run
  directories a failed `brokkr library` run kept
  (`<scratch>/library/<scenario>/run-N/`) go each through the gate, so one
  an orphaned harness still holds stays alone, and a scenario directory left
  empty goes with them; the brokkr lock clean holds is what protects a live
  run between its launches. Other pbfhogg scratch files survive.
- **Sidecar marker FIFOs**, every project: in `.brokkr/`, where measured and
  `brokkr library` runs make them. A FIFO is removed only when its name has the
  reserved `.sidecar-<token>.fifo` shape, it is a FIFO, and no process holds
  its read end (a non-blocking write open is refused with `ENXIO`); clean
  never parses the token. A live brokkr always holds its FIFO's read end, and
  the window before it opens one is covered by `.brokkr/.sidecar.lock`, which
  each creator holds shared across it and the sweep exclusive (clean waits
  for it); the lock file stays.
- **Elivagar also**: `corpus-calibrands/` (the default `-o` for
  `pmtiles-corpus mutate`). The `ocean-build_tmp`, `.ingest_tmp`, and
  `.tilegen_tmp` names are not brokkr-designated paths and are left alone.
- **Run-artefact trees**: the named `.brokkr/ratatoskr/{service,service-mock,sync,mock}/`
  trees and child directories under `.brokkr/piners/corpus/` (the `gate.db`
  and `runs.db` paths are protected), `.brokkr/test-hung/`, the isolated
  feature-unification and fixture-lint targets, `<target>/unify-*` and
  `<target>/fixture-*` under cargo's resolved target root, which nothing
  else removes, `.brokkr/artifacts/` (the `brokkr build` snapshots), and
  pbfhogg's `.brokkr/verify-reference/` (the verify reference cache, see
  "Reference cache" in `docs/projects/pbfhogg.md`), through the reservation
  gate; then, for litehtml, `.brokkr/prepare-cache/` and
  `.brokkr/litehtml-capture/` (the staging a Chrome capture is written to
  before it is published into `fixtures/<id>/`; the published references are
  never cleaned). Other directories under `.brokkr/ratatoskr/` are
  preserved. Hung-test snapshots also prune their oldest directories as
  new diagnostics arrive, keeping a bounded recent history even between
  cleans.
- **Litehtml also**: under `$XDG_CACHE_HOME/brokkr/litehtml-prepare/`, every
  version directory (a constructed 32-hex-digit name) other than the one this
  brokkr's bundled Node scripts hash to. The current one stays, so the next
  `prepare` or `visual` does not reinstall its dependencies. The capture
  script is stored in this same versioned cache, not as `.brokkr/capture.js`.

A routine clean **spares the durable tilegen output archives**
(`<output>/<dataset>-<variant>-<commit>.pmtiles`). They are reproducible but
expensive, retention already bounds their growth, and they are what `regress`
diffs. If persistent worktrees exist, a routine clean names them and points
at `--worktrees` rather than removing them. A routine clean that finds
nothing prints `nothing to clean` unless a cargo sweep already reported its
work; a `--dry-run` reports only what it would remove.

## After an interrupt

No clean runs after an interrupted command (Ctrl-C, `brokkr kill`). A sweep
there would run after the command released the brokkr lock, while another
brokkr may already be using the same paths, and the routine set holds what
runs keep on purpose for later: hung-test snapshots, the run directories of
failed corpus, ratatoskr and library runs, an elivagar archive whose
promotion failed, caches. Instead a measured attempt that a stop ends
discards its own scratch output as it unwinds, still under its hold
(`src/measure/runner.rs`): the plan's scratch directories, pbfhogg's command
output, elivagar's partial archive and `tilegen_tmp`, and an external tool's
declared outputs. It does so only once a preparation has cleared those
paths, and never for a stop during promotion, after execution completed; a
path it cannot remove is named and kept. Other disposable working state an
interrupted command leaves on disk (an unmeasured command's output, a
verify's output) sits under a fixed name its next run clears, or goes with
the next `brokkr clean`. What a stop records on purpose stays: the partial
sidecar data under `dirty`, the history entry, and every retained
diagnostic above.

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
