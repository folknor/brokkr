# brokkr lint-corpus: the piners differential-lint corpus

Gated to `project = "piners"`. Runs a keyword-selected slice of `.pine`
snippets through two **offline** validators - **piners** (this dirty tree,
brokkr-compiled) and **pine-lint** (pre-installed) - diffs their diagnostics on
a `(line, col, severity)` grain, and gates on a pinned agreement disposition
per snippet. A periodic **re-anchor** mode (`--reanchor`) additionally consults
TradingView (`pine-lint --tv`) to re-ground the corpus against authoritative
truth. The trade-parity sibling of `brokkr corpus`; helpers live in
`src/piners/lint/`. The query sibling is `brokkr lint-results`.

The point: two offline validators *agreeing* doesn't make them *right* - they
can drift into shared-but-wrong consensus. The frequent run cross-checks piners
against pine-lint cheaply (no network); the re-anchor periodically confirms that
consensus against TradingView so the pair can't quietly agree on a wrong answer.

## The three validators, two cadences

| Validator | Source | Network | Role |
|---|---|---|---|
| **piners** | this dirty tree, `cargo build`-ed | no | the tool under test |
| **pine-lint** | pre-installed CLI | no | the frequent gate's partner |
| **TV** `translate_light` (via `pine-lint --tv`) | network | yes | periodic re-anchor truth |

brokkr never links piners: it builds the validator binary from the working tree
(like `corpus` builds `piners-runner`) and invokes `<bin> validate <file>
--format json` per probe. pine-lint and `pine-lint --tv` are external CLIs
brokkr shells out to directly - piners need not know they exist.

## The validator JSON contracts

- **piners** (`<bin> validate --format json <file>`):
  `{"ok":bool,"diagnostics":[{"severity":"error|warning|hint","line":N,
  "column":N|null,"stage":"lex|parse|type|semantic","code":..,"message":..}]}`.
  `stage` drives the syntax-only filter. Exit code is **not** the signal (exits
  1 when `!ok`, JSON still on stdout); only unparsable stdout is a
  `piners_error`.
- **pine-lint** (offline and `--tv`, same schema):
  `{"success":bool,"result":{"errors":[{"start":{"line","column"},..}],
  "warnings":[..]}}`. `errors`/`warnings` are *absent* (not `[]`) when clean -
  folded to empty. `errors[]` -> severity `error`, `warnings[]` -> `warning`.

Only `error` and `warning` are gated (pine-lint has no `hint`). piners `hint`
diagnostics are informational - never `piners_only` divergence.

## Scope: which diagnostics count

Two filters narrow what the diff compares, both reversible per run:

