# Run, build and install

`brokkr run`, `brokkr build` and `brokkr install` launch, build and install a
workspace's own binaries. They work in any Rust project, aren't
project-gated, and need no `brokkr.toml`. `run` builds and runs one target;
`build` builds one and hands a script its path; `install` puts the project's
binaries on `PATH` and runs its post-install steps. Together they cover the
session loop that would otherwise reach for raw cargo. pbfhogg's
verification and dataset tools run through `run`; its benchmarks use
`brokkr bench NAME`.

None of them requires configuration. Targets come from `cargo metadata
--no-deps`, so every bin and executable example in the workspace can be run
by name as soon as it exists. The optional `[bin]` section only resolves
what discovery leaves ambiguous (`brokkr man config bin`).

This page covers `run` and its features, `--commit` and `--with` flags,
target discovery, the bare form, argument forwarding, stopping a run,
`build`, `install`, the profile, the lock and toolchain, unconfigured trees
and exit statuses.

## run

`brokkr run` resolves `NAME` to a discovered target, builds it and runs it.
It takes these options:

```text
brokkr run [NAME] [--debug|--release|--profile NAME] [--features LIST]
           [--all-features] [--no-default-features] [--commit REF]
           [--with TARGET [--with-commit REF] [--with-features LIST]
            [--with-all-features] [--with-no-default-features]]
           [-- ARGS...]
```

From the build root, the working directory, brokkr runs:

```text
cargo run --quiet [--release|--profile NAME] -p PKG --bin|--example NAME
  [feature flags] [-- ARGS...]
```

brokkr always names the target rather than leaving it to `-p`. Cargo bails
on a package with several bins instead of guessing, so naming the target
makes every resolution brokkr makes an explicit one.

