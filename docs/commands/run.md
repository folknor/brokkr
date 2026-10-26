# brokkr run + brokkr build + brokkr install

Three metadata-driven commands for launching, building and installing a
workspace's own binaries, available in any Rust project (not project-gated) and
working with no `brokkr.toml` at all. `run` builds and runs one target; `build`
builds one and hands a script a path to it; `install` puts the project's
binaries on `PATH` and runs its post-install steps. Together they close the
session loop that otherwise reaches for raw cargo.

None of them requires configuration: targets come from `cargo metadata
--no-deps`, so every bin and example in the workspace is reachable by name the
moment it exists. The optional `[bin]` section only curates what discovery
leaves ambiguous - see `docs/brokkr.toml.md` (`brokkr man config bin`).

## `brokkr run`

```
brokkr run [NAME] [--debug|--release|--profile NAME] [--features LIST]
           [--all-features] [--no-default-features] [--commit REF] [-- ARGS...]
```

Resolves `NAME` to a discovered target and runs

```
cargo run [--release|--profile NAME] -p <pkg> --bin|--example <NAME> [feature flags] [-- ARGS...]
```

from the build root (the working directory). The target is **always named
explicitly** rather than left to `-p`: a package with several bins is ambiguous
to cargo, which bails instead of guessing, so naming the target keeps every
resolution brokkr makes an explicit one.

Resolution order is `NAME` > `[bin] default` > the workspace's sole runnable >
an index. A name matching nothing is an error that says so - and says `([bin]
default)` when the name came from config rather than the command line, since a
stale default is otherwise indistinguishable from a typo. A name matching
**several** targets is also an error, listing them; brokkr does not guess
between two targets that share a name, and the fix belongs in `Cargo.toml`.

A non-zero exit from the program becomes brokkr's own exit code, silently: the
program (or cargo, for a build failure) already said what it had to say, so
brokkr adds no `[error]` line of its own. Under `--commit`, the built program's
non-zero code propagates silently too; a build failure prints its diagnostics
and a closing `cargo build failed` line and exits 1. A target
killed by a signal is reported by cargo and propagates cargo's non-zero code;
under `--commit`, brokkr reports the signal and exits 1. An accepted interrupt
or graceful stop exits 130. A failing `cargo install` ends with a one-line `[error]` naming
the package and cargo's exit code, and exits 1.

Refusals that are about the request rather than the build (no such target, a
name shared by several targets, no bin targets to install, a `[bin] install`
entry with no bin) print their message bare, without a `build:` or `config:`
prefix.

## Features

`--features` (short `-F`), `--all-features` and `--no-default-features` are
**forwarded verbatim** to `cargo run`. Brokkr does not resolve feature names
against the workspace and does not validate them: cargo owns that grammar
(comma- or space-separated, and `--features` is repeatable), and a typo should
fail with cargo's own message rather than be silently dropped. This is what
makes a feature-gated instrument - `--features opcode-counts`, a bench
built with an extra probe - invocable without reaching for raw cargo.

`--all-features` and `--features` conflict (clap-rejected): asking for
everything and asking for a list are two different questions.

There is deliberately **no `[bin]` feature default**. A per-target default
would change what bare `brokkr run <name>` builds without saying so, which is
exactly the way a keep/revert measurement gets corrupted - the flag has to be
visible in the invocation that produced the number.

The feature flags belong to `run` only. `install` ships the project's
binaries as configured, and has no feature surface (see below).

## `--commit`: running an older build

```
brokkr run --commit a5cc1f8 bench -- --preset mtf --repeat 3
brokkr run bench -- --preset mtf --repeat 3
```

is an A/B of two builds of the same target. `--commit REF` builds the target
in a persistent worktree of `REF` (the one `bench` and the measured commands
use - LRU-retained by `worktree_keep`, removed by `brokkr clean --worktrees`),
then runs the binary **from the directory `brokkr run` was invoked in**.

