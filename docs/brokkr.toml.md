# brokkr.toml

Per-project config consumed by brokkr. Lives at the project root (`./brokkr.toml`).

This file documents the **schema-universal** parts of brokkr.toml - every
section below applies in any project. Each `##` heading is addressable on its
own: `brokkr man config script_check` prints that section and nothing else,
`brokkr man config header textlint` prints two, and bare `brokkr man config`
lists them.

Project-specific config blocks live in their own docs, so a checkout only ever
reads the ones that apply to it:
- Datasets (`[<host>.datasets.*]` pbf/osc/pmtiles) and the `--variant` /
  `--osc-seq` / `--tiles` flags -> `docs/brokkr.toml.datasets.md` (map-data
  projects only; `brokkr man datasets`)
- `[<host>.tilegen.*]` -> `docs/brokkr.toml.elivagar.md`
  (`brokkr man elivagar-config`)
- `[litehtml]` -> `docs/projects/litehtml.md`
- `[ratatoskr]` and `[ratatoskr.harness]` -> `docs/projects/ratatoskr.md`
- `[piners]` and `[piners.harness]` -> `docs/brokkr.toml.piners.md`; runner
  behaviour is in `docs/commands/corpus.md`
- `[dellingr]` and `[dellingr.workloads.*]` -> `docs/projects/dellingr.md`
- `[mogwai]` and `[mogwai.targets.*]` -> `docs/projects/mogwai.md`

For project-specific CLI flags that adjust dataset resolution or cargo
features (`--snapshot`, `--as-snapshot`, `--direct-io`, `--io-uring`,
`--compression`, `--locations-on-ways`) see `docs/projects/pbfhogg.md` -
those are pbfhogg-only.

## Where brokkr.toml lives

Looked up in the working directory, or failing that in its **immediate parent** -
one level, never a walk to the filesystem root. A deeper search would silently
attach to a stray `brokkr.toml` in a home directory or to an unrelated enclosing
project.

Finding it one level up is the layout for driving a checkout that is not ours:
brokkr's state (`.brokkr/`, `data/`) stays in the parent and the foreign repo
stays clean. That splits two roots which coincide in the ordinary case:

- **project root** - the directory holding `brokkr.toml`. Anchors `.brokkr/`,
  `data/`, and every other brokkr-owned store.
- **build root** - the working directory, where cargo and git run.

The build root is always cwd, and there is deliberately no step that descends
from the config directory to find the checkout: one config directory may sit
above several, and guessing which was meant is how a command operates on the
wrong tree.

**So run brokkr from inside the checkout, not from the directory holding
`brokkr.toml`.** Standing in the config directory points every cargo-driving
command at a directory that holds no `Cargo.toml`; brokkr refuses by name
(`no Cargo.toml in <dir> or any parent`) rather than forwarding cargo's own
error, which walks to the root and reads as though the project were broken.
Config-only commands (`man`, `results`, `history`) work from either directory.

## The user-wide brokkr.toml

A second, optional config lives at `$XDG_CONFIG_HOME/brokkr/brokkr.toml`
(falling back to `$HOME/.config/brokkr/brokkr.toml`). It holds the conventions
that belong to *you* rather than to any one tree, and it applies in every
project brokkr detects.

It accepts three top-level keys and nothing else:

| Key | |
|---|---|
| `[[textlint]]` | rules, exactly as in a project config |
| `[textlint_preset.<name>]` | presets for those rules |
| `[[script_check]]` | gates, exactly as in a project config |

Any other key - `project`, `[[check]]`, a host section - is an error naming the
file. The rest of the schema describes one project (its datasets, its sweeps,
its bin targets) and has no meaning machine-wide; a project-shaped key in a
machine-wide file is a mistake, not a setting, and rejecting it beats
silently ignoring it.

`[[textlint]]` `paths` globs and `[[script_check]]` commands are interpreted
against each project's own tree, exactly as if the entry had been written in
that project's config. Presets are resolved within the file that defines them:
a user preset serves user rules, and is dead - an error - if it serves none.

**Merging.** User entries run first, project entries after. Shadowing is by
`name`: a project entry that reuses a user entry's name replaces it outright.
That is the opt-out - a project that must not run a personal rule redefines it
under the same name, rather than needing a suppression syntax nobody would
remember.

The layer is applied at project detection, not when a `brokkr.toml` is parsed,
so parsing a config file yields that file and nothing from the machine it runs
on.

**Trees with no `brokkr.toml` at all.** `check` runs in any Rust+git repo, and
the user-wide layer reaches those too - it is not about this project, and a repo
that never adopted a `brokkr.toml` is exactly where a personal convention is
likely to be the only rule there is. With nothing to merge into, the user
entries stand alone: `check` runs its gremlins, textlint and `script_check`
phases from them and skips the config-driven rest.

**Overriding the path.** `BROKKR_USER_CONFIG=/path/to/file` reads the layer from
somewhere else; `BROKKR_USER_CONFIG=` (set, empty) switches it off entirely, for
a CI job that must see only the project's own rules. An absent file is not an
error; a file that exists but does not parse is, because silently ignoring it
would let one typo switch off every rule you wrote.

`brokkr env` prints a `user cfg:` line with the resolved path and how many
entries of each kind it contributed - a rule that comes from outside the tree is
otherwise invisible from inside it.

## Top-level shape

