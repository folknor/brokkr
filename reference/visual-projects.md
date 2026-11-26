# Visual projects

This page records how brokkr's litehtml-rs and sluggrs support is built: the
module layout, the shared visual engine and the per-project visual stores.
How the commands and config are used is in `docs/projects/litehtml.md`,
`docs/projects/sluggrs.md` and `docs/commands/visual.md`.

## Shared visual engine

`src/visual/` holds what both projects share:

- pixel comparison
- typed visual statuses
- the run engine
- database operations, including the versioned SQLite layer in
  `src/visual/db.rs`
- common command formatting

Run IDs use the shared UUID helpers in `src/db/types.rs`. sluggrs doesn't
depend on litehtml's comparison module; element geometry comparison is
litehtml's alone.

## litehtml modules

The litehtml code is split across these modules:

- **`src/litehtml/cmd.rs`**: command dispatch. It also handles `prepare`,
  `extract` and `outline` by shelling out to a bundled Node.js script.
- **`src/litehtml/capture.js`**: the Puppeteer reference capture script,
  bundled into brokkr and materialized in brokkr's per-user runtime cache.
- **`src/litehtml/db.rs`**: the litehtml adapter for `MechanicalDb`.
- **`src/litehtml/reference.rs`**: the published Chrome reference: staged
  publication, the `chrome.meta.json` commit marker, and the width and
  identity checks.
- **`src/litehtml/compare.rs`**: element geometry comparison.
- **`scripts/litehtml-prepare/`**: Node.js fixture preprocessing (cheerio,
  image-size and pngjs) and the pinned Puppeteer dependency that reference
  capture uses. `prepare.js` handles the `prepare`, `extract` and `outline`
  subcommands.

The `scripts/litehtml-prepare/` dependencies are managed with pnpm
(`package.json`, `pnpm-lock.yaml`). A Rust unit test parses
`INLINE_ELEMENTS` and checks that it covers every tag in `compare.rs`'s
`is_inline_tag`. The sibling litehtml-rs repository's pipeline keeps a
separate copy of that list, so a tag added on one side has to be added on
the other by hand.

## litehtml visual store

The litehtml visual tables live in `.brokkr/litehtml-visual.db`. On first
open, brokkr imports existing `mechanical_*` rows from `.brokkr/results.db`
through the one-time import in `src/visual/db.rs`. `results.db` keeps its own
schema version, owned by `ResultsDb`.

The `mechanical_runs.completed` column has three states:

- **NULL**: the row predates completion tracking, so its state is unknown.
- **0**: `insert_run` wrote the row and the fixture loop hasn't ended.
- **1**: `finish` stamped the run complete.

Readers treat only 0 as unfinished, so a SIGKILL needs no cleanup.

Store version 3 records the width each scored result was measured at and the
id of the Chrome reference it was scored against, and each approval's width.
Rows from before version 3 have neither and read as 800. That default is
exact, not a guess: the pipeline was never passed a width before then, so it
rendered at its default of 800, and every litehtml configuration until then
set 800 with no fixture override.

## sluggrs modules

The sluggrs code is split across these modules:

- **`src/sluggrs/cmd.rs`**: snapshot rendering, approval and status commands.
  Its approved-image path helper also defines git's measurement-cleanliness
  exemption, so rendering, approval and dirty-tree checks share one layout.
  A change to where approvals live goes through that helper.
- **`src/sluggrs/db.rs`**: the sluggrs adapter for its versioned snapshot
  store. Snapshot tables live in `.brokkr/sluggrs-visual.db`, and brokkr
  imports existing `snapshot_*` rows from `.brokkr/results.db` on first open,
  through the same one-time import in `src/visual/db.rs`.
