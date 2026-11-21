# brokkr clippy

`brokkr clippy` runs the clippy phase of `brokkr check` and nothing else. It
skips gremlins, style, header, textlint, manifest, dependency and test, so
it is a probe, not a gate. Use it to recover the exact firing lints of any
crate and feature configuration under brokkr's env and toolchain discipline.
A raw `cargo +nightly clippy` silently loses `disable_toolchain` and the
project's `[[check]]` env. This page covers the two modes, `--lib`, the
environment, the output, and the lock and toolchain limits.

Because it is not a gate, `brokkr clippy` allows what a `[[check]]` entry
deliberately can't: a real `--all-features`, and free feature sweeps no
entry models. The `[[check]]` ban on the `features = "all"` sentinel still
stands: gates enumerate, probes may sweep.

It works in any Rust and git repo. With a `brokkr.toml`, its `[[check]]` env
and features are available (see Environment). Without one, it runs cargo's
defaults in the current directory.

## Modes

`brokkr clippy` has two modes: ad-hoc, the default, and sweep replay.

**Ad-hoc** mode picks the target with `-p` (repeatable) and the feature set
with `--all-features`, `--features LIST` or `--no-default-features`:

```sh
brokkr clippy                              # cargo's default packages
brokkr clippy -p mycrate --all-features    # one crate, all features
brokkr clippy --features a,b -p mycrate    # a virtual workspace needs -p
brokkr clippy --no-default-features
```

With no `-p`, cargo's default package selection applies: every member of a
virtual workspace, or the root package of a package-rooted one. brokkr
doesn't inject `--workspace`. In a virtual workspace `--features` requires
`-p`, exactly as bare cargo does, since brokkr passes the flag straight
through.

**Sweep replay** (`--sweep NAME`) borrows one `[[check]]` entry's packages,
features and env verbatim and runs only its clippy invocation. Use it to
reproduce the configuration a check sweep lints under, without the test
phase. brokkr resolves the entry's `feature_unification` at the same point
and by the same rule `check` uses. A `package` entry lints once per package
under the package pin, and a parallel `auto` entry that `check` would
promote to a workspace pin is promoted here too. A profile's `env` isn't
applied, because `--sweep` names a `[[check]]` entry, not a profile lane.

`NAME` may also be a `[[lint_sweep]]` entry, replayed the same way, its
`target` included, whatever its `when`. That is how you run a `when = "off"`
lint sweep on demand. It runs clippy only: nothing runs a lint sweep's
rustdoc pass outside `brokkr check`. The names of the two tables never
collide.

```sh
brokkr clippy --sweep ffi
```

`--sweep` conflicts with the ad-hoc target flags (`-p`, `--all-features`,
`--features`, `--no-default-features`), since the entry supplies all of
them.

## --lib

`--lib` lints the library target alone instead of the default
`--all-targets`. It composes with both modes, `--sweep` included: the target
selector is independent of where the sweep's shape came from.

```sh
brokkr clippy -p mycrate --lib
```

The flag exists because `--all-targets` hides a whole lint class. An import
used only from `#[cfg(test)]` or `#[cfg(feature = "test-support")]` code is
live in the test target, so `unused_imports` never fires. A plain `cargo
build` or `cargo doc` compiles only the lib and rejects the import. Without
`--lib`, `brokkr clippy` reports clean against a tree the doc build refuses,
and only whichever phase happened to catch the class reproduces it.

No `[[check]]` key reaches this. A gate sweep is always `--all-targets`,
because narrowing what a gate lints is how lints stop being caught. A
deliberately partial view belongs in the investigative runner.

## Environment

`--env KEY=VALUE` (repeatable) sets extra env on the cargo invocation and
wins over every other source. KEY must be non-empty; `KEY=` with an empty
value is legal.

The base env depends on the mode:

- **Ad-hoc**: the union of every `[[check]]` entry's `env`. brokkr treats
  these as build-affecting project invariants, such as a var a build script
  reads or a codegen toggle, which a probe must not silently drop. brokkr
  can't tell a load-bearing entry `env` from an inert one without knowing
  what every `build.rs` reads, so it applies all of them and the project
  decides what belongs there. Two entries that set one key to different
  values are a config error: resolve it with `--env KEY=...`, or replay one
  entry with `--sweep NAME`. `[[lint_sweep]]` env isn't in the union, since
  a cross-target or `off` entry's env belongs to its own shape, not to a
  probe that didn't name it. brokkr takes the union rather than the
  intersection so that an invariant present in most but not all entries
  survives. `brokkr check`'s ad-hoc `--features` path takes the same union
  from the same code, so the two commands can't resolve different shapes
  from one config.
