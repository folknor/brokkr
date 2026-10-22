# Visual reference testing

Gated to `project = "litehtml-rs"` and `project = "sluggrs"`. Visual reference
testing. Litehtml renders HTML fixtures and compares against Chrome captures;
sluggrs renders snapshots and compares them against approved PNGs.

All litehtml and sluggrs commands are top-level (no `brokkr litehtml` or
`brokkr sluggrs` namespace). Shared visual testing commands (`visual`, `list`,
`approve`, `report`, `visual-status`) dispatch to litehtml or sluggrs based on
the detected project.

`visual`, `approve`, and `prepare` follow the brokkr command lock policy.
They wait for the lock while building or changing visual baselines. `list`,
`report`, and `visual-status` do not acquire the command lock.

> Historical note: `visual` was formerly named `test`; that name is now owned
> by the generic cargo single-test runner - see `docs/commands/test.md`.

For project-specific fixture conventions and the prepare pipeline see
`docs/projects/litehtml.md` and `docs/projects/sluggrs.md`; each also documents
its project-specific config block.

## Commands

- `visual [ID] [--suite S] [--all] [--recapture]` - run visual fixtures.
  Litehtml uses Chrome reference artifacts; sluggrs uses approved PNGs. Builds
  the pipeline binary and produces pixel diff + element match comparison for
  litehtml, or a pixel diff for sluggrs. Give exactly one selector (an ID,
  `--suite`, or `--all`); combining them is refused rather than letting one
  silently win.
  `--suite` and `--recapture` are litehtml-only and refused on sluggrs.

  The build and the run's commit stamp use the code tree (cwd), so the
  one-level-up layout builds and describes the checkout, not the config
  dir; fixtures, snapshots and visual history stay under the config dir.
  Litehtml and sluggrs use separate versioned files at
  `.brokkr/litehtml-visual.db` and `.brokkr/sluggrs-visual.db`. The first open
  imports that subject's existing tables from `.brokkr/results.db`.

  A fixture whose Chrome reference (`chrome.png` + `chrome.json`) is missing
  is captured automatically only while it has no approval - its first run.
  Once approved, a missing reference is an `ERROR` naming the fix (restore
  it, or `--recapture`): the approved numbers were measured against that
  reference, and recapturing it silently would swap the baseline's ground
  truth. Each run deletes the previous `pipeline.png`/`pipeline.json`
  (sluggrs: `output.png`) before rendering, so a pipeline that stops writing
  one is an `ERROR`, not a comparison against last run's file.

  A capture, pipeline, or comparison failure is one `ERROR` row with the
  reason printed, and the run continues with the next fixture.

  Litehtml records whether a run finished (`completed` on `mechanical_runs`,
  store schema version 2). A run is stamped complete once its fixture loop has
  run to the end, however the fixtures scored. A run that was interrupted
  (`brokkr kill`, Ctrl-C), aborted by an error, or killed outright never gets
  the stamp. `report` on such a run prints an `INCOMPLETE RUN` line above the
  partial rows (kept for diagnosis), and `visual-status` and `approve` ignore
  its rows so partial numbers are never the "last run". Runs written before
  tracking have unknown completion: `report` notes that, and they still count
  for `visual-status` and `approve` exactly as before. Sluggrs does not track
  completion.

  Capture (litehtml, bundled Puppeteer script) awaits `document.fonts.ready`
  after load - `networkidle0` does not cover data-URI `@font-face` decoding,
  and measuring before Ahem is active captures fallback-font geometry. Each
  capture writes a `chrome.meta.json` sidecar (browser + puppeteer version,
  viewport, timestamp) so a silent Chrome update is attributable rather than
  looking like drift across all fixtures at once. The screenshot is
  `fullPage` at the measurement viewport - the viewport is never resized
  between the JSON dump and the PNG. The pinned Puppeteer package and its
  matching Chrome build are installed in the per-user script cache. If Chrome
  is missing, the error prints `PUPPETEER_CACHE_DIR=puppeteer pnpm exec
  puppeteer browsers install chrome` and the cache directory to run it from.
- `list` - show configured fixtures with tags, expected outcome, and approval
  state.
- `approve <ID>... | --all` - record current divergence as accepted baseline.
  Takes several IDs, or `--all` for every configured entry, because the
  natural flow is run `visual`, eyeball the images, approve the good ones,
  and commit the baselines once. The first bad ID stops the batch; IDs
  together with `--all` are refused.

  Only a scored result can be approved. Litehtml needs the latest result to
  carry both a pixel and an element score - an `ERROR` row is refused rather
  than stored as a perfect 0% baseline, and a missing element score would
  have disabled the element ratchet. Sluggrs refuses a snapshot whose latest
  run is `ERROR`; approving makes `output.png` the new `approved.png` and
  records a 0% pixel baseline, since that image is what later runs are
  compared against. The image is staged, the record written, then the image
  renamed into place, so a database failure leaves the old baseline intact.

  Requires a clean git tree, so that the commit an approval is pinned to
  actually describes what rendered the image. Sluggrs' own
  `snapshots/*/approved.png` is exempt from that check (`check_clean` in
  `src/git/tree_state.rs`, alongside `results.db`, `*.md` and `brokkr.toml`)
  because it is the approve operation's *output*, so it cannot invalidate the
  pin. Without the exemption, `approve` was self-blocking: the first one
  succeeded and every later one failed until you committed.
  The latest scored result must also point to a run recorded as clean at the
  same commit as the current checkout. If the checkout moved or the image came
  from a dirty run, rerun the visual command from the current clean tree before
  approving.
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
- **`head` is filtered from both sides** - the `head` element and its
  subtree, matched by whole path segment, so `thead` is scored.