brokkr captures the build's output. Before the run, it runs a `cargo build
--color=never --message-format=json` of the same selection with its output
held back, announced by one `[build]` line and closed by a `done in` line,
both suppressed in quiet mode. A failing build prints its diagnostics and a
closing `cargo build failed` line and exits 1, as under `--commit`. A
successful one prints only the warnings the quiet run wouldn't: build-script
`cargo::warning` lines and cargo's own, such as an unused `[patch]`. brokkr
leaves compiler warnings to the run, which replays a local crate's warnings
from cargo's cache, so each prints once.

The run then finds every unit fresh and compiles nothing, as a rule. Cargo
revalidates its inputs, so a file saved during the build makes it rebuild,
and that rebuild prints cargo's quiet output, uncaptured. Launching through
`cargo run`, rather than executing the artifact, keeps what cargo sets up
for the program:

- a configured `target.TRIPLE.runner`
- the dynamic library search path
- cargo config `[env]`
- the `CARGO_*` runtime variables

When the run ends with a code, `run` prints `[run] NAME finished after X
(exit CODE)`, in quiet mode too. The time runs from the launch to the end,
not counting the build or the launch's preparation. On the plain path it
times the quiet `cargo run`, so it includes cargo's freshness check and any
rebuild that check starts, and a non-zero code there can be cargo's own.
Under `--commit` and `--with` it times the program alone. A stop accepted
once the run is launched prints `[run] NAME interrupted`. A stop accepted
during the build prints no `[run]` line, and brokkr reports a signal death
as an error instead.

brokkr's own output stays off stdout. Every line brokkr prints for `run`,
the lock wait, the build, the target index and the runtime, goes to stderr.
`brokkr run NAME > out.json` holds only what the program, or a configured
runner, wrote. This holds under `--commit` and `--with` too.

The target resolves in this order, first match wins:

1. `NAME`
2. `[bin] default`
3. the workspace's sole runnable
4. an index (see Bare form)

A name that matches nothing is an error that says so. It says `([bin]
default)` when the name came from config rather than the command line, since
a stale default otherwise looks like a typo. A name that matches several
targets is an error too, listing them. brokkr doesn't guess between two
targets that share a name; the fix belongs in `Cargo.toml`.

A non-zero exit from the program becomes brokkr's own exit code, with no
`[error]` line, since the program already said what it had to say. On every
path:

- A failed captured build exits 1 after its diagnostics.
- A rebuild that the plain run's `cargo run` itself fails passes cargo's
  code through, as the program's would be.
- brokkr reports a target killed by a signal that didn't come from brokkr's
  stop, and exits 1. On Unix cargo execs the program, or a configured
  runner, in its own place, so brokkr sees the signal death itself.
- An accepted interrupt or graceful stop exits 130.

A failing `cargo install` ends with cargo's diagnostics and a one-line
`[error]` naming the package, and exits 1.

brokkr prints a refusal about the request rather than the build bare,
without a `build:` or `config:` prefix. These are such refusals:

- no such target
- a name shared by several targets
- no bin targets to install
- a `[bin] install` entry with no bin

## Features

brokkr forwards `--features` (short `-F`), `--all-features` and
`--no-default-features` to cargo verbatim. Each `--features` value reaches
cargo as its own `--features` argument, unchanged and in order, on every
path that builds: plain `run`, `--commit`, `--with`'s own flags and `brokkr
build`. brokkr doesn't resolve or validate feature names against the
workspace. Cargo owns that grammar, comma- or space-separated with
`--features` repeatable, and a typo should fail with cargo's own message
rather than be silently dropped. You can invoke a feature-gated instrument,
such as `--features opcode-counts` or a bench built with an extra probe,
without raw cargo.

`--all-features` and `--features` conflict, and clap rejects the pair:
asking for everything and asking for a list are different questions.

There is deliberately no `[bin]` feature default. A per-target default would
change what bare `brokkr run NAME` builds without saying so, which is how a
keep-or-revert measurement gets corrupted: the flag has to be visible in the
invocation that produced the number.

The feature flags belong to `run` and `build`. `install` ships the project's
binaries as configured and has no feature surface (see Locked by default).

## --commit

`--commit REF` runs an older build of a target, so you can A/B two builds of
the same target:

```sh
brokkr run --commit a5cc1f8 bench -- --preset mtf --repeat 3
brokkr run bench -- --preset mtf --repeat 3
```

`--commit REF` builds the target in a persistent worktree of `REF`, the one
`crit` and `brokkr bench` use. `worktree_keep` sets how many brokkr
retains, least recently used first out, and `brokkr clean --worktrees`
removes them. brokkr then runs the binary from the directory you invoked
`brokkr run` in.

Worktrees live in one container, `~/.brokkr/worktrees/CHECKOUT_KEY/HASH/`.
It holds the checkout plus a symlink for each path dependency that leaves
the repo, so `../pbfhogg` resolves to the live sibling. One pre-existing
directory is the only shape a sandboxed agent's writable roots can grant,
which is why worktrees live there rather than beside the checkout.
Their isolated target dirs live under the project's own effective target
dir (`TARGET/brokkr-worktrees/...`), so a worktree build writes to the same
disk as any other build of the project. The lock-time free-space gate
probes the live working directory's target and the environment override;
it doesn't resolve cargo-config target overrides.

The split between the two trees is the point. The build happens in the
worktree, so the commit's own manifests, tracked lockfile and `.cargo`
config decide what is compiled. The program runs in the live tree, so
relative paths in its arguments, and any cache or data it reads relative to
its working directory, resolve against the same directory as the plain
`run` it is compared against. That doesn't guarantee identical inputs. A
default can change between the two builds, and a path the build embeds, one
rooted on `CARGO_MANIFEST_DIR`, points into the worktree. A program that
asks git about its working directory sees the live checkout, not `REF`.

brokkr seeds an untracked lockfile from the live tree's. When the commit
doesn't track its root `Cargo.lock`, as in a project that gitignores it,
every admission of the worktree, cut or reused, copies the live tree's
`Cargo.lock` over the checkout's, read at that moment. Without the seed the
worktree would build against whatever cargo resolved the day the slot was
cut, and a `--commit` arm would differ from the live one in dependency
versions as well as code. Cargo may still amend each copy to fit the
commit's manifests: the seed equalizes the starting bytes, not the
resolution. brokkr reports a seeded build that fails as it is, never retried
unseeded.

brokkr never touches a lockfile the commit tracks. With no live lock, the
worktree keeps the one it holds, or cargo resolves afresh. brokkr manages
only the conventional layout, where each side's root `Cargo.toml` is its own
workspace root: a `[workspace]` table, or a `[package]` naming no other
workspace, with no directory above it declaring one. It doesn't detect a
`resolver.lockfile-path` override. The run log says which case applied, and
a script bench stores it with the file's digest after the build (`brokkr
man results`).

Without `--commit` you would hand-roll the A/B with a second
`CARGO_TARGET_DIR` or a tarball of the old source, and those leftovers pile
up in the tree, unknown to `clean`.

Under `--commit` the build is a `cargo build` followed by running the
executable it produced, not a `cargo run`, since the two halves happen in
different trees. The flags mean the same: the profile, `--features`,
`--all-features`, `--no-default-features` and the target name all apply.
brokkr discovers targets in the worktree, since the commit may predate a
target or have one the live tree dropped. It captures the cargo build's
output and prints it only on failure.

The executable is the one cargo reported for that package and target, bin
or example, by name, and there must be exactly one. A build that compiles
the target more than once fails rather than guess, as when cargo config
lists several `build.target`s or an artifact dependency names it. This
holds wherever brokkr runs what `cargo build` produced: `--commit`, `--with`,
`build` and script benches.

## --with

`--with TARGET` hands the program another target's path, for a program that
drives another workspace binary as a subprocess, such as a verifier against
the build under test:

```sh
brokkr run --with pbfhogg pbfhogg-verify -- all denmark
brokkr run --with pbfhogg --with-commit a5cc1f8 \
    --with-features zlib-ng pbfhogg-verify -- all denmark
