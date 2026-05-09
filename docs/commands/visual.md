# Visual reference testing

Gated to `project = "litehtml-rs"` and `project = "sluggrs"`. Visual reference
testing - renders HTML fixtures through a pipeline binary, compares against
Chrome screenshots.

All litehtml and sluggrs commands are top-level (no `brokkr litehtml` or
`brokkr sluggrs` namespace). Shared visual testing commands (`visual`, `list`,
`approve`, `report`, `visual-status`) dispatch to litehtml or sluggrs based on
the detected project.

> Historical note: `visual` was formerly named `test`; that name is now owned
> by the generic cargo single-test runner - see `docs/commands/check.md`.

For project-specific fixture conventions and the prepare pipeline see
`docs/projects/litehtml.md`. For the `[litehtml]` config block see
`docs/brokkr.toml.md`.

## Commands

- `visual [ID] [--suite S] [--all] [--recapture]` - run fixtures against
  Chrome reference artifacts. Builds pipeline binary, produces pixel diff +
  element match comparison. `--suite` and `--recapture` are litehtml-only.
- `list` - show configured fixtures with tags, expected outcome, and approval
  state.
- `approve <ID>` - record current divergence as accepted baseline (requires
  clean git tree).
- `report <run_id>` - show results table for a past test run.
- `visual-status` - dashboard: all fixtures with approved baseline vs last
  run, delta, improvements.

## Fixture preprocessing (litehtml only)

- `prepare <input.html> <output.html>` - normalize raw email HTML into a
  self-contained fixture (replaces images with correctly-sized gray PNGs,
  strips background-image/external CSS, injects Ahem font, pretty-prints).
  Shells out to Node.js script. Image cache in `.brokkr/prepare-cache/`.
- `html-extract <input.html> [--selector S | --from S --to S] <output.html>` -
  extract sub-fixture from prepared HTML. `--selector` for single element,
  `--from`/`--to` for sibling range. Preserves ancestor context and table
  cell stubs.
- `outline <input.html> [--depth N] [--full] [--selectors]` - structural
  overview of prepared HTML showing sections, image dimensions, text
  previews, and suggested CSS selectors for extract.
