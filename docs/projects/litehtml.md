# litehtml-rs project

This page covers the litehtml-rs project, selected by
`project = "litehtml-rs"` in `brokkr.toml`. It describes the visual store,
the `[litehtml]` config, the Node runtime cache and how `brokkr test` behaves
here. The sister project sluggrs shares the visual-test surface but uses a
different pipeline binary. The commands themselves (`visual`, `list`,
`approve`, `report`, `visual-status`, `prepare`, `html-extract`, `outline`)
are documented in `brokkr man visual`.

## Visual store

brokkr keeps litehtml's visual runs, results and approvals in
`.brokkr/litehtml-visual.db`. On first open it imports any existing
litehtml rows from `.brokkr/results.db`. `brokkr clean` never removes the
store.

Each scored result records the width it was measured at and the Chrome
reference it was scored against, and each approval records its width. Rows
older than that record read as width 800, the width every one of them was
measured at.

A run killed partway, by SIGKILL or otherwise, reads as unfinished and needs
no cleanup.

## Config

The `[litehtml]` table sets project-wide comparison settings, and each
`[[litehtml.fixture]]` entry declares one fixture. This example shows every
key:

```toml
[litehtml]
viewport_width = 800
mode = "ahem"                  # "ahem" or "default"
pixel_diff_threshold = 0.5
element_match_threshold = 95.0
fallback_aspect_ratio = 2.0    # optional, for prepare
regression_tolerance = 0.5     # optional, ratchet slack in pp
pixel_fuzz_percent = 5         # optional, per-channel match tolerance
position_tolerance = 2.0       # optional, element x/y tolerance in px
size_tolerance = 5.0           # optional, element w/h tolerance in px
offender_print_limit = 10      # optional, offenders per failing row

[[litehtml.fixture]]
id = "creatine_hero"
path = "fixtures/creatine_hero/creatine_hero.html"
tags = ["creatine"]
expected = "pass"
viewport_width = 720             # optional override
mode = "default"                 # optional override
pixel_diff_threshold = 0.4       # optional override
element_match_threshold = 96.0   # optional override
waive_element_threshold = false  # optional
notes = "narrow mobile layout"   # optional metadata
```

### Required keys

`[litehtml]` requires `viewport_width`, `mode`, `pixel_diff_threshold` and
`element_match_threshold`:

- **`viewport_width`**: the width both sides of the comparison render at.
  It sets the Chrome capture viewport and the pipeline's `--width`, which
  also drives the pipeline's media-query evaluation. It must be greater than
  zero.
- **`mode`**: `ahem` passes `--fixture` to the pipeline; `default` runs the
  pipeline without that flag.
- **`pixel_diff_threshold`, `element_match_threshold`**: percentages, in
  percentage points.

brokkr passes `--width` to the pipeline even at the pipeline's own default
of 800. A pipeline that doesn't accept the flag fails every fixture rather
than rendering at a width nobody configured. Changing a fixture's width makes
its stored reference and approval describe another comparison; see "Chrome
reference" and "Approvals and width" in `brokkr man visual`.

`fallback_aspect_ratio` is optional and used only by `prepare`. It must be
finite and greater than zero, and brokkr supplies a default when you omit it.

### Comparison policy

The comparison-policy keys are optional and project-wide. Each absent key
takes the value shown in the example:

- **`regression_tolerance`**: how far a score may move past its approval
  before it's a `REGRESSION`.
- **`pixel_fuzz_percent`**: an integer from 0 to 100, the per-channel
  difference still counted as a matching pixel.
- **`position_tolerance`, `size_tolerance`**: bounds on an element's geometry
  deltas before it's an offender.
- **`offender_print_limit`**: caps the offenders printed under a failing row.
  0 prints only the pointer to `offenders.txt`, which always holds the full
  list.

The tolerances must be finite and at least zero. `brokkr man visual`
describes how each one is applied.

### Fixtures

Each `[[litehtml.fixture]]` requires a unique, path-safe `id`, an HTML
`path`, and `expected = "pass"` or `"fail"`. The other keys are optional:

- **`tags`**: a list that `visual --suite` selects on.
- **`viewport_width`, `mode` and both thresholds**: override their
  `[litehtml]` values. A viewport override must be greater than zero.
- **`expected = "fail"`**: accepts a threshold failure as `EXPECTED_FAIL`. A
  passing render is still a `PASS`, and brokkr calls it out as a stale
  expectation.
- **`waive_element_threshold = true`**: disables the absolute element-match
  threshold for that fixture. brokkr still records the score, and an approved
  element score still acts as a regression ratchet.
- **`notes`**: metadata that `list` displays below the fixture. It doesn't
  change comparison behavior.

brokkr rejects these before a visual command runs: unknown `mode` or
`expected` values, duplicate IDs, and IDs that could escape the
`fixtures/ID` artifact directory.

## Node runtime cache

brokkr compiles the Node entry points and package files into its binary and
installs them under `$XDG_CACHE_HOME/brokkr/litehtml-prepare/KEY`. `KEY` is a
hash of the package, the lockfile and the script contents.

pnpm installs the dependencies there with `--frozen-lockfile`, once per key.
The install runs under an exclusive lock on a file in that directory, because
the cache is per user and runs in two trees can race. It has a 10-minute wall
bound. A failed or timed-out install removes the partial `node_modules` and
reports pnpm's stderr.

brokkr materializes the Chrome capture script in the same versioned
directory. The script then resolves the Puppeteer the package pins from that
directory's `node_modules`, and Puppeteer's matching Chrome build is
installed under it. `brokkr clean` in a litehtml tree removes the directories
of every other key.

## Test runner

`brokkr test` runs a single cargo test here, as it does in any project. It
doesn't overlap with `brokkr visual`: `visual` drives the fixture pipeline,
and `test` drives cargo.