```

Such a program can't build the other binary itself, since a locked brokkr
beneath `run` refuses. `--with TARGET` builds `TARGET` exactly as `brokkr
build TARGET` would, snapshot included. brokkr then builds and runs the
program with the snapshot's absolute path in `BROKKR_WITH_EXE`. One
invocation holds the lock across both builds and the run.

The program's own flags keep applying to the program: `--commit`,
`--features`, `--all-features` and `--no-default-features` choose what the
program is built from. The `--with` target has its own flags, each refused
without `--with`:

- `--with-commit REF`
- `--with-features LIST`, repeatable
- `--with-all-features`
- `--with-no-default-features`

The profile is shared: one `--debug`, `--release` or `--profile`, or the
`[bin]` default, builds both. brokkr builds the `--with` target first and
finishes its worktree stage before the program's build begins. Its path is
the snapshot, so nothing the program's build does can change what it runs.

With `--with`, brokkr always builds and executes the program directly, the
shape `--commit` uses, never through `cargo run`. `BROKKR_WITH_EXE` then
reaches the program alone and is never a build input of the program. An
`env!` or a build script reading it would otherwise see it on a live run
and not on a `--commit` one. The program therefore gets none of cargo's
launch environment, as under `--commit`, and runs from the directory you
invoked `brokkr run` in. brokkr sets the variable only for execution; one
already exported in your shell is inherited by builds like any other
variable.

## Target discovery

`cargo metadata --format-version 1 --no-deps` yields every workspace member,
and brokkr keeps each package's `bin` targets and its executable `example`
targets. An example whose `crate-type` names anything but `bin` is a
library example. It has no executable, so it is never listed, never the
sole runnable and never a rival for a shared name. brokkr refuses naming
it, or making it `[bin] default`, before anything builds.

The identifier is the target name, cargo's `--bin` and `--example`
namespace, not the package name. The two coincide often enough to confuse
when they don't.

`install` sees only the `bin` targets: an example is a thing you run, not a
thing you ship.

## Bare form

Bare `brokkr run` with several candidates and no `[bin] default` prints an
index and exits 0, the bare-is-an-index shape `sync`, `service` and `man`
share. Not knowing which target you meant is a question, not a failure:

```text
[run]     4 runnable targets:
            app (bin)
            migrate (bin, app)
            bench (example, app-core)
            replay (example, app-core)
          run one with `brokkr run <name>`, or set [bin] default
