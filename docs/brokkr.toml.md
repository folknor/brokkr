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
`gremlins`, `style`, `header`, `textlint`, `disable_toolchain`.

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
`U+` prefix, 1-6 hex digits) or inclusive ranges `U+AAAA..U+BBBB` (both ends
included; `..=` also accepted). Ranges let you ban a whole block cheaply, e.g.
`ban = ["U+0400..U+04FF"]` for Cyrillic. A bad token, a reversed range, or a
codepoint listed on both sides is rejected at parse time. The `U+XXXX` form
keeps `brokkr.toml` itself free of literal, possibly-invisible gremlin
characters. Omit the section to scan everything with the built-in set (the
default).

## `[style]` section

Opt-in native Rust style checks run by `brokkr check` (the style phase, after
gremlins). Every knob defaults to `false`, so omitting `[style]` - or listing
it with nothing enabled - runs no style checks and changes no behaviour.

```toml
[style]
rust_blank_line_above_control_flow = true
```

`rust_blank_line_above_control_flow` requires a blank line above
`if`/`match`/`for`/`while`/`loop`/`spawn` constructs in tracked `.rs` files,
skipping `[gremlins].exclude` directories. It honours an exemption ladder
(first expression in a block, comment/attribute above, string continuation,
an identifier shared with the line above or the first body line, plus
per-keyword carve-outs for else-if chains, expression position, loop labels,
and `.spawn` method chains). Ported from nautilus_trader's `check_formatting_rs`
convention hook; see `src/style.rs` and `docs/commands/check.md`.

## `[header]` section

A required file header whose year must be current (the header phase). A file
matching `paths` (and not `exempt`) must contain `pattern`, with `{year}`
expanded to the current UTC year; a missing header and a stale year both fail.
Absent by default.

```toml
[header]
paths = ["crates/**/*.rs"]
pattern = "Copyright (C) 2015-{year}"
exempt = ["**/examples/**", "**/core/rust/**"]
```

`paths`/`exempt` are globs (`**` matches any directories). The current year
comes from libc `gmtime` (no date-crate dependency). Ported from
nautilus_trader's `check_copyright_year` hook; see `src/header.rs`.

## `[[textlint]]` array

Declarative "forbid a regex on a line" rules (the textlint phase) - the generic
engine behind most grep-style convention hooks. Each entry scans files matching
`paths`; a line matching `pattern` is a violation. Empty by default.

```toml
[[textlint]]
name = "no-todo-macro"
pattern = "todo!\\("
paths = ["crates/**/*.rs"]
message = "finish or file an issue instead of todo!()"

[[textlint]]
name = "anyhow-import-context-only"
pattern = '^\s*use anyhow::'
paths = ["crates/**/*.rs"]
exclude = ["**/*ANYHOW*", "**/anyhow_style_guide*"]  # skip the docs that demo it
except = ['^\s*use anyhow::Context;\s*(//.*)?$']     # the one allowed form
message = "only `use anyhow::Context;` is allowed; fully-qualify the rest"

[[textlint]]
name = "no-tokio-spawn-in-adapters"
pattern = 'tokio::spawn\('
paths = ["crates/adapters/**/*.rs"]
exclude = ["**/tests/**"]
skip_after = '^#\[cfg\(test\)\]'   # ignore everything after the test module
message = "adapters must use get_runtime().spawn()"
```

Fields: `name`, `pattern` (a linear-time `regex`; a match is a violation),
`paths`, `message`, plus optional bounded modifiers:

- `exclude` (globs; files matching are excused, checked after `paths`) - for
  docs that deliberately show the forbidden pattern, or `tests/` trees.
- `allow_marker` (a line containing this literal, e.g. an author's
  `// allow-...` comment, is skipped). `allow_marker_above = N` widens it to
  also suppress when the marker is on one of the N lines above (0 = same line
  only; for markers a wrapped construct pushes off the offending line).
- `only_if_file_matches` (regex; the rule fires only in files where some line
  matches it) - a cheap import-awareness stand-in, e.g. flag bare
  `Instant::now()` only where the file imports `Instant`.
- `region` (`code` / `string` / `comment`; Rust files, tokenized with
  `rustc_lexer`) scopes where `pattern` may match: `code` never flags a pattern
  quoted in a comment or string, `string` targets message text (a `", got"`
  phrasing rule). Only `pattern` is scoped; markers/`except`/reporting stay on
  the physical line.
- `join_wrapped_use = true` matches `pattern` against whole `use ...;`
  statements: a rustfmt-wrapped import is reconstructed onto one line (comments
  stripped) first, so `use tracing::.*warn` catches a multi-line `use` block.
  Reported at the `use` line; `allow_marker` matches on any physical line of
  the statement. Rust-only.
- `except` (regexes; a line matching any is exempt) - the way to allow one
  specific form of an otherwise-forbidden pattern.
- `in_toml_section` (only consider lines while the last-seen `[section]` header
  equals this).
