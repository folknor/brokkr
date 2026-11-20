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

## Shape

**Declarative TOML for what a program is**: package, bin, profile,
features, rustflags. It must be readable without running anything: refusing a
conflicting `--profile` before a build, `install --names`, help visibility
and the project gate all read config that way.

**Lua for what happens around it**: post-install actions, deploys, smoke
checks, and multi-step builds cargo alone does not cover (codegen, bundling).
The dellingr VM seam already runs script benches with build and program
builtins; this would be a second kind of script beside them.

A script drives a declared build through a builtin and never calls cargo
itself, so every build still goes through the lock, the install stamp and the
recorded provenance. The builtin returns those facts, which answers #200 for
script consumers and for `--with` alike.

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
