# `[piners]` config

The piners block, read by `brokkr corpus` / `brokkr corpus-results`. Kept out of the
`[[check]]` sweep, like `[ratatoskr]`. All paths resolve relative to
`brokkr.toml`. See `docs/commands/corpus.md` for the runner and
`docs/projects/piners.md` for the data contracts + run store.

```toml
[piners]
corpus_root  = "corpus"          # read-only corpus submodule root (default)
registry_dir = "corpus-registry" # pins.toml + <keyword>.toml files (default)

# Shared OHLCV feeds, passed to the harness in the manifest verbatim
# (absolute after resolution). Not hash-gated.
[piners.feeds]
"1m" = "corpus/data/feed-1m.parquet"

# Corpus harness binary, reusing the shared [*.harness] shape. Required to
# run probes; --verify-only and --reseed work without it.
[piners.harness]
package = "piners-runner"  # cargo package
binary  = "corpus"         # bin (built as `cargo build -p piners-runner --bin corpus`)
# features = ["..."]       # optional
# debug = true             # corpus defaults to debug; --debug/--release also override
```

- `corpus_root` - root of the read-only corpus submodule (default `corpus`).
  Pinned probe paths in `pins.toml` resolve under here.
- `registry_dir` - the piners-owned registry: `pins.toml` (the canonical
  id -> path+xxh128+expected universe) plus one `*.toml` per keyword (id
  lists). Default `corpus-registry`.
- `feeds` - shared OHLCV feed paths keyed by an arbitrary label (e.g.
  timeframe), passed through to the harness in the manifest. Not hash-gated -
  only `strategy.pine` and `tv_trades.csv` are pinned oracles.
- `harness` - the corpus harness build spec (shared `[*.harness]` shape).
  Required to run probes; `--verify-only` and `--reseed` work without it.
