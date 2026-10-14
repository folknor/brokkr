# brokkr.toml datasets

Host dataset config is consumed by the map-data projects (pbfhogg, elivagar,
nidhogg) and by Mogwai's out-of-git input pins. Mogwai uses `path` plus
`xxh128`; map-data projects use the PBF, OSC and PMTiles entries described
below. litehtml, sluggrs, ratatoskr, and piners do not use these datasets.
Datasets are host-scoped - they live under `[<hostname>.datasets.<name>]`,
never a global `[datasets]` table. See the host example in
`docs/brokkr.toml.md`.

Host dataset blocks may live in a file shared between projects and pulled in
with `include` (`brokkr man config include`). Registration edits the file that
defines the dataset, not necessarily `brokkr.toml`; `data` and dataset `file`
paths still resolve against each including project's root.

## Dataset structure

- `pbf.<variant>` - PBF file entries keyed by variant name (e.g. `raw`,
  `indexed`, `locations`). Each has `file`, optional `xxhash` (XXH128),
  optional `seq`. Digest values are parsed as 32 hexadecimal XXH128 digits
  (either case); anything else, a 64-digit SHA-256 included, is rejected at
  config load. The old `sha256` key, once an alias of `xxhash`, is refused
  anywhere under `[<host>.datasets]` with the remedy: rename it to `xxhash`
  when its value already is a 32-digit XXH128, otherwise delete it and take
  the digest `brokkr env` prints for the unpinned file.
  Two optional booleans assert properties of the file itself, both default
  `false`: `locations_on_ways` (node coordinates are embedded in ways) and
  `force_sorted` (nodes are monotonic, so the compact node store is safe).
  Elivagar reads both from the PBF header and `brokkr tilegen` only forces
  what the header fails to declare, so the normal case leaves them unset.
  They live here rather than in a `[<host>.tilegen.<name>]` block because they
  describe the input, not the pipeline - a block would otherwise have to know
  which variant it was about to be run against.
- `osc.<seq>` - OSC diff file entries keyed by sequence number. Each has
  `file`, optional `xxhash` (XXH128).
- `pmtiles.<variant>` - PMTiles archive entries keyed by variant name (e.g.
  `elivagar`). Each has `file`, optional `xxhash` (XXH128).
  Used by nidhogg `serve` and `bench tiles`.
- `blessed` - the singular regress reference archive (elivagar). Fields:
  `file` (relative to the data dir, conventionally `blessed/<dataset>-<commit>.pmtiles`),
  `commit` (source commit, for provenance), optional `xxhash` (XXH128).
  Written by `brokkr bless`, read + hash-verified by `brokkr regress`. The
  archive lives under gitignored `data/blessed/`; only this registration is
  committed. Singular (not keyed by variant) - one pmtiles variant exists
  today. Each host blesses its own.
- Top-level dataset fields: `origin`, `download_date`, `bbox`, `data_dir`
  (nidhogg only).

```toml
[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "12.4,55.6,12.7,55.8"
data_dir = "denmark-data"          # nidhogg only

[plantasjen.datasets.denmark.pbf.indexed]
file = "denmark-with-indexdata.osm.pbf"
xxhash = "3f1977fd..."
seq = 4704

[plantasjen.datasets.denmark.osc.4705]
file = "denmark-4705.osc.gz"
xxhash = "fa581f7b..."

[plantasjen.datasets.denmark.blessed]
file = "blessed/denmark-661cd1c.pmtiles"
commit = "661cd1c"
xxhash = "9c2e0b4a..."
```

## Shared variant-selection flags

Every measurable command on a project that uses datasets accepts:

- `--variant <name>` - selects from `pbf.<name>`. Default: `indexed`
  (pbfhogg), `raw` (elivagar/nidhogg).
- `--osc-seq <seq>` - selects from `osc.<seq>`. Auto-selects if exactly one
  OSC is configured.
- `--tiles <variant>` - selects from `pmtiles.<variant>`. Auto-selects if
  exactly one PMTiles entry is configured. Only nidhogg's `serve` consumes it:
  elivagar's archive readers address the durable output store by
  `--dataset`/`--variant`/`--commit`/`--file` instead (see
  `docs/projects/elivagar.md`).

pbfhogg has additional flags for snapshots, I/O backends, and compression -
see `docs/projects/pbfhogg.md`.
