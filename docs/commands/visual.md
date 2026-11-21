# Visual reference testing

Visual reference testing is available in `project = "litehtml-rs"` and
`project = "sluggrs"`. Litehtml renders HTML fixtures and compares them
against Chrome captures; sluggrs renders snapshots and compares them against
approved PNGs. This page covers the commands, then the Chrome reference,
approvals and width, the pixel and element comparisons, the statuses, and
litehtml's fixture preprocessing commands.

All litehtml and sluggrs commands are top-level: there is no `brokkr
litehtml` or `brokkr sluggrs` namespace. The shared visual testing commands
(`visual`, `list`, `approve`, `report`, `visual-status`) dispatch to litehtml
or sluggrs based on the detected project. The generic cargo single-test
runner is `brokkr test` (`brokkr man test`), not a visual command.

`visual`, `approve` and `prepare` follow the brokkr command lock policy: they
wait for the lock while building or changing visual baselines. `list`,
`report` and `visual-status` don't take the command lock.

For project-specific fixture conventions, the prepare pipeline and each
project's config block, see `brokkr man litehtml` and `brokkr man sluggrs`.

## visual

`visual [ID] [--suite S] [--all] [--recapture]` runs visual fixtures. brokkr
builds the pipeline binary and compares its output against the reference:

- **Litehtml**: against Chrome reference artifacts, as a pixel diff plus an
  element match comparison.
- **Sluggrs**: against approved PNGs, as a pixel diff.

Give exactly one selector: an ID, `--suite` or `--all`. Combining them is
refused rather than letting one silently win. `--suite` and `--recapture`
are litehtml-only and refused on sluggrs.

The build and the run's commit stamp use the code tree (cwd), so the
one-level-up layout builds and describes the checkout, not the config dir.
Fixtures, snapshots and visual history stay under the config dir. Litehtml
and sluggrs keep separate versioned stores, `.brokkr/litehtml-visual.db` and
`.brokkr/sluggrs-visual.db`. The first open imports that subject's existing
tables from `.brokkr/results.db`.

A litehtml fixture whose Chrome reference isn't usable at the width it
renders at (see Chrome reference) is captured automatically only while it has
no approval, which is its first run. Once it has any approval, even one
measured at another width, an unusable reference is an `ERROR` naming the
fix: restore the reference, or `--recapture`. The approved numbers were
measured against the reference that is there, and recapturing it silently
would swap the baseline's ground truth.

Each run deletes the previous `pipeline.png` and `pipeline.json` (sluggrs:
`output.png`) before rendering. A pipeline that stops writing one is an
`ERROR`, not a comparison against last run's file.

A capture, pipeline or comparison failure is one `ERROR` row with the reason
printed, and the run continues with the next fixture. That includes a
failure to install the capture's script dependencies or Chrome for a
fixture's first capture. `--recapture` installs them once before the run
starts, and a failure there ends the command before any fixture runs.

### Completion

Litehtml records whether a run finished, in `completed` on `mechanical_runs`.
brokkr stamps a run complete once its fixture loop has run to the end,
however the fixtures scored. A run that was interrupted (`brokkr kill`,
Ctrl-C), aborted by an error, or killed outright never gets the stamp.

On an unstamped run, `report` prints an `INCOMPLETE RUN` line above the
partial rows, which are kept for diagnosis. `visual-status` and `approve`
ignore its rows, so partial numbers are never the "last run". Runs written
before completion tracking have unknown completion: `report` notes that, and
they still count for `visual-status` and `approve`. Sluggrs doesn't track
completion.

### Capture

Litehtml captures with a bundled Puppeteer script, which awaits
`document.fonts.ready` after load. `networkidle0` doesn't cover data-URI
`@font-face` decoding, and measuring before Ahem is active captures
fallback-font geometry. The screenshot is `fullPage` at the measurement
viewport, and the viewport is never resized between the JSON dump and the
PNG.

Each capture writes a `chrome.meta.json` sidecar: browser and Puppeteer
version, viewport and timestamp. A silent Chrome update is then attributable,
rather than looking like drift across all fixtures at once. brokkr adds the
reference id to the sidecar and uses it as the reference's commit marker
(see Chrome reference).

