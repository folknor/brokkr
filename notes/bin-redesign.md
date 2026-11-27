# Redesigning [bin] and install

Not scheduled. This collects the wants and the shape discussed so far, so the
redesign starts from them rather than from memory.

## Why

`[bin]` and its `install` key grew one case at a time. Several open tickets
are the same gap seen from different consumers: brokkr has no per-program
build definition, so a program's build is whatever the invocation happens to
resolve. brokkr is headed for crates.io once the project-specific commands are
gone, so whatever replaces `[bin]` has to stand on its own: no consumer, such
as an internal fleet tool, should have to supply what `brokkr install` cannot
express.

## Inputs

Wants, not workarounds. A survey of what projects do today mostly records the
limits they routed around, so it is a migration checklist at the end, not a
design input.

- **A program has a fixed way it must be built** (brokkr#203). piners runs
  its corpus tool as `brokkr run --profile corpus --with corpus-harness
  corpus`, and the tool records `cargo:corpus` as the harness profile. Nothing
  makes that true: `[bin] debug = true` defaults `run` to dev, and one profile
  applies to every package of the invocation. A forgotten `--profile` stores
  dev timings as corpus ones. The config should own the profile of a named
  program, its with-targets included, and the CLI should refuse to contradict
  it.
- **A program's name is its whole invocation.** `brokkr run corpus` should
  mean `brokkr run --profile corpus --with corpus-harness corpus -- run`, and
  `brokkr run corpus --all` should append `--all` after that, giving
  `... corpus -- run --all`. Today nobody types the long form the same way
  twice, and a forgotten piece is the #203 failure.
- **A consumer knows what it was handed** (brokkr#200). `run --with` passes
  only `BROKKR_WITH_EXE`. The profile, commit, dirty state and feature
  selection brokkr resolved never reach the program. piners worked around all
  but features, so nothing waits on the ticket, but the want stands.
- **Any commit can be built and run** (brokkr#199). Bisecting a piners
  divergence needed a `src/bin` diagnostic at a September commit. `run` builds
  only the working tree, the old commit's `brokkr.toml` no longer loads in the
  current brokkr, and `bench --commit` runs only declared benches.
- **Install-time actions.** brokkr itself needs `brokkr guard --install` and
  `brokkr-results-server deploy` after installing, and bz needs `bz deploy`.
  Today an outside tool runs them, because `brokkr install` cannot declare
  them.
- **A program can live in a nested workspace** (brokkr#214), with `--with`
  still resolving against the root workspace. In a declaration, that is a
  manifest or workspace field per program. For launching, it means the two
  sides' recipes differ in cargo config chain and target dir, which the
  one-`BuildRecipe` rule already carries.

## Shape

**Declarative TOML for what a program is**: package, bin, profile,
features, rustflags. It must be readable without running anything: refusing a
conflicting `--profile` before a build, `install --names`, help visibility
and the project gate all read config that way.

**A program entry is its own alias.** There is no separate alias namespace:
`[program.corpus]` declares the build (profile, with-targets) and a fixed
leading `args` list, here `["run"]`, and `brokkr run corpus` runs exactly
that. Once brokkr sees a program name, every later token is appended to
the program's arguments, so brokkr's own flags go before the name. A flag
that contradicts the declaration, such as `brokkr run --profile dev corpus`,
is refused before the build. One name means one way to build and run it,
which is #203 with the arguments included. Still open: whether a program
name may shadow a bin of the same name that has no entry, or is refused.

The build often depends on the program's own subcommand, so one flat entry per
invocation is the wrong grain. piners runs `corpus -- run|bless` as `--profile
corpus --with corpus-harness`, but `corpus -- reseed|results|verify|clean`
as plain dev. elivagar runs `elivagar-devkit -- tilegen|ocean-build` with
`--with elivagar`, but `archive` and `clean` without it. pbfhogg is the simple
case, one fixed with-target per bin (`--with pbfhogg pbfhogg-verify`). The
shape is a table keyed on the first program argument, still declarative so
`--names`, help and the pre-build refusal can read it:

```toml
[program.devkit]
bin = "elivagar-devkit"

[program.devkit.command.tilegen]
with = ["elivagar"]
```

`brokkr run devkit tilegen --dataset denmark` applies the `tilegen`
declaration over the program's; `brokkr run devkit archive` gets the bare
program.

What the declaration owns is narrower than "every flag". The profile and the
with-target's identity are owned, and a contradicting flag is refused. The
commit, `--with-commit` and extra `--features` stay per-invocation axes and
combine with the declaration. pbfhogg's `--with pbfhogg --with-commit REF
pbfhogg-verify` is its A/B axis, and piners' `--release --features
vortex-bench|hotpath|spill-profile bench` varies the instrument for each call,
which is why run.md has no feature default.

**Lua for what happens around it**: post-install actions, deploys, smoke
checks, and multi-step builds cargo alone does not cover (codegen, bundling).
The dellingr VM seam already runs script benches with build and program
builtins; this would be a second kind of script beside them.

A script drives a declared build through a builtin and never calls cargo
itself, so every build still goes through the lock, the install stamp and the
recorded provenance. The builtin returns those facts, which answers #200 for
script consumers and for `--with` alike.

## Launching: cargo computes, brokkr validates

Settled by spar with codex. The earlier direction below it, brokkr
constructing the environment itself, is rejected; the reasons follow the
design.

### The design

Every launch belongs to one of two classes, decided by one question: are the
launch's environment inputs exactly brokkr's own environment?

**Cargo launches it** (plain `run`, `run --commit`). Two cargo invocations,
as plain `run` has today: a captured `cargo build --message-format=json` for
diagnostics, then `cargo run --quiet` of the same recipe with
`--config target.TRIPLE.runner=[brokkr-launch, --plan, FILE]` and stdio
inherited. Cargo computes the whole launch environment from what it actually
receives: loader path, `[env]`, `CARGO_*`, build-script `rustc-env` with its
own unit association, the rustup proxy's variables, a live jobserver. It then
exec-replaces into `brokkr-launch`, which does only what needs no
reconstruction:

- changes to the plan's directory (the live tree under `--commit`, whose
  build runs in the worktree, so the commit's cargo config applies);
- adds `BROKKR_*` keys, which cargo never sets;
- writes the plan's nonce on the handshake;
- execs the user's resolved runner, if any, then the program.

The PID stays the same from cargo's spawn to the program, so signals, process
groups and the PID published in the lock are unchanged. All three launch forms
then time the same thing.

**Probe once, then launch directly** (script benches per arm, `serve`, and
both sides of `--with`). After the build, one `cargo run --message-format=json`
of the same recipe, with `brokkr-launch --probe` as the runner, records
cargo's full launch environment. brokkr refuses the probe if any artifact
reports `fresh: false`, or if the executable path or digest in the probe's
argv differs from the build it recorded. Launches then exec the snapshot
directly, with that environment and the caller's per-call additions
(`BROKKR_WITH_EXE`, a bench call's `env`) applied last, key by key, the loader
variable included. That last rule is brokkr's stated contract. It claims no
cargo parity for inputs cargo never saw.

Under `--with` the order is: build the peer, build the outer program, probe
the peer, probe the outer program, launch. No cargo runs after the last probe,
so nothing brokkr schedules can rewrite either side's runtime paths.
`BROKKR_WITH_EXE` names `brokkr-launch` hard-linked into the peer's snapshot
directory. It execs the peer with exactly the frozen environment. The outer
program's own changes don't reach the peer; arguments are its channel.
`BROKKR_WITH_ARTIFACT` names the raw executable.

**Shared rules:**

- One `BuildRecipe` value builds the args, env (`CARGO_TARGET_DIR`,
  `CARGO_BUILD_BUILD_DIR`, rustflags placement) and cwd of the build, the
  run, and the probe, so they can't diverge.
- Plan and probe files live under `~/.brokkr`, never in a tree a build script
  could watch.
- Frozen launches never take `MAKEFLAGS`, `MFLAGS` or `CARGO_MAKEFLAGS` from
  the probe. They carry brokkr's own live values and inherited descriptors.
  Residual: an `[env]` or `rustc-env` assignment to one of those names doesn't
  reach a frozen launch.
- Preflight runs before any cargo, and refuses when brokkr can't prove the
  probe override wins. Three configurations defeat it: an exact-target runner
  from config or env (cargo concatenates runner arrays, so the probe can't come
  first), `target-applies-to-host = false`, or two matching `cfg()` runners
  (cargo's own error, which the override would mask). Plain `run` then falls
  back to plain `cargo run`; every other form refuses. A missing nonce on the
  handshake is a hard error, a backstop behind the preflight.
- Runners: the cargo-launched class honors the user's runner. The probed
  class refuses one, because timing, the marker FIFO, the published PID and
  leader-exit escalation can't hold through an arbitrary wrapper.
- `brokkr build` keeps its bare-path contract. A script that execs the path
  gets no launch environment, as documented today.
- Check's direct test lanes are unchanged. They keep `direct_runtime` and
  their refusal of runners; a runner that prints on stdout would break the
  mandatory `--list` pass.

**Scope of the guarantee:** the launched artifact and its runner. Build
scripts and compiler wrappers run during the probe as ordinary build time. A
cargo config that an outside writer edits mid-command is outside the
guarantee, as for bench inputs. Snapshots stay byte-only. Runtime libraries
are whatever cargo's loader directories hold, as under `cargo run`.

**`brokkr-launch` must have no ELF interpreter.** A dynamically linked
helper isn't fail-closed. An `LD_PRELOAD` from the target's launch
environment that interposes `execve` (proxychains and faketime-style
libraries do) could let the helper pass validation and the handshake, then
exec the program with a changed environment. Under cargo, that preload loads
only into the program. The plan: a `#![no_std]`, `#![no_main]` bin in this
crate, using raw syscalls (read the plan, `chdir`, write the nonce,
`execve`), linked through `build.rs`'s
`cargo:rustc-link-arg-bin=brokkr-launch=` with `-static -nostdlib`, with
`test = false`. Verify the installed file has no `PT_INTERP` and no
`DT_NEEDED`. Whether panic handling links under the dev profile's unwinding
is the first thing to confirm with a build.

### Why not reconstruct

Each reconstruction attempt failed on a concrete case:

- Rebuilding cargo's environment at spawn missed the rustup proxy's
  variables, and got cargo's order wrong for rustc-env against `CARGO_PKG_*`,
  and for `[env]`'s `force` against keys cargo sets.
- A recorded value diff can't tell "cargo forced this value" from "cargo
  inherited it". A peer could then inherit the outer program's forced value.
- A record replayed under a later caller environment can't reproduce what
  cargo computes from that environment. With
  `LD_LIBRARY_PATH=P:/old` inherited and a call passing `/new`, cargo would
  give `P:/new`, and every recovery gives something else.
- A bundle of `DT_NEEDED` libraries can't cover `dlopen` plugins, absolute
  `DT_NEEDED` entries or `DT_RPATH`. Library immutability was also never
  something `cargo run` offers.

Cargo can compute an environment for the inputs it actually receives;
nothing can recover what it would have computed for others. The design uses
only the first.

### The old picture, for reference

The launch paths disagree today. Plain `run` builds captured and then
launches with `cargo run --quiet`, so cargo's launch setup applies. `--commit`
and `--with` (and benches, `serve`, `build` consumers) execute the artifact
directly and get none of it. A program that needs that setup works under one
form and fails under another, which breaks an A/B between them. `--with`
cannot simply move to `cargo run`: `BROKKR_WITH_EXE` would reach cargo and
build scripts and become a build input.

What cargo's launch sets up, which a reimplementation has to own (cargo's
`target_process` in `compiler/compilation.rs` is the reference):

- **Runner.** `target.<triple>.runner` / `CARGO_TARGET_<TRIPLE>_RUNNER`,
  resolved for the actual compile target (implicit host, `build.target`,
  `cfg()` matching, config-relative paths, string or array form), launched as
  `runner <runner args> <artifact> <args>`.
- **Loader search path.** Ordered: eligible build-script `rustc-link-search`
  dirs (cargo filters ones outside its build outputs), the profile's output
  and `deps` dirs, the toolchain's target sysroot lib dir, then the inherited
  value. `LD_LIBRARY_PATH` on Linux, `DYLD_FALLBACK_LIBRARY_PATH` on macOS,
  `PATH` on Windows.
- **Runtime variables.** `CARGO`, `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`,
  `CARGO_PKG_*`, cargo config `[env]` with its precedence, and the selected
  unit's build-script `rustc-env`.

Where the facts come from: cargo's stable `--message-format=json` gives the
executable, and build-script `env` and `linked_paths`, but no resolved launch
command; runner and `[env]` need cargo config resolution of brokkr's own (or
`cargo config get`, unstable). Paths and values should stay OS-native through
to the spawn, not go through `display()`.

## Boundary

This is the role `make install`, `make bench` and friends play around a Rust
project, not make itself. Cargo owns the dependency graph. brokkr owns the
policy around it: locking, provenance, clean-tree rules, stamps, worktrees,
measurement. No general target graph where one step depends on another; once
`[bin]` grows that, brokkr is a worse make.

## Open questions

- Which config declares a program built at an old commit: that commit's, read
  leniently, or the current one. A script is versioned with its commit, which
  favors the commit's own actions; the declaration is the hard part.
- How features enter provenance and any key that groups runs by build (piners'
  schedule estimates key on profile alone).
- The script contract: which builtins, what an action may touch, how a failed
  action leaves an install (stamped binary, action owed).
- Migration of every existing `[bin]` and `install` use, under the config
  contract: no config that loads today may stop loading without approval.