```

The index shows the owning package only when it differs from the target
name, since `app (bin, app)` says nothing twice. With exactly one runnable
in the workspace, the bare form runs it: an index of one would prompt you
to type the only possible name.

## Forwarding arguments

`brokkr run NAME -- ARGS...` forwards everything after `--` to the program,
after cargo's own `--`.

The bare form with arguments, `brokkr run -- --variations 64`, forwards
everything after the `--` too. A `--` reached before any `NAME` leaves
`NAME` empty, so `run` falls back to `[bin] default` or the sole runnable.
Only the first `--` is brokkr's and any later one is the program's, so
`brokkr run -- -- --help` forwards `-- --help`. Flags before the `--`, with
their values, parse as usual (`brokkr run --features a,b -- --help`).

## Stopping a run

`brokkr kill` asks the lock holder to stop with SIGTERM, and cancels that
project's queued commands first. It acts in the holder's own project, or in
another with `--project NAME` or `--all` (`brokkr man hygiene`).

During `run`, brokkr forwards SIGTERM to its direct child and waits up to
1.5 seconds for that child to exit before sending it SIGKILL:

- With `--commit` or `--with`, the child is the program itself, so a
  program that handles SIGTERM has that window to shut down and print its
  summary.
- Plain `run` launches `cargo run`, which on Unix execs the program, or a
  configured runner, in its own place, so the signal reaches that process.
  A runner must forward it for the program beneath to see it.

A stop observed by the last check before the spawn launches nothing; one
that arrives after it is delivered to the launched child. Run children
inherit brokkr's process group, and brokkr doesn't signal their descendants
on this path, so a shutdown summary isn't guaranteed.

For `brokkr bench`, preflight children included, the same stop sends
SIGTERM to the child's process group and waits up to 1.5 seconds for the
group leader to exit. As soon as the leader exits, or the grace expires,
brokkr sends SIGKILL to the group to stop the remaining members. This
interruption grace is separate from a bench's `stop_at` marker grace and
wall deadline. `brokkr kill --hard` skips the grace and cleanup and sends
SIGKILL to brokkr and its descendants.

brokkr stops a server started by `brokkr serve` differently. Once its start
has returned it holds no lock, so `kill` doesn't stop it, and lists the
project's live serves as left running. `brokkr serve --stop` sends its
process group SIGTERM, waits the serve's grace (5 seconds unless `--grace`
set another at the start), then sends the group SIGKILL. See `brokkr man
serve`.

## build

`brokkr build` builds one target and prints a path to a snapshot of it. It
takes these options:

```text
brokkr build [NAME] [--debug|--release|--profile NAME] [--features LIST]
             [--all-features] [--no-default-features] [--commit REF]