- **Stage** - default **syntax-only** (`--all-stages` widens). The two
  validators' *type/semantic* diagnostics diverge enough that comparing them is
  mostly noise (the `compare-piners.mjs` prototype's finding). piners tags each
  diagnostic with a `stage` (`lex`/`parse` = syntax); pine-lint is expected to
  emit a `stage` too, and until it does brokkr falls back to a message
  heuristic (`unexpected token`, `mismatched input`, ... ) ported from the
  prototype.
- **Severity** - default **errors only** (`--warnings` includes warnings).

## Config

```toml
[piners.lint]
package      = "<pkg>"           # cargo package brokkr builds from the tree
binary       = "<bin>"           # bin exposing `validate --format json`
subcommand   = "validate"        # default
registry_dir = "corpus/lint-registry"  # lints.toml + <keyword>.toml
snippets_dir = "corpus/lint-registry/snippets"  # .pine tree --reseed walks (default: registry_dir)
pine_lint_bin = "pine-lint"      # external validator (default: pine-lint on PATH)
```

Kept out of the `[[check]]` sweep, like `[ratatoskr]` and `[piners]`. Paths
resolve relative to `brokkr.toml`. A dedicated lint registry, separate from the
trade corpus (`pins.toml`) - lint snippets are their own curated set.

## The registry (`lints.toml`)

No feeds, no OHLCV, no `[roots]` - lint needs no market data. Each probe is a
`.pine` snippet plus its pinned disposition and optional TV anchor:

```toml
[probes.unterminated-string-01]
pine     = { path = "lint/unterminated-string-01.pine", xxh128 = "<hex>" }
expected = "agree_flagged"          # the gated piners<->pine-lint disposition

# TV anchor - written only by --reanchor, informational on frequent runs:
tv_anchored_at = "2026-06-22T14:03:00Z"
tv = [ { line = 4, col = 8, severity = "error" } ]
```

`path` is relative to the registry's snippet tree; `xxh128` is brokkr's
standard file hash (`preflight::compute_xxh128`), verified before any run.
`<keyword>.toml` files are pure selection groupings (`probes = ["id", ..]`),
ids only - the volatile fields live only in `lints.toml`.

## Selection

Same surface as `corpus`. No selection (and no `--all`/`--verify-only`) is a
hard error listing the keywords - the full pass never runs by accident.

- `--keyword <k>` (repeatable / comma-separated) - union of the groupings.
- `--probe <id>` (repeatable / comma-separated) - union of named probes.
- `--all` - the whole pinned universe.
- `--verify-only` - hash-verify every pinned snippet and exit, no build/run.
- `--reseed` - stamp `lints.toml` from the snippet tree (below).
- `--reanchor` - refresh the TV anchor for the selection (below).
- `--bless` - run, then stamp current dispositions into `expected` (below).

## Reseed: the bootstrap writer

`lints.toml` is created and its hashes refreshed only by `--reseed` (there is
no `xxhsum` on PATH). It walks the snippet directory (`[piners.lint]
snippets_dir`, default `registry_dir`, must live under `corpus_root`)
recursively for `*.pine`, keyed by file stem:

- `--reseed --all` - stamp every snippet; vanished ones drop out.
- `--reseed --probe <id>` (repeatable) - upsert the named snippet(s).

It touches the pinned *content* (snippet path + `xxh128`) only - each
surviving probe's `expected` and TV anchor are carried forward. No build, no
run. Bootstrap order: `--reseed --all` -> write keyword files -> `--bless
--all` -> runs are gated.

## The diff and the disposition

Each probe yields two `(line, col, severity)` key sets - piners `P`, pine-lint
`L` (error+warning only). The disposition:

- `agree_clean` - both empty.
- `agree_flagged` - `P == L`, non-empty.
- `divergent` - `P != L`, carrying a **signature**: `piners_only` (`L ⊂ P`),
  `lint_only` (`P ⊂ L`), `severity_mismatch` (same `(line,col)`, different
  severity), or `mixed`.
- `piners_error` / `lint_error` - a tool produced no parsable output.

Columns are compared as reported (piners is 1-based byte columns; pine-lint
1-based). Non-ASCII source can spuriously mismatch columns; lint snippets stay
ASCII where practical.

## The expected-disposition gate

Each probe pins one `expected` disposition; brokkr compares actual vs expected
per selected probe and **any** deviation fails - regression and surprise
convergence alike, each as `id: expected X, got Y`. No `expected` yet is a hard
"must bless". `--no-gate` downgrades to informational (still runs, aggregates,
prints; exit governed by tool errors only).

## Re-anchor: the periodic TV writer

`--reanchor [--all|--keyword <k>|--probe <id>]` drives `pine-lint --tv` over the
selection and stamps each probe's TV diagnostic fingerprint + an absolute
`tv_anchored_at` into `lints.toml` (via `toml_edit`, comment-preserving). It is
the deliberate, network-touching registry writer - the analogue of `corpus`'s
`--reseed`/`--bless`, run on a cadence, never in a normal run. On frequent runs
the anchor is **informational**: when piners and pine-lint agree but both
diverge from a fresh anchor, brokkr surfaces it (`agree but TV-divergent,
anchored Nd ago`) - the shared-but-wrong consensus the corpus exists to catch.
TV is rate-limited and times out at 10s; re-anchor is sequential and tolerant of
per-probe transport failures (reported, not fatal).

## Bless

`--bless [selection]` runs the selection (verify + build + both offline tools),
then stamps each probe's current disposition into `expected`. Records reality
including divergences a snippet legitimately pins. Never gates. Excludes
`--verify-only`/`--reanchor`.

## Exit codes

`0` clean; non-zero on a tool error (`piners_error`/`lint_error`) **or** an
active gate deviation. Hash mismatch fails before the build. `--no-gate` and
`--bless` never fail on gate diffs; `--verify-only` exits 0 once all pins
verify; `--reanchor` exits 0 unless every TV call failed.

## Artefacts and the run store

Each run's per-probe results ingest into `.brokkr/piners/lint/runs.db` (mirrors
the corpus run store): a `run` row plus child `disposition` rows (outcome,
disposition, expected, gate_ok, the raw piners/pine-lint diagnostic counts, the
signature, the TV-anchor age). Gitignore it. `brokkr clean` spares it.

## See also

- `docs/commands/corpus.md` - the trade-parity sibling this mirrors.
- `docs/commands/measure.md` - measurement-mode conventions.
