# brokkr clippy

A single-phase, investigative clippy runner. `brokkr clippy` runs **only** the
clippy phase - no gremlins/style/header/textlint/manifest/dependency/test - so
it is a probe, not a gate. It exists to recover the exact firing lints of an
arbitrary crate/feature configuration under brokkr's env and toolchain
discipline, instead of dropping to a raw `cargo +nightly clippy` that silently
loses `disable_toolchain` and the project's `[[check]]` env (e.g. the
`HIGH_PRECISION=1` header-regeneration guard).

Because it is not a gate, it allows things a `[[check]]` entry deliberately
cannot: a real `--all-features`, and free feature sweeps no entry models. The
`[[check]]` ban on the `features = "all"` sentinel still stands - gates
enumerate, probes may sweep.

Works in any Rust+git repo. With a `brokkr.toml` its `[[check]]` env/features
are available (see env rules below); without one, it just runs cargo's defaults
in the current directory.

## Modes

**Ad-hoc** (the default): pick the target with `-p` (repeatable) and the feature
set with `--all-features` / `--features <list>` / `--no-default-features`.

```
brokkr clippy                              # cargo's default package selection
brokkr clippy -p mycrate --all-features    # one crate, all features
brokkr clippy --features a,b -p mycrate    # a virtual workspace needs -p
brokkr clippy --no-default-features
```

With no `-p`, cargo's default package selection applies: every member of a
virtual workspace, or the root package of a package-rooted one (brokkr does not
inject `--workspace`). In a virtual workspace `--features` requires `-p`, exactly
as bare cargo does - the flag is passed straight through.

**Sweep replay** (`--sweep NAME`): borrow one `[[check]]` entry's
packages/features/env verbatim and run just its clippy invocation. Useful for
reproducing the precise configuration a check sweep lints under, without the
test phase.

```
brokkr clippy --sweep ffi
```

`--sweep` conflicts with the ad-hoc target flags (`-p`, `--all-features`,
`--features`, `--no-default-features`) - the entry supplies all of them.

## Environment

`--env KEY=VALUE` (repeatable) sets extra env on the cargo invocation and wins
over every other source. KEY must be non-empty; `KEY=` (empty value) is legal.

The base env depends on the mode:

- **Ad-hoc:** the **union** of every `[[check]]` entry's `env`. These are
  build-affecting project invariants (codegen toggles like `HIGH_PRECISION`) a
  probe must not silently drop. If two entries set the same key to *different*
  values, that is a config error - resolve it with `--env KEY=...`, or replay one
  entry with `--sweep NAME`. Union (rather than intersection) is deliberate: an
  invariant present in most-but-not-all entries must survive, which intersection
  would drop.
- **`--sweep NAME`:** just that entry's `env`.

`--env` overrides both, and a key set via `--env` is exempt from the cross-sweep
conflict check (so `--env` can resolve a conflict, not just be masked by it).

## Output

Identical to `brokkr check`'s clippy phase. Cargo always runs with
`--message-format=json` so every diagnostic - including repeats of the same rule
- carries its lint code in the header:

```
cargo clippy --keep-going --all-targets --message-format=json <sel> <feat> -- --cap-lints=warn
```

`--cap-lints=warn` lets a deny-level lint produce its `.rmeta` so the whole graph
is checked in one pass; because a capped lint no longer makes cargo exit
non-zero, **pass/fail is brokkr's decision: any diagnostic is a failure**, and a
capped `warning` is promoted back to `error` in the output.

- default: one line per diagnostic, capped at `--limit N` (default 20), with
  branch-changed files surfaced first and a trailer summarising what is hidden.
- `--all`: show everything, sorted by (level, lint, file, line) so every hit of a
  rule clumps together for bulk triage.
- `--raw`: cargo's terminal-style rendering (full source annotations and help
  suggestions). This is human-rendered text, not machine JSON - there is no
  `--json` mode (it was removed from `check`; `clippy` never had one).

Exit code: `0` iff clippy produced zero diagnostics; `1` (with a
`clippy failed in Ns` summary) on any diagnostic or a genuine build error. An
unknown `--sweep`, an empty `--env` key, or a cross-sweep env conflict on an
un-overridden key exits via the normal config-error path; a cargo-spawn failure
or interruption propagates its real cause rather than the `clippy failed`
summary.

## Discipline and limitations

- `disable_toolchain` is honoured automatically: if `brokkr.toml` sets it, the
  pinned `rust-toolchain*` is moved aside for the run.
- Takes the global per-user lock **blocking**, like every build-running brokkr
  command - a concurrent bench just makes it wait, never error.
- **Inherited limitations** (shared with `brokkr check`'s clippy phase, not
  introduced here): the toolchain file is moved aside *before* the lock is
  acquired, so two near-simultaneous invocations can briefly race on it (a
  documented, opt-in-feature window that self-heals on the next run; see
  `src/toolchain.rs`); and the child cargo process is not registered with the
  lockfile, so `brokkr kill` cannot target it directly.
