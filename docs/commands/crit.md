# brokkr crit

`brokkr crit` runs criterion benchmarks and keeps their baselines. It isn't
project-gated: like `run`, `deps` and `clippy`, it works in any Rust repo
with criterion bench targets. This page covers measuring and comparing,
baseline names, the environment gate, the baseline store, the cargo
invocation, the toolchain, worktree isolation, and how criterion targets are
told from iai ones.

These are the forms the command takes:

```sh
brokkr crit                       # list the workspace's bench targets
brokkr crit NAME                  # measure, saving a baseline named for HEAD
brokkr crit NAME --commit REF     # measure REF in a worktree
brokkr crit NAME --compare A B    # diff two stored baselines, sampling nothing
brokkr crit --baselines           # list what has been recorded
```

## Measuring and comparing

`brokkr crit` makes measuring and comparing two separate invocations.
`cargo bench` with no baseline flags writes to a baseline called `base` and
compares against whatever `base` held before, so every run destroys the
previous reference. That suits a tight edit loop but not a series of
commits, where the second measurement overwrites the first.

A measuring run always saves under a name. A comparison samples nothing:
criterion's `--load-baseline` reads the new side from storage instead of
measuring it. You pay for the measurement once per commit, then ask as many
questions of the result as you like.

A three-commit study is three measurements and as many comparisons as you
want:

```sh
brokkr crit black_scholes --commit 03062cce63
brokkr crit black_scholes --commit 8fa769d4eb
brokkr crit black_scholes
brokkr crit black_scholes --compare 03062cce 8fa769d4
```

`--compare A B` reads B against A: `A` is the reference and `B` the side
being judged, the same verb and argument order as `brokkr results
--compare`.

A comparison still builds and launches the bench binary. Sampling nothing
makes it cheap, not instant.

## Baseline names

A clean tree names its baseline after the short commit hash, which is
stable, meaningful and recoverable from the git log. The hash is always the
same fixed-width abbreviation, never `git rev-parse --short`'s width, which
depends on the repo's size. A commit keeps its baseline name as the repo
grows.

A `--commit` run takes its name from the commit the ref resolves to and
never consults the working tree. The worktree is a fresh checkout of exactly
that commit, so nothing uncommitted can make the name misleading.

`--compare` takes a recorded name as it is. brokkr resolves anything else as
a revision: a hash at any width, or a ref, finds the baseline saved for that
commit, including one saved under a shorter abbreviation.

For a run against the working tree, brokkr refuses a dirty tree unless you
pass `--name LABEL`. A label must be a single path component, since brokkr
joins it into its store and hands it to criterion for its own paths. An
absolute or `../` label would relocate both.

brokkr never falls back to a `HASH-dirty` name, so an edit, measure, edit,
measure loop can't silently overwrite its own baselines: give each iteration
its own `--name`. `--name` also labels a baseline for a reason git can't
express.

## Environment gate

Each saved baseline gets a stamp of the environment that built it, and
`--compare` refuses two baselines whose stamps disagree. Without the gate,
criterion would report a confident percentage between, say, a
`-Ctarget-cpu=native` build and one from another toolchain, and a toolchain
bump easily outweighs a one or two percent effect.

The stamp records:

- the `rustc` version
- the host triple
- the CPU model
- any `RUSTFLAGS` in the environment
- a digest of the `$CARGO_HOME` cargo config
- a digest of every `.cargo/` config from the build root upwards

In each directory brokkr digests the file cargo reads: the extensionless
legacy `config` when both it and `config.toml` exist (cargo warns and
ignores `config.toml`), otherwise whichever exists. The repo-local configs
matter as much as the user-level one. A project pins `target-cpu`, a linker
or target-specific rustflags there, cargo merges the two, and under
`--commit` such a file can differ between the sides you compare.

`--lenient` downgrades the refusal to a warning, because "these differ and I
know why" is a legitimate position. Not being told is not.

The gate has these limits:

- No stable cargo interface exposes the flags set in
  `~/.cargo/config.toml`, so the stamp records a digest of the file rather
  than the flags. It can say the file changed, not which flag. That is
  enough to refuse a comparison.
- "Not there" is a reading and "could not read" is not. An unset
  `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS`, a missing user cargo config and
  a build root with no `.cargo` config are recorded as `(unset)` or
  `(absent)` and compared like any value. Unset against set, or a config
  added between two commits, refuses. An ancestor above the build root
  stays one-sided when absent, since a `--commit` worktree's parent
  directories aren't the live tree's.
