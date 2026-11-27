# brokkr.toml

`brokkr.toml` is the per-project config brokkr reads from the project root.
This page covers the core of the schema that applies in every project: where
the file lives, the user-wide layer, `include`, the top-level keys and host
sections, and the reference sections for each core key and table. Each `##`
section is addressable on its own: `brokkr man config lints` prints that
section and nothing else, `brokkr man config include bin` prints two, and
bare `brokkr man config` lists them.

The rest of the universal schema is split by subject:

- **Source-convention gates** (`[gremlins]`, `[header]`, `[[textlint]]`,
  `[textlint_preset]`, `[[script_check]]`, `[manifest]`,
  `[[dependency_rule]]`, `[rustdoc]`): `brokkr man conventions`
- **Sweeps and test policy** (`[[check]]`, `[[lint_sweep]]`, `parallel`,
  `[test]` and its profiles, `[[quarantine]]`): `brokkr man sweeps`
- **Script benches** (`[bench.*]`, `[bench_params.*]`, `[data]`):
  `brokkr man bench-config`

Project-specific config blocks live in their own docs, so a checkout only
reads the ones that apply to it:

- **Datasets** (`[HOST.datasets.*]` pbf, osc, pmtiles and polygon):
  `brokkr man datasets`
- **`[litehtml]`**: `docs/projects/litehtml.md`
- **`[sluggrs]`**: `docs/projects/sluggrs.md`
- **`[mogwai]`**: retired with the `brokkr mogwai` command. It still loads
  in a mogwai project, whatever it holds, unvalidated and with a warning,
  and stays in the composed config a script's `config()` sees. mogwai
  benchmarks through script benches (`brokkr man bench`).
- **`[piners]`**: retired with `brokkr corpus` and `brokkr corpus-results`.
  It still loads in a piners project, whatever it holds, unvalidated and with
  a warning, and stays in the composed config a script's `config()` sees.
  piners runs its parity corpus through its own in-tree tool.
- **`[dellingr]`**: retired with the `brokkr dellingr` driver. It still loads
  in a dellingr project, whatever it holds, unvalidated and with a warning,
  and stays in the composed config a script's `config()` sees. dellingr
  benchmarks through `[bench.*]` (`brokkr man bench-config`).

pbfhogg uses script benches and generic commands (`docs/projects/pbfhogg.md`).

## Location

brokkr looks for `brokkr.toml` in the working directory, or failing that in
its **immediate parent**. It looks one level up, never all the way to the
filesystem root. A deeper search would silently attach to a stray
`brokkr.toml` in a home directory or to an unrelated enclosing project.

The one-level-up layout is for driving a checkout that isn't yours: brokkr's
state (`.brokkr/`, `data/`) stays in the parent and the foreign repo stays
clean. That layout splits two roots which coincide in the ordinary case:

- **Project root**: the directory holding `brokkr.toml`. It anchors
  `.brokkr/`, `data/` and every other brokkr-owned store.
- **Build root**: the working directory, where cargo and git run.

The build root is always the working directory. brokkr deliberately never
descends from the config directory to find the checkout: one config directory
may sit above several, and guessing which you meant is how a command operates
on the wrong tree.

**Run brokkr from inside the checkout, not from the directory holding
`brokkr.toml`.** From the config directory, every cargo-driving command would
point at a directory with no `Cargo.toml`. brokkr refuses by name
(`no Cargo.toml in DIR or any parent`) rather than forwarding cargo's own
error, which walks to the root and reads as though the project were broken.
Config-only commands (`man`, `results`, `history`) work from either
directory.

## User config

A second, optional config lives at `$XDG_CONFIG_HOME/brokkr/brokkr.toml`,
falling back to `$HOME/.config/brokkr/brokkr.toml`. An empty or relative
`XDG_CONFIG_HOME` counts as unset, as the XDG spec says, rather than
resolving against the working directory. The user config holds the
conventions that belong to you rather than to any one tree, and it applies in
every project brokkr detects.

The user config accepts these top-level keys and nothing else:

| Key | |
|---|---|
| `[[textlint]]` | rules, exactly as in a project config |
| `[textlint_preset.NAME]` | presets for those rules |
| `[[script_check]]` | gates, exactly as in a project config |
| `results_server` | where your script-bench runs go; user-only (`brokkr man config results_server`) |

Any other key (`project`, `[[check]]`, a host section, `[bench.*]`) is an
error naming the file. The rest of the schema describes one project (its
datasets, its sweeps, its bin targets) and has no meaning machine-wide. A
project-shaped key in a machine-wide file is a mistake, not a setting, so
brokkr rejects it rather than silently ignoring it.

