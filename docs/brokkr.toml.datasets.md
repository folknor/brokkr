# brokkr.toml datasets

Dataset config consumed by the map-data projects (pbfhogg, elivagar, nidhogg).
litehtml, sluggrs, ratatoskr, and piners do not use any of this. Datasets are
host-scoped - they live under `[<hostname>.datasets.<name>]`, never a global
`[datasets]` table. See the host example in `docs/brokkr.toml.md`.

## Dataset structure

- `pbf.<variant>` - PBF file entries keyed by variant name (e.g. `raw`,
  `indexed`, `locations`). Each has `file`, optional `xxhash` (XXH128),
  optional `seq`. `sha256` is accepted as an alias during migration.
- `osc.<seq>` - OSC diff file entries keyed by sequence number. Each has
  `file`, optional `xxhash`. `sha256` accepted as alias.
- `pmtiles.<variant>` - PMTiles archive entries keyed by variant name (e.g.
  `elivagar`). Each has `file`, optional `xxhash`. `sha256` accepted as alias.
  Used by nidhogg `serve` and `bench tiles`.
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
```

## Shared variant-selection flags

Every measurable command on a project that uses datasets accepts:

- `--variant <name>` - selects from `pbf.<name>`. Default: `indexed`
  (pbfhogg), `raw` (elivagar/nidhogg).
- `--osc-seq <seq>` - selects from `osc.<seq>`. Auto-selects if exactly one
  OSC is configured.
- `--tiles <variant>` - selects from `pmtiles.<variant>`. Auto-selects if
  exactly one PMTiles entry is configured.

pbfhogg has additional flags for snapshots, I/O backends, and compression -
see `docs/projects/pbfhogg.md`.
