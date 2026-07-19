# brokkr.toml

Per-project config consumed by brokkr. Lives at the project root (`./brokkr.toml`).

This file documents the **schema-universal** parts of brokkr.toml:
- Top-level shape (project key, host sections)
- `[gremlins]` section - directories the gremlin scanner skips
- `[[check]]` array - feature sweeps for clippy + tests
- `[[dependency_rule]]` array - direct Cargo dependency boundary rules
- `[test]` section - default package, default profile, named test profiles

For project-specific config blocks see:
- Datasets (`[<host>.datasets.*]` pbf/osc/pmtiles) and the `--variant` /
  `--osc-seq` / `--tiles` flags -> `docs/brokkr.toml.datasets.md` (map-data
  projects only)
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
output = "data/tilegen"   # durable tilegen output store (map-data projects)
target = "target"
port = 3033
drives.source = "nvme"
drives.data = "ssd"
features = ["linux-direct-io", "linux-io-uring"]

# Map-data projects add [<host>.datasets.*] tables here -
# see docs/brokkr.toml.datasets.md
# elivagar adds [<host>.tilegen.*] blocks - see below.
```

Top-level keys that aren't `project` are treated as hostname sections
(unknown non-table keys are rejected). Datasets are host-scoped (no global
`[datasets]` section). Path resolution: host config -> defaults (`data/`,
`data/scratch/`, `data/tilegen/`, cargo target dir). `output` is the durable
tilegen output store (map-data projects): `tilegen` renames each run's archive
to `<output>/<dataset>-<commit>.pmtiles`, and `pmtiles-inspect`/`diag`/`svg`/
`regress`/`bless` resolve it by `--commit`. It is kept SEPARATE from `scratch`
on purpose - elivagar wipes its `--tmp-dir` (`<data>/tilegen_tmp`) every run,
and on some hosts `scratch` points at that same dir, so archives written into
scratch were destroyed by the next run. `brokkr` refuses to write outputs into
a dir that coincides with scratch/tmp; retention keeps the last 5 archives per
dataset. Host `features` are cargo features
appended to every build command (all measurable commands, `verify`, `serve`,
`ingest`, `update`). CLI `--features` are additive on top of host features
(deduped). Reserved top-level keys (skipped by host parsing): `project`,
`litehtml`, `sluggrs`, `check`, `dependency_rule`, `test`, `capture_env`,
`gremlins`, `disable_toolchain`.

## `disable_toolchain`

```toml
disable_toolchain = true
```

Top-level boolean (default `false`). When set, brokkr moves the project's
`rust-toolchain.toml` (or the legacy bare `rust-toolchain`) aside for the
duration of every command, so rustup ignores the pin and falls back to its
normal default. brokkr picks no replacement toolchain - it only disables the
file. The file lives in the code tree (the working directory / build root),
which is where cargo runs; combined with the parent-directory `brokkr.toml`
lookup, this is the setup for driving a foreign checkout whose pinned toolchain
you don't have or don't want.

The file is restored when the command exits normally, on error, or on a
cooperative interrupt. A hard kill (`brokkr kill --hard`, SIGKILL) during a
non-tracked window can leave it moved aside as `rust-toolchain.toml.brokkr-disabled`;
the next brokkr run in that directory adopts the leftover and restores it.
Worktree builds (`--commit`) are a separate checkout and keep their own pin.

## `[gremlins]` section

```toml
[gremlins]
disable = false                                    # skip the phase entirely
exclude = ["docs/reference-manual", "vendor/upstream-docs"]
allow = ["U+2019"]                                 # un-ban these codepoints
ban = ["U+2011"]                                   # flag these codepoints
```

- `disable` (default `false`) - skip the whole gremlin phase, both the scan
  and `--fix-gremlins`. The escape hatch for driving a foreign checkout whose
  Unicode you don't want brokkr to police or edit.
- `exclude` - directories the scanner skips (both the scan and
  `--fix-gremlins`) - for vendored material from an outside source that
  legitimately carries typographic punctuation, BOMs, and bidi marks. Entries
  are project-root-relative directories matched by path prefix on the
  git-relative path: `docs/manual` covers `docs/manual/` and below, but not a
  sibling `docs/manual-extra`. Empty/absolute entries are rejected at parse
  time.
- `allow` - codepoints to remove from the built-in banned set. The scan skips
  them and `--fix-gremlins` leaves them in place, even though they are normally
  gremlins (e.g. permit `U+2019` if a repo deliberately uses curly
  apostrophes).
- `ban` - codepoints to flag beyond the built-in set. **Scan-only**: brokkr has
  no ASCII mapping for an arbitrary codepoint, so `--fix-gremlins` does not
  rewrite banned chars - the scan flags them and you fix them by hand.

`allow` and `ban` entries are `U+XXXX` codepoint strings (case-insensitive
`U+` prefix, 1-6 hex digits); a bad token or a codepoint listed in both is
rejected at parse time. The `U+XXXX` form keeps `brokkr.toml` itself free of
literal, possibly-invisible gremlin characters. Omit the section to scan
everything with the built-in set (the default).

## Datasets and variant-selection flags

Host-scoped `[<host>.datasets.<name>]` tables (pbf/osc/pmtiles entries) and the
`--variant` / `--osc-seq` / `--tiles` flags that select between them apply only
to the map-data projects (pbfhogg, elivagar, nidhogg). See
`docs/brokkr.toml.datasets.md`.

## `[<host>.tilegen.<name>]` blocks (elivagar)

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

Two axes deliberately live elsewhere. `locations_on_ways` / `force_sorted` are
assertions about the *input file*, so they sit on
`[<host>.datasets.<D>.pbf.<variant>]` - a tilegen block would otherwise have to
know which variant it was about to be run against. `--skip-to` is a
per-invocation resume point, not part of the contract.

For an A/B arm, write a sibling block: drop the `.pmtiles` line for an
artifact-absent arm, and `brokkr results --grep-v ocean-tiles` selects it off
`cli_args`. Key order in the expanded argv is stable (the maps are `BTreeMap`),
so identical config produces byte-identical `cli_args`.

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

# Virtual workspace (no root package): scope with `packages` so `--features`
# is legal, and pin a build-affecting var for the whole sweep with `env`.
[[check]]
name = "core"
packages = ["nautilus-core", "nautilus-common"]
features = ["high-precision"]
env = { HIGH_PRECISION = "1" }
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
- `packages` (optional, default `[]`) - packages the sweep is scoped to,
  emitted as `-p <pkg>` on both `cargo clippy` and `cargo test`. Required to
  use `features` in a **virtual workspace** (one with no root package): cargo
  rejects `--features` at the workspace root, so the sweep must name the
  package(s) the features belong to. Distinct from `build_packages`, which
  only pre-builds CLI binaries; `packages` scopes the check itself.
- `env` (optional, default `{}`) - environment variables exported to *every*
  cargo subprocess the sweep runs: clippy, the test-phase pre-build, and the
  test run. Use it to pin a build-affecting toggle (e.g. a codegen flag whose
  drift you'd otherwise catch only in `git status`) so `brokkr check` is
  reproducible without exporting it by hand. Merged under a referencing
  profile's `env`, with the entry winning on a key collision.

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
- `debug` (default `false`) flips `brokkr test`'s cargo profile from release
  to dev. Use it when the project's tests aren't profile-sensitive and the
  faster compile is worth more than the faster run. CLI overrides win:
  `--debug` forces dev, `--release` forces release; the field only decides
  when neither is passed. (Affects `brokkr test` only - `brokkr check`'s
  test phase always builds dev.)
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