- `table_row_only` (only markdown table rows).
- `skip_after` (regex; once a line in a file matches it, every *following* line
  in that file is exempt - the matching line itself is still checked). For
  "don't fire inside the test module": `skip_after = '^#\[cfg\(test\)\]'`.

No arbitrary multiline matching, except `join_wrapped_use` (bounded to `use`
statements). See `src/textlint.rs` and `src/lex.rs`.

## `[manifest]` section

Native structural `Cargo.toml` conventions (the manifest phase), on the
`[style]` model - discrete named toggles, not a rule DSL. Each check reads a
manifest with `toml_edit`, so it sees structure a value-only parse discards
(blank-line groups, key order). Inert unless a check is enabled; absent = the
phase is skipped.

```toml
[manifest]
paths = ["**/Cargo.toml"]   # default when omitted
exclude = ["fuzz/**"]
sort_dependencies = true    # keys sorted within each blank-line dependency group
```

- `paths` / `exclude` - globs for the manifests checked (default
  `["**/Cargo.toml"]`) and any excused from every check.
- `sort_dependencies` (default `false`) - dependency keys must be alphabetical
  within each blank-line-separated group of a `[dependencies]` /
  `[dev-dependencies]` / `[build-dependencies]` / `[workspace.dependencies]`
  table (target-cfg variants included). A blank line resets the ordering, so
  intentionally grouped manifests pass.
- `section_order` (list; empty = off) - required relative order of top-level
  sections. Only sections both present and listed are constrained; a listed
  section appearing before an earlier-listed one is a violation.
- `crate_type_order` (list; empty = off) - required relative order of
  `[lib] crate-type` entries, e.g. `["rlib", "staticlib", "cdylib"]`.

`section_order` and `crate_type_order` (and the later structural checks) skip a
`cargo-fuzz = true` crate (its `[package.metadata]`), matching the hook's
standalone-fuzz-workspace exemption.

See `src/manifest.rs`.

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
- `test_exclude_packages` (optional, default `[]`) - packages to omit from
  the **test phase only**, emitted as `cargo test --workspace --exclude <pkg>`.
  Clippy still runs workspace-wide. For a workspace member whose test binary
  can't link in this environment (e.g. it needs a system library the build
  host lacks) and would otherwise fail the whole test phase. Mutually exclusive
  with `packages` (you can't both `-p`-select and `--workspace`-exclude);
  setting both is a parse error.
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
  direct dependency list is checked. The wildcard `"*"` means every workspace
  package - use it to ban an external crate across the whole workspace.
- `forbid` (required) - package name, or array of package names, that may not
  appear in those direct dependencies. This can name workspace crates or
  external crates.
- `except` (optional) - workspace packages to drop from the `from` set. Pairs
  with `from = "*"` to express "no crate may depend on X, except these".
- `kinds` (optional) - dependency kinds the rule applies to: `normal`, `dev`,
  `build` (string or array). Empty = every kind (the default; never flips to
  silently ignore a kind you used to catch). `kinds = ["normal"]` means a
  `[dev-dependencies]` entry is allowed - the self-documenting "dev-deps OK".
- `optional` (optional) - when set, only match deps whose `optional` flag
  equals it. `optional = false` matches only non-optional deps, i.e. "if this
  crate is present it must be `optional = true`".

`kinds` and `optional` both scope the *same* present-dep match (absence is
never a violation), so manifest conventions fall straight out of the forbid
mechanism:

```toml
[[dependency_rule]]
name = "openssl-only-in-tls-adapter"
from = "*"
forbid = "openssl"
except = ["tls-adapter"]

# Sync-core crates must not have tokio as a regular dependency (dev-deps OK).
[[dependency_rule]]
name = "sync-core-no-tokio"
from = ["nautilus-core", "nautilus-model", "nautilus-data"]
forbid = "tokio"
kinds = ["normal"]

# The common crate's tokio must be optional (a non-optional tokio is flagged).
[[dependency_rule]]
name = "common-tokio-optional"
from = "nautilus-common"
forbid = "tokio"
kinds = ["normal"]
optional = false
```

Rules are intentionally direct-edge checks: `app -> db` is rejected when `db`
appears in `app`'s manifest dependencies. Transitive architectural constraints
should be encoded by adding rules for the intermediate crates too.

## `[test]` section

Optional. Four things live here: a default cargo package, a default
validation profile, a doctest toggle, and the named profiles that selectively
reference `[[check]]` entries.

```toml
[test]
default_package = "pbfhogg"
default_profile = "tier1"
doctests = false

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
- `doctests` (default `false`) decides whether `brokkr check`'s test phase runs
  doctests. Off by default because CI runs under cargo-nextest, which never
  executes doctests - running them here would gate on a signal CI can't see. In
  the default state each sweep's `cargo test` is scoped to `--tests` (no
  doctests) unless it already names a target (`--test <name>`). Set `true` to
  restore the full `cargo test` default. Project-wide only (no per-`[[check]]`
  or CLI override); `brokkr test <name>` is unaffected. See
  `docs/commands/check.md`.
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