```toml
project = "pbfhogg"

[plantasjen]
data = "data"
scratch = "data/scratch"
output = "data/tilegen"   # durable tilegen output store (map-data projects)
target = "target"
worktree_keep = 6         # persistent --commit worktrees kept before LRU eviction
port = 3033
drives.source = "nvme"
drives.data = "ssd"
features = ["linux-direct-io", "linux-io-uring"]

# Map-data projects add [<host>.datasets.*] tables here -
# see docs/brokkr.toml.datasets.md
# elivagar adds [<host>.tilegen.*] blocks -
# see docs/brokkr.toml.elivagar.md
```

Top-level keys that aren't `project` are treated as hostname sections
(unknown non-table keys are rejected). Datasets are host-scoped (no global
`[datasets]` section). Path resolution: host config -> defaults (`data/`,
`data/scratch/`, `data/tilegen/`, cargo target dir). `output` is the durable
tilegen output store (map-data projects): `tilegen` renames each run's archive
to `<output>/<dataset>-<variant>-<commit>.pmtiles`, and
`pmtiles-inspect`/`diag`/`svg`/`regress`/`pmtiles-corpus` resolve it by
`--variant` + `--commit`. It is kept SEPARATE from `scratch`
on purpose - elivagar wipes its `--tmp-dir` (`<data>/tilegen_tmp`) every run,
and on some hosts `scratch` points at that same dir, so archives written into
scratch were destroyed by the next run. `brokkr` refuses to write outputs into
a dir that coincides with scratch/tmp; retention keeps the last 5 archives per
`(dataset, variant)` pair - scoped so that building one variant can never evict
another's archives at the same commit. Group membership is decided by
*constructing* the name shape and requiring a hyphen-free commit token after it
(`resolve::pmtiles_archive_matches`), never by parsing a filename back: dataset
names carry hyphens, and a prefix test alone would let a variant claim a
dash-extending sibling's files (`raw` swallowing `raw-fast`) and evict them. Host `features` are cargo features
appended to every build command (all measurable commands, `verify`, `serve`,
`ingest`, `update`). CLI `--features` are additive on top of host features
(deduped). Reserved top-level keys (skipped by host parsing): `project`,
`litehtml`, `sluggrs`, `check`, `dependency_rule`, `test`, `capture_env`,
`gremlins`, `header`, `textlint`, `textlint_preset`, `script_check`,
`manifest`, `deps`, `disable_toolchain`.

## `worktree_keep`

```toml
[plantasjen]
worktree_keep = 6
```

How many persistent `--commit` worktrees this project keeps before the
least-recently-used are evicted. Default 6. Per host because the disk is a host
property. `0` is ignored and the default applies - zero would evict every
worktree the moment a second was wanted, which is indistinguishable from the
feature being broken.

Eviction runs when a **new** worktree is cut, never on reuse and never on a
plain run. That keeps the cost adjacent to a build you are already paying for
rather than appearing as an unexplained pause, and it stops a measurement from
becoming a destructive operation merely by happening. The consequence is that a
project which stops growing never shrinks on its own; `brokkr clean
--worktrees` is the explicit hammer for that.

**This is a growth damper, not a bound.** The count is per project, so the
disk-wide total is this number times however many projects you have worktrees
for. Only a global byte budget could promise "never fills the disk", and a count
is in any case a proxy for the real constraint, which is bytes. brokkr caches
each worktree's measured size in its bookkeeping so a size-based rule can be
added later without walking the tree at eviction time.

The default of 6 is chosen for the **heaviest** dependency graph, not the
lightest. A cold worktree build is ~36s on a mid-sized workspace but minutes on
elivagar, and a silent multi-minute stall is a worse failure than some gigabytes
that were not strictly needed. Six also clears the shape of a real comparison -
a baseline, the commit under test, the head, a rework, and room to iterate the
rework twice without evicting the baseline you are still comparing against.
Projects with cheap builds can lower it.

Two rules the eviction obeys:

- **A worktree with uncommitted work is never evicted**, regardless of whether
  git would have succeeded in removing it. That is correctness, not courtesy: a
  dirty worktree is the one place where removal destroys something
  unrecoverable. If git cannot be consulted at all, the worktree is assumed
  dirty and kept.
- **Failures skip and continue**, because housekeeping must not fail the
  measurement you actually asked for. Since skips can hold the count above the
  limit, brokkr reports the overage on *every* run where it is exceeded, not
  once when a removal fails - a damper that has quietly stopped working is the
  original problem, and you should not learn about it from the volume filling.

Bookkeeping lives in `.brokkr/worktrees.toml` at the project root, written on
both create and reuse. It is safe to delete; losing it costs a suboptimal
eviction order, never data. A worktree with no record sorts as oldest, since it
predates the bookkeeping - treating it as freshest would make pre-existing
worktrees permanently un-evictable.

## `disable_toolchain`

```toml
disable_toolchain = true
```

Top-level boolean (default `false`). When set, brokkr moves the project's
`rust-toolchain.toml` (or the legacy bare `rust-toolchain`) aside for the
duration of the global lock, so rustup ignores the pin and falls back to its
normal default. brokkr picks no replacement toolchain - it only disables the
file. The file lives in the code tree (the working directory / build root),
which is where cargo runs; combined with the parent-directory `brokkr.toml`
lookup, this is the setup for driving a foreign checkout whose pinned toolchain
you don't have or don't want.

