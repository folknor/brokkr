# crit design

This page records the decisions `brokkr crit` is built on, each with the
failure it prevents, so a change knows what it must keep. How the command is
used is in `docs/commands/crit.md`.

## Separate measure and compare

`cargo bench` with no baseline flags writes to `base` and compares against
whatever `base` held, so every run destroys the previous reference. crit
makes a measuring run always save under a name and makes comparison a
separate invocation that samples nothing (criterion's `--load-baseline`).
The expensive work happens once per commit. A change must not add a mode
that measures into a name another run already owns without being asked.

## Baseline naming

Names use the fixed-width `git::SHORT_HASH_LEN` abbreviation, never `git
rev-parse --short`, whose width grows with the repo, so a commit keeps its
name. `--compare` resolves a non-name as a revision so that baselines saved
under a shorter abbreviation still resolve.

A dirty tree is refused rather than named `HASH-dirty`. Edit, measure, edit,
measure is the most common way to use the command, and a dirty-suffixed name
would let each iteration silently overwrite the last. A `--name` label must
be one path component because brokkr joins it into its store and hands it
to criterion for its own paths; an absolute or `../` label would relocate
both.

## Environment stamp

A criterion baseline has no record of how its code was built, and criterion
reports a confident percentage between incomparable baselines. At the effect
sizes crit exists to resolve, a percent or two against criterion's 1%
default noise threshold, a toolchain bump is larger than the signal. Hence
the stamp, and `--compare` refusing on a mismatch.

The stamp's comparison rules each prevent a specific false answer:

- A digest of a cargo config stands in for its flags, because no stable
  cargo interface exposes them. Saying the file changed is enough to refuse.
- "Not there" is a recorded value (`(unset)`, `(absent)`), so unset against
  set refuses. "Could not read" is not a value and doesn't refuse on its
  own.
- Only fields both stamps recorded are compared, so adding a stamp field
  never invalidates the existing baselines.
- A stamp that exists but can't be read refuses, since the environments
  were never checked.

## Baseline location

`CRITERION_HOME` points at `.brokkr/bench/`. That name predates the command's
rename from `brokkr bench` to `brokkr crit` and was kept so the baselines
recorded before the rename keep reading; renaming the directory would orphan
them.

`.brokkr/` is anchored to the project root, not the build root, so driving a
checkout from a config one level up leaves the foreign repo with no
untracked files. That also keeps the build root's cleanliness, which decides
whether a baseline may be named after its commit, unaffected by crit's own
writes.

`--baselines` lists brokkr's stamp store, not criterion's tree, so a
criterion layout change can't break the listing. The empty-run check, which
looks for `BENCHMARK_ID/NAME/estimates.json`, is the one place brokkr reads
criterion's tree, and a layout change there refuses every save rather than
accepting empty ones.

## Cargo invocation

The build-purpose resolver in `src/build_purpose.rs` selects the bench
profile, default features and the worktree target override, as for every
build. `-p` comes from the bench target's own package, never the caller:
building several bench targets in one cargo invocation can link a crate
declaring `panic = "abort"` against a harness compiled to unwind, which
fails at link time. Deriving the package makes the one-crate-at-a-time rule
structural. Criterion knobs (`--sample-size`, `--measurement-time`,
`--noise-threshold`) pass through after `--` and are never fixed by brokkr,
because a near-noise-floor effect needs them tuned per study.

## Toolchain pin and dirty check

Taking the global lock moves a `disable_toolchain` project's
`rust-toolchain.toml` aside as a `.brokkr-disabled` sidecar, which `git
status` sees as a deletion plus an untracked file. So crit resolves the
baseline name, and runs the dirty-tree check, before it takes the lock,
while only you could have dirtied the tree. A `--commit` run leaves the live
root's pin alone and moves the worktree's pin once the checkout exists.

## Worktree target isolation

A `--commit` run pins `CARGO_TARGET_DIR` to
`TARGET/brokkr-worktrees/CHECKOUT_KEY/SHORT_HASH`. On a host that points
every checkout at one shared target dir, the artifact path doesn't encode
the worktree, cargo's freshness check is mtime-based, and a reused
worktree's sources are older than an artifact a later commit built. Cargo
would then skip the rebuild and the run would measure another commit's code
under the requested name, with nothing visibly wrong. The isolation costs a
full build per worktree; removing it reopens that misattribution.

## Criterion and iai targets

Nothing in cargo metadata or the manifest tells a criterion bench target
from an iai or plain libtest one: both are `kind: ["bench"]` with `harness
= false`. The only reliable test is behavioral (criterion implements
`--list`, iai doesn't), and asking costs a build, so the index lists every
bench target and a non-criterion one fails when the child rejects a flag.
