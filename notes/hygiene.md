# Hygiene findings

Current gaps only: remove an entry when it is resolved, and delete the file
once none remain. An entry closed with no action owes a comment at the code
sites it names, so the finding is not hunted again. Entries carried over from
the hunts are as the hunters reported them, unverified.

## Validation pipeline

### No format gate in `brokkr check`

`brokkr check` never runs `cargo fmt --check`, so the tree drifts from rustfmt layout unnoticed until a layout-assuming textlint rule trips (a whole-tree `brokkr fmt` once moved a same-line `// gate:` marker off its call). The line-pattern rules document that they assume rustfmt layout, but nothing enforces it.

### `operator-prefixes-use-output` misses a wrapped print

The rule needs the printer, the opening parenthesis, an optional `&format!` and the tagged string on one physical line, so a rustfmt-wrapped call slips through:

```rust
eprintln!(
    "[build] something"
);
```

`warnings-use-output-levels` backs it up for `[warn]` only; other tags have no backup. No instance exists today.

### Check/test tests that cannot fail

The clippy-order and pinned-unification tests in `test_execution_tests.rs`, among other weak tests, cannot fail.

### Stale claims in check/test code and `check.md`

Stale test-phase comments were reported but never identified.

### Dead or unreachable code in the check/test pipeline

Further dead code in the check/test pipeline was reported but never identified or verified.

## Platform

### brokkr enforces very little of its own convention on itself

No broader print, panic, indexing or expect denies.

### Operator prefixes break the column

The 10-char prefix column is broken by longer tags (`[download] `, `[litehtml] `, `[sluggrs] `, `[ratatoskr] `, `[sidecar] `, `[history] `). Cosmetic, deliberately not taken so far.

### Operator path lines are not all on `output::path`

`output::path` (src/output/paths.rs) prints a project path relative to the invocation directory. Some output lines use it; most `DevError` text and the remaining `.display()` sites (install steps, guard, reservations, registry errors, fixture lint, nextest lane, rustflags, and others) still print absolute. Nothing enforces the helper: `.display()` is also correct for argv, stored values and foreign paths, so a textlint ban would need per-site exemptions. Options: migrate per module as touched, or a typed message representation that keeps paths as `Path` until rendering.

### `kill --hard` misses a dead leader's surviving group

A recorded process-group leader that has exited (skipped as gone, or failing its re-verify before `kill(-pgid)`) is not signalled as a group, so group members still alive are reached only by the descendant sweep - and not at all once they were reparented away from brokkr. Killing by pgid is safe while any member lives (the kernel keeps the number allocated), so a "group still has members" check, or the per-acquisition cgroup where delegation allows one, could cover it.

## Project layers

### The ratatoskr protocol list is spelled out field by field

`RatatoskrConfig` has nine `test_endpoint_env_*` fields instead of a protocol-keyed map.

### litehtml/sluggrs visual limits are not configurable

Visual thresholds and offender limits are hard-coded. Puppeteer is pinned, with an unverified hand-written lockfile.
