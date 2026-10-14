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
  version, owned by `ResultsDb`.
- `src/litehtml/compare.rs` - element geometry comparison.
- `src/visual/` - shared pixel comparison, typed visual statuses, run engine,
  database operations, and common command formatting. Run IDs use the shared
  UUID helpers in `src/db/types.rs`.
- `scripts/litehtml-prepare/` - Node.js fixture preprocessing (cheerio,
  image-size, and pngjs). `prepare.js` handles `prepare`, `extract`, and `outline`
  subcommands. Dependencies managed via pnpm (`package.json`,
  `pnpm-lock.yaml`). A Rust unit test parses its `INLINE_ELEMENTS` and checks
  that it covers every tag in `compare.rs`'s `is_inline_tag`. The sibling
  litehtml-rs repository's pipeline keeps a separate copy of that list.

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
`element_match_threshold`. `viewport_width` sets the Chrome capture viewport
and must be greater than zero; both thresholds are percentages in percentage
points. `mode` is `ahem` (pass
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
removes the directories of every other key. The Chrome capture still resolves
Puppeteer from the global npm module path; the checked-in litehtml package does
not yet pin Puppeteer.

## Test runner

`brokkr test` runs a single cargo test here like it does anywhere else. It used
to be refused in litehtml/sluggrs with a pointer to `brokkr visual`, from when
`test` *was* the visual runner; `visual` has been its own command for a long
time, and the two surfaces do not overlap - `visual` drives the fixture
pipeline, `test` drives cargo.