brokkr interprets `[[textlint]]` `paths` globs and `[[script_check]]`
commands against each project's own tree, exactly as if you had written the
entry in that project's config. One exception: a project that declares no
complete profile doesn't refuse a user `[[script_check]]` with
`when = "gate"`, as it would its own. That check runs under a complete claim
and is logged as not run on every other run.

Presets resolve after all config layers are composed, so a user rule can use
a project preset and a project rule can use a user preset. A preset no
composed rule references is an error. The exception is a user-only preset
whose last referencing rule a project shadowed: brokkr drops it, so the
shadowing opt-out below never fails the load. A project rule that uses a user
preset loads only on machines that have the user config.

**Merging.** The user file is the lowest-precedence config layer. Its entries
run first, followed by included project files and the project's own file.
Shadowing is by `name`: a higher layer that reuses a lower entry's name
replaces it outright. That is the opt-out: a project that must not run a
personal rule redefines it under the same name, with no suppression syntax to
remember. brokkr composes the layer before it parses config sections, so
named entries and presets resolve across the same complete config.

**Trees with no brokkr.toml.** `check` runs in any Rust and git repo, and the
user layer applies there too. A repo that never adopted a `brokkr.toml` is
where a personal convention is likeliest to be the only rule there is. With
nothing to merge into, the user entries stand alone: `check` runs its
gremlins, textlint and `script_check` phases from them and skips the
config-driven rest.

**Overriding the path.** `BROKKR_USER_CONFIG=/path/to/file` reads the layer
from somewhere else. `BROKKR_USER_CONFIG=` (set, empty) switches it off
entirely, `results_server` included, for a CI job that must see only the
project's own rules. An absent file isn't an error. A file that exists but
doesn't parse is, because silently ignoring it would let one typo switch off
every rule you wrote.

`brokkr env` prints a `user cfg:` line with the resolved path and how many
entries of each kind the layer contributed, since a rule from outside the
tree is otherwise invisible from inside it. When `results_server` is set, it
also prints a `bench runs:` line naming the server.

## results_server

The user-only `results_server` key names a `brokkr-results-server`
(`brokkr man results-server`) by URL:

```toml
results_server = "https://bench.example.ts.net:8443"
```

With the key set, `brokkr bench` publishes every run there and
`brokkr results` reads script-bench runs from there, in every project. brokkr
neither reads nor writes the local `$XDG_DATA_HOME/brokkr/bench.db`. Without
the key, everything stays in that local file, and so it does when
`BROKKR_USER_CONFIG=` switches the user layer off.

The URL must satisfy all of these:

- The scheme is `http://` or `https://`, with a host.
- A port, if given, is a number.
- It has no credentials, since the server has no authentication.
- It has no query or fragment.

A path is a prefix for a proxy that routes by path. brokkr drops trailing
slashes and sends every request to the path with `/bench` appended. An
invalid value fails every command, naming the file.

The key belongs to you, not to a project. Composition takes it out of the
user layer before any merge, so it never appears in the composed config or in
a run's stored config. A project's `brokkr.toml`, or any file it includes,
that sets it to a value is an error naming that file. A `[results_server]`
table there is a host section.

## include

A top-level `include` lists further config files that brokkr folds in beneath
the file naming them:

```toml
project = "pbfhogg"
include = ["../shared/hosts.toml", "../shared/checks.toml"]
```

Any key of the project schema may appear in an included file (host sections,
datasets, `[[check]]`, `[lints]`) except `project`, which belongs to the
project's own `brokkr.toml`. Included files may `include` more files in turn.

**Precedence.** A file's own keys override everything it includes, and a
later include overrides an earlier one. Merging is key by key:

