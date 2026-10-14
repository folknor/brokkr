# brokkr.toml - elivagar blocks

The elivagar-only parts of `brokkr.toml`. For the schema-universal sections
(top-level shape, `[gremlins]`, `[[check]]`, `[test]`, …) see
`docs/brokkr.toml.md`; for the host-scoped `[<host>.datasets.*]` tables the
map-data projects share, see `docs/brokkr.toml.datasets.md`.

## `tilegen` blocks

`[<host>.tilegen.<name>]` - the elivagar tilegen contract. `brokkr tilegen` is
configured entirely from here: **either it is explicit in the block, or it is not set.** There are no
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
dedup_cap = 60000000           # payload dedup map entry cap; plain integer,
                               # no size suffix. Absent = elivagar's 1M default
fanout_cap_default = 0         # 0/absent = uncapped
polygon_simplify_factor = 1.0
seam_reconcile_layers = { boundaries = 8 }   # layer -> maxzoom
fanout_caps = { water = 10 }                 # layer -> cap; beats the default
allow_unsafe_flat_index = false              # expert debugging only
```

Every key is optional; omit one and elivagar's own default applies. `tilegen`
currently selects `default`; there is no selector for sibling blocks yet. A host with **no**
`[<host>.tilegen.*]` block at all is an error rather than an implicit bare run:
a bare run is a legitimate statement and already has a spelling, an empty
block.

### `ocean`

`ocean` is the only ocean input, and **omitting it means no ocean** - the
statement elivagar's removed `--no-ocean` flag used to make. Each entry is
either a zoom-banded shapefile (`z0-z14:<f.shp>`, `z0-z7:<f.shp>`,
`z8-z14:<f.shp>`) or a bare `<f.pmtiles>` artifact. Paths are relative to the
host's `data`, like every other path in the file. While loading `brokkr.toml`,
brokkr parses the enum, compression level, budget, and ocean values into typed
values and validates their ranges and syntax. It checks every tilegen block
before command planning, so an invalid unused block is still a config error.
Budget sizes are normalized to byte counts in the expanded argv.

The ocean rules are:

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

That last rule is the point of the whole block. Ocean inputs are explicit in
the invocation, so a run cannot silently select different geometry based on
which shapefiles or artifacts happen to be present in `data/`. The recorded
arguments identify whether the configured artifact or the computed path was
used.

### `dedup_cap`

Caps the PMTiles payload dedup map. Like the budgets and `threads`, it sits
outside elivagar's provenance comparability contract: it can only change how
many duplicate payload copies the archive stores and the map's RAM (~56
bytes/entry), never what a consumer decodes from a tile. brokkr applies no
default of its own - the key is either set or the flag is absent - so
`brokkr results --grep dedup-cap` separates the arms after the fact.

### What lives elsewhere

Two axes deliberately live outside the block. `locations_on_ways` /
`force_sorted` are assertions about the *input file*, so they sit on
`[<host>.datasets.<D>.pbf.<variant>]` - a tilegen block would otherwise have to
know which variant it was about to be run against. `--skip-to` is a
per-invocation resume point, not part of the contract.

Only `default` is currently selectable. A sibling block does not create a
runnable A/B arm until a block selector is added; the CLI cannot select one.
Key order in the expanded argv is stable (the maps are `BTreeMap`), so
identical selected config produces byte-identical `cli_args`.