```

Use it from a script that runs a workspace binary itself, such as a
differential test or a comparison against another implementation, and so
needs its path rather than a `brokkr run`. A script that runs `cargo build`
on its own is foreign to the host. The stray reaper kills a cargo with no
brokkr owner, and the rustc guard refuses a compile while a hold exists, so
the script dies beside any `brokkr check`. A script that runs
`target/PROFILE/NAME` without building gets whatever the last build of
anything sharing that path left there, possibly another commit's or another
feature set's.

`build` resolves `NAME` exactly as `run` does (see Target discovery). It
builds the target under the lock, from the live tree or a `--commit`
worktree, and copies the executable to this path under the project root:

```text
.brokkr/artifacts/XXH128/NAME
```

Then it prints that absolute path. On a build, the path is the only line on
stdout. Everything else, from a lock wait to the cargo build and any error,
goes to stderr, so `bin=$(brokkr build dellingr --release)` captures the
path and nothing else. `--help` isn't a build: clap prints it to stdout
before anything else runs. A `NAME` that doesn't pick exactly one target
fails after printing the same index `run` prints. A command whose output is
a path has nothing to print when it chose nothing, and an exit 0 would hand
the script an empty value.

The snapshot is keyed by its own content: brokkr hashes the copy, not the
source a later build could replace, and never writes it again once in place.
Building identical bytes again reuses it, so a later build never changes
what an earlier path runs. The file keeps its name, so it is the same
executable in another directory, and that is the whole promise: it isn't a
bundle. A program that finds siblings or resources beside its own path
(`current_exe`), or loads libraries through an `$ORIGIN`-relative path, sees
the snapshot directory instead of `target/PROFILE/`. Like running the
uplifted file directly, it gets none of cargo's launch environment.

Snapshots accumulate, one per distinct build. `brokkr clean` removes the
store, and nothing else does: an interrupted command runs no clean. `clean`
needs a `brokkr.toml`, so in a tree with none the store is yours to delete,
since `.brokkr/artifacts/` holds nothing else.

## install

`brokkr install` installs the project's binaries with `cargo install`, stamps
them and runs the project's post-install steps. It takes these options:

```text
brokkr install [--debug|--release|--profile NAME] [--unlocked]
brokkr install --names
```

For each selected package, in the order `[bin] install` lists them, brokkr
announces and runs:

```text
cargo install [--debug|--profile NAME] --locked --path PKG_DIR
  --root CARGO_HOME