| Both sides hold | Result |
|---|---|
| a table | merged recursively, so a host section may be split across files |
| an array of tables (`[[check]]`, `[[textlint]]`, `[[script_check]]`, ...) | joined, included entries first; an included entry whose `name` the including side reuses is dropped (the user layer's shadowing rule) |
| anything else | the including side's value replaces the included one |

**Paths.** `include` paths resolve against the directory of the file naming
them, and so does a bench's `script_file` (`brokkr man bench-config`): both
name a piece of the config's own file layout. Every other path in an included
file (dataset files, `[[script_check]]` commands, globs) means what it means
in `brokkr.toml` and resolves against the project root. An included file is
config for the project that includes it, not a project of its own. The
directory is the file's real one: a file reached through a symlink resolves
beside its target.

**Errors, not skips.** Each of these is an error naming the file that
included it:

- a missing, unreadable or malformed include
- a cycle
- a file reached twice, whose `[[array]]` entries would be counted twice

**Where brokkr writes.** A write to `[HOST.datasets.NAME]` edits the file
that defines it, located from the composed config's per-table provenance. A
dataset not registered anywhere yet goes into the file that already holds
that host's datasets, when that is one file, and into `brokkr.toml`
otherwise. brokkr refuses a dataset table split across two files, since
editing one half would leave a mixture.

**Clean-tree checks.** Included files inside the checkout are exempt from the
measured-run dirty check, exactly as `brokkr.toml` is, since brokkr writes
registrations into them too.

## Top-level shape

A `brokkr.toml` names its project and then holds reserved sections and host
sections:

```toml
project = "pbfhogg"

[plantasjen]
data = "data"
scratch = "data/scratch"
target = "target"
worktree_keep = 6
drives.source = "nvme"
drives.data = "ssd"
features = ["linux-direct-io", "linux-io-uring"]

# Map-data projects add [HOST.datasets.*] tables here
# (brokkr man datasets).
```

brokkr consumes `include` while it composes config layers. It parses the
other reserved top-level keys as their named sections. An unrecognized table
is a legacy hostname section (`brokkr man config host-sections`), and an
unrecognized non-table value is rejected.

`[litehtml]`, `[sluggrs]`, `[ratatoskr]`, `[piners]`, `[dellingr]` and
`[mogwai]` belong only to projects with the matching `project` value; a
section for a different project is a load error. `litehtml-rs` and
`sluggrs` require their matching section. The project docs
listed at the top of this page hold the details.

These reserved top-level keys are parsed as config sections or consumed by
composition:
`project`, `include`, `host`, `litehtml`, `sluggrs`, `ratatoskr`, `piners`,
`dellingr`, `mogwai`, `check`, `dependency_rule`, `test`, `quarantine`,
`capture_env`, `gremlins`, `header`, `textlint`, `textlint_preset`,
`script_check`, `manifest`, `rustdoc`, `deps`, `lints` (and its `clippy`
alias), `bin`, `member_check`, `fixture_lint`, `disable_toolchain`,
`cargo_env`, `bench`, `bench_params`, `data`.

A host literally named `bench`, `bench_params` or `data` (the script-bench
sections, `brokkr man bench-config`) is written `[host.NAME]`.

Some keys are brokkr's only as non-table values. A table with any of these
names is still read as a hostname section:

- the `strip_rustflags = [...]` array
- the `[[lint_sweep]]` and `[[install]]` arrays of tables
- the `bench_lib = "dir"`, `bench_scripts = "dir"` and `bench_tests = "dir"`
  strings (`brokkr man bench-config`)
- the `bench_scripts_exempt = [...]` list (`brokkr man bench-config`)

## Host sections

A host section is a table named after a machine's hostname, holding that
host's options. `[host.NAME]` is also accepted and lets a hostname use a
reserved section name, such as `[host.test]`. A host table must set at least
one supported option, and brokkr rejects unknown fields inside one.

When host sections exist but none names this machine, defaults apply
silently on ordinary commands, since a config shared across machines is
normal. `brokkr env` warns and lists the configured hostnames, which is where
a misspelled hostname shows up. Legacy hostnames are open-ended, so brokkr
interprets any table with a valid host shape as a host, even if its name
resembles a misspelled section. `[pbfhogg]` is an ordinary hostname section;
a `[pbfhogg.library]` table fails to load as an unknown host field.

Datasets are host-scoped; there is no global `[datasets]` section. Paths
resolve from the host config, then the defaults (`data/`, `data/scratch/`),
from the config alone. Only `brokkr env`'s storage report reads `target`, and
it falls back to cargo's target dir when the key is absent. Builds and
`brokkr clean` always use cargo's.

**Host features.** brokkr adds a host's `features` to these builds:

- measured-request builds, through the shared build context
- the `passthrough` path
- a script bench's build (`brokkr bench`), before the bench's own `features`

Standalone `run`, `install`, `check`, `clippy`, `test` and Criterion `crit`
use their own cargo default or sweep feature selection and don't inherit host
features. Where a path applies host features, CLI `--features` add to them,
with duplicates removed. This list is the complete policy for which cargo
builds inherit host features.

**port.** A host's `port` is for the project's own tools, which read it from
the composed config (nidhogg's do). No built-in command reads it; a script
bench sees it in the resolved view's `host` table (`brokkr man
bench-config`).

**elivagar's host keys.** `output` (elivagar's durable tilegen output store)
and `[HOST.tilegen.NAME]` blocks are elivagar's own. brokkr reads neither,
but accepts both so an elivagar host loads. A tilegen block is an opaque
table with any contents, and counts as host content. A script bench sees both
in the resolved view's `host` table, with `output` resolved and defaulted to
`data/tilegen` (`brokkr man bench-config`).

**verify_reference_budget.** This host key is retired: a host that sets it
loads, and nothing reads the value. It bounded pbfhogg's verify reference
cache, which no command writes. `brokkr clean` removes a leftover cache.

## worktree_keep

The host key `worktree_keep` sets how many persistent `--commit` worktrees
this project keeps before brokkr evicts the least-recently-used:

```toml
[plantasjen]
worktree_keep = 6
```

The default is 6. The key is per host because disk is a host property. `0`
is a config error; omit the key to use the default. Zero would evict every
worktree the moment a second was wanted, which looks the same as the feature
being broken.

The default suits the heaviest dependency graph, not the lightest. A cold
worktree build on that graph is costly, and a silent multi-minute stall is a
worse failure than keeping some extra worktrees. A host's `worktree_keep`
lowers or raises it.

**When eviction runs.** brokkr evicts only when it cuts a new worktree, never
on reuse and never on a plain run. The cost then comes with a build you are
already paying for, rather than as an unexplained pause, and a measurement
never becomes destructive merely by happening. A project that stops growing
never shrinks on its own; run `brokkr clean --worktrees` for that.

Eviction makes room for the worktree being cut, so the steady state is
`worktree_keep` worktrees, not one more. The worktree being cut is never
itself a victim, including when it replaces a stale directory of the same
name. The whole sequence (eviction, replacement, creation, bookkeeping) runs
under the global lock, which stays held through the build and measurement.
A concurrent brokkr can therefore neither evict a worktree mid-build nor lose
a bookkeeping update.

**A growth damper, not a bound.** Retention is per project, so the disk-wide
total grows with the number of projects that use worktrees. Only a global
byte budget could promise never to fill the disk; a count is a proxy for the
real constraint, bytes. The bookkeeping records last-use time but not
worktree size.

Eviction obeys two rules:

- **A worktree with uncommitted work is never evicted**, whether or not git
  would have managed to remove it. A dirty worktree is the one place where
  removal destroys something unrecoverable. If brokkr can't consult git at
  all, it assumes the worktree is dirty and keeps it. The same rule guards
  replacement: brokkr replaces a directory at the name `--commit` wants, not
  checked out at the requested commit, only if it is clean. A dirty or
  unreadable one fails the run with its path rather than being force-removed.
- **Failures skip and continue**, because housekeeping must not fail the
  measurement you asked for. Skips can hold the count above the limit, so
  brokkr reports the overage on every run where it is exceeded, not once
  when a removal fails. You should learn that the damper stopped working
  before the volume fills.

**Bookkeeping.** brokkr records worktrees in `.brokkr/worktrees.toml` at the
project root, written on both create and reuse. The file is safe to delete;
losing it costs a suboptimal eviction order, never data. A worktree with no
record sorts as oldest, since treating it as freshest would make it
permanently un-evictable. A file that exists but doesn't parse is reported as
an error on every `--commit` run and left untouched: eviction and bookkeeping
are skipped and the run itself proceeds. brokkr never reads it as empty and
overwrites it; delete it to recover.

Where one project root governs several checkouts (the one-level-up layout in
`brokkr man config location`), they share that one file. Each checkout's
eviction and pruning touch only records named for its own worktrees,
`CHECKOUT_KEY/SHORT_HASH`. The key is the checkout's name plus a hash of its
path, matched by construction, so checkout `foo` never claims `foo-bar`'s.

**Reuse.** brokkr reuses a worktree only if its checkout is exactly its
commit. Sandboxed agents can write to the worktree container
(`~/.brokkr/worktrees`), so a worktree can be edited between runs, and
reusing it would measure the edit under the commit's name. Such a run fails
with the worktree's path; run `brokkr clean --worktrees` to clear it.

## disable_toolchain

The top-level boolean `disable_toolchain` (default `false`) moves the
project's `rust-toolchain.toml`, or the legacy bare `rust-toolchain`, aside
while brokkr holds the global lock:

```toml
disable_toolchain = true
```

rustup then ignores the pin and falls back to its normal default. brokkr
picks no replacement toolchain; it only disables the file. The file lives in
the code tree (the working directory, the build root), where cargo runs.
Combined with the parent-directory `brokkr.toml` lookup, this is the setup
for driving a foreign checkout whose pinned toolchain you don't have or don't
want.

**Tied to the lock.** brokkr moves the file aside the moment it takes the
global lock and restores it just before releasing the lock. Every command
that runs cargo or rustup against the tree therefore takes the lock, so the
pin is disabled while it works. That includes commands that aren't builds in
the timing sense:

- `deps`, which shells out to `cargo metadata`
- `env`, whose target report does too
- `fmt`, since rustfmt is toolchain-pinned

Resolving a host's data, scratch and output paths is config alone and asks
cargo nothing. A command that takes no lock leaves the file alone.

**Restoring.** brokkr restores the file when it releases the lock: on normal
exit, on error, or on a cooperative interrupt. A hard kill
(`brokkr kill --hard`, SIGKILL) during a non-tracked window can leave it
moved aside as `rust-toolchain.toml.brokkr-disabled`. The next brokkr run in
that directory adopts the leftover and restores it, printing a `[warn]` that
it did so. The routine move aside is identical on every run, so a command
that keeps a run log (`check`, `test`, measured runs, `crit`) records it only
there. Any other command prints it as a quiet-gated `[run]` line.

**Worktrees.** A `--commit` build runs in a separate checkout that may have
its own committed pin. brokkr re-points the disable there for the build, so
the worktree's pin is disabled too.

**A newer host toolchain.** When the host toolchain is newer than the pin,
brokkr builds with lints the pinned toolchain doesn't have. That is often the
point, since it lints ahead of the pin, but the failure mode presents three
layers away from its cause. A lint the host knows and the pin doesn't, such
as a freshly deprecated API, meets a `-Dwarnings` in the project's
`.cargo/config.toml` and becomes a hard compile error. The error is in code
no local change touched, and CI, building on the pin, never sees it. Suppress
the unactionable ones with `[lints] allow` (`brokkr man config lints`), which
reaches the clippy phase and the test build alike, or install the pinned
toolchain.

## strip_rustflags

The top-level array `strip_rustflags` removes listed flags from the tree's
cargo config while brokkr holds the global lock:

```toml
strip_rustflags = ["-Clink-arg=-fuse-ld=lld"]
```

It is a plain key rather than a table, so it claims no name a hostname
section could already use.

The key is for a foreign checkout whose own `.cargo/config.toml` has a flag
that is wrong on this host. The case it was written for:

1. The project's `[target.'cfg(target_os = "linux")'] rustflags` names
   `-fuse-ld=lld`.
2. The user-wide `~/.cargo/config.toml` names another linker.
3. cargo joins every matching target table, so the C driver gets both.
4. lld isn't installed, so every link fails in code nobody touched.

cargo can't subtract a flag. `--config` merges arrays, and `RUSTFLAGS`
replaces every config source at once, dropping the project's `-Dwarnings`
with the flag you meant. Moving the whole file aside would drop those too, so
local gates would run laxer than CI.

**What is edited.** brokkr selects the one file cargo reads in `TREE/.cargo`
by cargo's own probe: the legacy extensionless `config` when it can be
opened, else `config.toml`. A selected path that isn't a regular file is
refused. brokkr replaces it with a copy with the listed flags removed, and
puts the original back before releasing the lock. The lifecycle is
`disable_toolchain`'s exactly: the build root on every locked command, the
checkout itself under `--commit`, restored on exit, error, interrupt or
watchdog kill.

**What an entry matches.** An entry is a token sequence: a string splits on
whitespace, and an array of strings gives tokens verbatim, for an operand
that contains a space. A `-C` or `-Z` token followed by another forms one
unit with it. So `"-Clink-arg=x"` and `"-C link-arg=x"` are the same entry
and match either spelling in the file, while a payload alone
(`"link-arg=x"`) never matches half of a pair.

Matching is physical deletion from each `rustflags` value of the file, not an
interpretation of rustc's command line. The values are `build.rustflags`,
`host.rustflags`, `host.TRIPLE.rustflags` and every
`target.TRIPLE_OR_CFG.rustflags`. Matching follows these rules:

- brokkr finds every match against the original value before removing
  anything, so removal never creates a new match. It refuses two entries
  matching overlapping runs rather than choosing between them.
- A value with no match stays byte-for-byte as it was, and a file with no
  match anywhere isn't replaced at all. A string value stays a string, its
  surviving tokens rejoined with single spaces, so cargo's merge (strings
  replace, arrays append) is unchanged.
- brokkr refuses to edit a value where a match follows a response file
  (`@path`) or `--`. It also refuses where a match directly follows an
  option that may take it as an operand: a bare short option such as `-o`,
  or a long option without `=`. The operand-less
  `-O -g -v -V -h --test --verbose --version --help` are exempt. What
  another config layer contributes before the value is outside the contract.
- brokkr doesn't follow includes; it edits only flags written in the
  selected file. An entry that matched nothing draws a warning on every run,
  naming any `include` or response file it saw as places the flag might live
  instead.
- When a removal empties every matching target table, cargo falls through to
  `build.rustflags`. brokkr warns when the source a host build uses changed.

**Config errors.** brokkr rejects a non-array value, empty entries, entries
that aren't strings or string arrays, and two entries naming the same flag.
It refuses a symlinked config file. The user-wide config layer rejects the
key. Under `include`, the including file's list replaces an included one,
like any plain value.

**What is promised about the file.** brokkr moves the original, never copies
it, and every move is a rename that won't replace an existing file. An editor
saving the config during the window is never lost: brokkr finds a file it
didn't install and stops with a refusal naming every file present, changing
nothing. The installed copy starts with a `# brokkr strip_rustflags v1` line
naming hashes of the original and of itself. That line is how a later run
tells its own untouched install from an edit.

The next brokkr run in that tree puts back a run that died mid-window,
whatever `brokkr.toml` says by then. brokkr puts a `--commit` worktree back
before checking its cleanliness for reuse. Three names next to the file are
brokkr's own while it works: `FILE.brokkr-original`, `FILE.brokkr-installing`
and `FILE.brokkr-displaced`. Changing those by hand, or writing through a
descriptor an editor had open on the file before the window, is outside what
is guarded.

The installed copy and its original count as clean for measurement and
history only when the original still equals `HEAD` and the copy is brokkr's
untouched install over it.

## cargo_env

The `[cargo_env]` table sets environment variables computed per cargo
invocation. Each entry names a variable and a shell command:

```toml
[cargo_env]
BA_SOURCE_IDENTITY = "python3 scripts/source_identity.py"
```

Before a cargo invocation that can run a build script, brokkr runs each
applicable command and sets its trimmed stdout on cargo as the variable. A launch
probe reuses the environment from its build (see How a program is launched
in `brokkr man run`). The table is for a build script
whose real input cargo can't watch through mtimes, such as a content-exact
source identity. The build script declares
`cargo::rerun-if-env-changed=NAME` instead of rerunning on every build. A
rerun build script dirties its own package, so the difference is every
consumer recompiling on every cargo invocation, against recompiling only when
the value changes. A plain `cargo` or rust-analyzer never sets the variable,
so such a build script should keep rerunning when it is absent.

The rules for entries:

- **Per build invocation.** Each evaluation reads the inputs at that
  point in time. An edit between builds changes what the second gets.
  Detecting an edit during a build is the command's job.
- **What counts.** Every cargo brokkr launches itself that may build code,
  except a launch probe. Commands that run no build script include `metadata`, `clean`, `tree`, `locate-project`,
  `pkgid`, `version`, `help`, `fmt`, and cargo's own `--version` and
  `--help`. A cargo run by a script check, a test or a build script is
  outside it. A `brokkr check` lane that runs its prepared binaries without
  invoking cargo evaluates nothing: those binaries have the value they were
  built with.
- **Where it runs.** brokkr runs `sh -c` in the directory cargo is about to
  run in: the worktree for a `--commit` build, so a relative command runs
  that tree's own script against that tree. The entries are the current
  tree's, though, so a commit older than the script has no script to run. A
  plain entry that runs it fails there, and with it every `--commit` build
  of that commit.
- **Its environment.** The command gets the environment cargo gets
  (brokkr's, the sweep's, the hold capability) with every configured name
  removed, so no command reads a stale value of itself or another entry.
  brokkr evaluates entries in name order, independently.
- **Where it applies.** The table form names a path that must exist for the
  entry to apply. See "Conditional entries" below.
- **Failure is fatal.** Any of these fails that cargo spawn: a non-zero exit,
  30s without finishing, more than 4 KiB of output, output that is empty,
  not UTF-8, more than one line or has a control character, or output that
  didn't close. brokkr prints the failure once, with the command, the
  directory and its stderr. Every later cargo of the run is refused, and the
  command exits non-zero even where a path that tolerates a failed build
  carried on.
- **Build inputs, not test environment.** brokkr sets the value on cargo, so
  build scripts and rustc see it, and a test `cargo test` runs inherits it.
  The lanes that execute test binaries directly don't set it. Tests must not
  read it at runtime.
- **Refused at load:** a name that isn't an environment variable name; one
  in a namespace brokkr, cargo, rustc or the loader owns (`BROKKR_*`,
  `CARGO*`, `RUST*`, `LD_*`, `PATH`, `HOME`); a blank command; a name a
  `capture_env` pattern matches, since capture reads brokkr's own
  environment, which never has the per-invocation value; and in the table
  form a missing `command`, a blank or absolute `if_exists`, or any other
  key.
- **Refused per invocation:** a cargo config `[env]` entry for the name, in
  the chain cargo reads or a `--config` it is given. `force = true` there
  would replace what brokkr hands cargo, and a plain entry would be a second
  source for one input. This holds for an entry `if_exists` skipped too,
  since cargo would hand the build that entry's value where brokkr promised
  it absent.

**Conditional entries.** The table form adds `if_exists`, a path that must
exist for the entry to apply:

```toml
[cargo_env.BA_SOURCE_IDENTITY]
command = "python3 scripts/source_identity.py"
if_exists = "scripts/source_identity.py"
```

`if_exists` is relative to the directory the command would run in, checked
per cargo invocation. A directory counts; a dangling symlink doesn't. Where
nothing exists there, such as a commit from before the script arrived,
brokkr doesn't run the command. It removes the name from that cargo's
environment, inherited value included, so the build sees the variable
absent. That is the state a plain `cargo` leaves it in, which its build
scripts must handle anyway. The run log says so
(`cargo_env NAME unset: PATH does not exist in DIR`), or the console when no
run log is open.

A path whose existence can't be determined fails like an evaluation.
Existence decides only whether the entry applies: a script that is there and
fails still fails. A plain string entry applies to every tree. Under
`include`, a layer that names an entry replaces it whole, so a redefined
command never inherits another layer's `if_exists`.

**Logging and scope.** brokkr writes the values to the run log, one line per
variable per cargo invocation
(`cargo_env NAME=value (12ms in DIR, for cargo test)`). The user-wide config
layer rejects the table: what a project's build scripts read is the
project's to say.

## deps

The optional `[deps]` table tunes the `brokkr deps` audit, which runs in any
Rust and git repo:

```toml
[deps]
workspace_dep_ignore = ["lychee", "cargo-*", "backtest"]
```

- **`workspace_dep_ignore`**: workspace-dependency names the
  unused-workspace-dep phase must not flag even when no member inherits
  them, such as dev tools or top-level members whose use cargo metadata
  can't attribute. An entry ending in `*` is a prefix match. Every entry
  must match a declared `[workspace.dependencies]` key when `brokkr deps`
  runs; brokkr refuses stale entries. See `brokkr man deps`.

## bin

The optional `[bin]` table sets shared defaults for `brokkr run` and
`brokkr install`. Both commands discover their targets from cargo metadata:
every workspace bin target, and for `run` every executable example target,
is runnable by name with no config at all. The table curates what discovery
leaves ambiguous:

```toml
[bin]
default = "app"
install = ["app", "runner"]
debug = true
```

The keys:

- **`default`**: the bin or example name bare `brokkr run` launches. Without
  it, a sole runnable runs; several runnables print an index and exit 0.
- **`install`**: package names `brokkr install` installs, in order, via
  `cargo install --locked --path PKG_DIR`. Without it, the workspace's sole
  bin-carrying package installs; several is an error naming this key. A
  package with no bin target is a config error.
- **`debug`**: default both commands to the dev profile (`cargo run`
  without `--release`, `cargo install --debug`). Unset means release for
  both, matching `brokkr test`. CLI `--debug`, `--release` and `--profile`
  (mutually exclusive) override in either direction.
- **`profile`**: the cargo profile both commands default to, by name:
  `"dev"`, `"release"`, or a custom `[profile.NAME]` from the workspace's
  `Cargo.toml` (`cargo run --profile NAME`, `cargo install --profile NAME`).
  See "profile" below.
- **`install_feature_check`**: when `brokkr check` compiles the `install`
  packages' bins under package-mode feature unification. See
  "install_feature_check" below.

**profile.** `profile` wins over `debug` when both are set, with a warning,
since `include` composition can leave both set from different layers.
`"release"` or `"dev"` here overrides an inherited custom one. brokkr refuses
at load names cargo or brokkr owns under the target directory: `debug`,
`test`, `bench`, `doc`, `tmp`, `verify`, `brokkr-worktrees`, and anything
starting `rustflags-`, `unify-` or `fixture-`. cargo checks that the profile
exists when it builds.

**install_feature_check.** The values are `"off"`, `"gate"` (the default)
and `"always"`; `"gate"` means only under a `certifies = "complete"`
profile. Package-mode unification is how `cargo install` resolves the bins,
without the features sibling workspace members contribute. The check runs at
the install profile, or release for a custom one: the check runs under dev
or release only, and the feature resolve it verifies doesn't depend on the
profile. The key requires an explicit `install` list; without one it is a
load error. This is the member-check phase run over the install set: a
`[member_check]` table supersedes it, and setting both is a load error. See
`brokkr man test-lanes member_check`.

## install

The optional `[[install]]` array lists post-install steps `brokkr install`
runs, in order, after every package installed:

```toml
[[install]]
name = "claude-mod"
copy = { from = "mod/orchestrate", to = "~/.claude/skills/orchestrate" }

[[install]]
name = "completions"
run = { command = "orchestrate completions zsh > ~/.zfunc/_orchestrate" }
```

Each entry has these keys:

- **`name`**: required and unique. It names the step in output and errors,
  and an including file reusing it replaces the included entry.
- **One action key**, each holding its own table:
  - `copy`: `from` (relative, inside the project root) and `to` (absolute,
    or starting with `~/`). Neither may hold an empty, `.` or `..` component
    or a trailing slash. Unknown fields are refused.
  - `run`: `command`, run as `sh -c` from the build root.

These are load errors: a blank name, a repeated name, an entry with no action
or with both, a blank `command`, and a `from` or `to` breaking the rules
above. Like `lint_sweep`, the key is claimed only as an array: an `[install]`
table is still a hostname section. The user-wide layer can't declare steps.
What the steps do (exclusive ownership of a copy destination, the ownership
check, atomic replacement and crash recovery) is in `brokkr man run install`.

## member_check

The optional `[member_check]` table compiles every workspace member's
production targets under that member's own package-mode resolve, the
`cargo install` boundary, with one cargo invocation per member:

```toml
[member_check]
when = "gate"
targets = ["lib", "bins"]
profile = "dev"
skip = [{ package = "xtask", reason = "tooling, never installed" }]
```

`when` is `"off"`, `"gate"` (the default) or `"always"`. `targets` defaults
to `["lib", "bins"]` and takes any non-empty subset of `lib` and `bins`.
`profile` is `"dev"` (the default) or `"release"`.

Without the table, a non-empty `[bin] install` implies the narrower install
gate (`brokkr man config bin`). With it, every member is checked and
`[bin] install_feature_check` is a conflict. The semantics, refusals and
what a green run claims are in `brokkr man test-lanes member_check`.

## lints

The optional `[lints]` table holds lint suppressions, applied to every phase
that compiles: `brokkr check`'s clippy phase and test phase, and
`brokkr clippy`.

```toml
[lints]
allow = ["clippy::unused_async_trait_impl"]
allow_exact = ["clippy::unused_async_trait_impl@crates/system/src/kernel.rs"]
```

"Every phase that compiles" is the literal rule. It includes the parts of a
phase that aren't the obvious cargo run: the sweep pre-builds, every lane's
prebuild, and the `prepare` phase's `cargo test --no-run` builds (each
lane's and each shape's universe). Anything that invokes the compiler fails
on a lint the project's `-Dwarnings` promotes to an error, whether or not
brokkr would have read its diagnostics.

The table may also be spelled `[clippy]`. The two are a true alias: if both
are present brokkr unions their lists, so neither silently shadows the
other.

The keys:

- **`allow`**: lint names suppressed on every sweep. See "allow" below.
- **`allow_exact`**: sited suppressions in the `"lint@path"` form. See
  "allow_exact" below.

**allow.** The clippy phase appends `-A LINT` after `--cap-lints=warn`. The
test phase has no rustc passthrough of its own and routes them through
cargo's rustflags (see "In the test phase" below). `allow` is the escape
hatch for driving a foreign checkout under `disable_toolchain`. There the
host's newer compiler surfaces lints the project's pinned-toolchain CI can't
see, and a `-Dwarnings` in the project's `.cargo/config.toml` turns each into
a hard compile error.

Entries are bare lint names, `clippy::`-qualified or plain rustc names.
brokkr rejects a leading `-` or embedded whitespace at parse time. Each phase
announces the allowed lints at the start of every run.

A known limit in the clippy phase: a site with its own lint-level attribute
can defeat the injected `-A`. On clippy 1.98, an `expect` attribute for a
sibling lint let the allowed lint through at error severity despite `-A` and
`--cap-lints=warn`. See `brokkr man check` for the full shape.

**allow_exact.** `allow_exact` is the remedy for that known limit. A
matching diagnostic has the same lint (the `clippy::` qualifier is optional)
and the same build-root-relative file, copied from the failing diagnostic's
location. brokkr drops matching diagnostics at its JSON ingestion, after the
compiler has spoken, so no attribute at the site can defeat them.

An entry covers one lint in one file: every occurrence in that file, but
never workspace-wide. brokkr injects no `-A`, so other sites of the same lint
still fail. brokkr announces the entries up front, grouped by lint with a
file count; a lint with one site keeps its path. An entry that suppressed
nothing draws a stale notice, reported per entry, since that is the part you
have to act on. An entry whose path is absent suppresses nothing, and
`brokkr check --audit` names it without a build.

Parse-time rejection mirrors `allow`, plus the `@` split: exactly one `@`,
non-empty halves, no whitespace, and a relative path. brokkr rejects
absolute paths. See `brokkr man check`.

**The file scoping is clippy-phase only.** A lint that fails a build fails
it during compilation, before any diagnostic reaches brokkr to be filtered,
and `-A` has no path-scoped form. So in the test phase each entry contributes
its lint name build-wide, and the run says so on its notice line. The narrow
guarantee still holds everywhere brokkr can enforce it, and the entry still
fixes the build failure it was written for.

**In the test phase.** brokkr injects only rustc's own lints: `clippy::`,
`rustdoc::` and `cargo::` names never reach a build. Each injected allow
changes every unit's fingerprint, so a lint the project can allow in its
manifest's `[lints]` is better allowed there. brokkr adds the `-A` flags to
whichever rustflags source cargo already uses, never by exporting
`RUSTFLAGS`, which would discard the project's own flags. It prints the layer
it chose:

```text
test: allowing deprecated via --config target."cfg(all())".rustflags ([lints] allow)
```

Don't use `[[check]] rustflags` to carry a lint allow; it isolates the sweep
into its own target dir.
