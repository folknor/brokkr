# brokkr corpus: the piners parity-corpus runner

Gated to `project = "piners"`. Runs a keyword-selected slice of the
PineScript-v6 parity corpus to completion, uncapped, so the VM-iteration
loop (edit Rust, run the relevant probes, read the verdict) stays inside
the prompt-cache-warm window. The full-corpus characterization pass still
exists (`--all`) but is explicitly off the fast budget.

Helpers live in `src/piners/` (`registry.rs`, `select.rs`, `manifest.rs`,
`report.rs`, `cmd.rs`).

## `[piners]` config

Kept out of the `[[check]]` sweep, like `[ratatoskr]`. All paths resolve
relative to `brokkr.toml`.

```toml
[piners]
corpus_root  = "corpus"          # read-only corpus submodule root (default)
registry_dir = "corpus-registry" # pins.toml + <keyword>.toml files (default)

# Shared OHLCV feeds, passed to the harness in the manifest verbatim
# (absolute after resolution). Not hash-gated.
[piners.feeds]
"1m" = "corpus/data/feed-1m.parquet"

# Corpus harness binary, reusing the shared [*.harness] shape. Required to
# run probes; --verify-only works without it.
[piners.harness]
package = "piners-runner"  # cargo package
binary  = "corpus"         # bin (built as `cargo build -p piners-runner --bin corpus`)
# features = ["..."]       # optional
# debug = true             # corpus defaults to debug; --debug/--release also override
```

## The corpus and the registry

The corpus is a read-only git submodule (default `./corpus`, upstream
`pineforge-corpus`). Each probe is a directory with `strategy.pine` (the
input) and `tv_trades.csv` (the TradingView oracle). It is read-only:
anything written inside diverges from upstream and is clobbered on re-pin.

Probes are pinned in a piners-owned registry (default `corpus-registry/`),
normalized into two file kinds:

- `pins.toml` - the canonical, verified universe. One `[probes.<id>]`
  table per probe, each pinning both files by path + xxh128:

  ```toml
  [probes.magnifier-tick-dist-endpoints-01]
  pine = { path = "validation/<id>/strategy.pine", xxh128 = "<hex>" }
  csv  = { path = "validation/<id>/tv_trades.csv", xxh128 = "<hex>" }
  ```

  `path` is relative to `corpus_root`. `xxh128` is brokkr's standard file
  hash (`preflight::compute_xxh128`, 32 lowercase hex chars,
  case-insensitive compare).

- `<keyword>.toml` (any other `*.toml`) - a pure selection grouping. The
  keyword is the file stem; the body is `probes = ["id", ...]`. Keyword
  files carry ids only, never hashes - the hash is the most volatile field
  (it changes on every upstream re-pin), so it lives in exactly one place.

The hash is pinned, not just the name: a name alone is unverifiable because
upstream can re-pin and change a probe's bytes under the same name.

## Selection

Selection is over the pinned universe. No selection (and no `--all` /
`--verify-only`) is a hard error listing the available keywords - the slow
full-corpus pass never runs by accident.

- `--keyword <k>` (repeatable) - union of the listed groupings.
- `--probe <id>` - one probe, resolved directly against `pins.toml`. A
  probe that is pinned but absent from every keyword file is still
  selectable this way.
- `--all` - the whole pinned universe (slow characterization pass).
- `--verify-only` - verify every pinned probe against the submodule and
  exit, without building or running. Use after a submodule re-pin to catch
  drift.
- `--reseed` - stamp `pins.toml` from the corpus filesystem; see below.

## Verification (the hard gate)

On every run, each selected probe's two pinned files are resolved under
`corpus_root` and hashed. A missing path or a hash mismatch is a hard
error before anything is built or run - the registry is lying or the
submodule drifted. There is no `--allow-drift` override; re-stamp the
registry with `--reseed` (the deliberate act of re-validating the oracle)
or fix the submodule. This is the only correctness gate today; parity
pass/fail baselines are deferred.

## Reseed: creating and re-stamping pins.toml

`--reseed` is the bootstrap and after-re-pin re-stamp - the only
sanctioned way `pins.toml` is created or refreshed (`--verify-only` only
compares against existing pins). No build, no harness, no
`[piners.harness]` needed; mutually exclusive with `--verify-only` and
`--keyword`. Unlike every other mode its universe is the corpus
**filesystem**, not `pins.toml` - it pins probes not yet pinned, resolving
ids against `corpus_root/validation/<id>/`.

