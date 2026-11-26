# brokkr clean

`brokkr clean` removes the scratch and temporary files a brokkr project
accumulates. It runs in any project; what it finds is project-shaped, but the
rule it follows is not. This page states the ownership rule, then what clean
never removes, what it leaves alone, what a routine clean removes, and the
`--cargo`, `--worktrees`, `--all` and `--dry-run` flags.

The command takes these flags:

```text
brokkr clean [--worktrees] [--cargo [PKG]] [--all] [--dry-run]
```

## Disk gate

Every other locked command refuses to start when the disk holding the
invocation cwd's `target` has under 5% of its size available. brokkr
resolves symlinks first, so a `target` linked elsewhere is measured where it
points. `clean` is exempt, being the remedy, and so is `env`, which shows the
free space.

The gate probes these places and no others:

- the cwd's `target`, even when the config was found in the parent
  directory; the project root's own disk is not gated
- an environment `CARGO_TARGET_DIR`, a relative one resolved from cwd

The gate doesn't resolve cargo-config target overrides.

## Ownership

`clean` removes only what brokkr created, identified either by a
brokkr-designated directory or by a name brokkr can construct. It never
parses a filename to decide whether it owns it.

brokkr builds the names it owns from configured values and matches them
exactly, so a dataset `foo` never claims dataset `foo-bar`'s files. Anything
it can't name that way is kept unconditionally, whatever its name looks
like.

## Kept stores

No flag combination removes the measurement stores:

| store | why |
| --- | --- |
| `.brokkr/results.db` | the benchmark record `brokkr results` reads |
| `.brokkr/sidecar.db` | the profiler record `brokkr sidecar` reads |
| `.brokkr/litehtml-visual.db` | litehtml visual runs, results, and approvals |
| `.brokkr/sluggrs-visual.db` | sluggrs visual runs, results, and approvals |
| `$XDG_DATA_HOME/brokkr/history.db` | global command history |
| `$XDG_DATA_HOME/brokkr/bench.db` | every attempted script-bench run, all projects |
| `.brokkr/ratatoskr/` | what brokkr's retired ratatoskr commands left, `gate.db` included; clean never names anything under it |
| `.brokkr/library-certifications/` | certification records an older brokkr's library verification wrote; small, and nothing writes them now |
| `.brokkr/bench/` | `brokkr crit`'s criterion baselines and their environment stamps |
| `$XDG_DATA_HOME/brokkr/sidecar-backups/` | backup copies of sidecar.db the typed commands made; nothing writes them now |

`brokkr crit` keeps its baselines in `.brokkr/bench/` rather than
criterion's default `target/criterion`, so neither `brokkr clean` nor your
own `cargo clean` can reach them.

## Unswept state

No flag removes these either. The table says what keeps each in check, where
anything does:

| state | what limits it |
| --- | --- |
| `.brokkr/check-logs/` | each new log prunes its kind to the newest 10, best effort; a single log's size is not capped. The `bench-` logs an older `brokkr crit` wrote under its old name belong to no kind, so nothing prunes them |
| `.brokkr/accounting/` | each new check run prunes to the newest 10, itself among them, best effort; a plan or journal's size is not capped |
| `$XDG_DATA_HOME/brokkr/history.db` | nothing: every invocation, never pruned |
| `$XDG_DATA_HOME/brokkr/bench.db` | nothing: every attempted run with its evidence, never pruned |
| `SCRATCH/bench-iterations/` | each script-bench iteration's directory goes with its lease, at its end or, after a crash, once nothing holding its reservation runs; the empty container stays |
| `$XDG_DATA_HOME/brokkr/sidecar-backups/` | nothing: up to three whole copies of each project root's sidecar.db, in a directory per root whose `origin` file names it, plus the kind-keyed `KIND-sidecar.db`, `.1` and `.2` at the top level; never written again, never pruned; delete by hand |
| `.brokkr/hash_cache`, `hash_cache.lock` | a save drops entries for files that no longer exist; a run that only hits the cache does not save |
| `.brokkr/pbfhogg-capabilities` | not read or written; safe to delete |
| `.brokkr/worktrees.toml` | cutting a new `--commit` worktree drops the vanished records of that checkout only; nothing else does |
| `.brokkr/parallel-timings.toml` | nothing: measured costs merge in per sweep and binary, and a sweep or binary that stops existing keeps its entries; advisory, and safe to delete |
| `.brokkr/.sidecar.lock`, `.gitignore` | one small file each; the lock persists, and an existing `.gitignore` is never rewritten |

Some files exist only while their writer runs, and a writer killed with
SIGKILL leaves them behind. Only commands that hold the brokkr lock make
them:

