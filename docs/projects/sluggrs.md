# sluggrs project

This page covers the sluggrs project, selected by `project = "sluggrs"` in
`brokkr.toml`. sluggrs shares the visual commands with litehtml, but renders
snapshots with its own pipeline binary and compares them with approved PNGs.
The sections below cover the `[sluggrs]` config, snapshot outputs and the
snapshot store. The shared command surface and status meanings are in
`brokkr man visual`.

## Config

The `[sluggrs]` table sets the render size and comparison settings, and each
`[[sluggrs.snapshot]]` entry declares one snapshot. This example shows every
key:

```toml
[sluggrs]
width = 800
height = 600
pixel_diff_threshold = 0.5
regression_tolerance = 0.5   # optional, ratchet slack in pp
pixel_fuzz_percent = 5       # optional, per-channel match tolerance

[[sluggrs.snapshot]]
id = "home"
description = "home page"
fonts = ["assets/fonts/body.ttf"]
optional_fonts = ["assets/fonts/emoji.ttf"]
```

`[sluggrs]` requires `width`, `height` and `pixel_diff_threshold`. The
threshold is a pixel-difference percentage, in percentage points. Two keys
are optional, default to the values shown, and mean what they do for
litehtml (see `brokkr man visual`):

- **`regression_tolerance`**: finite, at least zero
- **`pixel_fuzz_percent`**: an integer from 0 to 100

Each `[[sluggrs.snapshot]]` requires a unique, path-safe `id`, a
`description` and a `fonts` list. brokkr resolves font paths under the
project config root and passes them to the snapshot binary as `--font`
arguments. `optional_fonts` is an optional list of paths passed as
`--optional-font`; omit it or leave it empty when you need no optional fonts.
brokkr rejects duplicate IDs, and IDs that could escape the `snapshots/ID`
artifact directory, before a visual command runs.

## Snapshot outputs

sluggrs records rendered output under `snapshots/ID/output.png` and compares
it with `approved.png`. A snapshot without an approval is `NO_BASELINE`. An
approval record whose PNG is missing is an `ERROR`, so brokkr never treats a
deleted baseline as a new snapshot.

The approved PNGs don't count against a clean tree: brokkr exempts them from
brokkr's dirty-tree check, so an approval you haven't committed yet doesn't
make the tree read as dirty.

## Snapshot store

brokkr keeps sluggrs snapshot runs, results and approvals in
`.brokkr/sluggrs-visual.db`. On first open it imports any existing snapshot
rows from `.brokkr/results.db`. `brokkr clean` never removes the store.