The pinned Puppeteer package and its matching Chrome build are installed in
the per-user script cache. If Chrome is missing, the error prints
`PUPPETEER_CACHE_DIR=puppeteer pnpm exec puppeteer browsers install chrome`
and the cache directory to run it from.

## list

`list` shows the configured fixtures with their tags, expected outcome and
approval state.

## approve

`approve ID... | --all` records the current divergence as the accepted
baseline. It takes several IDs, or `--all` for every configured entry. The
natural flow is to run `visual`, look at the images, approve the good ones,
and commit the baselines once. The first bad ID stops the batch, and IDs
together with `--all` are refused.

Only a scored result can be approved:

- **Litehtml** needs the latest result to have both a pixel and an element
  score. An `ERROR` row is refused rather than stored as a perfect 0%
  baseline, and a missing element score would disable the element ratchet.
- **Sluggrs** refuses a snapshot whose latest run is `ERROR`. Approving
  makes `output.png` the new `approved.png` and records a 0% pixel baseline,
  since that image is what later runs are compared against. brokkr stages the
  image, writes the record, then renames the image into place, so a database
  failure leaves the old baseline intact.

`approve` requires a clean git tree, so the commit an approval is pinned to
describes what rendered the image. The clean-tree check exempts
`results.db`, `*.md`, `brokkr.toml` and sluggrs' own
`snapshots/*/approved.png`. The image is the approve operation's output, so
it can't invalidate the pin; without the exemption, every `approve` after
the first would refuse until you committed.

The latest scored result must also come from a run recorded as clean at the
same commit as the current checkout. If the checkout moved or the image came
from a dirty run, rerun the visual command from the current clean tree
before approving.

Litehtml also requires that result to be the comparison a run would make
now. It must be measured at the width the fixture renders at, and scored
against the Chrome reference published now (see Approvals and width).

## report

`report RUN_ID` shows the results table for a past test run.

## visual-status

`visual-status` is a dashboard of all fixtures: the approved baseline
against the last run, the delta and improvements.

Litehtml shows a delta only when the approval, the last run and the
fixture's current width agree. An approval measured at another width shows
that width, and a scored last run at another width shows as `STALE` with its
width. A scored last run at the current width is also `STALE` whenever
`approve` would refuse it for its reference:

- it records no reference
- the reference was replaced since
- the reference isn't usable
- the published reference can't be read

An `ERROR` last run records no width and is shown as it is.

## Chrome reference

In litehtml, a fixture's reference is three files in `fixtures/ID/`:
`chrome.png`, `chrome.json` and the `chrome.meta.json` sidecar. It is usable
for a run when all three are there, the sidecar parses, and the viewport
width it records is the width the fixture renders at (`viewport_width`; see
`brokkr man litehtml`).

brokkr renders the pipeline at that same width, so it never scores a
reference captured at another width against it. The pixel diff pads the
smaller image rather than failing, so a cross-width comparison would be a
plausible, silent score.

brokkr writes a capture to `.brokkr/litehtml-capture/ID/` first. Only once
it is complete and at the requested width does brokkr give it a fresh
reference id and publish it:

1. Remove the old `chrome.meta.json`; a failure to remove it stops before
   anything else is replaced.
2. Move in the PNG and JSON.
3. Move in the new sidecar last.

The sidecar is the commit marker: one that is present describes the PNG and
JSON beside it. A capture interrupted at any point leaves either the old
reference untouched or no sidecar at all, which is unusable. That holds with
brokkr as the only writer under the command lock; brokkr doesn't detect
hand-edited reference files.

A sidecar from before reference ids existed gets one on the fixture's next
run, by rewriting the sidecar in place. A sidecar whose id is present but not
a non-empty string makes the reference unusable.

## Approvals and width

In litehtml, each scored result records the width it was measured at and the
id of the reference it was scored against, and each approval records the
width its result was measured at. Rows from before these were recorded read
as 800, which is what every one of them measured (see `brokkr man
litehtml`).

An approval ratchets only the comparison it measured. At another width it
doesn't apply: the fixture scores as unapproved (`NO_BASELINE`,
`FAIL_THRESHOLD` or `EXPECTED_FAIL`) and the run prints a note naming the
approved width. It isn't an `ERROR`, because `approve` refuses an `ERROR`
result and the fixture could then never be approved at its new width.