- `~/.brokkr/harness/PID-NANOS-N/`, a `check` or `test` harness session
- `.brokkr/nextest-synth.PID-N.toml` and
  `.brokkr/nextest-sweep-env.PID-N.toml`, one private config file per
  nextest-lane read
- `.brokkr/artifacts/.staging-PID-NAME`, a `brokkr build` snapshot's partial
  copy

Store maintenance removes these the next time dispatch takes a fresh hold
for a command whose spec locks, before that command does anything. Those
commands are `check`, `test`, `build`, `run`, `clean` and the measured
commands; not `crit`, `fmt` or `env`, which lock inside their own handler.
Maintenance prints one line saying how many went and the allocated bytes
observed. `clean --dry-run` only lists them.

Nothing sweeps these, and they can accumulate:

- `.NAME.tmp-PID-N` beside a file brokkr replaces atomically (the hash
  cache, `worktrees.toml` and others), removed once the replacement is
  published. Their writers don't all share one exclusion, and a lockless
  `brokkr env` writes the hash cache, so no sweep can prove one dead.
- `.brokkr/hotpath-report.json`, at most one: each hotpath capture removes
  it before and after the run.

## Routine clean

Bare `brokkr clean` removes these, in order:

- **Verify output**: `TARGET/verify`, the output a pbfhogg verify of an older
  brokkr left, under the current cargo-resolved target root, where it wrote.
  A host `target` plays no part.
