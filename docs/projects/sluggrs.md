# sluggrs project notes

`project = "sluggrs"` in `brokkr.toml`. The visual commands are shared with
litehtml, but sluggrs renders snapshots with its own pipeline binary and
compares them with approved PNGs.

## Config

```toml
[sluggrs]
width = 800
height = 600
pixel_diff_threshold = 0.5

[[sluggrs.snapshot]]
id = "home"
description = "home page"
fonts = ["assets/fonts/body.ttf"]
optional_fonts = ["assets/fonts/emoji.ttf"]
```

`[sluggrs]` requires `width`, `height`, and `pixel_diff_threshold`. The
threshold is a pixel-difference percentage in percentage points.

Each `[[sluggrs.snapshot]]` requires a unique, path-safe `id`, a `description`,
and a `fonts` list. Font paths are resolved under the project config root and
passed to the snapshot binary as `--font` arguments. `optional_fonts` is an
optional list of paths passed as `--optional-font`; omit it or use an empty
list when no optional fonts are needed. Duplicate IDs and IDs that could
escape the `snapshots/<id>` artifact directory are rejected before a visual
command runs.

Sluggrs records rendered output under `snapshots/<id>/output.png` and compares
it with `approved.png`. A snapshot without an approval is `NO_BASELINE`; an
approval record whose PNG is missing is an `ERROR`, so a deleted baseline is
not silently treated as a new snapshot.

## Module layout

- `src/sluggrs/cmd.rs` - snapshot rendering, approval, and status commands.
- `src/sluggrs/db.rs` - the sluggrs adapter for its versioned snapshot store;
  shared SQLite operations and the one-time import from the former shared
  file live in `src/visual/db.rs`. Snapshot tables now live in
  `.brokkr/sluggrs-visual.db`; existing `snapshot_*` rows are imported from
  `.brokkr/results.db` on first open.
- `src/sluggrs/hotpath.rs` - the sluggrs rendering benchmark path.
- `src/visual/` - shared pixel comparison, typed visual statuses, run engine,
  database operations, and common command formatting. Sluggrs has no
  dependency on litehtml's comparison module.

For the shared command surface and status meanings see
`docs/commands/visual.md`.
