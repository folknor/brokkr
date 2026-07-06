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
`gremlins`.

## `[gremlins]` section

```toml
[gremlins]
exclude = ["docs/reference-manual", "vendor/upstream-docs"]
```

Directories the `brokkr check` gremlin scanner skips (both the scan and
`--fix-gremlins`) - for vendored material from an outside source that
legitimately carries typographic punctuation, BOMs, and bidi marks. Entries
are project-root-relative directories matched by path prefix on the
git-relative path: `docs/manual` covers `docs/manual/` and below, but not a
sibling `docs/manual-extra`. Empty/absolute entries are rejected at parse
time. Omit the section to scan everything (the default).

## Datasets and variant-selection flags

Host-scoped `[<host>.datasets.<name>]` tables (pbf/osc/pmtiles entries) and the
`--variant` / `--osc-seq` / `--tiles` flags that select between them apply only
to the map-data projects (pbfhogg, elivagar, nidhogg). See
`docs/brokkr.toml.datasets.md`.

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