Suppression is tied to the lock, not to the whole command: the file is moved
aside the moment brokkr takes the global lock and restored just before it is
released. Every command that runs cargo or rustup against the tree therefore
takes the lock so the pin is disabled while it works - including `deps` and
`compare-tiles` (which shell out to `cargo metadata`) and `fmt` (rustfmt is
toolchain-pinned), none of which are builds in the timing sense. A command that
touches neither cargo nor rustup takes no lock and leaves the file alone.

**When the host toolchain is newer than the pin,** brokkr builds with lints the
pinned toolchain does not have. That is often the point - it lints ahead of the
pin - but it has a failure mode worth recognising, because it presents three
layers away from its cause. A lint the host knows and the pin does not (a
freshly deprecated API, say) meets a `-Dwarnings` in the project's
`.cargo/config.toml` and becomes a *hard compile error*, in code no local
change touched, that CI - building on the pin - never sees. It reads as "your
code doesn't compile". Suppress the unactionable ones with
[`[lints] allow`](#lints-section), which reaches the clippy phase and the test
build alike, or install the pinned toolchain.

The file is restored when the lock is released - on normal exit, on error, or
on a cooperative interrupt. A hard kill (`brokkr kill --hard`, SIGKILL) during a
non-tracked window can leave it moved aside as `rust-toolchain.toml.brokkr-disabled`;
the next brokkr run in that directory adopts the leftover and restores it.
Worktree builds (`--commit`) run in a separate checkout that may carry its own
committed pin; brokkr re-points the disable there for the build, so the
worktree's pin is disabled too rather than honored.

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
  the statement. Rust-only. This *replaces* the per-physical-line pass for the
  rule - it no longer scans ordinary lines, only reconstructed `use` statements,
  so it is for import rules, not use-site rules. Setting it on a rule whose
  `except` exempts ordinary `use` lines is a config error (the rule could never
  fire), rejected at load time.
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

## `[textlint_preset]` blocks

`[textlint_preset.<name>]` - a named bundle of textlint scope/predicate fields that rules pull in with
`preset`. For a family of rules that differ only in `pattern` and `message` but
share a long `exclude` list - the shape a ported hook suite converges on - this
single-sources the scope, so adding one exempt file is a one-line edit rather
than the same edit repeated across every rule in the family.

```toml
[textlint_preset.dst-scope]
paths = ["crates/*/src/**/*.rs"]
exclude = [
    "crates/adapters/**", "crates/backtest/**", "crates/pyo3/**",
    "**/tests/**", "**/*_test.rs",
]
region = "code"
skip_after = '^\s*#\[cfg\((all\()?test'
allow_marker = "dst-ok"

[[textlint]]
name = "dst-no-wallclock-qualified"
preset = "dst-scope"
pattern = 'std::time::Instant::now\(\)|chrono::Utc::now\(\)'
message = "route through a DST seam"

[[textlint]]
name = "dst-tcp-seam-qualified"
preset = "dst-scope"
pattern = 'tokio::net::TcpStream::connect\b'
exclude = ["crates/network/src/net.rs"]   # appended to the preset's list
message = "route through nautilus_network::net"
```

A preset may set any `[[textlint]]` field except `name`, `pattern`, and
`message` - the three that identify one rule. An unknown or misspelled field in
a preset is a load-time error even if no rule uses the preset. A preset that
**no rule references** is also a load-time error - dead config that would load
clean is exactly what this parser rejects, so define a preset only where a rule
draws on it.

Merge rules:

- **Nearest value wins.** A field the rule sets itself is kept. This includes a
  field set back to its own default (`join_wrapped_use = false` beats a preset's
  `true`) - the merge happens on raw TOML before deserialization, so "set to
  false" and "absent" stay distinguishable.
- **`paths`, `exclude`, and `except` concatenate**, preset entries first, so a
  rule *adds* to a shared list instead of replacing it. Narrow a preset's
  `paths` with the rule's own `exclude`.
- `preset` also takes a list (`preset = ["rust-src", "dst-markers"]`), applied
  left to right under the same nearest-wins rule: for a **scalar** the rule's
  own value wins, then the first-listed preset, and so on. For a **list** the
  entries concatenate in that same declaration order - `preset = ["a", "b"]`
  gives `a`'s entries, then `b`'s, then the rule's own - so lists and scalars
  agree on "earlier-listed wins".

Presets are resolved entirely at parse time - the check phases see ordinary
fully-resolved rules and have no notion of a preset.

## `[[script_check]]` array

Run a command and assert its output (the script-check phase) - the escape hatch
for gates brokkr's native phases can't express (semantic analysers, external
formatter conventions). Each entry runs `command` via `sh -c` and passes iff the
captured output matches `expect`; on failure the full captured output is shown.
Matching the sentinel (not the exit code) catches a check stubbed to `exit 0`.

```toml
[[script_check]]
name    = "docs-conventions"
command = "bash .pre-commit-hooks/check_docs_conventions.sh"
expect  = "All documentation conventions are valid"
match   = "last-line"   # exact | last-line | contains   (default: last-line)
stream  = "stdout"      # stdout | stderr | both          (default: stdout)
stage   = "pre-clippy"  # pre-clippy | pre-test | post-test (default: pre-clippy)
```

- `name` - label shown when the entry **fails** (`beta: stdout did not match
  "ok"`). A passing stage prints one collapsed line, `script-check: ok (N
  check(s))`, rather than a line per entry: a passing gate's name carries
  nothing to act on, while the count still says which corpus passed. A stage
  with some failures prints `script-check: M of N ok` above the failure block.
- `command` - run as `sh -c "<command>"`, cwd = the code tree, so
  pipes/redirects/env expansion work.
- `expect` - the success sentinel. Keep it ASCII: a non-ASCII sentinel (e.g. a
  check mark) would trip the gremlin scan on `brokkr.toml` itself.
- `match` - `exact` (whole trimmed output equals `expect`), `last-line` (last
  non-empty line equals `expect`; the default), or `contains` (substring).
- `stream` - `stdout` (default), `stderr`, or `both` (concatenated).
- `stage` - where in the check pipeline the entry runs (below).

The command's exit code is ignored - only the output match decides. See
`src/script_check.rs` and `docs/commands/check.md`.

### `stage`

An entry runs once, at the point its `stage` names:

| `stage` | Runs | For |
| --- | --- | --- |
| `pre-clippy` (default) | With the other convention phases, before clippy builds anything | Cheap source-level gates - they fail before the run spends time compiling |
| `pre-test` | After clippy, before the test phase | Gates that want a clippy-clean tree but shouldn't wait on the suite |
| `post-test` | After the test phase and the coverage audit | Gates that need built binaries or test output |

Entries at the same stage run in declaration order, and every entry at a stage
runs even when an earlier one failed, so a run surfaces all broken gates at
once.

`post-test` entries are **skipped entirely when the test phase failed**. The
test phase fails fast, so its later lanes never ran; a script-check has no
partial-run reading, unlike the coverage audit, which deliberately still runs
there. Leaving `stage` off is exactly the old behaviour.

All three stages share the one `script_check` phase name, so a profile's
`skip_phases = ["script_check"]` drops every stage, and a failure is reported
as `failed_phase: "script_check"` regardless of where it happened - the failing
entry is named in the output either way.

## `[manifest]` section

Native structural `Cargo.toml` conventions (the manifest phase), on the
model of discrete named toggles, not a rule DSL. Each check reads a
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
- `package_field_order` (list; empty = off) - required relative order of
  `[package]` keys.
- `lints_workspace_required` (default `false`) - a crate with a `[lib]` or
  `[[bin]]` target must set `[lints] workspace = true`.
- `bin_doc_false` / `bin_test_false` (default `false`) - every `[[bin]]` must
  set `doc = false` / `test = false` (a missing or `true` flag is a violation).
- `example_doc_false` (default `false`) - every `[[example]]` must set
  `doc = false`.
- `cargo_machete_ignored_declared` (default `false`) - each
  `[package.metadata.cargo-machete] ignored` entry must name a declared
  dependency.
- `[[manifest.version_align]]` (repeatable) - `crates = [...]` whose version
  requirements must agree at `granularity` (`"minor"` default, or `"major"`).
  Absent crates are skipped, so a group only fires when 2+ are present. Reads
  both the bare-string and `{ version = "..." }` dep forms.

```toml
[[manifest.version_align]]
crates = ["arrow", "parquet"]
granularity = "minor"
```

- `[manifest.adapter_group]` - a comment-labelled group inside the workspace
  root's `[workspace.dependencies]` (found by `marker`, a substring of the
  header comment) whose members must not be depended on by the crates in
  `forbidden_in`. A workspace-level check: it reads the group from the root
  manifest and scans every member's dependency tables.

