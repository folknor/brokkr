# brokkr.toml

Per-project config consumed by brokkr. Lives at the project root (`./brokkr.toml`).

This file documents the **schema-universal** parts of brokkr.toml:
- Top-level shape (project key, host sections)
- Dataset entry structure (pbf / osc / pmtiles tables)
- Shared variant-selection flags (`--variant`, `--osc-seq`, `--tiles`)
- `[[check]]` array - feature sweeps for clippy + tests
- `[[dependency_rule]]` array - direct Cargo dependency boundary rules
- `[test]` section - default package, default profile, named test profiles

For project-specific config blocks see:
- `[litehtml]` -> `docs/projects/litehtml.md`
- `[ratatoskr]` and `[ratatoskr.harness]` -> `docs/projects/ratatoskr.md`
- `[piners]` and `[piners.harness]` -> the `[piners]` section below; runner behaviour is in `docs/commands/corpus.md`

For project-specific CLI flags that adjust dataset resolution or cargo
features (`--snapshot`, `--as-snapshot`, `--direct-io`, `--io-uring`,
`--compression`, `--locations-on-ways`) see `docs/projects/pbfhogg.md` -
those are pbfhogg-only.

## Top-level shape

```toml
project = "pbfhogg"

[plantasjen]
data = "data"
scratch = "data/scratch"
target = "target"
port = 3033
drives.source = "nvme"
drives.data = "ssd"
features = ["linux-direct-io", "linux-io-uring"]

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

Top-level keys that aren't `project` are treated as hostname sections
(unknown non-table keys are rejected). Datasets are host-scoped (no global
`[datasets]` section). Path resolution: host config -> defaults (`data/`,
`data/scratch/`, cargo target dir). Host `features` are cargo features
appended to every build command (all measurable commands, `verify`, `serve`,
`ingest`, `update`). CLI `--features` are additive on top of host features
(deduped). Reserved top-level keys (skipped by host parsing): `project`,
`litehtml`, `sluggrs`, `check`, `dependency_rule`, `test`, `capture_env`.

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

## `[[check]]` array

Optional. Each entry is one (clippy + test) sweep with the entry's feature
flags. Profiles in `[test.profiles]` reference these by name.

```toml
[[check]]
name = "all"
features = ["test-hooks", "linux-direct-io", "linux-io-uring", "commands"]
build_packages = ["pbfhogg-cli"]

[[check]]
name = "consumer"
no_default_features = true
features = ["commands"]
build_packages = ["pbfhogg-cli"]
```

- `name` (required) - label surfaced in output and the key profiles use to
  reference this entry. Must be unique.
- `features` (optional, default `[]`) - explicit list of cargo features. The
  `features = "all"` sentinel (which used to mean `--all-features`) is
  rejected; enumerate features explicitly so adding a new feature to
  `Cargo.toml` doesn't silently broaden the test sweep.
- `no_default_features` (optional, default `false`) - emits
  `--no-default-features`.
- `build_packages` (optional, default `[]`) - cargo packages rebuilt with the
  entry's feature flags before the test phase. Required when `tests/cli_*.rs`
  integration tests invoke a separate CLI workspace member, otherwise
  `cargo test -p <lib>` leaves the binary in whatever state it was last built
  and the consumer-sweep contract goes unverified.

The legacy `[check]` table form (with `consumer_features`) is rejected at
parse time with a migration message - move the flags into a `[[check]]` entry.

## `[[dependency_rule]]` array

Optional. Each entry forbids direct Cargo dependencies from one or more
workspace packages to one or more package names. `brokkr check` enforces these
rules before clippy/tests by reading `cargo metadata --no-deps`. With no
entries, the phase is skipped silently.

```toml
[[dependency_rule]]
name = "app-db-boundary"
from = "app"
forbid = ["db", "service-state"]

[[dependency_rule]]
name = "core-no-sqlite"
from = ["rtsk", "app"]
forbid = "rusqlite"
```

- `name` (optional) - label surfaced in violation output.
- `from` (required) - workspace package name, or an array of names, whose
  direct dependency list is checked.
- `forbid` (required) - package name, or array of package names, that may not
  appear in those direct dependencies. This can name workspace crates or
  external crates.

Rules are intentionally direct-edge checks: `app -> db` is rejected when `db`
appears in `app`'s manifest dependencies. Transitive architectural constraints
should be encoded by adding rules for the intermediate crates too.

## `[test]` section

Optional. Three things live here: a default cargo package, a default
validation profile, and the named profiles that selectively reference
`[[check]]` entries.

```toml
[test]
default_package = "pbfhogg"
default_profile = "tier1"

[test.profiles.tier1]
description = "Fast edit loop used by brokkr check (tier 1)"
sweeps = ["all", "consumer"]
skip = ["tier2::", "tier3::", "platform::", "serial::"]
include_ignored = false

[test.profiles.full]
sweeps = ["all"]
include_ignored = true
```

- `default_package` is the cargo package `brokkr test` passes to
  `cargo test -p` when no `-p/--package` is given. Resolution order:
  explicit CLI `-p` > `[test].default_package` > `Project::cli_package()` >
  error.
- `default_profile` is the validation profile `brokkr check` uses when no
  `--profile` is passed. With no profile config, `brokkr check` runs every
  `[[check]]` entry without libtest filters; with no `[[check]]` either, it
  falls back to a single `--all-features` sweep.
- `[test.profiles.<name>]` declares a test selection layered onto one or more
  `[[check]]` entries. Fields: `sweeps` (required, list of `[[check]]` entry
  names), `tests` (`--test <name>`), `only` (positional substring filter),
  `skip` (`--skip <substring>`), `include_ignored`, `test_threads`, `env`.
  `extends = "<other>"` walks the chain with cycle detection; collections are
  replaced (child wins), env merges key-by-key.
- Profiles use Rust module paths as the annotation surface; `only` / `skip`
  translate directly into cargo substring filters and `--skip`.
- The legacy `[test.sweeps.*]` map is rejected at parse time. Sweeps now live
  in `[[check]]` entries; profiles reference them by name.

For the sweep-selection ladder used by `brokkr check` (and how `brokkr test`
diverges) see `docs/commands/check.md`.