So a fixture moved from 800 to 720 runs `ERROR` until `--recapture`, since
its reference is at 800 and it has an approval. It then scores unapproved at
720 until you approve a 720 result. A fixture keeps one approval, so
approving at 720 replaces the 800 one. If the fixture moves back before
that, the 800 approval applies again once an 800 reference is captured.

`approve` refuses a latest result in these cases:

- it was measured at another width than the fixture renders at
- it doesn't record its reference
- its reference is no longer the published one, for example after an
  interrupted `--recapture` run, which leaves the earlier run's result as the
  latest while the reference it was scored against has been replaced

An explicit `--recapture` at the same width keeps the approval. It is the
documented, deliberate swap of the ground truth, and the approval then
ratchets scores against the new reference. That lets you judge a Chrome bump
against the prior baseline: a score that moved reads as a `REGRESSION` or an
improvement, and approving the new run re-baselines.

Keying approvals by reference instead would leave every recaptured fixture
unapproved, hiding that movement. References are local files brokkr
publishes, so the only path that replaces one under an approval is that
explicit flag; brokkr doesn't detect a hand edit of the files.

## Pixel comparison

Both subjects pad the two PNGs to the larger size and count a pixel as
different when any of its R, G or B channels differs by more than
`pixel_fuzz_percent` (default 5) percent of 255, rounded to the nearest step
(13 at the default). Alpha is not compared. The pixel score is the share of
differing pixels, and `diff.png` marks them red.

## Element comparison

In litehtml, `compare_elements` joins the pipeline and Chrome layout dumps by
dom path, such as `html>body[0]>div[2]`. Both sides use the same convention,
and the matcher is not positional. The scoring is designed to be honest
rather than flattering:

- **Parent-relative positions**: brokkr compares x and y relative to the
  parent's box when the parent resolves in both dumps; roots and orphaned
  elements fall back to absolute. Cumulative document drift then cancels out
  instead of failing every element below the drift point.
  Integer-versus-fractional line-height rounding accumulates about 6px over
  a 10,000px email, for example. The position tolerance
  (`[litehtml] position_tolerance`, default 2px) means "sits wrong inside its
  parent". Sizes (`size_tolerance`, default 5px) are always absolute.
- **`head` is filtered from both sides**: the `head` element and its
  subtree, matched by whole path segment, so `thead` is scored.
- **Both dumps are required**: a missing or unreadable `pipeline.json` or
  `chrome.json` is an `ERROR`, never "no element score", which would enforce
  neither the element threshold nor the ratchet.
- **`br` is filtered from both sides**: Chrome emits `br` boxes, and the
  pipeline folds line breaks into rich-text leaves and never will emit them.
- **Chrome-only inline elements leave the denominator**: see the next
  paragraph.
- **Zero-height reference elements are skipped**: empty `tbody` and `tr`
  differ only in an invisible convention (Chrome: container width at h=0;
  pipeline: 0x0). They still anchor their children's frames.

The pipeline folds plain CSS-inline tags (`span`, `a`, `b` and so on) into
rich-text leaves, so a Chrome-only inline element is a representation
choice, not a defect. The local tag list mirrors the sister project's
`is_inline_tag` in its `src/style.rs`. A Rust unit test checks that the local
prepare list covers the scorer list; the sibling repository's separate copy
still needs review when either list changes.

Unlike `br`, this filter applies to the Chrome-only side only. The pipeline
does emit inline elements when they are `display:inline-block` (MJML nav
links, buttons, social tables), and those path-matched comparisons catch real
bugs, so they are always scored.

Matched elements outside tolerance are offenders. brokkr writes the full list
to `fixtures/ID/offenders.txt`, worst first, with deltas as pipeline minus
reference. The highest-ranked offenders, up to
`[litehtml] offender_print_limit` (default 10), print below the fixture's row
when its status is `FAIL_THRESHOLD` or `REGRESSION`.

brokkr removes the file only when a run finds that fixture completely clean.
A `PASS` whose score cleared the threshold can still keep a file listing its
out-of-tolerance stragglers.

