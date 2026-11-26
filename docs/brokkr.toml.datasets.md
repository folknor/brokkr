# brokkr.toml datasets

This page covers host dataset config: the entries a dataset holds, what
brokkr validates at load, and who reads them. Dataset structure is the
per-key reference.

Two kinds of project use host datasets:

- the map-data projects, which use the PBF, OSC, PMTiles and polygon entries
  described below
- projects that pin out-of-git inputs with `path` plus `xxh128`

Other project-specific inputs belong in their project's config. Datasets are
host-scoped: they live under `[HOSTNAME.datasets.NAME]`, never in a global
`[datasets]` table. `brokkr man config` has the host example.

You can keep host dataset blocks in a file shared between projects and pull
them in with `include` (`brokkr man config include`). Registration edits the
file that defines the dataset, not necessarily `brokkr.toml`. The `data` and
dataset `file` paths still resolve against each including project's root.

## Dataset structure

A dataset table holds file entries grouped by kind, plus a few top-level
fields:

- **`pbf.VARIANT`**: PBF file entries keyed by variant name (`raw`,
  `indexed`, `locations`, for example). Each has `file`, an optional
  `xxhash` (XXH128) and an optional `seq`. Two optional booleans assert
  properties of the file itself, both default `false`: `locations_on_ways`
  (node coordinates are embedded in ways) and `force_sorted` (nodes are
  monotonic, so the compact node store is safe). brokkr reads neither;
  elivagar's own tools and script benches read them from the composed
  config.
- **`pbf.VARIANT.expect`**: accepted for compatibility and unused. The keys
  `check-refs`, `check-refs-relations`, `check-ids`, `check-index`,
  `check-locations` and `check-locations-agreement` accept `"clean"` or
  `"violations"`. `diff` maps OSC sequence keys to `"same"` or `"differs"`.
  Unknown keys and invalid values still fail config load. Include
  composition still preserves expectations when only `file` is overridden.
- **`expect.diff-snapshots`**: accepted for compatibility and unused. Keys
  are `"XXHASH+XXHASH"`, in either order and hex case; values are `"same"`
  or `"differs"`. Invalid digests and duplicate unordered pairs still fail
  load.
- **`osc.SEQ`**: OSC diff file entries keyed by sequence number. Each has
  `file` and an optional `xxhash` (XXH128).
- **`snapshot.KEY`**: another point-in-time capture of the dataset. It has
  its own `pbf.VARIANT` and `osc.SEQ` tables, shaped like the top-level ones
  and resolved under the same data directory. The top-level tables are the
  `base` snapshot.
- **`pmtiles.VARIANT`**: PMTiles archive entries keyed by variant name
  (`elivagar`, for example). Each has `file` and an optional `xxhash`
  (XXH128). brokkr validates them at load and `brokkr env` lists them, but
  no built-in command opens one. A project's own tools read them from the
  composed config (nidhogg's server wrapper does), and a script bench sees
  them in the resolved view.
- **`polygon.NAME`** (pbfhogg, optional): GeoJSON extract regions keyed by a
  name matching `[a-zA-Z0-9_-]+`. They're dataset-level like `bbox`, shared
  by every variant and snapshot. Each has `file` (under the data directory,
  like every entry) and an optional `xxhash` (XXH128). `brokkr env` and
  script benches can use them.
- **`origin`, `download_date`, `bbox`**: top-level dataset fields. brokkr
  parses `bbox` at config load as four finite `lon_min,lat_min,lon_max,
  lat_max` coordinates. Longitude must be within -180..180, latitude within
  -90..90, and each minimum must not exceed its maximum.
- **`data_dir`**: a top-level dataset field that loads but that no built-in
  command reads. It's present in a script bench's resolved view.
- **`getid_ids`, `getparents_ids`**: accepted for compatibility and unused.
  Each entry is `n`, `w` or `r` followed by a decimal ID from 0 to
  9223372036854775807. Lists must stay non-empty and free of duplicates
  after parsing (`n1` and `n01` collide). brokkr keeps the parsed order.

brokkr parses every digest value as 32 hexadecimal XXH128 digits, in either
case, and rejects anything else at config load, a 64-digit SHA-256
included. brokkr refuses a `sha256` key anywhere under `[HOST.datasets]`,
with this remedy: rename it to `xxhash` when its value already is a 32-digit
XXH128, otherwise delete it and take the digest `brokkr env` prints for the
unpinned file.

`brokkr env` lists every `pbf`, `osc`, `pmtiles` and `polygon` entry, and
every `snapshot.KEY.pbf.VARIANT` and `snapshot.KEY.osc.SEQ` entry. It names
a snapshot entry as `DATASET/snapshot.KEY.pbf.VARIANT` and a polygon as
`DATASET/polygon.NAME`. Each line has a presence mark and the pin check. An
existing file with no `xxhash` prints the computed digest (`no hash
configured, actual: ...`) for you to copy into the config.

This example declares one dataset with a PBF variant, an OSC diff and a
polygon:

```toml
[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "12.4,55.6,12.7,55.8"
getid_ids = ["n115722", "w2080", "r174"]    # compatibility, unused

[plantasjen.datasets.denmark.pbf.indexed]
file = "denmark-with-indexdata.osm.pbf"
xxhash = "1234567890abcdef1234567890abcdef"
seq = 4704

[plantasjen.datasets.denmark.pbf.indexed.expect]   # compatibility, unused
check-refs = "clean"
check-ids = "clean"
diff = { "4705" = "differs" }

[plantasjen.datasets.denmark.osc.4705]
file = "denmark-4705.osc.gz"

[plantasjen.datasets.denmark.polygon.copenhagen]
file = "polygons/copenhagen.geojson"
```

## Variant selection

No built-in command selects a dataset entry. Script benches declare and bind
their own params, and read the entries from the resolved view (`brokkr man
bench-config`).