- **Both dumps are required.** A missing or unreadable `pipeline.json` /
  `chrome.json` is an `ERROR`, never "no element score" (which enforced
  neither the element threshold nor the ratchet).
- **`br` is filtered from both sides.** Chrome emits `br` boxes; the
  pipeline folds line breaks into rich-text leaves and never will.
- **Chrome-only inline elements leave the denominator.** The pipeline
  folds plain CSS-inline tags (`span`/`a`/`b`/... - the local list mirrors the
  sister project's `is_inline_tag` in `src/style.rs`) into rich-text leaves,
  so a chrome-only inline element is a representation choice, not a
  defect. A Rust unit test checks that the local prepare list covers the
  scorer list; the sibling repository's separate copy still needs review when
  either list changes. Deliberately chrome-only-side only, unlike `br`: the
  pipeline *does* emit inline elements when they are display:inline-block (MJML
  nav links, buttons, social tables), and those path-matched comparisons
  catch real bugs, so they are always scored.
- **Zero-height reference elements are skipped.** Empty `tbody`/`tr`
  differ only in an invisible convention (Chrome: container width at h=0;
  pipeline: 0x0). They still anchor their children's frames.

Matched-but-out-of-tolerance elements are **offenders**: the full list
lands in `fixtures/<id>/offenders.txt` (worst first, deltas are
pipeline-minus-reference), and the highest-ranked offenders print below the
fixture's row when its status is `FAIL_THRESHOLD` or `REGRESSION`. The file is
removed only when a run finds that fixture completely clean - a PASS whose
score cleared the threshold can still keep a file listing its out-of-tolerance
stragglers.

The consumer repo is expected to gitignore `fixtures/*/offenders.txt`
(litehtml-rs does, d5c9b82) alongside the other run artifacts - it is
rewritten or deleted every run, so tracking it would dirty the tree in
both directions and block `approve`'s clean-tree check. Deliberately not
exempted in `check_clean` instead: git already treats ignored files as
clean, and the exemption list stays minimal so a pinned approval keeps
describing the tree.

## Statuses

`PASS`, `NO_BASELINE`, `FAIL_THRESHOLD`, `REGRESSION`, `EXPECTED_FAIL`,
`ERROR` (`visual::status::Status`, `src/visual/status.rs`). Litehtml's
element comparison remains in `src/litehtml/compare.rs`. Sluggrs reaches all
but `EXPECTED_FAIL`. `[[sluggrs.snapshot]]` has no expected-outcome field.

A litehtml `expected = "fail"` fixture that passes is reported as `PASS
(unexpected pass: ...)` and counted separately in the summary. It does not
fail the run - the render improved - but the stale expectation is named so it
gets dropped.

A sluggrs snapshot with an approval record but no `approved.png` is an
`ERROR`, not `NO_BASELINE`: the baseline was deleted, not never set.

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
  strips background-image/external CSS and remote-URL legacy `background`
  attributes, injects Ahem font, pretty-prints).
  Shells out to Node.js script. Image cache in `.brokkr/prepare-cache/`;
  successful responses are cached by URL hash in a bounded cache, with older
  entries pruned as needed. Failed fetches are not cached, so a later prepare
  retries them; responses above the size limit are refused. Fetches send a
  browser-like User-Agent.

  Fidelity rules (the prepared fixture should lay out like the raw email):
  author `width`/`height` attrs are never overwritten - the fetched
  natural size only fills in when the author provided neither, since the
  placeholder PNG itself carries the natural size; pre-existing image
  data URIs (JPEG/GIF/PNG) are re-encoded to gray placeholders too,
  idempotently; every author `font-family` declaration is rewritten to
  `'ahem'` (preserving `!important`) and author `@font-face` rules are
  removed, rather than fighting the cascade with a zero-specificity
  universal rule; the pretty-printer keeps runs of inline siblings
  verbatim on one line regardless of length, so it can't invent or drop
  whitespace between inline elements. The manual
  `scripts/litehtml-prepare/smoke.js` helper exercises these offline
  (`node scripts/litehtml-prepare/smoke.js`) after that package's dependencies
  are installed, including rejection of unknown options for all three
  subcommands; it is not part of `brokkr test` or `brokkr check`.
- `html-extract <input.html> [--selector S | --from S --to S] <output.html>` -
  extract sub-fixture from prepared HTML. `--selector` for single element,
  `--from`/`--to` for sibling range. Preserves ancestor context and table
  cell stubs. Known limitation: sibling `<td>` stubs are preserved but
  `<colgroup>` and other rows are not, so auto-layout column widths in a
  sub-fixture can legitimately differ from the full email.
- `outline <input.html> [--depth N] [--full] [--selectors]` - structural
  overview of prepared HTML showing sections, image dimensions, text
  previews, and suggested CSS selectors for extract.