Worktrees live in one container, `~/.brokkr/worktrees/<checkout key>/<short
hash>/`, which holds the checkout plus a symlink for each path dependency that
leaves the repo (`../pbfhogg` resolves to the live sibling). Their isolated
target dirs live under the project's own effective target dir
(`<target>/brokkr-worktrees/...`), so a worktree build lands on the same disk
as any other build of the project. The lock-time free-space gate probes the
live cwd's target and the environment override; cargo-config target overrides
are not resolved by that gate. One pre-existing directory is the only shape a sandboxed agent's writable roots can
grant, which is why worktrees are no longer cut as siblings of the checkout.
The design is in `src/worktree.rs`.

That split is the point. The build happens in the worktree, so the commit's
own manifests, lockfile and `.cargo` config decide what is compiled. The
program runs in the live tree, so relative paths in its arguments, and any
cache or data it reads relative to its working directory, resolve exactly as
they do for the plain `run` it is being compared against. The pair varies the
code and nothing else.

Without it the A/B gets hand-rolled - a second `CARGO_TARGET_DIR`, a tarball of
the old source - and those leftovers pile up in the tree, unknown to `clean`.

Under `--commit` the build is a `cargo build` followed by running the
executable it produced, not a `cargo run`, since the two halves happen in
different trees. The flags mean the same thing: profile, `--features`,
`--all-features`, `--no-default-features` and the target name all carry
over. Targets are discovered in the worktree, since the commit may predate a
target or carry one the live tree has dropped. The cargo build's output is
captured and printed only on failure.

## Target discovery