- brokkr compares only the fields both stamps recorded. A field one side
  never had isn't a difference, so a new field doesn't invalidate every
  existing baseline. A reading that failed, such as a digest brokkr
  couldn't compute or a CPU model the host doesn't report, doesn't refuse
  on its own.
- A stamp that exists but can't be read (permissions, I/O) isn't a pass:
  `--compare` refuses, since the environments were never checked.
  `--lenient` downgrades that to a warning like any other mismatch.

## Baseline store

brokkr keeps baselines under `.brokkr/bench/`, through criterion's
`CRITERION_HOME`, rather than criterion's default `target/criterion`.

`.brokkr/` is anchored to the project root, the directory holding
`brokkr.toml`, like every other brokkr state directory, not to the build
root where cargo runs. When the config sits one level above the working
directory, to drive a checkout that isn't yours, baselines stay in the
parent and the foreign repo gets no untracked files.

Neither `brokkr clean` nor `cargo clean` removes the baselines.

`--baselines` lists brokkr's own stamp store, not criterion's directory
tree.

## Empty runs

A measuring run that measured nothing is refused. Criterion exits 0 when the
filter after `--` matches no benchmark, so a mistyped filter or a renamed
benchmark id would otherwise save an empty baseline, stamped as if it were
real.

Every benchmark measured into baseline `NAME` writes
`BENCHMARK_ID/NAME/estimates.json` under the criterion home. A run that
wrote or rewrote none of them is refused with `cargo bench measured nothing
into baseline ...` and leaves no new stamp. If every save is refused this
way after a criterion upgrade, criterion has moved those files.

## Cargo invocation

brokkr builds and runs one bench target with this command:

```text
cargo bench -p OWNING_PACKAGE --bench TARGET --no-fail-fast
  -- CRITERION_ARGS
```

brokkr uses Cargo's bench profile and default features. This command has no
CLI feature surface and inherits no host features.

brokkr derives `-p` from the bench target's own package; you never supply
it, and each invocation builds one bench target, which avoids the link
failure several targets with mixed `panic` strategies can cause.

brokkr forwards everything after `--` to the criterion harness untouched.
That is where `--sample-size`, `--measurement-time` and `--noise-threshold`
go when an effect sits near the noise floor.

## Toolchain

`crit` takes the global lock, so brokkr moves a `disable_toolchain`
project's pinned `rust-toolchain.toml` aside while cargo runs, the same
mechanism every build path uses. Under that lock, a run that samples into a
baseline refuses while a `brokkr serve` server is live, since a serving
process perturbs the measurement. `--compare` and `--baselines` sample
nothing and aren't refused (`brokkr man serve`).

The move renames the pin to a `.brokkr-disabled` sidecar, which `git status`
shows as a deletion plus an untracked file while cargo runs. brokkr resolves
the baseline name and checks for a dirty tree before that, so the move never
makes a run read as dirty. A `--commit` run leaves the live root's pin alone
and moves the worktree's pin instead.

## Worktree isolation

A `--commit` run builds in a persistent worktree, and `brokkr crit` pins its
cargo invocation to a per-worktree `CARGO_TARGET_DIR`. brokkr's cargo build
and workspace metadata helpers use the same target dir. It sits under the
project's own target, at `TARGET/brokkr-worktrees/CHECKOUT_KEY/SHORT_HASH`,
reached through the worktree slot's `.brokkr-target` link.

The isolation applies even on a host that points every checkout at one
shared target dir. It stops cargo from reusing another commit's bench binary
and measuring it under the requested commit's name. The cost is a full build
per worktree.

brokkr doesn't announce the isolation on every run, since it is the same
each time. Its narration goes to `crit`'s run log,
`.brokkr/check-logs/crit-MS.log`, where brokkr keeps the newest ten.

Worktrees persist and are reused, so the cost is one cold build per commit
rather than per run. They are also expensive to keep, since the isolated
`target/` is the bulk of a worktree's footprint. Cutting a new one first
evicts the least recently used beyond `worktree_keep` (default 6). Eviction
costs a rebuild, never data, since baselines live in `.brokkr/bench/`, not
in the worktree. See `brokkr man config worktree_keep`.

Not recompiling is correct when you re-measure a commit whose worktree
already built: that is why worktrees persist. The isolation makes a skipped
rebuild safe, so a fast run isn't by itself a symptom.

## Criterion versus iai

brokkr can't tell a criterion bench target from an iai or plain libtest one
without building it, so the index lists every bench target. A baseline verb
aimed at a non-criterion target fails when the child rejects the flag. In a
workspace that mixes the two, the target names are your only guide.
