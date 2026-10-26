# brokkr.toml

Per-project config consumed by brokkr. Lives at the project root (`./brokkr.toml`).

This file documents the core of the **schema-universal** parts of
brokkr.toml - every section below applies in any project. Each `##` heading is
addressable on its own: `brokkr man config lints` prints that section and
nothing else, `brokkr man config include bin` prints two, and bare
`brokkr man config` lists them.

The rest of the universal schema is split by subject:
- Source-convention gates (`[gremlins]`, `[header]`, `[[textlint]]`,
  `[textlint_preset]`, `[[script_check]]`, `[manifest]`, `[[dependency_rule]]`,
  `[rustdoc]`) -> `docs/brokkr.toml.conventions.md` (`brokkr man conventions`)
- Sweeps and test policy (`[[check]]`, `[[lint_sweep]]`, `parallel`, `[test]`
  and its profiles, `[[quarantine]]`) -> `docs/brokkr.toml.sweeps.md`
  (`brokkr man sweeps`)

Project-specific config blocks live in their own docs, so a checkout only ever
reads the ones that apply to it:
- Datasets (`[<host>.datasets.*]` pbf/osc/pmtiles) and the `--variant` /
  `--osc-seq` / `--tiles` flags -> `docs/brokkr.toml.datasets.md` (map-data
  projects only; `brokkr man datasets`)
- `[<host>.tilegen.*]` -> `docs/brokkr.toml.elivagar.md`
  (`brokkr man elivagar-config`)
- `[litehtml]` -> `docs/projects/litehtml.md`
- `[sluggrs]` -> `docs/projects/sluggrs.md`
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
(falling back to `$HOME/.config/brokkr/brokkr.toml`; an empty or relative
`XDG_CONFIG_HOME` counts as unset, as the XDG spec says, rather than resolving
against the cwd). It holds the conventions
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
that project's config. One exception: a user `[[script_check]]` with `when =
"gate"` is not refused in a project that declares no complete profile, as a
project's own would be; it runs under a complete claim and is logged as not
run on every other run. Presets resolve after all config layers are composed,
so a user rule can use a project preset and a project rule can use a user
preset. A preset not referenced by any composed rule is an error, except a
user-only preset whose last referencing rule a project shadowed: that one is
dropped, so the shadowing opt-out below never fails the load. A project rule
that uses a user preset only loads on machines that have the user config.

**Merging.** The user file is the lowest-precedence config layer. Its entries
run first, followed by included project files and the project's own file.
Shadowing is by `name`: a higher layer that reuses a lower entry's name
replaces it outright. That is the opt-out - a project that must not run a
personal rule redefines it under the same name, rather than needing a
suppression syntax nobody would remember.

The layer is composed before config sections are parsed, so named entries and
presets resolve across the same complete config.

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

## `include` - composing brokkr.toml from several files

```toml
project = "pbfhogg"
include = ["../shared/hosts.toml", "../shared/checks.toml"]
```

A top-level `include` lists further config files that are folded in *beneath*
the file naming them. Any key of the project schema may appear in an included
file - host sections, datasets, `[[check]]`, `[lints]` - except `project`, which
belongs to the project's own `brokkr.toml`. Included files may `include` more
files in turn.

**Precedence.** A file's own keys override everything it includes, and a later
include overrides an earlier one. Merging is key by key:

| Both sides hold | Result |
|---|---|
| a table | merged recursively - a host section may be split across files |
| an array of tables (`[[check]]`, `[[textlint]]`, `[[script_check]]`, ...) | joined, included entries first; an included entry whose `name` the including side reuses is dropped (the user layer's shadowing rule) |
| anything else | the including side's value replaces the included one |

**Paths.** `include` paths resolve against the directory of the file naming
them. Every *other* path in an included file (dataset files, `[[script_check]]`
commands, globs) means what it means in `brokkr.toml`: it resolves against the
project root. An included file is config for the project that includes it,
not a project of its own.

**Errors, not skips.** A missing, unreadable or malformed include is an error
naming the file that included it, as is a cycle, or a file reached twice (its
`[[array]]` entries would be counted twice).

**Where brokkr writes.** Dataset registration (`download`, `--as-snapshot`,
`--refresh`) edits the file that defines `[<host>.datasets.<name>]`, located
from the composed config's per-table provenance. A dataset not registered
anywhere yet goes into the file that already holds that host's datasets, when
that is one file, and into `brokkr.toml` otherwise. A dataset table split across
two files is refused: editing one half would leave a mixture.

**Clean-tree checks.** Included files inside the checkout are exempt from the
measured-run dirty check, exactly as `brokkr.toml` is, since brokkr writes
registrations into them too.

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
# A hostname colliding with a reserved section can use [host.<name>],
# e.g. [host.test].
```