```

It then stamps the bins each one installed (see Build stamp) and runs the
project's `[[install]]` steps (see Post-install steps). `cargo install
--path` installs every bin the package has, so the selection is by package,
not by target.

Each `cargo install` runs with `--color=never --message-format=json` and
its output captured, as `run`'s build is. A `[build]` line announces it and
a `done in` line closes it, both suppressed in quiet mode. A failure prints
cargo's diagnostics and a closing `cargo install failed for PACKAGE` line,
and exits 1. A success prints the compiler warnings, build-script
`cargo::warning` lines and cargo's own warnings, a `PATH` hint among them,
and nothing of cargo's progress.

### Build stamp

Every bin `install` installs has the commit it was built from in it, so you
can detect a stale install from the file itself, without running it:

```sh
objcopy --dump-section .brokkr.build_info=OUT BIN /dev/null
```

`OUT` then holds one line, `sha FULL_COMMIT_ID clean Y_OR_N`, and a newline;
`readelf -p .brokkr.build_info BIN` shows it too. A file with no such
section has no known commit: it was built by plain cargo, installed before
brokkr stamped, or one brokkr failed to stamp.

A script reading the stamp should know two `objcopy` traps:

- `objcopy` reports a missing section on stderr with exit status 0, so test
  whether `OUT` was written rather than the exit status.
- Dumping to `/dev/stdout` truncates a stdout that is a file.

The last argument is where `objcopy` writes a copy of `BIN`. `/dev/null`
discards it, and leaving it out rewrites `BIN` in place.

Nothing in the build changes. After cargo installs the file, brokkr adds the
section with `objcopy --add-section`, writing a copy beside it. It checks
that the copy reads back exactly the stamp, then renames it over the
original with its permissions. The section isn't loaded at run time. A
project needs no code or config for it, but `objcopy` (binutils) must be on
`PATH`.

brokkr samples the commit and the clean bit once per package, under the
lock and before the first cargo runs. It samples at the git top level of the
package's own directory, which need not be the directory you ran install
from. Clean follows the record rule `check` uses for invocation records,
with two differences: submodule changes always count, whatever git config
says, and a tracked file under `.brokkr` counts too. brokkr's own verified
tree edits during the locked window, such as a moved toolchain pin or a
filtered cargo config, don't count, since they are the project's configured
install policy at that commit. `install` also refuses to run when
`brokkr.toml` or a file it includes changed while it waited for the lock, so
a moved checkout can't pair one commit's stamp with another's plan.

To know where the bins land, brokkr passes `--root` itself: the cargo home
(`CARGO_HOME`, else `.cargo` under the home directory, as cargo resolves
it), where cargo would install anyway. When `CARGO_INSTALL_ROOT` is set, or
a cargo config `install` reads (from the package directory or the directory
install runs in) sets `install.root`, brokkr leaves the root to cargo and
warns that the bins have no stamp. Outside a git checkout the bins have none
either, which brokkr says in one line. A stamp brokkr should write and
can't fails the install, naming the bin: it is installed but unstamped.

`brokkr --version` prints brokkr's own stamp and protocol. It then asks the
user-wide `results_server` once, for a few seconds at most, and prints its
stamp and protocol, noting a protocol or commit that differs from this
brokkr's. An unconfigured or unreachable server is reported as such, and
the command still exits 0. brokkr reads stamps itself, so the check after
`objcopy` adds a section doesn't depend on `objcopy`'s own reading.

The stamp doesn't see:

- edits made while install runs
- changes git is told to hide (assume-unchanged, skip-worktree)
- ignored build inputs
- path dependencies outside the checkout
- whether cargo was right that an artifact was fresh

`[[install]]` copies aren't stamped.

### --names

`brokkr install --names` prints the bin names `install` would install, one
per line on stdout, for the same packages, and stops. It builds nothing and
takes no lock. brokkr lists a bin when the package's default features
enable each of its `required-features`, following only the package's own
feature table, since `install` passes no feature flags. Two cases make the
command fail naming the bin rather than guess:

- a bin that requires a dependency's feature (`dep/feat`)
- a bin whose answer depends on whether a `dep/feat` entry also enables an
  optional dependency's implicit feature

`install` itself still installs such a package and warns that its bins have
no stamp.

### Locked by default

brokkr passes `--locked` to `cargo install`, so what `check` was green on is
what gets installed. `cargo install --path` doesn't read the workspace
`Cargo.lock`. Left to itself, it re-resolves every dependency to the newest
semver-compatible version available at install time, which shows in its
output as `Locking N packages to highest Rust VERSION compatible versions`.

That would make `install` the one command in the workflow that disagrees
with the others about the dependency graph. `check`, `test`, `run` and
`check`'s own member-check phase all resolve against the committed lockfile,
so a green gate says nothing about a re-resolving install. An upstream crate
that publishes a breaking minor can break the deploy of a tree whose last
commit was days ago, in a dependency, on a version no test compiled. A
lockfile exists so that what ships is what was tested, and re-resolving at
install time throws that away.

`--unlocked` restores cargo's default for when you want the newer graph.
The better way to adopt it is `cargo update`, a `check` and a commit, so the
lockfile records the decision.

A tree with no `Cargo.lock` is re-resolved either way. Cargo refuses
`--locked` without one, so brokkr omits the flag and prints a line saying
the install isn't the resolution `check` validated, rather than aborting.
"No lockfile" means none at the `workspace_root` cargo metadata reports,
where cargo reads it for every member, so invoking from a member
subdirectory still installs locked.

`install` takes no feature flags. It closes the session workflow and ships
the project as configured, and what it ships shouldn't depend on which
instrument you last enabled to take a measurement. `install` and `run`
share the build-purpose resolver and the `[bin]` section; `run`'s feature
selection is per invocation and reaches nothing else.

A multi-crate workspace names its shippable packages explicitly:

```toml
[bin]
default = "ba"
install = ["broadarrow-ba", "broadarrow-worker",
           "broadarrow-daemon", "broadarrow-web-bridge"]