`cargo metadata --format-version 1 --no-deps` yields every workspace member;
brokkr keeps each package's `bin` and `example` targets. The identifier is the
**target** name (cargo's `--bin`/`--example` namespace), not the package name -
they coincide often enough to be confusing when they don't.

`install` sees only the `bin` targets. An example is a thing you run, not a
thing you ship.

## The bare form

Bare `brokkr run` with several candidates and no `[bin] default` **prints an
index and exits 0** - the bare-is-an-index shape `sync`, `service` and `man`
share. Not knowing which target you meant is a question, not a failure:

```
[run]     4 runnable targets:
            app (bin)
            migrate (bin, app)
            bench (example, app-core)
            replay (example, app-core)
          run one with `brokkr run <name>`, or set [bin] default
```

The owning package is shown only when it differs from the target name, since
`app (bin, app)` says nothing twice. With exactly one runnable in the
workspace, the bare form runs it: an index of one is a prompt to type a name
that could not have been anything else.

## Forwarding arguments

`brokkr run NAME -- ARGS...` forwards everything after `--` to the program,
after cargo's own `--`.

The bare form with arguments - `brokkr run -- --variations 64` - needs a
pre-pass. As far as clap is concerned that `--` sits in the `NAME` position and
the first real argument would be swallowed as a target name. `bare_run_sentinel`
in `src/runnables.rs` rewrites argv before parsing. It acts only when `run` is
the subcommand itself - the first argument, since brokkr has no top-level
options that take a value - so a `run` that is some other command's value
(`brokkr mogwai run -- ...`, `history --command run`) is never touched. It
walks past `run`'s leading flags, and if the next token is `--`, replaces it
with a sentinel that
`cmd_run` reads back as "no name". The sentinel contains a NUL byte, so no
target name can collide with it. `brokkr run NAME -- ARGS` is left untouched,
as is every other command's argv.

Walking past the leading flags means the pre-pass has to know which of them
take a **separate value** (`--features` / `-F`, `--profile`, `--commit`), so
`brokkr run --features a,b -- --help` skips the `a,b` and still finds the
`--`. The set is read from `run`'s clap definition rather than kept by hand,
so a value-taking flag added to `run` is covered without anyone remembering
to list it.

## `brokkr build`

```
brokkr build [NAME] [--debug|--release|--profile NAME] [--features LIST]
             [--all-features] [--no-default-features] [--commit REF]
```

For a script that runs a workspace binary itself - a differential test, a
comparison against another implementation - and so needs its path rather than
a `brokkr run`. A script that runs `cargo build` on its own is foreign to the
host: the stray reaper kills a cargo with no brokkr owner, and the rustc guard
refuses a compile while a hold exists, so it dies beside any `brokkr check`. A
script that runs `target/<profile>/<name>` without building gets whatever the
last build of anything sharing that path left there, possibly another commit's
or another feature set's.

`build` resolves `NAME` exactly as `run` does (see Target discovery), builds it
under the lock - from the live tree, or a `--commit` worktree - and copies the
executable to

```
.brokkr/artifacts/<xxh128>/<name>
```

under the project root, then prints that absolute path. **On a build it is the
only line on stdout**; everything else the run says, from a lock wait to the
cargo build and any error, goes to stderr, so `bin=$(brokkr build dellingr
--release)` captures the path and nothing else. (`--help` is not a build: clap
prints it to stdout before anything else runs.) A `NAME` that does not pick exactly one target fails, after
the same index `run` prints: a command whose output is a path has nothing to
print when it chose nothing, and an exit 0 would hand the script an empty value.

The snapshot is keyed by its own content - the copy is hashed, not the source
a later build could replace - and is never written again once in place;
building identical bytes again reuses it. A later build therefore never
changes what a path already handed out runs. The file keeps its name, so it is
the same executable in another directory, and that is the whole promise: it is
not a bundle. A program that finds siblings or resources beside its own path
(`current_exe`), or loads libraries through an `$ORIGIN`-relative path, sees
the snapshot directory instead of `target/<profile>/`. Like running the
uplifted file directly, it gets none of cargo's launch environment.

Snapshots accumulate, one per distinct build. A `brokkr clean` run by hand
removes the store; the cleanup that follows an interrupted command does not,
since a script may still be running one. `clean` needs a `brokkr.toml`, so in
a tree with none the store is yours to delete: `.brokkr/artifacts/` holds
nothing else.

## `brokkr install`

```
brokkr install [--debug|--release|--profile NAME] [--unlocked]
```

Runs `cargo install [--debug|--profile NAME] --locked --path <pkg dir>` once per selected
package, in the order `[bin] install` lists them, announcing each invocation,
then the project's `[[install]]` steps (see Post-install steps below).
`cargo install --path` installs every bin the package carries, so the
selection is by *package*, not by target.

### `--locked` is the default

`cargo install --path` does not read the workspace `Cargo.lock`. Left to
itself it re-resolves every dependency to the newest semver-compatible
version available at install time, which you can see in its output as
`Locking N packages to highest Rust <version> compatible versions`.

That makes `install` the one command in the workflow that does not agree with
the others about what the dependency graph is. `check`, `test`, `run` and
`check`'s own member-check phase all resolve against the committed
lockfile, so a green gate says nothing about a re-resolving install: an
upstream crate publishing a breaking minor can break the deploy of a tree
whose last commit was days ago, and the failure lands in a dependency, on a
version no test ever compiled. A lockfile exists so that what ships is what
was tested; re-resolving at install time spends exactly that.

So brokkr passes `--locked`, and what `check` was green on is what gets
installed. `--unlocked` restores cargo's default for the rare case where you
*want* the newer graph - and the honest way to adopt it is `cargo update`,
a `check`, and a commit, so the lockfile carries the decision.

A tree with **no** `Cargo.lock` is re-resolved either way: cargo refuses
`--locked` without one, so brokkr omits the flag and prints a line saying the
install is not the resolution `check` validated, rather than aborting. "No
lockfile" means none at the `workspace_root` cargo metadata reports - where
cargo itself reads it for every member - so invoking from a member
subdirectory still installs locked.

`install` takes **no feature flags**. It is the session-workflow closer -
it ships the project as configured, and what it ships should not depend on
which instrument you last enabled to take a measurement. The two commands
share the build-purpose resolver and the `[bin]` section; `run`'s feature selection
is per-invocation and reaches nothing else.

A multi-crate workspace names its shippable packages explicitly:

```toml
[bin]
default = "ba"
install = ["broadarrow-ba", "broadarrow-worker", "broadarrow-daemon", "broadarrow-web-bridge"]
debug = true
```

Four `cargo install --debug --locked --path <dir>` runs, in that order. Note the two
namespaces at work: `default` is a **target** name (`ba`, what `cargo run
--bin` takes), while every `install` entry is a **package** name
(`broadarrow-ba`). They are not interchangeable, and a workspace that names
its crates `<project>-<thing>` while their binaries stay short - the common
shape - will never see the two agree. `debug = true` puts both commands on the
dev profile; `brokkr install --release` overrides it for one run.

Without `[bin] install`, the workspace's sole bin-carrying package is
installed. Several bin-carrying packages is an **error** listing them, not an
index - unlike `run`, `install` takes no target argument, so there is nothing
you could type to resolve the ambiguity in the moment. The answer is a
`[bin] install` list, and the error says so. A `[bin] install` entry naming a
package with no bin target is likewise an error rather than a silent skip.

### Post-install steps: `[[install]]`

What a project needs on the machine besides its binaries - a Claude Code
plugin folder under `~/.claude`, a shell completion, a man page - is declared
as steps that `install` runs after every package installed:

```toml
[[install]]
name = "claude-mod"
copy = { from = "mod/orchestrate", to = "~/.claude/skills/orchestrate" }

[[install]]
name = "completions"
run = { command = "orchestrate completions zsh > ~/.zfunc/_orchestrate" }
```

A build script cannot do this job: it cannot tell an install from a `check`
or a release `test`, cargo skips it when its inputs are unchanged (so a
deleted copy is never restored), and it would write outside `OUT_DIR` under
every command.

Each entry has a unique `name` and exactly one action key, holding that
action's own table. Steps run in the composed order of the config, once per
`install`, including one where cargo found nothing to rebuild; an entry an
including file redefines by `name` moves to the including file's position, as
with every other named array. Every step is checked before the first `cargo
install` runs - except that a copy source an earlier `run` step may generate
is allowed to be missing then - and each copy step is checked again just
before it runs. Each step is announced as an `[install]` line when it starts.
The first step that fails stops the rest and fails the install with exit 1,
naming the step; the binaries and any earlier steps stay installed. The
user-wide `brokkr.toml` cannot declare steps.

**`run = { command }`** runs `sh -c command` from the build root, under the
lock `install` holds and inside the same `disable_toolchain` window as the
cargo installs, with inherited output. A non-zero exit, a signal death or a
failure to start fails the step. A command that itself runs a locked brokkr
command refuses rather than waiting on its own parent.

**`copy = { from, to }`** publishes the file or directory `from` (a plain
relative path inside the project root, where `brokkr.toml` lives - note that
`run` starts in the build root instead) at `to`, which is absolute or starts
with `~/`. The copy carries regular files and directories with their
permission bits; a symlink or special file anywhere in `from` refuses the
step. The destination is an artifact brokkr owns **exclusively**: every
install replaces it wholesale, so a file deleted from the source is gone from
the install. A change made inside the destination by hand is never
overwritten - it makes the next install refuse, as below.

That ownership is checked before anything is written. `install` replaces a
destination only when it is absent or holds exactly what this project's step
of that name last installed there, judged by a fingerprint of its paths,
types, permission bits and bytes kept in `~/.brokkr/install/`. Ownership is
by project and step name, so renaming either makes the old artifact foreign.
A destination brokkr did not install, or that another step installed, is
refused on sight, without being read - pick a `to` of the step's own. One
changed since this step installed it is refused too, naming the remedy
(remove it, or undo the change). When the source is unchanged the step
reports the destination up to date and writes nothing. Ownership, timestamps,
xattrs and hard links are not part of the fingerprint. A destination may not
contain the home directory, the project or build root, the source, or
`~/.brokkr`, may not lie inside `~/.brokkr`, and may not nest with another
step's destination or with a destination any project still holds. A
symlinked parent (`~/.claude` pointing into a dotfiles repo) is followed; the
destination itself is never a symlink. Missing parents are created, and only
once every check has passed.

The new copy is staged in a private `.brokkr-install-<random>` directory
beside the destination and swapped in with one atomic rename, so the
destination path always names either the old artifact or the new one. Every
step of that exchange is journaled, and the next `install` - of any project -
finishes or undoes one a crash interrupted. What is not promised: a reader
that walks the artifact while it is swapped can see files of both versions;
the destination must not be written while it is replaced, since a write
through a file already open on the old copy can escape the check that
otherwise preserves an unexpected object rather than deleting it; and nothing
else may create `.brokkr-install-*` names. An install brokkr cannot resolve
is left in place, warned about, and blocks its destination until it is
cleaned up by hand. A record file in `~/.brokkr/install/` that cannot be read
is warned about on every install and refuses every copy step, since a claim
it may hold cannot be ruled out; `run` steps and installs without copy steps
are unaffected.

Removing a step from the config does not uninstall what it installed; the
artifact stays, and a later step targeting the same path is refused until it
is removed.

## Profile

Both commands ask `src/build_purpose.rs` for their Cargo shape. Profiles
resolve the same way: `--debug` / `--release` / `--profile <name>` (mutually
exclusive, rejected by clap together) > `[bin] profile` > `[bin] debug` >
**release**.

Release-by-default matches `brokkr test`. `run` expresses the choice as
`cargo run --release`, `install` as `cargo install --debug` - cargo's two
commands disagree about which profile needs naming, and `[bin] debug = true`
means the same thing to both regardless. A custom profile is `--profile
<name>` to both, building into `<target>/<name>/`; `--profile dev` and
`--profile release` are the built-ins under their own names. Cargo, not
brokkr, checks that a custom profile exists.

## Lock, toolchain, and unconfigured trees

Both commands take the global per-user lock **blocking**, like every other
brokkr command that runs cargo: a concurrent bench run means `[lock] waiting
for the brokkr lock ...` rather than an error. Both ride the `disable_toolchain` window, so a
foreign checkout's pinned `rust-toolchain.toml` is moved aside for the build
exactly as it is under `check`.

`run` holds the lock for the program's whole run, not just its build: the
program is serialized against benchmarks and tracked by `brokkr lock` and
`brokkr kill`. So the program cannot itself be a locked brokkr command -
`brokkr run brokkr -- test NAME` would wait on its own parent forever. It
refuses at once instead, naming the remedy: run the built binary
(`target/debug/brokkr`) or an installed one directly, outside `run`. See `brokkr
man check` ("A descendant of the holder refuses rather than waits").

With no `brokkr.toml`, the working directory is the project root and there is
no `[bin]` section - discovery, the bare index, and the sole-runnable and
sole-package rules all still work. For running, building and installing
binaries, configuration buys disambiguation, never capability; the one thing
only a `brokkr.toml` provides is `install`'s post-install steps.

## Exit statuses

Most commands return `0` on success and `1` on an ordinary failure. The
following codes are shared or command-specific outcomes; a code does not have
one global meaning across every command.

| Code | Meaning |
| --- | --- |
| `0` | Success, including the bare `brokkr run` target index. |
| `1` | Ordinary failure; also a failed `check`/`test` verdict and a refused rustc compile. |
| `2` | Command-specific: clap usage failure, `brokkr-rustc-guard` called without a wrapped command, or a corpus-gate archive refusal. |
| `3` | Command-specific: `brokkr regress` could not complete, or the corpus gate found baseline trouble. |
| `4` | `brokkr pmtiles-corpus` could not complete. |
| `10` | A `check`/`test` run ended with a partial verdict. |
| `124` | A `check`/`test`/`clippy` watchdog ceiling fired. |
| `127` | `brokkr-rustc-guard` could not exec the wrapped compiler. |
| `130` | An accepted interrupt or graceful stop. |

`brokkr run` is a passthrough boundary: the wrapped program's non-zero status
is propagated as-is. Its status can therefore overlap with a brokkr outcome
above; use the command output to distinguish them. See the command-specific
details in `brokkr man check`, `brokkr man corpus`, and `brokkr man elivagar`.