`include` is consumed while config layers are composed. Other reserved
top-level keys are parsed as their named sections; an unrecognized table is a
legacy hostname section, and an unrecognized non-table value is rejected.
`[litehtml]`, `[sluggrs]`, `[ratatoskr]`, `[piners]`, `[dellingr]`, and
`[mogwai]` belong only to projects with the matching `project` value; a
section for a different project is a load error. The matching section is
required for `litehtml-rs`, `sluggrs`, `ratatoskr`, `dellingr`, and `mogwai`.
Piners keeps its default corpus and registry paths when `[piners]` is absent.
Project-specific config details are in the linked project docs at the top of
this document.
`[host.<name>]` is also accepted and lets a hostname use a reserved section
name. Host tables must set at least one supported option, and unknown fields
inside one are rejected. When host sections exist but none names this machine,
defaults apply silently on ordinary commands (a config shared across machines
is normal); `brokkr env` warns and lists the configured hostnames, which is
where a misspelled hostname shows up. Since legacy hostnames are open-ended, a table with a
valid host shape is necessarily interpreted as a host even if its name
resembles a misspelled section. Datasets are host-scoped (no global
`[datasets]` section). Path resolution: host config -> defaults (`data/`,
`data/scratch/`, `data/tilegen/`, cargo target dir). `output` is the durable
tilegen output store (map-data projects): `ArchiveStore` admits each successful
tilegen archive as `<output>/<dataset>-<variant>-<commit>.pmtiles`, and
`pmtiles-inspect`/`diag`/`svg`/`regress`/`pmtiles-corpus` resolve it by
`--variant` + `--commit`. It is kept SEPARATE from `scratch`
on purpose - elivagar wipes its `--tmp-dir` (`<data>/tilegen_tmp`) every run,
and on some hosts `scratch` points at that same dir, so archives written into
scratch were destroyed by the next run. `brokkr` refuses to write outputs into
a dir that coincides with scratch/tmp; the store keeps the last 5 archives per
`(dataset, variant)` pair - scoped so that building one variant can never evict
another's archives at the same commit. Group membership is decided by
constructing the name shape and requiring a hyphen-free commit token after it,
never by parsing a filename back: dataset names carry hyphens, and a prefix test
alone would let a variant claim a dash-extending sibling's files (`raw`
swallowing `raw-fast`) and evict them. `brokkr clean --archives` uses the same
store pruning operation with its configured keep count and dry-run policy.

**Host feature policy.** Host `features` are added to measured-request builds
through the shared build context, and to the `passthrough`, `verify`, `serve`,
`ingest`, and `update` paths. Standalone `run`, `install`, `check`, `clippy`,
`test`, and Criterion `bench` use their own cargo/default or sweep feature
selection and do not inherit host features. CLI `--features` are additive to
host features in paths that apply them, with duplicates removed. This is the
complete policy for which Cargo builds inherit host features. Host `port` is
the nidhogg server port (default 3033) that
`serve`/`status`/`query`/`geocode`/`verify`/the benches
target; brokkr passes it to the server it spawns as `PORT`, but never reads
`PORT` from its own environment - an unrelated `PORT` in the shell must not
retarget the commands.
Reserved top-level keys parsed as config sections, or consumed by composition:
`project`, `include`, `host`,
`litehtml`, `sluggrs`, `ratatoskr`, `piners`, `dellingr`, `mogwai`, `check`,
`dependency_rule`, `test`, `quarantine`, `capture_env`, `gremlins`, `header`,
`textlint`, `textlint_preset`, `script_check`, `manifest`, `rustdoc`, `deps`,
`lints` (and its `clippy` alias), `bin`, `member_check`, `fixture_lint`,
`disable_toolchain`, `cargo_env`.

`strip_rustflags`, `lint_sweep` and `install` are also brokkr's, but only as
non-table values: a `[strip_rustflags]`, `[lint_sweep]` or `[install]` table
is still read as a hostname section, while the `strip_rustflags = [...]` array
and the `[[lint_sweep]]` and `[[install]]` arrays of tables are brokkr's.

## `worktree_keep`