```toml
[manifest.adapter_group]
marker = "Adapter dependencies"
forbidden_in = ["nautilus-core", "nautilus-model"]
```

The section/target-shape checks (`section_order`, `crate_type_order`,
`package_field_order`, `lints_workspace_required`, the bin/example flags) skip a
`cargo-fuzz = true` crate, matching the hook's standalone-fuzz-workspace
exemption. The dependency-content checks (`sort_dependencies`, cargo-machete,
`version_align`) apply to every manifest.

See `src/manifest.rs`.

## `[deps]` section

Tuning for the `brokkr deps` audit (optional; `deps` runs in any Rust+git repo).

```toml
[deps]
workspace_dep_ignore = ["lychee", "cargo-*", "backtest"]
```

- `workspace_dep_ignore` - workspace-dependency names the unused-workspace-dep
  phase must not flag even when no member inherits them (dev tools, top-level
  members whose use cargo metadata can't attribute). An entry ending in `*` is
  a prefix glob. See `docs/commands/deps.md`.

## `[bin]` section

Shared defaults for `brokkr run` and `brokkr install` (optional). Both
commands discover their targets from cargo metadata - every workspace bin
target (and, for `run`, example target) is runnable by name with no config at
all. The section curates what discovery leaves ambiguous:

```toml
[bin]
default = "app"              # bare `brokkr run` target (bin or example name)
install = ["app", "runner"]  # packages `brokkr install` installs
debug = true                 # dev profile for run + `--debug` for install
```

- `default` - the target name bare `brokkr run` launches. Without it, a sole
  runnable runs; several runnables print an index and exit 0.
- `install` - package names installed via `cargo install --path <pkg dir>`,
  in order. Without it, the workspace's sole bin-carrying package installs;
  several is an error naming this key. A package with no bin target is a
  config error.
- `debug` - default both commands to the dev profile (`cargo run` without
  `--release`, `cargo install --debug`). Unset means release for both,
  matching `brokkr test`. CLI `--debug` / `--release` (mutually exclusive)
  override in either direction.

## `[lints]` section

Lint suppressions, applied to **every phase that compiles** - `brokkr check`'s
clippy phase and its test phase, plus `brokkr clippy` (optional).

"Every phase that compiles" is the literal rule and includes the parts of a
phase that are not the obvious cargo run: the sweep pre-builds, the
process-isolated lane's enumeration, and the coverage audit's `cargo test
--no-run`. Anything that invokes the compiler fails on a lint the project's
`-Dwarnings` promotes to an error, whether or not brokkr would have read its
diagnostics.

Spelled `[clippy]` before the test phase started reading it, and that spelling
still parses. The two are a true alias: if both are present their lists are
unioned, so a project part-way through the rename never has one section
silently shadow the other.

```toml
[lints]
allow = ["clippy::unused_async_trait_impl", "clippy::default_constructed_unit_structs"]
allow_exact = ["clippy::unused_async_trait_impl@crates/system/src/kernel.rs"]
```

- `allow` - lint names suppressed on every sweep. The clippy phase appends
  `-A <lint>` after `--cap-lints=warn`; the test phase has no rustc
  passthrough of its own and routes them through cargo's rustflags (see
  *How the test phase injects them* below). The escape hatch for driving a
  foreign checkout under `disable_toolchain`, where the host's newer compiler
  surfaces lints the project's pinned-toolchain CI cannot see - and where a
  `-Dwarnings` in the project's `.cargo/config.toml` turns each of them into a
  hard compile error. Bare lint names only (`clippy::`-qualified or plain
  rustc names) - a leading `-` or embedded whitespace is rejected at parse
  time. Each phase announces the allowed lints at the start of every run.
  Known limit in the clippy phase: a site carrying its own lint-level
  attribute can defeat the injected `-A` - observed on clippy 1.98, where an
  `#[expect]` for a sibling lint let the allowed lint through at error
  severity despite `-A` and `--cap-lints=warn`. See `docs/commands/check.md`
  for the full shape.
