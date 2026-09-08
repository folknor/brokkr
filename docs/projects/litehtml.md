# litehtml-rs project notes

`project = "litehtml-rs"` in `brokkr.toml`. Sister project: sluggrs (same
visual-test surface, different pipeline binary).

## Module layout

- `src/litehtml/cmd.rs` - command dispatch. Also handles `prepare` /
  `extract` / `outline` by shelling out to a Node.js script.
- `src/litehtml/db.rs` - `MechanicalDb`.
- `src/litehtml/compare.rs` - pixel + element comparison.
- `src/litehtml/mod.rs` - UUID generation.
- `scripts/litehtml-prepare/` - Node.js fixture preprocessing (cheerio +
  pngjs). `prepare.js` handles `prepare`, `extract`, and `outline`
  subcommands. Dependencies managed via pnpm (`package.json`,
  `pnpm-lock.yaml`).

## Commands

For the user-facing command list (visual / list / approve / report /
visual-status / prepare / html-extract / outline) see
`docs/commands/visual.md`.

## Config

```toml
[litehtml]
viewport_width = 800
mode = "ahem"
pixel_diff_threshold = 0.5
element_match_threshold = 95.0
fallback_aspect_ratio = 2.0  # optional, for prepare command

[[litehtml.fixture]]
id = "creatine_hero"
path = "fixtures/creatine_hero/creatine_hero.html"
tags = ["creatine"]
expected = "pass"
```

See `docs/brokkr.toml.md` for full schema.

## Test runner

`brokkr test` runs a single cargo test here like it does anywhere else. It used
to be refused in litehtml/sluggrs with a pointer to `brokkr visual`, from when
`test` *was* the visual runner; `visual` has been its own command for a long
time, and the two surfaces do not overlap - `visual` drives the fixture
pipeline, `test` drives cargo.