```toml
[plantasjen]
worktree_keep = 6
```

How many persistent `--commit` worktrees this project keeps before the
least-recently-used are evicted. Default 6. Per host because the disk is a host
property. `0` is a config error; omit the key to use the default. Zero would
evict every worktree the moment a second was wanted, which is indistinguishable
from the feature being broken.

Eviction runs when a **new** worktree is cut, never on reuse and never on a
plain run. That keeps the cost adjacent to a build you are already paying for
rather than appearing as an unexplained pause, and it stops a measurement from
becoming a destructive operation merely by happening. The consequence is that a
project which stops growing never shrinks on its own; `brokkr clean
--worktrees` is the explicit hammer for that.

Eviction makes room for the worktree being cut, so the steady state is
`worktree_keep` worktrees, not one more. The worktree being cut is never itself
a victim - including when it replaces a stale directory of the same name. The
whole sequence (eviction, replacement, creation, bookkeeping) runs under the
global lock, which is then held through the build and measurement, so a
concurrent brokkr can neither evict a worktree mid-build nor lose a
bookkeeping update.

**This is a growth damper, not a bound.** Retention is per project, so the
disk-wide total grows with the number of projects that use worktrees. Only a
global byte budget could promise "never fills the disk", and a count is a proxy
for the real constraint, which is bytes. The bookkeeping records last-use time
but does not currently record worktree size; a future size-based rule would
need to add that measurement.

The default comes from `worktree_record::DEFAULT_KEEP` and is chosen for the
**heaviest** dependency graph, not the lightest. A cold worktree build on that
graph is costly, and a silent multi-minute stall is a worse failure than
keeping some extra worktrees. The host's `worktree_keep` value can lower or
raise the default.

Two rules the eviction obeys:

- **A worktree with uncommitted work is never evicted**, regardless of whether
  git would have succeeded in removing it. That is correctness, not courtesy: a
  dirty worktree is the one place where removal destroys something
  unrecoverable. If git cannot be consulted at all, the worktree is assumed
  dirty and kept. The same rule guards replacement: a directory at the name
  `--commit` wants that is not checked out at the requested commit is replaced
  only if clean, and a dirty (or unreadable) one fails the run with its path
  rather than being force-removed.
- **Failures skip and continue**, because housekeeping must not fail the
  measurement you actually asked for. Since skips can hold the count above the
  limit, brokkr reports the overage on *every* run where it is exceeded, not
  once when a removal fails - a damper that has quietly stopped working is the
  original problem, and you should not learn about it from the volume filling.

Bookkeeping lives in `.brokkr/worktrees.toml` at the project root, written on
both create and reuse. It is safe to delete; losing it costs a suboptimal
eviction order, never data. A worktree with no record sorts as oldest, since it
predates the bookkeeping - treating it as freshest would make pre-existing
worktrees permanently un-evictable. A file that exists but does not parse is
reported as an error on every `--commit` run and left untouched (eviction and
bookkeeping are skipped, the run itself proceeds) rather than silently read as
empty and overwritten; delete it to recover.

