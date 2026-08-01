# brokkr.toml - elivagar blocks

The elivagar-only parts of `brokkr.toml`. For the schema-universal sections
(top-level shape, `[gremlins]`, `[[check]]`, `[test]`, …) see
`docs/brokkr.toml.md`; for the host-scoped `[<host>.datasets.*]` tables the
map-data projects share, see `docs/brokkr.toml.datasets.md`.

## `[<host>.tilegen.<name>]` blocks

The elivagar tilegen contract. `brokkr tilegen` is configured entirely from
here: **either it is explicit in the block, or it is not set.** There are no
override flags, and nothing is inferred from the filesystem.

```toml
[plantasjen.tilegen.default]
ocean = [
    "z0-z7:simplified-water-polygons-split-3857/simplified_water_polygons.shp",
    "z8-z14:water-polygons-split-3857/water_polygons.shp",
    "ocean-tiles.pmtiles",
]
compression_level = 6          # gzip 0-10, a BASE: elivagar clamps low zooms
                               # up and caps z13/z14 down
tile_format = "mvt"            # mvt | mlt
tile_compression = "gzip"      # gzip | brotli (mvt only)
compress_sort_chunks = "lz4"   # lz4 | snappy - less disk I/O, more CPU
in_memory = false              # keep the tile blob in RAM (small extracts)
threads = 16                   # -j; default logical CPUs
sort_budget = "1G"             # per-chunk sort buffer, min 64M
way_budget = "128M"            # in-flight way processing, min 1M
assemble_budget = "32M"        # tile assembly batch, min 1M
fanout_cap_default = 0         # 0/absent = uncapped
polygon_simplify_factor = 1.0
seam_reconcile_layers = { boundaries = 8 }   # layer -> maxzoom
fanout_caps = { water = 10 }                 # layer -> cap; beats the default
allow_unsafe_flat_index = false              # expert debugging only
```

Every key is optional; omit one and elivagar's own default applies. `tilegen`
uses `default` - there is no selector flag yet. A host with **no**
`[<host>.tilegen.*]` block at all is an error rather than an implicit bare run:
a bare run is a legitimate statement and already has a spelling, an empty
block.

### `ocean`

`ocean` is the only ocean input, and **omitting it means no ocean** - the
statement elivagar's removed `--no-ocean` flag used to make. Each entry is
either a zoom-banded shapefile (`z0-z14:<f.shp>`, `z0-z7:<f.shp>`,
`z8-z14:<f.shp>`) or a bare `<f.pmtiles>` artifact. Paths are relative to the
host's `data`, like every other path in the file. Parse-time rules:

- Shapefiles must partition z0-z14 **exactly** - either a single `z0-z14` or
  the `z0-z7` + `z8-z14` pair. `ocean::selected_pass_grid` implements one
  split, at z7/z8, and nowhere else, so a `z0-z5` request is rejected rather
  than accepted and quietly served at z7.
- At most one `.pmtiles`, and it may not stand alone. It is a cache over the
  shapefiles, not a substitute: an extract computes its boundary band near the
  bbox edge from the shapefiles, and the artifact's key is validated by
  re-hashing them.
- A named input that cannot be honoured is an error at run time, not a
  fallback.

That last rule is the point of the whole block. brokkr used to stat `data/` for
the shapefiles and pass whichever it found, so a run's meaning lived in the
filesystem rather than the invocation - two runs of the same binary on the same
PBF produced different ocean geometry with nothing in `cli_args` saying which.
On 2026-07-14 a denmark archive was built, verified and blessed as the regress
baseline while `ocean-tiles.pmtiles` was missing; it took the computed path
throughout and every gate passed.

### What lives elsewhere

Two axes deliberately live outside the block. `locations_on_ways` /
`force_sorted` are assertions about the *input file*, so they sit on
`[<host>.datasets.<D>.pbf.<variant>]` - a tilegen block would otherwise have to
know which variant it was about to be run against. `--skip-to` is a
per-invocation resume point, not part of the contract.

For an A/B arm, write a sibling block: drop the `.pmtiles` line for an
artifact-absent arm, and `brokkr results --grep-v ocean-tiles` selects it off
`cli_args`. Key order in the expanded argv is stable (the maps are `BTreeMap`),
so identical config produces byte-identical `cli_args`.
