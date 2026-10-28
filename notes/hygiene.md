# Hygiene findings

## No format gate in `brokkr check`

`brokkr check` never runs `cargo fmt --check`, so the tree drifts from rustfmt layout unnoticed until a layout-assuming textlint rule trips (a whole-tree `brokkr fmt` once moved a same-line `// gate:` marker off its call). The line-pattern rules document that they assume rustfmt layout, but nothing enforces it.

## `operator-prefixes-use-output` misses a wrapped print

The rule needs the printer, the opening parenthesis, an optional `&format!` and the tagged string on one physical line, so a rustfmt-wrapped call slips through:

```rust
eprintln!(
    "[build] something"
);
```

`warnings-use-output-levels` backs it up for `[warn]` only; other tags have no backup. No instance exists today.

## Operator path lines are not all on `output::path`

`output::path` (src/output/paths.rs) prints a project path relative to the invocation directory. The artefact, manifest, pins, verify, download and elivagar/nidhogg/litehtml output lines use it; most `DevError` text and the remaining `.display()` sites (install steps, guard, reservations, registry errors, fixture lint, nextest lane, rustflags, and others) still print absolute. Nothing enforces the helper: `.display()` is also correct for argv, stored values and foreign paths, so a textlint ban would need per-site exemptions. Options: migrate per module as touched, or a typed message representation that keeps paths as `Path` until rendering.