Where one project root governs several checkouts (the config-one-level-up
layout), they share that one file. Each checkout's eviction and pruning touch
only records named for its own worktrees (`<checkout key>/<short hash>`, the
key being the checkout's name plus a hash of its path, matched by construction,
so checkout `foo` never claims `foo-bar`'s).

A worktree is never *reused* unless its checkout is exactly its commit. The
worktree container (`~/.brokkr/worktrees`) is writable to sandboxed agents, so
one can be edited between runs, and reusing it would measure the edit under
the commit's name. Such a run fails with the worktree's path; `brokkr clean
--worktrees` clears it.

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
the next brokkr run in that directory adopts the leftover and restores it,
printing a `[warn]` that it did so. The routine move aside is identical on
every run, so a command that keeps a run log (`check`, `test`, measured runs,
`bench`) records it there only; any other command prints it as a quiet-gated
`[run]` line rather than drop it.
Worktree builds (`--commit`) run in a separate checkout that may carry its own
committed pin; brokkr re-points the disable there for the build, so the
worktree's pin is disabled too rather than honored.

## `strip_rustflags` - removing flags from the tree's cargo config

```toml
strip_rustflags = ["-Clink-arg=-fuse-ld=lld"]
```

A top-level array, beside `disable_toolchain`. It is a plain key rather than
a table so it claims no name a hostname section could already use.

For a foreign checkout whose own `.cargo/config.toml` carries a flag that is
wrong on this host. The case it was written for: the project's
`[target.'cfg(target_os = "linux")'] rustflags` names `-fuse-ld=lld`, the
user-wide `~/.cargo/config.toml` names another linker, cargo joins every
matching target table, the C driver gets both, lld is not installed, and every
link fails in code nobody touched. Cargo cannot subtract a flag: `--config`
merges arrays, and `RUSTFLAGS` replaces every config source at once, dropping
the project's `-Dwarnings` with the flag you meant. Moving the whole file aside
would drop those too, so local gates would run laxer than CI.

While brokkr holds the global lock, the one file cargo reads in
`<tree>/.cargo` (the legacy extensionless `config` when it exists, else
`config.toml`) is replaced by a copy with the listed flags removed, and the
original is put back before the lock is released. It follows
`disable_toolchain`'s lifecycle exactly: the build root on every locked
command, the checkout itself under `--commit`, restored on exit, error,
interrupt or watchdog kill.

**What an entry matches.** An entry is a token sequence: a string splits on
whitespace, and an array of strings gives tokens verbatim, for an operand that
contains a space. A `-C` or `-Z` token followed by another forms one unit with
it, so `"-Clink-arg=x"` and `"-C link-arg=x"` are the same entry and match
either spelling in the file, while a payload alone (`"link-arg=x"`) never
matches half of a pair. Matching is physical deletion from each `rustflags`
value of the file - `build.rustflags`, `host.rustflags`,
`host.<triple>.rustflags` and every `target.<triple-or-cfg>.rustflags` - not an
interpretation of rustc's command line:

- Every match is found against the original value before anything is removed,
  so removal never creates a new match. Two entries matching overlapping runs
  are refused rather than chosen between.
- A value with no match is left byte-for-byte as it was, and a file with no
  match anywhere is not replaced at all. A string value stays a string (its
  surviving tokens rejoined with single spaces), so cargo's string-replaces,
  array-appends merge is unchanged.
- brokkr refuses to edit a value where a match follows a response file
  (`@path`) or `--`, or directly follows an option that may take it as an
  operand (a bare short option such as `-o`, or a long option without `=`),
  other than the operand-less `-O -g -v -V -h --test --verbose --version
  --help`. What another config layer contributes before the value is outside
  the contract.
- Includes are not followed; only flags written in the selected file are
  edited. An entry that matched nothing is warned about on every run, naming
  any `include` or response file it saw as places the flag might live instead.
- When a removal empties every matching target table, cargo falls through to
  `build.rustflags`; brokkr warns when the source a host build uses changed.

A non-array value, empty entries, entries that are not strings or string
arrays, and two entries naming the same flag are config errors. A symlinked
config file is refused. The user-wide config layer rejects the key; under
`include` the including file's list replaces an included one, like any plain
value.

**What is promised about the file.** The original is moved, never copied, and
every move is a rename that will not replace an existing file, so an editor
saving the config during the window is never lost: brokkr finds a file it did
not install and stops with a refusal naming every file present, changing
nothing. The installed copy starts with a `# brokkr strip_rustflags v1` line
naming hashes of the original and of itself, which is how a later run tells
its own untouched install from an edit. A run that died mid-window is put back
by the next brokkr run in that tree, whatever `brokkr.toml` says by then, and a
`--commit` worktree is put back before its cleanliness is checked for reuse.
Three names next to the file are brokkr's own while it works -
`<file>.brokkr-original`, `<file>.brokkr-installing` and
`<file>.brokkr-displaced`; changing those by hand, or writing through a
descriptor an editor had open on the file before the window, is outside what
is guarded. The installed copy and its original count as clean for
measurement and history only when the original still equals `HEAD` and the
copy is brokkr's untouched install over it. Code:
`src/cargo_config_strip/`, `src/tree_policy.rs`.

## `[cargo_env]` - variables computed per cargo invocation

```toml
[cargo_env]
BA_SOURCE_IDENTITY = "python3 scripts/source_identity.py"
```

Each entry names an environment variable and a shell command. Before every
cargo brokkr launches, every command runs and its trimmed stdout is set on that
cargo as the variable. The use it exists for: a build script whose real input
cargo cannot watch through mtimes (a content-exact source identity, say)
declares `cargo::rerun-if-env-changed=NAME` instead of rerunning on every
build. A rerun build script dirties its own package, so the difference is every
consumer recompiling on every cargo invocation against recompiling only when
the value changes. A plain `cargo` or rust-analyzer never sets the variable,
so such a build script should keep rerunning when it is absent.

- **Per invocation, never cached.** An edit between two cargo invocations of one
  run changes what the second gets. Each evaluation is a point-in-time reading:
  an edit during the build itself is the command's to detect.
- **What counts.** Every cargo brokkr launches itself, except invocations that
  run no build script (`metadata`, `clean`, `tree`, `locate-project`, `pkgid`,
  `version`, `help`, `fmt`, and cargo's own `--version`/`--help`). A cargo run
  by a script check, a test or a build script is outside it. A
  `brokkr check` lane that runs its prepared binaries without invoking cargo
  evaluates nothing: those binaries carry the value they were built with.
- **Where it runs.** `sh -c` in the directory cargo is about to run in - the
  worktree for a `--commit` build, so a relative command runs that tree's own
  script against that tree. The environment is the one that cargo gets
  (brokkr's, the sweep's, the hold capability) with every configured name
  removed, so no command reads a stale value of itself or another entry.
  Entries are evaluated in name order, independently.
- **Failure is fatal.** A non-zero exit, 30 seconds without finishing, more than
  4 KiB of output, output that is empty, not UTF-8, more than one line or
  carries a control character, or output that did not close - any of these
  fails that cargo spawn and is printed once, with the command, the directory
  and its stderr. Every later cargo of the run is refused, and the command exits
  non-zero even where a path that tolerates a failed build carried on.
- **Build inputs, not test environment.** The value is set on cargo, so build
  scripts and rustc see it, and a test `cargo test` runs inherits it; the
  lanes that execute test binaries directly do not set it. Tests must not read
  it at runtime.
- **Refused at load:** a name that is not an environment variable name; one in
  a namespace brokkr, cargo, rustc or the loader owns (`BROKKR_*`, `CARGO*`,
  `RUST*`, `LD_*`, `PATH`, `HOME`); a blank command; and a name a
  [`capture_env`](#top-level-shape) pattern matches - capture reads brokkr's own
  environment, which never carries the per-invocation value.
- **Refused per invocation:** a cargo config `[env]` entry for the name, in the
  chain that cargo reads or a `--config` it is given. `force = true` there
  would replace what brokkr hands cargo, and a plain entry would be a second
  source for one input.

The values go to the run log, one line per variable per cargo invocation
(`cargo_env NAME=value (12ms in <dir>, for cargo test)`). The user-wide config
layer rejects the table: what a project's build scripts read is the project's
to say. Code: `src/cargo_env.rs`.

## `[deps]` section

Tuning for the `brokkr deps` audit (optional; `deps` runs in any Rust+git repo).

```toml
[deps]
workspace_dep_ignore = ["lychee", "cargo-*", "backtest"]
```

- `workspace_dep_ignore` - workspace-dependency names the unused-workspace-dep
  phase must not flag even when no member inherits them (dev tools, top-level
  members whose use cargo metadata can't attribute). An entry ending in `*` is
  a prefix match. Every entry must match a declared `[workspace.dependencies]`
  key when `brokkr deps` runs; stale entries are refused. See
  `docs/commands/deps.md`.

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
# profile = "fast"           # or a cargo profile by name; wins over `debug`
```

- `default` - the target name bare `brokkr run` launches. Without it, a sole
  runnable runs; several runnables print an index and exit 0.
- `install` - package names installed via `cargo install --locked --path
  <pkg dir>`, in order. Without it, the workspace's sole bin-carrying package installs;
  several is an error naming this key. A package with no bin target is a
  config error.
- `debug` - default both commands to the dev profile (`cargo run` without
  `--release`, `cargo install --debug`). Unset means release for both,
  matching `brokkr test`. CLI `--debug` / `--release` / `--profile`
  (mutually exclusive) override in either direction.
- `profile` - the cargo profile both commands default to, by name: `"dev"`,
  `"release"`, or a custom `[profile.<name>]` from the workspace's
  `Cargo.toml` (`cargo run --profile <name>`, `cargo install --profile
  <name>`). Wins over `debug` when both are set - with a warning, since
  `include` composition can leave both set from different layers - and
  `"release"` or `"dev"` here overrides an inherited custom one. Names Cargo
  or brokkr owns under the target directory (`debug`, `test`, `bench`,
  `doc`, `tmp`, `verify`, `brokkr-worktrees`, and anything starting
  `rustflags-`, `unify-` or `fixture-`) are refused at load. Cargo checks
  that the profile exists when it builds.
- `install_feature_check` - `"off"` / `"gate"` (the default) / `"always"`:
  when `brokkr check` compiles the `install` packages' bins under
  package-mode feature unification - the way `cargo install` resolves them,
  without features sibling workspace members contribute - at the install
  profile (release for a custom one: the check runs under dev or release
  only, and the feature resolve it verifies does not depend on the
  profile). `"gate"` means only under a `certifies = "complete"` profile.
  Requires an explicit `install` list; the key without one is a load error.
  This is the member-check phase run over the install set: a `[member_check]`
  table (below) supersedes it, and setting both is a load error. See
  `brokkr man test-lanes member_check`.

## `[[install]]` - post-install steps

Steps `brokkr install` runs, in order, after every package installed
(optional):

```toml
[[install]]
name = "claude-mod"
copy = { from = "mod/orchestrate", to = "~/.claude/skills/orchestrate" }

[[install]]
name = "completions"
run = { command = "orchestrate completions zsh > ~/.zfunc/_orchestrate" }
```

- `name` - required and unique; it names the step in output and errors, and
  an including file reusing it replaces the included entry.
- exactly one action key per entry, each holding its own table:
  - `copy` - `from` (relative, inside the project root) and `to` (absolute,
    or starting with `~/`). Neither may hold an empty, `.` or `..`
    component or a trailing slash. Unknown fields are refused.
  - `run` - `command`, run as `sh -c` from the build root.

A blank name, a repeated name, an entry with no action or with both, a blank
`command`, and a `from` or `to` breaking the rules above are load errors.
Like `lint_sweep`, the key is claimed only as an array: an `[install]` table
is still a hostname section.
The user-wide layer cannot declare steps. What the steps do - exclusive
ownership of a copy destination, the ownership check, atomic replacement and
crash recovery - is in `brokkr man run install` (its "Post-install steps").

## `[member_check]` section

Compile every workspace member's production targets under that member's own
package-mode resolve - the `cargo install` boundary - one cargo invocation per
member (optional):

```toml
[member_check]
when = "gate"               # "off" | "gate" (default) | "always"
targets = ["lib", "bins"]   # default; any non-empty subset of lib, bins
profile = "dev"             # "dev" (default) | "release"
skip = [{ package = "xtask", reason = "tooling, never installed" }]
```

Without the table, a non-empty `[bin] install` implies the narrower install
gate described under `[bin]`; with it, every member is checked and
`[bin] install_feature_check` is a conflict. The semantics, refusals and what a green run claims are in
`brokkr man test-lanes member_check`.

## `[lints]` section

Lint suppressions, applied to **every phase that compiles** - `brokkr check`'s
clippy phase and its test phase, plus `brokkr clippy` (optional).

"Every phase that compiles" is the literal rule and includes the parts of a
phase that are not the obvious cargo run: the sweep pre-builds, every lane's
prebuild, and the `prepare` phase's `cargo test --no-run` builds (each lane's
and each shape's universe). Anything that invokes the compiler fails on a lint the project's
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
  the note below the list). The escape hatch for driving a
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
  has to act on. Parse-time rejection mirrors `allow`, plus the `@` split:
  exactly one `@`, non-empty halves, no whitespace, and a relative path.
  Absolute paths are rejected. See `docs/commands/check.md`.

  **The file scoping is clippy-phase only.** A lint that fails a *build* fails
  it during compilation, before any diagnostic reaches brokkr to be filtered,
  and `-A` has no path-scoped form - so in the test phase each entry
  contributes its lint name build-wide, and the run says so on its notice
  line. The narrow guarantee still holds everywhere it can be enforced. The
  alternative was an `allow_exact` that silently does nothing about the build
  failure it was written for.

In the test phase only rustc's own lints are injected (`clippy::`, `rustdoc::`
and `cargo::` names never reach a build). Each injected allow changes every
unit's fingerprint, so a lint the project can allow in its manifest's `[lints]`
is better allowed there. brokkr adds the `-A` flags to whichever rustflags
source cargo is already using - never by exporting `RUSTFLAGS`, which would
discard the project's own flags - and prints the layer it chose:

```
test: allowing deprecated via --config target."cfg(all())".rustflags ([lints] allow)
```

Do not use `[[check]] rustflags` to carry a lint allow; it isolates the sweep
into its own target dir. How the layer is chosen is in
`reference/check-internals.md`.

