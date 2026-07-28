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

  Capture (litehtml, embedded puppeteer script) awaits `document.fonts.ready`
  after load - `networkidle0` does not cover data-URI `@font-face` decoding,
  and measuring before Ahem is active captures fallback-font geometry. Each
  capture writes a `chrome.meta.json` sidecar (browser + puppeteer version,
  viewport, timestamp) so a silent Chrome update is attributable rather than
  looking like drift across all fixtures at once. The screenshot is
  `fullPage` at the measurement viewport - the viewport is never resized
  between the JSON dump and the PNG.
- `list` - show configured fixtures with tags, expected outcome, and approval
  state.
- `approve <ID>... | --all` - record current divergence as accepted baseline.
  Takes several IDs, or `--all` for every configured entry, because the
  natural flow is run `visual`, eyeball the images, approve the good ones,
  and commit the baselines once. The first bad ID stops the batch.

  Requires a clean git tree, so that the commit an approval is pinned to
  actually describes what rendered the image. Sluggrs' own
  `snapshots/*/approved.png` is exempt from that check (`check_clean` in
  `src/git.rs`, alongside `results.db`, `*.md` and `brokkr.toml`) - it is the
  approve operation's *output*, so it cannot invalidate the pin. Without the
  exemption `approve` was self-blocking: the first one succeeded and every
  later one failed until you committed.
- `report <run_id>` - show results table for a past test run.
- `visual-status` - dashboard: all fixtures with approved baseline vs last
  run, delta, improvements.

## Element comparison (litehtml only)

`compare_elements` joins the pipeline and Chrome layout dumps by dom path
(`html>body[0]>div[2]` - the same convention on both sides; the matcher is
not positional). Scoring is designed to be honest rather than flattering:

- **Parent-relative positions.** x/y are compared relative to the parent's
  box when the parent resolves in both dumps (roots and orphaned elements
  fall back to absolute). Cumulative document drift - integer-vs-fractional
  line-height rounding accumulates ~6px over a 10,000px email - cancels
  out instead of failing every element below the drift point, so the 2px
  position tolerance means "sits wrong inside its parent". Sizes (5px
  tolerance) are always absolute.
- **`br` is filtered from both sides.** Chrome emits `br` boxes; the
  pipeline folds line breaks into rich-text leaves and never will.
- **Zero-height reference elements are skipped.** Empty `tbody`/`tr`
  differ only in an invisible convention (Chrome: container width at h=0;
  pipeline: 0x0). They still anchor their children's frames.

Matched-but-out-of-tolerance elements are **offenders**: the full list
lands in `fixtures/<id>/offenders.txt` (worst first, deltas are
pipeline-minus-reference; stale files are removed on clean runs), and the
top 10 print under the fixture's row when its status is `FAIL_THRESHOLD`
or `REGRESSION`.

The consumer repo is expected to gitignore `fixtures/*/offenders.txt`
(litehtml-rs does, d5c9b82) alongside the other run artifacts - it is
rewritten or deleted every run, so tracking it would dirty the tree in
both directions and block `approve`'s clean-tree check. Deliberately not
exempted in `check_clean` instead: git already treats ignored files as
clean, and the exemption list stays minimal so a pinned approval keeps
describing the tree.

## Statuses

`PASS`, `NO_BASELINE`, `FAIL_THRESHOLD`, `REGRESSION`, `EXPECTED_FAIL`,
`ERROR` (`compare::Status`, `src/litehtml/compare.rs`). Sluggrs reaches all
but `EXPECTED_FAIL`, which needs a per-fixture `expected_fail` flag that
`[[sluggrs.snapshot]]` does not have.

`REGRESSION` is the ratchet against the approved baseline, on either
metric: pixel diff rising more than 0.5pp above the approved value, or
element match dropping more than 0.5pp below it. Absolute thresholds
can't be meaningful across fixtures (denominator bias, email length);
the ratchet is the gate, absolute numbers are for humans.

`NO_BASELINE` means the render succeeded but nothing has been approved yet.
It is **not** a failure and does not set the exit code - it is the expected
state of every newly registered snapshot, and folding it into
`FAIL_THRESHOLD` made a first run on a fresh project report "4 failed" for
what was really "4 awaiting approval".

A snapshot whose binary exits 0 but omits the `{"adapter":...}` stdout line
is recorded as one `ERROR` row; it does not abort the remaining snapshots.

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
