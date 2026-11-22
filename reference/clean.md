# Clean internals

This page records the rules `brokkr clean` is built on and the code that
enforces them: ownership by construction, where results live, the disk gate,
store maintenance and the worktree purge. What clean removes is in
`docs/commands/clean.md`.

## Ownership by construction

`src/clean.rs` removes only what brokkr created, identified by a
brokkr-designated directory or by a name brokkr can construct from
configured values. It never parses a filename to decide ownership.

That is the safety property, and every new removal must keep it. Names are
matched forward from configured values and never split back: dataset names
contain hyphens, so a filename can't be parsed into its parts unambiguously,
and a prefix test alone would let one name claim a dash-extending sibling's
files (`foo` claiming `foo-bar`'s). Anything the constructed matcher doesn't
claim is kept unconditionally.

## Results are not artifacts

A store whose contents cost real time to produce and can't be rebuilt from
the source tree is a result, and a result must not live in a directory
whose purpose is being safe to delete. Placing such a store in a
brokkr-designated directory is how clean spares it, with no special case:

- `brokkr crit` points `CRITERION_HOME` at `.brokkr/bench/` rather than
  criterion's default `target/criterion`, which also puts the baselines out
  of reach of `cargo clean`.

## Disk gate

`src/disk_gate.rs` is the lock-time free-space gate every locked command but
`clean` and `env` runs. It probes the invocation cwd's `target` and an
environment `CARGO_TARGET_DIR` only, and doesn't resolve cargo-config
target overrides.

## Store maintenance

`src/maintenance.rs` removes the files a SIGKILLed lock holder leaves, the
next time dispatch takes a fresh hold for a command whose spec locks. It
runs only there because only lock holders create those files, so a fresh
hold proves their writers are gone.

Atomic-replace temporaries (`.NAME.tmp-PID-N`) are outside maintenance on
purpose. Their writers don't all share one exclusion, and a lockless
`brokkr env` writes the hash cache, so no sweep can prove one dead.

Clean doesn't run after an interrupt either: the interrupted command has
released the lock by then, and another brokkr may already be using the same
paths.

## Worktree purge

`cmd_clean` receives the build root from dispatch and hands it to
`worktree::purge_all` (`src/worktree.rs`); it never recomputes a root from
cwd. Worktrees are keyed by the build root, the repo they're cut from and
the checkout whose path names their directory, and dispatch and clean must
agree on it. A function that recomputes a root it wasn't given states an
invariant it can't enforce. Under the config-one-level-up layout the project
root gives the wrong key, which is why the purge reports `removed N of M
worktrees found` whenever the two counts differ.

The purge doesn't spare a worktree with uncommitted work, unlike retention
eviction: it was asked for by name, and a purge that quietly kept some
would misreport what it reclaimed.