The consumer repo should gitignore `fixtures/*/offenders.txt` (litehtml-rs
does) alongside the other run artifacts. brokkr rewrites or deletes it every
run, so tracking it would dirty the tree in both directions and block
`approve`'s clean-tree check. The clean-tree check deliberately doesn't
exempt it: git already treats ignored files as clean, and the exemption list
stays minimal so a pinned approval keeps describing the tree.

## Statuses

A fixture or snapshot ends in one of `PASS`, `NO_BASELINE`,
`FAIL_THRESHOLD`, `REGRESSION`, `EXPECTED_FAIL` or `ERROR`. Sluggrs reaches
all but `EXPECTED_FAIL`, since `[[sluggrs.snapshot]]` has no
expected-outcome field.

A litehtml `expected = "fail"` fixture that passes is reported as `PASS
(unexpected pass: ...)` and counted separately in the summary. It doesn't
fail the run, since the render improved, but the stale expectation is named
so you drop it.

A sluggrs snapshot with an approval record but no `approved.png` is an
`ERROR`, not `NO_BASELINE`: the baseline was deleted, not never set.

`REGRESSION` is the ratchet against the approved baseline, on either metric:
the pixel diff rising more than the subject's `regression_tolerance`
(default 0.5pp) above the approved value, or the element match dropping more
than it below. The same tolerance decides the `(improved)` annotation in
`status`. Absolute thresholds can't be meaningful across fixtures
(denominator bias, email length), so the ratchet is the gate and absolute
numbers are for humans.

`NO_BASELINE` means the render succeeded but nothing has been approved yet.
It is not a failure and doesn't set the exit code: it is the expected state
of every newly registered snapshot. Folded into `FAIL_THRESHOLD`, a first run
on a fresh project would report "4 failed" for what is "4 awaiting
approval".

brokkr records a snapshot whose binary exits 0 but omits the
`{"adapter":...}` stdout line as one `ERROR` row; it doesn't abort the
remaining snapshots.

## prepare

`prepare INPUT.html OUTPUT.html` normalizes raw email HTML into a
self-contained litehtml fixture. It does these:

- replaces images with correctly-sized gray PNGs
- strips `background-image`, external CSS and remote-URL legacy `background`
  attributes
- injects the Ahem font
- pretty-prints

`prepare` shells out to a Node.js script. Its image cache is in
`.brokkr/prepare-cache/`: brokkr caches successful responses by URL hash in
a bounded cache, pruning older entries as needed. Failed fetches aren't
cached, so a later prepare retries them, and responses above the size limit
are refused. Fetches send a browser-like User-Agent.

The prepared fixture should lay out like the raw email, so `prepare` follows
these fidelity rules:

- It never overwrites author `width` and `height` attributes. The fetched
  natural size fills in only when the author provided neither, since the
  placeholder PNG itself has the natural size.
- It re-encodes pre-existing image data URIs (JPEG, GIF, PNG) to gray
  placeholders too, idempotently.
- It rewrites every author `font-family` declaration to `'ahem'`, keeping
  `!important`, and removes author `@font-face` rules, rather than fighting
  the cascade with a zero-specificity universal rule.
- The pretty-printer keeps runs of inline siblings verbatim on one line
  regardless of length, so it can't invent or drop whitespace between inline
  elements.

The manual `scripts/litehtml-prepare/smoke.js` helper exercises these rules
offline (`node scripts/litehtml-prepare/smoke.js`) once that package's
dependencies are installed, including the rejection of unknown options for
all three subcommands. It isn't part of `brokkr test` or `brokkr check`.

## html-extract

`html-extract INPUT.html [--selector S | --from S --to S] OUTPUT.html`
extracts a sub-fixture from prepared HTML: `--selector` for a single
element, `--from` and `--to` for a sibling range. It keeps ancestor context
and table cell stubs.

Sibling `<td>` stubs are kept, but `<colgroup>` and other rows are not, so
auto-layout column widths in a sub-fixture can legitimately differ from the
full email.

## outline

`outline INPUT.html [--depth N] [--full] [--selectors]` prints a structural
overview of prepared HTML: sections, image dimensions, text previews and
suggested CSS selectors for `html-extract`.
