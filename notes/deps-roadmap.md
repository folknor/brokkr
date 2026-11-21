# brokkr deps roadmap

Deferred phases for `brokkr deps`, recovered from the pre-rewrite
`docs/commands/deps.md`. Status: **planned** means designed, not built;
**idea** means on the wishlist and may never ship. For the usage-driven gaps
(member-scoped reverse, forward tree, feature inspection) see
`notes/deps.md`.

## Shipped so far

The first cut shipped in this order, each as its own change:

1. Scaffolding: `Command::Deps`, `DepsEvent` with `Summary` only, the
   `--json` path, and an empty run emitting `Summary { findings: 0 }`.
2. `duplicate_version`.
3. `git_dependency` and `path_dependency`.
4. `outdated` and `stale` through the `ccu --json` shell-out.

`publish_cycle`, `workspace_dep` and `native_code` shipped outside that
list.

## advisory

Status: planned, and the next phase. Shell out to `cargo audit --json` and
adapt its findings into `Advisory` events with id, severity, patched-in
version, and kind (`vulnerability` or `unmaintained`). Same graceful
`ToolMissing` skip as the ccu phases. No native network code and no
advisory-db cache management in `deps` itself. It would land as
`src/deps/advisory.rs`.

## yanked

Status: idea. `cargo audit` covers some yanks through advisories but not
all. A direct sparse-index lookup phase could follow if advisories turn out
to miss real problems.

## single_publisher

Status: idea. Flag crates whose only publisher is a single account with
fewer than N total crates, or fewer than M monthly downloads, as a
supply-chain hygiene signal. Risky to surface because it implicates real
people; keep it informational if it ships.