debug = true
```

This config gives four `cargo install --debug --locked --path DIR` runs, in
that order. Two namespaces are at work. `default` is a target name (`ba`,
what `cargo run --bin` takes), while every `install` entry is a package name
(`broadarrow-ba`). They aren't interchangeable, and a workspace that names
its crates `PROJECT-THING` while their binaries stay short, the common
shape, never sees the two agree. `debug = true` puts both commands on the
dev profile; `brokkr install --release` overrides it for one run.

Without `[bin] install`, brokkr installs the workspace's sole bin-carrying
package. Several bin-carrying packages are an error listing them, not an
index. Unlike `run`, `install` takes no target argument, so you can't type
anything to resolve the ambiguity in the moment. The answer is a `[bin]
install` list, and the error says so. A `[bin] install` entry naming a
package with no bin target is an error too, not a silent skip.

### Post-install steps

`[[install]]` declares what a project needs on the machine besides its
binaries, such as a Claude Code plugin folder under `~/.claude`, a shell
completion or a man page. `install` runs the steps after every package has
installed:

```toml
[[install]]
name = "claude-mod"
copy = { from = "mod/orchestrate", to = "~/.claude/skills/orchestrate" }

[[install]]
name = "completions"
run = { command = "orchestrate completions zsh > ~/.zfunc/_orchestrate" }
```

A build script can't do this job. It can't tell an install from a `check`
or a release `test`, cargo skips it when its inputs are unchanged, so a
deleted copy is never restored, and it would write outside `OUT_DIR` under
every command.

Each entry has a unique `name` and exactly one action key, holding that
action's own table. brokkr runs the steps in the composed order of the
config, once per `install`, including one where cargo found nothing to
rebuild. An entry that an including file redefines by `name` moves to the
including file's position, as with every other named array.

brokkr checks every step before the first `cargo install` runs, except that
a copy source an earlier `run` step may generate may be missing then. It
checks each copy step again just before it runs, and announces each step
with an `[install]` line when it starts. The first step that fails stops the
rest and fails the install with exit 1, naming the step; the binaries and
any earlier steps stay installed. The user-wide `brokkr.toml` can't declare
steps.

**`run = { command }`** runs `sh -c command` from the build root, under the
lock `install` holds and inside the same `disable_toolchain` window as the
cargo installs, with inherited output. A non-zero exit, a signal death or a
failure to start fails the step. A command that itself runs a locked brokkr
command refuses rather than wait on its own parent.

**`copy = { from, to }`** publishes the file or directory `from` at `to`.
`from` is a plain relative path inside the project root, where `brokkr.toml`
lives, while `run` starts in the build root instead. `to` is absolute or
starts with `~/`. The copy has regular files and directories with their
permission bits; a symlink or special file anywhere in `from` refuses the
step.

brokkr owns the destination exclusively: every install replaces it
wholesale, so a file deleted from the source is gone from the install.
brokkr never overwrites a change you made inside the destination by hand;
the change makes the next install refuse, as below.

brokkr checks that ownership before it writes anything. `install` replaces
a destination only when it is absent, or holds exactly what this project's
step of that name last installed there. It judges that by a fingerprint of
paths, types, permission bits and bytes, kept in `~/.brokkr/install/`.
Ownership is by project and step name, so renaming either makes the old
artifact foreign.

- A destination brokkr didn't install, or that another step installed, is
  refused on sight, without being read: pick a `to` of the step's own.
- A destination changed since this step installed it is refused too, naming
  the remedy: remove it, or undo the change.
- When the source is unchanged, the step reports the destination up to date
  and writes nothing.

Ownership, timestamps, xattrs and hard links aren't part of the
fingerprint. A destination may not:

- contain the home directory, the project or build root, the source, or
  `~/.brokkr`
- lie inside `~/.brokkr`
- nest with another step's destination, or with a destination any project
  still holds

brokkr follows a symlinked parent, such as `~/.claude` pointing into a
dotfiles repo; the destination itself is never a symlink. brokkr creates
missing parents only once every check has passed.

brokkr stages the new copy in a private `.brokkr-install-RANDOM` directory
beside the destination and swaps it in with one atomic rename, so the
destination path always names either the old artifact or the new one.
brokkr journals every step of that exchange, and the next `install`, of any
project, finishes or undoes one a crash interrupted.

For a destination inside the home directory, brokkr syncs its parents and
the registry under `~/.brokkr` from the home down whenever an install
writes them. It takes the home itself, and any symlink on the way, as
durable, so once a journal names them a crash never loses the
destination's parent or the registry. Outside the home, brokkr syncs only
the parents an install creates, as it creates them. A parent created by an
install that died before its sync stays unsynced for every retry. A later
crash that loses it leaves that destination's journaled install
unresolvable, blocking it until you clean it up by hand.

What isn't promised:

- A reader that walks the artifact while it is swapped can see files of
  both versions.
- The destination must not be written while brokkr replaces it. A write
  through a file already open on the old copy can escape the check that
  otherwise preserves an unexpected object rather than deleting it.
- Nothing else may create `.brokkr-install-*` names.

brokkr leaves an install it can't resolve in place, warns about it, and the
install blocks its destination until you clean it up by hand. A record file
in `~/.brokkr/install/` that can't be read is warned about on every install
and refuses every copy step, since brokkr can't rule out a claim it may
hold. `run` steps and installs without copy steps are unaffected.

Removing a step from the config doesn't uninstall what it installed. The
artifact stays, and brokkr refuses a later step that targets the same path
until you remove it. The step's ownership record stays too, even once the
artifact is gone: brokkr can't tell an artifact deleted for good from one
moved aside, and an unchanged one that comes back is still that step's to
replace. A gone destination holds no claim, so the record blocks nothing.

## Profile

`run`, `build` and `install` resolve the profile through the same
build-purpose resolver, in this order, first match wins:

1. `--debug`, `--release` or `--profile NAME`, mutually exclusive, which
   clap rejects together
2. `[bin] profile`
3. `[bin] debug`
4. release

Release by default matches `brokkr test`. `run` expresses the choice as
`cargo run --release` and `install` as `cargo install --debug`, since cargo's
two commands disagree about which profile needs naming; `[bin] debug =
true` means the same to both. A custom profile is `--profile NAME` to both,
building into `TARGET/NAME/`, and `--profile dev` and `--profile release`
are the built-ins under their own names. Cargo, not brokkr, checks that a
custom profile exists.

## Lock and toolchain

The commands take the global per-user lock blocking, like every other
brokkr command that runs cargo, so a concurrent bench run means `[lock]
waiting for the brokkr lock ...` rather than an error. They run inside the
`disable_toolchain` window, so brokkr moves a foreign checkout's pinned
`rust-toolchain.toml` aside for the build exactly as under `check`.

`run` holds the lock for the program's whole run, not only its build: brokkr
serializes the program against benchmarks, and `brokkr lock` and `brokkr
kill` track it. The program therefore can't itself be a locked brokkr
command, since `brokkr run brokkr -- test NAME` would wait on its own parent
forever. It refuses at once instead, naming the remedy: run the built binary
(`target/debug/brokkr`) or an installed one directly, outside `run`. See "A
descendant of the holder refuses rather than waits" in `brokkr man
hygiene`. A program that serves rather than finishes, such as a dev server
its clients must reach while it runs, belongs under `brokkr serve`. That
releases the lock once the server is up and refuses only measurements while
it lives (`brokkr man serve`).

## Unconfigured trees

With no `brokkr.toml`, the working directory is the project root and there
is no `[bin]` section. Discovery, the bare index, and the sole-runnable and
sole-package rules all still work. For running, building and installing
binaries, configuration buys disambiguation, never capability. The one
thing only a `brokkr.toml` provides is `install`'s post-install steps.

## Exit statuses

`run`, `build` and `install` exit with these codes:

| Code | Meaning |
| --- | --- |
| `0` | Success, including the bare `brokkr run` target index. |
| `1` | A failed build or install, a refusal, or a target killed by a signal brokkr didn't send. |
| `2` | A clap usage error. |
| `130` | An accepted interrupt or graceful stop. |

`brokkr run` passes the program's non-zero exit through as it is, so its
code can coincide with one of brokkr's own; the command output tells them
apart. The
codes of `check`, `test` and `clippy` are on their own pages (`brokkr man
check exit-codes`).