- `--reseed --all` - stamp every parity probe under `validation/`;
  top-level dirs without `tv_trades.csv` (multi-mode self-tests, per-symbol
  containers) are skipped with a count. Authoritative full regen: a probe
  whose dir vanished upstream drops out.
- `--reseed --probe <id>` - upsert one probe (hard-errors on a missing
  file, since you named it explicitly).

Output is deterministic (sorted by id, inline `pine`/`csv = { path,
xxh128 }`) and idempotent. It prints `added=N changed=M removed=K`; `git
diff pins.toml` is the review surface where a re-pin's drift becomes
visible. Bootstrap: `brokkr corpus --reseed --all`, commit, write keyword
files, run a slice.

## The manifest hand-off

After verification brokkr writes `manifest.json` into the run dir and
hands its path to the harness. The harness consumes only the manifest - it
never re-resolves paths or re-checks hashes. Schema:

```json
{
  "version": 1,
  "corpus_root": "/abs/path/to/corpus",
  "probes": [
    {
      "probe": "magnifier-tick-dist-endpoints-01",
      "probe_dir": "validation/magnifier-tick-dist-endpoints-01",
      "pine": { "path": "validation/<id>/strategy.pine", "xxh128": "..." },
      "csv":  { "path": "validation/<id>/tv_trades.csv", "xxh128": "..." },
      "keywords": ["magnifier"]
    }
  ],
  "feeds": { "1m": "/abs/path/to/feed.parquet" }
}
```

All probe paths are relative to the top-level `corpus_root`. Each entry
carries the explicit canonical `probe` id (the `pins.toml` key) so the
harness emits it verbatim rather than inferring an id from `probe_dir`'s
basename (fragile once first-party probes land). The harness ignores
`pine`/`csv`/`keywords` - brokkr already verified them; they are a record.
`feeds` are absolute (resolved relative to `brokkr.toml`) and passed
through verbatim.

## The harness contract

brokkr builds the `[piners.harness]` binary once (`cargo build -p <pkg>
--bin <bin>`, debug profile by default - parity is opt-level-independent),
then spawns it as:

```
<bin> --manifest <run-dir>/manifest.json
```

with `BROKKR_HARNESS_ARTEFACT_DIR` (the run dir) and `BROKKR_TEST_BIN_DIR`
(`target/debug/`) set, mirroring ratatoskr's `--test-harness` spawn. The
binary emits NDJSON to stdout: one disposition line per probe, then a
summary line.

Per-probe line:

```json
{"probe":"<id>","outcome":"parity","matched":42,"ours_only":0,"tv_only":0,
 "count_tier":"exact",
 "acceptance":{"tier":"accepted","profile":"production",
               "p90":{"entry":0.0,"exit":0.18},"failing":["exit_price"]}}
```

`outcome` is `parity | compile_fail | runtime_fail | no_tv_data |
no_overlap`. `count_tier` and `acceptance` (tier `byte_exact | accepted |
actionable_drift | count_divergent`; profile `strict | production`) are
present only when `outcome == "parity"`. A `*_fail` outcome carries an
`error` string instead. Summary line: `{"summary":true,"total":...,...}`.

brokkr parses these tolerantly (unknown fields ignored, forward-compat)
and renders a per-probe table plus the summary.

## Exit codes

The harness exit code is authoritative:

- `0` - clean.
- `1` - one or more `compile_fail` / `runtime_fail` breaks.
- `2` - harness error (bad manifest, unreadable feeds).

brokkr maps a non-zero harness exit (and any signal) to `PASS`/`FAIL` and
exits non-zero. `actionable_drift` does not fail the run yet - the tier is
reported and read by the caller; tier-based pass/fail arrives with the
deferred parity-baseline work. A hash mismatch fails earlier, before the
build.

## Artefacts

Each invocation gets `.brokkr/piners/corpus/run-N/` holding `manifest.json`
plus the captured `harness.stdout` / `harness.stderr`. The dir is dropped
on success unless `--keep-artefacts`; failures are always preserved.
`brokkr clean` wipes `.brokkr/piners/`.
