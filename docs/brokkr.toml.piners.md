# `[piners]` config

The piners block, read by `brokkr corpus` / `brokkr corpus-results`. Kept out of the
`[[check]]` sweep, like `[ratatoskr]`. All paths resolve relative to
`brokkr.toml`. See `docs/commands/corpus.md` for the runner and
`docs/projects/piners.md` for the data contracts + run store.

```toml
[piners]
corpus_root  = "corpus"          # piners-owned corpus tree root (default)
registry_dir = "corpus/registry" # pins.toml + <keyword>.toml files (default: corpus-registry)

# Corpus harness binary, reusing the shared [*.harness] shape. Required to
# run probes; --verify-only and --reseed work without it.
[piners.harness]
package = "piners-runner"  # cargo package
binary  = "corpus"         # bin (built as `cargo build -p piners-runner --bin corpus`)
# features = ["..."]       # optional
# debug = true             # corpus defaults to debug; --debug/--release also override
```

- `corpus_root` - root of the piners-owned corpus tree (default `corpus`):
  vendor submodules (e.g. `vendor/pineforge-engine/`,
  `vendor/pineforge-benchmarks-assets/`) plus first-party probe dirs (e.g.
  `piners/`, `vendor/retired/`). Pinned probe **and feed** paths in
  `pins.toml` resolve under here.
- `registry_dir` - the piners-owned registry: `pins.toml` (the canonical
  id -> path+xxh128+expected universe, plus the `[feeds]` and `[roots]`
  tables) and one `*.toml` per keyword (id lists). Default
  `corpus-registry`; the relocated layout puts it at `corpus/registry`
  (inside the data tree but containing no probe markers - the
  corpus_root-vs-registry_dir distinction, data the pins point into vs
  piners-owned metadata describing it, still holds).
- `harness` - the corpus harness build spec (shared `[*.harness]` shape).
  Required to run probes; `--verify-only` and `--reseed` work without it.

There is no `[piners.feeds]` block. OHLCV feeds are hash-pinned registry
content - `[feeds.<name>]` groups in `pins.toml`, referenced per probe via
`feed = "<name>"` - because a probe's TV export was taken against one
specific feed, making the feed part of its oracle identity. See
`docs/commands/corpus.md`.
