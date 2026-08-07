# brokkr run + brokkr install

Two metadata-driven commands for launching and installing a workspace's own
binaries, available in any Rust project (not project-gated) and working with no
`brokkr.toml` at all. `run` builds and runs one target; `install` puts the
project's binaries on `PATH`. Together they close the session loop that
otherwise reaches for raw cargo.

Neither command requires configuration: targets come from `cargo metadata
--no-deps`, so every bin and example in the workspace is reachable by name the
moment it exists. The optional `[bin]` section only curates what discovery
leaves ambiguous - see `docs/brokkr.toml.md` (`brokkr man config bin`).

## `brokkr run`

```
brokkr run [NAME] [--debug|--release] [--features LIST] [--all-features]
           [--no-default-features] [-- ARGS...]
```

Resolves `NAME` to a discovered target and runs

```
cargo run [--release] -p <pkg> --bin|--example <NAME> [feature flags] [-- ARGS...]
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

A non-zero exit from the program propagates as brokkr's own failure, carrying
cargo's exit code; a target killed by a signal is reported as such.

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
in `src/runnables.rs` rewrites argv before parsing: it walks past `run`'s
leading flags, and if the next token is `--`, replaces it with a sentinel that
`cmd_run` reads back as "no name". The sentinel contains a NUL byte, so no
target name can collide with it. `brokkr run NAME -- ARGS` is left untouched,
as is every other command's argv.

Walking past the leading flags means the pre-pass has to know which of them
take a **separate value**: `RUN_VALUE_FLAGS` lists `--features` / `-F`, so
`brokkr run --features a,b -- --help` skips the `a,b` and still finds the `--`.
A value-taking flag added to `run` and not to that list would silently
reintroduce the swallowed-first-argument bug.

## `brokkr install`

```
brokkr install [--debug|--release]
```

Runs `cargo install [--debug] --path <pkg dir>` once per selected package, in
the order `[bin] install` lists them, announcing each invocation. `cargo
install --path` installs every bin the package carries, so the selection is by
*package*, not by target.

`install` takes **no feature flags**. It is the session-workflow closer -
it ships the project as configured, and what it ships should not depend on
which instrument you last enabled to take a measurement. The two commands
share only `resolve_debug` and the `[bin]` section; `run`'s feature selection
is per-invocation and reaches nothing else.

A multi-crate workspace names its shippable packages explicitly:

```toml
[bin]
default = "ba"
install = ["broadarrow-ba", "broadarrow-worker", "broadarrow-daemon", "broadarrow-web-bridge"]
debug = true
```

Four `cargo install --debug --path <dir>` runs, in that order. Note the two
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

## Profile

Both commands resolve the same way: `--debug` / `--release` (mutually
exclusive, rejected by clap together) > `[bin] debug` > **release**.

Release-by-default matches `brokkr test`. `run` expresses the choice as
`cargo run --release`, `install` as `cargo install --debug` - cargo's two
commands disagree about which profile needs naming, and `[bin] debug = true`
means the same thing to both regardless.

## Lock, toolchain, and unconfigured trees

Both commands take the global per-user lock **blocking**, like every other
brokkr command that runs cargo: a concurrent bench run means `[lock] waiting
for …` rather than an error. Both ride the `disable_toolchain` window, so a
foreign checkout's pinned `rust-toolchain.toml` is moved aside for the build
exactly as it is under `check`.

With no `brokkr.toml`, the working directory is the project root and there is
no `[bin]` section - discovery, the bare index, and the sole-runnable and
sole-package rules all still work. Configuration buys disambiguation, never
capability.
