# litehtml-rs project notes

`project = "litehtml-rs"` in `brokkr.toml`. Sister project: sluggrs (same
visual-test surface, different pipeline binary).

## Module layout

- `src/litehtml/cmd.rs` - command dispatch. Also handles `prepare` /
  `extract` / `outline` by shelling out to a bundled Node.js script.
- `src/litehtml/capture.js` - Puppeteer reference capture script, bundled into
  brokkr and materialized in its per-user runtime cache.
- `src/litehtml/db.rs` - the litehtml adapter for `MechanicalDb`; shared
  versioned SQLite operations and the one-time import from the former shared
  file live in `src/visual/db.rs`. Its tables now live in
  `.brokkr/litehtml-visual.db`; existing `mechanical_*` rows are imported from
  `.brokkr/results.db` on first open. `results.db` keeps its independent schema
  version, owned by `ResultsDb`. `mechanical_runs.completed` is NULL for rows
  from before completion tracking (unknown), 0 from `insert_run` until the
  fixture loop ends, 1 once `finish` stamps it; readers treat only 0 as
  unfinished, so a SIGKILL needs no cleanup. Store version 3 records on each
  scored result the width it was measured at and the id of the Chrome
  reference it was scored against, and on each approval its width. Rows from
  before version 3 have neither; they read as 800, which is what every one of
  them measured: the pipeline was never passed a width before then and so
  rendered at its default of 800, and every litehtml configuration until then
  set 800 with no fixture override.
- `src/litehtml/reference.rs` - the published Chrome reference: staged
  publication, the `chrome.meta.json` commit marker, width and identity checks.
- `src/litehtml/compare.rs` - element geometry comparison.
- `src/visual/` - shared pixel comparison, typed visual statuses, run engine,
  database operations, and common command formatting. Run IDs use the shared
  UUID helpers in `src/db/types.rs`.
- `scripts/litehtml-prepare/` - Node.js fixture preprocessing (cheerio,
  image-size, and pngjs) and the pinned Puppeteer dependency used by reference
  capture. `prepare.js` handles `prepare`, `extract`, and `outline` subcommands.
  Dependencies are managed via pnpm (`package.json`, `pnpm-lock.yaml`) and
  installed in the per-user runtime cache with Puppeteer's matching Chrome
  build in a versioned subdirectory. A Rust unit test parses `INLINE_ELEMENTS`
  and checks that it covers every tag in `compare.rs`'s `is_inline_tag`. The
  sibling litehtml-rs repository's pipeline keeps a separate copy of that list.

## Commands

For the user-facing command list (visual / list / approve / report /
visual-status / prepare / html-extract / outline) see
`docs/commands/visual.md`.

## Config

```toml
[litehtml]
viewport_width = 800
mode = "ahem"                     # "ahem" or "default"
pixel_diff_threshold = 0.5
element_match_threshold = 95.0
fallback_aspect_ratio = 2.0  # optional, for prepare command

[[litehtml.fixture]]
id = "creatine_hero"
path = "fixtures/creatine_hero/creatine_hero.html"
tags = ["creatine"]
expected = "pass"
viewport_width = 720                 # optional fixture override
mode = "default"                     # optional fixture override
pixel_diff_threshold = 0.4           # optional fixture override
element_match_threshold = 96.0       # optional fixture override
waive_element_threshold = false      # optional; retain score and ratchet
notes = "narrow mobile layout"       # optional metadata
```

`[litehtml]` requires `viewport_width`, `mode`, `pixel_diff_threshold`, and
`element_match_threshold`. `viewport_width` is the width both sides of the
comparison render at: the Chrome capture viewport and the pipeline's
`--width`, which also drives its media-query evaluation. It must be greater
than zero. The pipeline is passed `--width` even at its own default of 800, so
a pipeline that does not accept the flag fails every fixture rather than
rendering at a width nobody configured. Changing a fixture's width makes its
stored reference and approval describe another comparison; see "Chrome
reference" and "Approvals and width" in `docs/commands/visual.md`. Both
thresholds are percentages in percentage points. `mode` is `ahem` (pass
`--fixture` to the pipeline) or `default` (run the pipeline without that flag).
The optional `fallback_aspect_ratio` is used only by `prepare` and must be
finite and greater than zero. When omitted, brokkr supplies its default value.
Fixture viewport overrides must also be greater than zero.

Each `[[litehtml.fixture]]` requires a unique, path-safe `id`, an HTML `path`,
and `expected = "pass" | "fail"`. `tags` is an optional list used by
`visual --suite`. Fixture `viewport_width`, `mode`, and both thresholds
override their `[litehtml]` values. `expected = "fail"` accepts a threshold
failure as `EXPECTED_FAIL`; a passing render is still a `PASS` and is called
out as a stale expectation. `waive_element_threshold = true` disables
the absolute element-match threshold for that fixture; its score is still
recorded and an approved element score still acts as a regression ratchet.
`notes` is optional metadata displayed below each fixture by `list`; it does
not change comparison behavior. Unknown mode/expected values, duplicate IDs,
and IDs that could escape the `fixtures/<id>` artifact directory are rejected
before a visual command runs.

The Node entry points and package files are compiled into brokkr and installed
under `$XDG_CACHE_HOME/brokkr/litehtml-prepare/<key>`, the key a hash of the
package, lockfile, and script contents. pnpm installs dependencies there with
`--frozen-lockfile`, once per key, under an exclusive lock on a file in that
directory (the cache is per user, so runs in two trees can race) and a
10-minute wall bound; a failed or timed-out install removes the partial
`node_modules` and reports pnpm's stderr. `brokkr clean` in a litehtml tree
removes the directories of every other key. The Chrome capture script is
materialized in the same versioned directory, so it resolves the Puppeteer the
package pins from that directory's `node_modules`, and Puppeteer's matching
Chrome build is installed under it.

## Test runner

`brokkr test` runs a single cargo test here like it does anywhere else. It used
to be refused in litehtml/sluggrs with a pointer to `brokkr visual`, from when
`test` *was* the visual runner; `visual` has been its own command for a long
time, and the two surfaces do not overlap - `visual` drives the fixture
pipeline, `test` drives cargo.
