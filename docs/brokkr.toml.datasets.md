# brokkr.toml datasets

Host dataset config is used by the map-data projects and by Mogwai's out-of-git
input pins. Mogwai uses `path` plus `xxh128`; map-data projects use the PBF,
OSC and PMTiles entries described below. Other project-specific inputs belong
to their project config.
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
- `pbf.<variant>.expect` (pbfhogg, optional) - the answers pbfhogg's
  answer commands should give on this file, so a contradicting answer fails
  the run instead of being stored (`docs/projects/pbfhogg.md`, Declared
  answers). `check-refs`, `check-ids` and `check-index` take `"clean"` or
  `"violations"`; `diff` is a table keyed by an OSC sequence of the same
  snapshot, `"same"` or `"differs"`, for this file against itself with that
  OSC applied. Unknown keys and values are load errors. Absent, either
  answer is stored. It belongs to the entry as composed, so an `include`
  override of `file` alone keeps the declarations of the file it replaced.
  A `--refresh` rotation moves it with its `pbf` table.
- `expect.diff-snapshots` (pbfhogg, optional) - on the dataset:
  `"<xxhash>+<xxhash>" = "same" | "differs"`, the `diff-snapshots` answer
  between the two PBF files whose registered digests the key names, in
  either order and either hex case; one pair spelled twice is a load error.
  Addressed by content, it never follows a snapshot key to another file. A
  `repack --as-snapshot` writes its `same` entry here.
- `osc.<seq>` - OSC diff file entries keyed by sequence number. Each has
  `file`, optional `xxhash` (XXH128).
- `pmtiles.<variant>` - PMTiles archive entries keyed by variant name (e.g.
  `elivagar`). Each has `file`, optional `xxhash` (XXH128).
  Used by nidhogg `serve` and `bench tiles`.
- Top-level dataset fields: `origin`, `download_date`, `bbox`, `data_dir`
  (nidhogg only). `bbox` is parsed at config load as four finite
  `lon_min,lat_min,lon_max,lat_max` coordinates; longitude must be within
  -180..180, latitude within -90..90, and each minimum must not exceed its
  maximum. A pbfhogg extract row that took its bbox from here carries it as
  an arm qualifier, so changing the box splits `results --compare` pairing
  (see `docs/commands/results.md`).
- `getid_ids`, `getparents_ids` (pbfhogg, optional) - the element IDs the
  getid and getparents benches request on this dataset, every variant and
  snapshot of it (`brokkr getid`/`getparents` in any measured mode, and the
  suite's `getid`, `getid-invert` and `getparents` presets). Each entry is
  `n`, `w` or `r` and a decimal ID from 0 to 9223372036854775807 (the
  non-negative IDs pbfhogg getid accepts); the list must be non-empty and
  free of duplicates (compared after parsing, so `n1` and `n01` collide), and
  its order is the argv order. Absent, the bench uses the built-in lists,
  which were chosen in Denmark: a dataset that does not contain Denmark may
  lack most of them, and its bench then times a mostly empty selection. The IDs are a request,
  not a guarantee: nothing checks that they exist in the file, and a
  getparents seed without parents measures little. The built-in getparents
  way seed `w2080` is such a seed where it was checked: on the europe and
  planet inputs of 2026-10-28 the built-in list selected two ways and no
  relations, so `w2080` adds lookup work but no parent (Denmark is
  unverified). Set `getparents_ids` on a dataset to measure a seed with
  relation parents there. A list that differs from
  the built-in one keeps its rows apart in `brokkr results --compare` (see
  `brokkr man results`).

```toml
[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "12.4,55.6,12.7,55.8"
data_dir = "denmark-data"          # nidhogg only
getid_ids = ["n115722", "w2080", "r174"]    # pbfhogg only

[plantasjen.datasets.denmark.pbf.indexed]
file = "denmark-with-indexdata.osm.pbf"
xxhash = "3f1977fd..."
seq = 4704

[plantasjen.datasets.denmark.pbf.indexed.expect]   # pbfhogg only
check-refs = "clean"
check-ids = "clean"
diff = { "4705" = "differs" }

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
  exactly one PMTiles entry is configured. Only nidhogg's `serve` consumes it:
  elivagar's archive readers address the durable output store by
  `--dataset`/`--variant`/`--commit`/`--file` instead (see
  `docs/projects/elivagar.md`).

pbfhogg has additional flags for snapshots, I/O backends, and compression -
see `docs/projects/pbfhogg.md`.