- **`--sweep NAME`**: only that entry's `env`, `[[check]]` or
  `[[lint_sweep]]`.

`--env` overrides both. A key set through `--env` is exempt from the
cross-sweep conflict check, so `--env` can resolve a conflict rather than be
masked by it. `--env` also wins over the `CARGO_TARGET_DIR` and `RUSTFLAGS`
pair brokkr composes for a `rustflags` or package-mode sweep: the same
sweep-env-wins rule every `check` phase applies.

## Output

The output is identical to `brokkr check`'s clippy phase, which shares its
pipeline. Cargo always runs with `--message-format=json`, so every
diagnostic, including repeats of the same rule, has its lint code in the
header:

```text
cargo clippy --keep-going --all-targets --message-format=json SEL FEAT
  -- --cap-lints=warn
```

`--lib` replaces `--all-targets` in that line when you pass it.

brokkr prints the command exactly once, after the run, including after a
spawn failure, an interrupt or a deadline. A green run prints the line
above, since showing it is the point of an investigative runner. A failing
run prints the shape and `failing command:` pair beside its diagnostics and
doesn't stream the same line beforehand. The ad-hoc sweep is labelled
`clippy`, so the shape line reads `clippy: SHAPE`.

`--cap-lints=warn` lets a deny-level lint produce its `.rmeta`, so cargo
checks the whole graph in one pass. A capped lint no longer makes cargo exit
non-zero, so brokkr decides pass or fail: any diagnostic is a failure,
reported as an error, except a warning from a dependency outside the
workspace.

A `[lints] allow` list in `brokkr.toml` applies here exactly as in `check`'s
clippy phase. brokkr appends each entry as `-A LINT` after
`--cap-lints=warn`, and the printed command shows them, with no separate
announcement line on stdout. `[lints] allow_exact` (`"lint@path"` sited
suppressions, filtered on brokkr's side at JSON ingestion rather than
through `-A`) applies here too; see `brokkr man check clippy`. Its
`suppressed nothing (stale entry?)` notice fires only when the probe covers
the whole lint surface:

- no package scope, from ad-hoc `-p` or the `--sweep` entry's `packages`
- no `--lib`
- `--all-features`

A narrower probe never lints the entry's site, so the notice would be
noise.

brokkr prints one line per error, sorted by lint, file and line, with errors
in files that have unstaged changes first. Every error prints, with no cap,
the same rule as `check`. There is no unfiltered mode and no `--json` mode:
one line per diagnostic is the whole output.

The exit code is `0` if clippy produced no diagnostics, and `1`, with a
`clippy failed in Ns` summary, on any diagnostic or a genuine build error.
These exit through the normal config-error path:

- an unknown `--sweep`
- an empty `--env` key
- a cross-sweep env conflict on a key `--env` doesn't override

A missing `Cargo.toml`, a failed `cargo metadata`, a cargo spawn failure or
an interruption reports its real cause. The `clippy failed` summary is
reserved for a lint run whose diagnostics were printed.

## Lock and toolchain

`brokkr clippy` follows the same lock and toolchain discipline as `check`:

- brokkr honours `disable_toolchain` automatically: if `brokkr.toml` sets
  it, brokkr moves the pinned `rust-toolchain*` aside for the run.
- It takes the global per-user lock blocking, like every build-running
  brokkr command, so a concurrent bench makes it wait, never error.
- It runs under `check`'s five-minute `clippy` phase ceiling, measured from
  lock acquisition. An overrun kills the run's processes and exits 124, as
  in `check` (`brokkr man hygiene time-ceilings`). A graceful `brokkr kill`
  or Ctrl-C ends it as an interruption, exit 130.

Two limitations come from `brokkr check`'s clippy phase, which shares them:

- brokkr moves the toolchain file aside before it acquires the lock, so two
  near-simultaneous invocations can briefly race on it. The window is
  opt-in, since only `disable_toolchain` opens it, and the next run heals
  it.
- brokkr doesn't register the child cargo process with the lockfile, so
  `brokkr kill --hard` reaches it only by walking the processes beneath
  brokkr, not as a named child.