- `allow_exact` - sited suppressions in the `"lint@path"` form, the remedy
  for that known limit: matching diagnostics (same lint, `clippy::`
  qualifier optional; same build-root-relative file, copied from the failing
  diagnostic's location) are dropped at brokkr's JSON ingestion, after the
  compiler has spoken, so no attribute at the site can defeat them. One lint
  in one file - every occurrence in that file, but never workspace-wide, and
  no `-A` is injected, so other sites of the same lint still fail. Each
  entries are announced up front, grouped by lint with a file count (a lint
  with one site keeps its path); one that suppressed nothing draws a stale
  notice, and that one is reported per entry, since it is the part a reader
  has to act on. Parse-time rejection mirrors `allow`, plus the `@` split: exactly
  one `@`, non-empty halves, no whitespace. See `docs/commands/check.md`.

  **The file scoping is clippy-phase only.** A lint that fails a *build* fails
  it during compilation, before any diagnostic reaches brokkr to be filtered,
  and `-A` has no path-scoped form - so in the test phase each entry
  contributes its lint name build-wide, and the run says so on its notice
  line. The narrow guarantee still holds everywhere it can be enforced. The
  alternative was an `allow_exact` that silently does nothing about the build
  failure it was written for.

### How the test phase injects them

`cargo test` has no `-- <rustc flags>` passthrough, so the test phase puts the
`-A` flags into cargo's rustflags. Which layer it uses is decided per sweep,
because cargo does **not** merge rustflags across kinds of source - it picks
exactly one, first match wins:

1. `CARGO_ENCODED_RUSTFLAGS`
2. `RUSTFLAGS`
3. every *matching* `target.<triple>.rustflags` / `target.<cfg>.rustflags`,
   joined together
4. `build.rustflags`

This is why brokkr does not simply export `RUSTFLAGS`: that promotes its own
flags to source 2 and discards the project's `-Dwarnings`, its
`-fuse-ld=lld`, its `relocation-model=pic`, all at once. (It is also why
`[[check]] rustflags` isolates a sweep into `target/rustflags-<hash>` and
should not be used to carry a lint allow.)

Instead brokkr finds the layer already live and adds to *that* one, where
cargo's own array merging applies and its entry lands last - which is what a
lint allow needs, since rustc resolves conflicting lint levels last-wins. A
config layer is reached with `--config`, the highest-precedence config source,
so the project's `.cargo/config.toml` is left untouched.

The chain inspected is the whole chain cargo reads: every `.cargo/config.toml`
from the build root upwards, plus `$CARGO_HOME/config.toml`. A user-level
`[target.<host-triple>] rustflags` is common - a linker choice, a
`-Ctarget-cpu` - and it silently decides the winning layer for **every**
project on that machine, including ones whose own flags live in
`build.rustflags` and are therefore already being ignored.

An unrecognised `cfg(...)` selector resolves to the inert direction, never the
destructive one: injecting at `build.rustflags` when a target table actually
wins merely does nothing, while injecting at `target."cfg(all())"` when
`build.rustflags` was the live layer would demote it and drop the project's
flags. The run always prints which layer it used, so an inert injection is
diagnosable rather than mysterious:

```
test: allowing deprecated via --config target."cfg(all())".rustflags ([lints] allow)
```

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
- `rustflags` (optional, default `[]`) - extra `rustc` flags (token list, e.g.
  `["--cfg", "madsim"]`) exported as `RUSTFLAGS` on the sweep's cargo processes
  only, **composed** with any inherited `RUSTFLAGS` (appended; falls back to
  `CARGO_ENCODED_RUSTFLAGS` when the environment already carries the encoded
  form). A non-empty value **auto-isolates** the sweep into its own target dir,
  `target/rustflags-<hash>`, keyed by the flag content: a global cfg like
  `--cfg madsim` reshapes every fingerprint in the build graph, so sharing the
  default `target/` would force a full recompile in both directions on every
  alternation with the plain sweeps. Isolation keeps the caches apart; sweeps
  carrying *identical* flags share one dir (so several madsim legs compile the
  simulator once). There is no key to set - isolation is automatic and derived
  from the flags. Setting `RUSTFLAGS` or `CARGO_TARGET_DIR` in `env` alongside
  `rustflags` is a parse error (one unambiguous source each).
- `tests` / `skip` / `only` (optional, default `[]`) - per-`[[check]]` libtest
  filters, **ANDed** with any referencing profile's filters of the same name
  (they append, never replace): `tests` -> cargo `--test <name>` target
  selectors, `skip` -> libtest `--skip <substr>`, `only` -> libtest positional
  substring filters. This lets a curated subset (e.g. one named test per
  package) be expressed as several sibling `[[check]]` entries under one
  profile, rather than one sweep running a whole package set's tests.
- `profile` (optional, unset) - the cargo profile this sweep compiles and runs
  under: `"dev"` (alias `"debug"`) or `"release"`. Unset means the command's own
  default, which is what every sweep did before this key existed: dev under
  `brokkr check`, and the CLI / `[test] debug` answer under `brokkr test`.
  Custom `[profile.*]` names are not accepted - brokkr derives
  `BROKKR_TEST_BIN_DIR` from the profile's `target/` subdirectory, and cargo's
  mapping from a custom profile to its directory isn't reliably readable.

  This is the only way to express a **per-sweep profile split**. `[test] debug`
  is one project-wide default and applies to `brokkr test` alone; a profile lane
  can't carry a profile either, because a lane selects *which tests run*, never
  *how they are built*. A repo with a wall-clock contract that only holds
  optimized declares one entry with `profile = "release"` and puts the timing
  tests in it, while the rest of the suite keeps the fast dev build:

  ```toml
  [[check]]
  name = "timing"
  profile = "release"
  only = ["tape_lateness_under_acceleration", "read_market_latency"]
  curated = true

  [test.profiles.timing]
  sweeps = ["timing"]
  ```

  The profile reaches **every** cargo run that compiles the sweep - clippy, the
  test-phase pre-build, the test run, the process-isolated per-test
  invocations, and the coverage enumeration - and `BROKKR_TEST_BIN_DIR` points
  at the matching `target/` subdirectory, so a test that spawns its
  just-rebuilt binary finds the one this sweep actually built.

  It is part of the **build shape**, unlike `curated` and
  `test_exclude_packages`: `cfg(debug_assertions)` decides which code exists, so
  a dev and a release sweep of the same features are two different compiles and
  two different lint surfaces, and neither dedupes into the other. Two
  consequences worth planning for - clippy runs once per profile rather than
  once for both, and under a `certifies = "complete"` profile the release sweep
  enumerates its own coverage universe, so a narrow one wants `curated = true`
  the same way a narrow `rustflags` sweep does.

  `brokkr test` honours it too, one sweep at a time: an explicit
  `--debug`/`--release` still wins, then the sweep's `profile`, then
  `[test] debug`. That is what keeps the split honest - a project can set
  `[test] debug = true` for the fast inner loop without the documented
  `brokkr test tape_lateness_under_acceleration` silently switching to dev and
  failing on the build profile rather than on the code.
- `curated` (optional, default `false`) - declares the entry a hand-picked
  subset for coverage purposes, and requires the entry to carry its own
  `tests`/`skip`/`only` filters (an unfiltered "curated" entry is just an
  uncounted full sweep - parse error). Two effects, both scoped to
  `certifies = "complete"` profiles: the entry is **exempt from the
  complete-universe rule** (a `complete` profile need not reference it, so a
  curated subset can live in its own deliberately-run profile while a gate
  exists), and when a gate lane *does* run it, the coverage audit exempts its
  build shape's non-run pairs from the universe - counted, reported in the
  trailer (like `test_exclude_packages`) and in the `--json` `coverage.curated`
  field, never orphaned, and never credited to a `[[quarantine]]` entry. The
  exemption is keyed on the sweeps, not the shape: a non-curated entry sharing
  a build shape with a curated one keeps that shape fully audited. Coverage
  policy only - never part of the build shape, so a curated and a plain sweep
  of the same shape still dedupe to one clippy run.

A worked example - a deterministic-simulation (madsim) gate as several sibling
sweeps sharing one isolated madsim target dir, each running its own curated
subset, selected together by a `sim` profile:

```toml
[[check]]
name = "sim-common"
packages = ["nautilus-common"]
features = ["simulation"]
rustflags = ["--cfg", "madsim"]

[[check]]
name = "sim-core"
packages = ["nautilus-core"]
features = ["simulation"]
rustflags = ["--cfg", "madsim"]
only = ["virtual_time"]
curated = true

[test.profiles.sim]
sweeps = ["sim-common", "sim-core"]
```

Here `sim-common` runs its package's full test set, so under a complete gate
it must join the gate's lanes like any other entry; `sim-core` runs only a
`virtual_time` subset and is `curated`, so its remaining pairs are exempt and
the `sim` profile stays legal alongside the gate whether or not the gate's
lanes reference it.

The legacy `[check]` table form (with `consumer_features`) is rejected at
parse time with a migration message - move the flags into a `[[check]]` entry.

## `parallel` - running a sweep's test binaries at once

`parallel = { budget = N }` on a `[[check]]` entry runs that sweep's test
binaries concurrently instead of letting cargo run them one after another.

The floor it dissolves: cargo runs each test binary sequentially, and
`--test-threads` parallelizes only *within* a binary. A sweep's wall time
therefore cannot fall below the sum, over binaries, of each binary's slowest
test - and no amount of `test_threads` moves it, which is why a project that
has already tuned threads measures 8 and 16 identically. A single-test binary
contributes its whole duration because it has nobody to overlap with. Running
the binaries concurrently collapses that sum to a maximum.

**The budget counts tests in flight, not binaries.** Binaries times threads is
the real concurrency: seven binaries at `test_threads = 8` is fifty-six
concurrent tests, which on an ordinary box is *slower* than the figure the lane
is chasing. A key naming concurrent binaries would let a config ask for that
while looking like it asked for seven. The number a project has already tuned
is the total, so the total is what the key takes - each binary claims
`min(its test count, budget)` slots, at least one, and runs under a matching
`--test-threads`. Slices are handed out largest-binary-first, so the long pole
is admitted while the pool is whole.

`budget = 0` is a load error. There is no spelling for "unlimited": an
unbounded lane is precisely the mistake the key exists to make unspellable.

### What it does not do

It does **not** isolate. Tests within one binary still share a process, and
that is the point - a shared-process parallel lane is the only place the
process-global-state class (two tests contending over a global logger and a
shared capture buffer) is visible at all. `isolation = "process"` dissolves
that contention along with the bug's visibility, and the two keys are mutually
exclusive: a sweep setting both is refused before any test runs.

What it *adds* is exposure to **machine-global** state. Several binaries at
once is several processes at once, so a per-machine singleton - a daemon
holding an instance lock, a fixed socket path, a shared state dir - becomes
contended where a sequential lane never showed it. That class arrives as
**flakes rather than clean failures**, which is the thing most likely to get
the lane blamed for a defect it merely exposed. Establish how such state
isolates across concurrent binaries before adopting, and treat the serial side
as the safety valve rather than the tuning knob.

Doctests are not run on a `parallel` sweep - they live in the `--doc`
pseudo-target, which is not one of the binaries the lane fans out over. The
omission is announced rather than swallowed; run them from a sweep without
`parallel`.

### Enumerate the parallel side, not the serial side

There is deliberately no per-entry serial-group key, because the sweep list
already composes: `[[check]]` entries run strictly one after another, so a
second entry with no `parallel` key **is** the serial lane.

Write the partition with the parallel side enumerated and the complement
unfiltered, never the reverse:

```toml
[[check]]
name = "fanout"
parallel = { budget = 8 }
tests = ["parser_*", "codec_*"]   # opt in, explicitly

[[check]]
name = "rest"                      # unfiltered: today's sequential behaviour
```

Both halves then fail safe. A binary added later lands in the unfiltered
complement and shows up **serial and slow** rather than parallel and flaky. A
*stale* name in the enumerated `tests` list is a hard cargo error (`no test
target named X`, globs included), so the parallel side cannot quietly shrink -
note that the dead-filter guard covers `skip` and `only` only, and does not
judge `tests`, so cargo's own target resolution is what is load-bearing here.
It fires on every run rather than only under `certifies = "complete"`, which
makes the coverage gate a third line of defence rather than the first.

Both entries carry the same features, so they share a build shape and there is
no second compile; the coverage ledger already reconciles multi-sweep ran-sets.

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

Optional. Five things live here: a default cargo package, a default
validation profile, a gate profile, a doctest toggle, and the named profiles
that selectively reference `[[check]]` entries.

```toml
[test]
default_package = "pbfhogg"
default_profile = "tier1"
gate_profile = "gate"
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
  when neither is passed. It is the project-wide **default**, not a rule: a
  `[[check]]` entry's own `profile` sits between it and the CLI flags, so a
  sweep that must run optimized still does under `brokkr test`. Affects
  `brokkr check` not at all - check's test phase builds dev unless the sweep's
  `profile` says otherwise, and this field is never consulted there.
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
- `gate_profile` names the profile `brokkr check --gate` runs. Load-time
  validation requires it to exist and to carry `certifies = "complete"`, so
  docs and hooks can say `brokkr check --gate` and stay correct through
  profile renames.
- `[test.profiles.<name>]` declares a test selection layered onto one or more
  `[[check]]` entries. Fields: `sweeps` (required, list of `[[check]]` entry
  names), `tests` (`--test <name>`), `only` (positional substring filter),
  `skip`, `include_ignored`, `test_threads`, `env`. `extends = "<other>"`
  walks the chain with cycle detection; collections are replaced (child
  wins), env merges key-by-key.
- `skip` entries are either bare substrings (libtest `--skip`) or
  package-qualified tables - `{ package = "nautilus-infrastructure",
  pattern = "serial_tests::" }` - filtered out of the enumerated set rather
  than expressed as cargo selection, which is the only way to distinguish
  identical module paths in different packages (integration-test paths carry
  no crate prefix). Qualified entries require `isolation = "process"`
  (enforced at resolve time), and a test name existing in both a skipped and
  an unskipped package is a reported collision, never half-obeyed.
- **`skip` and `only` substrings must be at least four characters**, in a
  profile block or a `[[check]]` entry alike (for a qualified skip, the
  floor is on the `pattern` half). Shorter is a load-time error: a
  substring that short matches nearly every test name, so it suppresses or
  selects tests nobody chose while still matching *something* - which is
  the one dead-filter shape the `coverage` phase cannot detect. Breaking
  change for configs that carried one; the fix is the substring that was
  meant, and there is no opt-in for a broad filter.
- **A filter that matches no test fails `brokkr check`.** Under a
  `certifies = "complete"` profile the `coverage` phase asserts every
  `skip` and every `only` - profile-level and entry-level alike, each
  individually - against the enumeration, and reports a dead one with the
  block it was written in. A filter is judged against the sweeps *it*
  applies to: an entry's filter against the lanes running that entry, a
  profile's filter against every sweep the profile runs. So a profile-level
  skip naming a test outside a package-scoped sweep is live as long as some
  sweep of that profile runs it. Same rule as a stale `[[quarantine]]` entry, and
  for the same reason: a filter selecting nothing is a name that drifted,
  and a dead `only` leaves its lane evaluating nothing while still reading
  as a gate. Complementary to the test phase's `zero tests ran` refusal,
  which catches the same defect at run time in any sweep that runs. See
  `docs/commands/check.md`.
- `isolation = "process"` runs each of the profile's tests in its own
  `cargo test … -- --exact <name>` process.
  `--test-threads=1` serializes tests inside one process per test binary; it
  does not isolate them, and tests touching process-global state (a global
  logger) need the fresh-process guarantee CI's nextest provides. The sweep's
  selection argv is reused verbatim per test - identical build fingerprint,
  cargo-provided test env - at the cost of one cargo spawn per test, sized
  for a serial family of a dozen tests, not thousands. Requires
  `test_threads` unset or 1; never runs doctests; `brokkr check -- …` extra
  args are rejected on an isolated sweep. Merges through `extends`.
- `lanes = ["tier1", "serial"]` composes profiles as a *list of runs*:
  each lane resolves independently and the test
  phase runs every lane's sweeps in order (labels lane-qualified,
  `tier1/default`), while the clippy phase dedupes on build shape so two
  lanes sharing a `[[check]]` entry are linted once. The opposite of
  `extends`: tier1's `skip` and serial's `only` are contradictory by
  construction, so no merge can express them. A lanes profile may carry only
  `lanes`, `certifies`, `skip_phases`, and `description`; its lanes must
  exist, must not nest, and must not declare `certifies` themselves (the
  claim belongs to the composing profile) - all load-time errors. Under
  `certifies = "complete"`, every lane must individually satisfy the interim
  no-narrowing rule. `brokkr test` under a lanes profile keeps one sweep per
  build shape, since it drops filters anyway.
- `certifies = "complete" | "partial"` declares what a green run of the
  profile claims, and permissions derive from the claim.
  `partial` may set `skip_phases` (subtractive list of check phases, validated
  against the phase names) and use `-p` scoping; it prints `check partial` and
  exits **10** on success so `brokkr check && git commit` fails closed.
  `complete` prints `check complete`, exits 0, rejects `skip_phases`, `-p`,
  and `extends` (an inherited filter set defeats an explicit claim), and runs
  the **coverage phase**: every (build shape, test) pair that no lane runs
  must be justified by a `[[quarantine]]` entry or be `#[ignore]`d at the
  source, or the check fails as orphaned. `[test] doctests = false` needs a
  `[[quarantine]] category = "doctests"` entry. Profiles without `certifies`
  behave exactly as before the key existed (binary exit codes, `check
  passed`). Neither `certifies` nor `skip_phases` is inherited through
  `extends`.
- Profiles use Rust module paths as the annotation surface; `only` / `skip`
  translate directly into cargo substring filters and `--skip`.
- The legacy `[test.sweeps.*]` map is rejected at parse time. Sweeps now live
  in `[[check]]` entries; profiles reference them by name.

## `[[quarantine]]` entries

The justification ledger for coverage accounting, audited on every `certifies = "complete"` run:

```toml
[[quarantine]]
pattern = "test_twap_calculates_size_schedule_with_remainder"
issue   = "B14"
reason  = "expectation width-sensitive under unified 128-bit build"

[[quarantine]]
category = "doctests"
issue    = "B42"
reason   = "42/55 persistence doctests fail to compile"
```

- Exactly one of `pattern` (a test-name substring, libtest filter
  semantics) or `category` (only `"doctests"`). `issue` and `reason` are
  required - the issue ID is what turns the list from a graveyard with
  good manners into a countdown.
- `package = "<pkg>"` (optional, `pattern` entries only) restricts the
  entry to one package's pairs. Without it, a name-only pattern absorbs
  same-named pairs in every package, so a test that later stops running
  for an unrelated reason lands as accounted instead of orphaned.
- Staleness is mechanical in both directions: a `pattern` entry justifying
  zero non-run pairs fails the check (delete it when the bug closes), and
  a `doctests` entry with `[test] doctests = true` is rejected at load
  time. Per-entry pair counts print on every complete run, so an entry
  silently growing (a new test riding an old substring) is visible.

For the sweep-selection ladder used by `brokkr check` (and how `brokkr test`
diverges) see `docs/commands/check.md`.