- **Rustflags targets**: `TARGET/rustflags-*`, the per-sweep isolated target
  dirs a `rustflags`-carrying `[[check]]` entry builds into, and the
  per-recipe ones a script bench with `rustflags` builds the live tree into
  (a `--commit` one's sit under its worktree target). They are reproducible
  caches nothing else reaches. Clean takes them from cargo's real resolved
  target dir, not a host `target` override, since that is where the check
  phase built them.
- **Leases**: in every project, first. Clean reclaims every free lease in
  the launch lease store with its private directory, wherever that sits,
  including a script-bench iteration's unless a borrow of it is still held.
  It names each lease it keeps and why. Script-bench iterations live under
  `bench-iterations/` under the host's scratch, which clean never removes by
  name.
- **Scratch**: its shape differs per project, as described after this list.
- **Sidecar marker FIFOs**: in every project, in `.brokkr/`, where measured
  runs make them. Clean removes a FIFO only when its name has the reserved
  `.sidecar-TOKEN.fifo` shape, it is a FIFO, and no process holds its read
  end (a non-blocking write open is refused with `ENXIO`). Clean never
  parses the token. A live brokkr always holds its FIFO's read end. Each
  creator holds `.brokkr/.sidecar.lock` shared across the window before it
  opens one, and the sweep holds it exclusive (clean waits for it); the lock
  file stays.
- **Elivagar**: nothing. elivagar-devkit's own clean removes what brokkr
  swept for it before (scratch, `tilegen_tmp`, `corpus-calibrands/`, the
  corpus-sort spill directories and the tilegen archive store).
- **Piners**: nothing. The in-tree corpus tool's own `clean` removes the run
  directories under `.brokkr/piners/corpus/`.
- **Run-artefact trees**: described after this list.
- **Litehtml prepare cache**: under `$XDG_CACHE_HOME/brokkr/litehtml-prepare/`,
  every version directory (a constructed 32-hex-digit name) other than the
  one this brokkr's bundled Node scripts hash to. The current one stays, so
  the next `prepare` or `visual` doesn't reinstall its dependencies. The
  capture script is stored in this same versioned cache, not as
  `.brokkr/capture.js`.

Scratch removal differs per project:

- **Nidhogg**: clean removes the `run-ingest-output/`,
  `bench-ingest-output/` and `hotpath-ingest-output/` directories the
  retired nidhogg commands left in scratch. These removals go through the
  reservation gate, since a configured directory can enclose a script-bench
  iteration a surviving child still holds; clean names and keeps such a
  directory.
- **Pbfhogg outputs**: clean removes the retired commands' known
  bench and hotpath PBF, OSC and diff text outputs, `multi-extract/`, the
  `geocode-DATASET/` dirs for configured datasets, and merged-PBF cache
  names constructed from configured PBF and OSC entries. Other pbfhogg
  scratch files survive.
- **Pbfhogg gate**: removals of pbfhogg's leftover verify output and scratch
  go through the reservation gate. Clean names, keeps and skips a path a
  reserved child still uses while the rest of the clean goes on, and never
  removes a tree holding a `.pbfhogg-backup-*`.
- **Pbfhogg staging**: clean removes the fixed-name `.brokkr-pbfhogg/`
  directories an older brokkr left in scratch and the data dir, where a
  killed child's staging can be a whole PBF.
- **Library runs**: the run directories a failed run of the retired
  `brokkr library` kept (`SCRATCH/library/SCENARIO/run-N/`) each go through
  the gate, so one an orphaned harness still holds stays. A scenario
  directory left empty goes with them. The brokkr lock clean holds is what
  protects a live run between its launches.

The run-artefact trees clean removes, through the reservation gate, are:

- `.brokkr/test-hung/`
- the isolated feature-unification and fixture-lint targets,
  `TARGET/unify-*` and `TARGET/fixture-*` under cargo's resolved target
  root, which nothing else removes
- `.brokkr/artifacts/`, the `brokkr build` snapshots
- the leftover `.brokkr/verify-reference/` cache
- for litehtml, `.brokkr/prepare-cache/` and `.brokkr/litehtml-capture/`,
  the staging a Chrome capture is written to before it is published into
  `fixtures/ID/`; the published references are never cleaned

Hung-test snapshots also prune their oldest directories as new diagnostics
arrive, so they keep a bounded recent history even between cleans.

If persistent worktrees exist, a routine clean names them and points at
`--worktrees` rather than removing them. A routine clean that finds nothing
prints `nothing to clean` unless a cargo sweep already reported its work; a
`--dry-run` reports only what it would remove.

## After an interrupt

No clean runs after an interrupted command (Ctrl-C, `brokkr kill`). A sweep
there would run after the command released the brokkr lock, while another
brokkr may already be using the same paths. The routine set also holds what
runs keep on purpose for later: hung-test snapshots and caches.

A script-bench iteration's directory goes with its lease instead. Other
disposable working state an interrupted command leaves on disk sits under a
fixed name its next run clears, or goes with the next `brokkr clean`. What a
stop records on purpose stays: the partial sidecar data under `dirty`, the
history entry, and every retained diagnostic above.

## cargo

`--cargo [PKG]` also runs `cargo clean -p PKG`, wiping the package's own
build artifacts across all profiles while keeping dependency caches. `PKG`
defaults to the `brokkr.toml` project name; pass one to clean a different
package.

This is the fix for stale incremental build state: phantom undefined
`anon.*.llvm.*` symbols at link time, or a confident false `E0599` from a
`-p`-scoped build that another shape compiles fine. In both cases, name the
failing package, which is usually a workspace member rather than the
project-name default. See `brokkr man test-lanes build-errors`.

## Retired flags

`--archives` and `--keep` belonged to elivagar's archive pruning, which
elivagar-devkit's clean now does. brokkr refuses both flags with a message
saying so, rather than parsing and ignoring them.

## worktrees

`--worktrees` purges every persistent `--commit` worktree of this checkout:
each slot under `~/.brokkr/worktrees/CHECKOUT_KEY/`, together with its
isolated target dir, `TARGET/brokkr-worktrees/CHECKOUT_KEY/SHORT_HASH`.
Clean follows the slot's `.brokkr-target` link to that dir only when the
link still points at that constructed name. It removes the dependency
symlinks in a slot as links and leaves the live checkouts they point at
untouched. `--commit` in `brokkr man run` describes the placement.

The purge also finds sibling `.brokkr-worktree-PROJECT-SHORT_HASH`
directories next to the checkout, the layout worktrees had before the
container, so nothing is orphaned. Clean matches that name by construction,
the prefix followed by a bare hex hash, so a checkout named `foo` never
claims a sibling checkout `foo-bar`'s worktrees.

Unlike retention eviction, the purge doesn't spare a worktree with
uncommitted work. Commit or copy anything you need from one first.

`--worktrees` is the explicit hammer. Cutting a new `--commit` worktree
already evicts the least-recently-used ones beyond `worktree_keep` (default
6, per host; see `brokkr man config worktree_keep`). That bound applies only
when a new worktree is created, so a project that has stopped growing never
shrinks by itself, which is what `--worktrees` is for. The bound can't
promise the disk stays clear either, since the count is per project.

Worktrees are keyed by the build root, the git repo they're cut from, even
when `brokkr.toml` sits one level up. The count reads `removed N of M
worktrees found` whenever the two numbers differ, so a purge that removed
nothing tells you how many it found. Each worktree has an isolated target
dir: about 1.3 GB apiece on the nautilus workload.

## all

`--all` is `--worktrees` plus `--cargo` with the default package.

## dry-run

`--dry-run` lists what would go without deleting anything: the output says
`would remove` or `would clean` where a real run says `removed` or
`cleaned`. Run it first on any project where you have placed files by hand
near a brokkr-designated directory.
