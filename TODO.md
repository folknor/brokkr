# TODO

## Won't fix

### Inconsistent path-to-string conversion
All paths are constructed from known UTF-8 components, so `.display().to_string()` won't corrupt in practice. 100+ occurrences across 30+ files — not worth the churn.

### Hand-rolled UUID via `/dev/urandom`
10 correct lines in `src/db.rs`. Not worth adding the `uuid` crate as a dependency.

### `#[allow(clippy::too_many_arguments)]` proliferation
Functions genuinely need many parameters. `BenchContext` and `HarnessContext` cover the common cases; remaining allows are the pragmatic choice.

---

## Backlog

### Counter diffs in --compare-timeline
Include counter values at matching phase boundaries in the comparison table. Currently `--compare-timeline` only shows duration, peak anon, and disk read.

### --phase filter for --markers --counters
`--phase` currently requires `--timeline` (clap constraint). Adding `--markers --counters --phase STAGE4` to show only counters within a phase would be useful for targeted analysis.

### `brokkr results` dataset short-name is heuristic

The `dataset` column in `brokkr results` output collapses `input_file` to the
first dash-separated component of the basename (e.g.
`europe-20260301-seq4714-with-indexdata.osm` → `europe`,
`denmark-elivagar.pmtiles` → `denmark`). This is a pure string heuristic — it
does not consult `brokkr.toml` to learn which dataset names are actually
configured.

Breaks if a dataset name itself contains a dash (e.g. `europe-west`, `asia-japan`)
— display would show `europe` / `asia`, losing the distinction. Filtering is
unaffected: `--dataset` is a substring match on the full `input_file` column, so
`--dataset europe-west` still works even when the displayed name is truncated.

Proper fix: load the dataset keys from the active `brokkr.toml` on `results`
invocation and match the `input_file` basename against the longest known prefix.
Requires plumbing `DevConfig` (or at least the dataset key list) into
`src/db/format.rs::format_input`. Not worth the coupling until we actually hit
a hyphenated dataset name in practice.

### Sidecar: result+sidecar persistence is not atomic

The benchmark result row is committed first, then sidecar rows are inserted in separate per-run transactions. If sidecar storage fails after the result is committed, the DB has a result with partial/no sidecar data. Not catastrophic (partial data is better than none), but could be wrapped in a single transaction.

### Make default binary configurable per-project in brokkr.toml

Currently `find_executable` infers the expected binary name from `BuildConfig.bin` or `BuildConfig.package`. This should be configurable in `brokkr.toml` (e.g. a `default_bin` field per project) so projects with multiple binaries can declare which one brokkr should run by default.

### `Worktree` has no `Drop` impl

If the process panics or is killed (SIGKILL/SIGTERM) inside a `--commit` benchmark, the worktree at `.brokkr/worktree/<hash>` is left behind. Mitigated: `Worktree::create` cleans up stale worktrees at the same path before creating a new one. A `Drop` impl would require interior mutability or an `Option` wrapper — probably not worth the complexity.

### `--mem` systemd-run wrapping for `brokkr run`

The old `run_elivagar` had `--mem 8G` support via systemd-run. Could be promoted to a project-agnostic `brokkr run --mem 8G` flag in `src/cli.rs`.

### Project::Other memory leak
`config.rs`: `Project::Other(Box::leak(...))` leaks memory. Called once at startup so technically fine, but would leak in a loop (tests). The `Copy` derive on `Project` forces the leak.

### hostname() called multiple times per run
`config::hostname()` calls `libc::gethostname()` via FFI every time. Cheap but could be called once during config loading and stored on `DevConfig`.

---

## History command enhancements

### Capture brokkr's own output
All brokkr output goes through `output::*` helpers (`build_msg`, `bench_msg`, `error`, etc.). Add a tee layer that copies prefixed lines into a global buffer, flushed to a nullable `output TEXT` column at end of invocation. Cap at 64KB. Does NOT cover passthrough subprocess output (`brokkr run`, `brokkr serve`) which uses `Stdio::inherit()` — capturing that would require piped tee threads and changes live output UX. Schema v2 migration alongside `error_tail`.

### Capture stderr tail on failure
On non-zero exit, capture the last 4KB of stderr into a nullable `error_tail TEXT` column. Requires schema v2 migration. Only stored on failure — success path stays lightweight. The `history <id>` detail view would display it, and `brokkr history --failed` could show a one-line preview.

### `--json` output
Useful for scripting (jq, dashboards, CI analysis) instead of only human-formatted lines.

### `history <id>` detail view
One command to inspect full metadata for a specific entry (cwd, commit, dirty, kernel, memory, exit status).

### `--from-last-success` / `--failed` + `--rerun <id>`
Fast recovery loop: find last failed command and re-execute it exactly.

### `--project-dir <path-substring>` filter
`--project` is great, but directory filter helps when you have multiple clones/worktrees of the same project.

### `--until <date>` in addition to `--since`
Time-range queries are much more useful than one-sided filtering.

### `--status <code>` filter
`--failed` is coarse; filtering specific exit codes (e.g. 130 for interrupt) is valuable.

### `--sort slow|recent` and `--top-slowest N`
Makes performance triage easier without external tooling.

---

## CLI remaining issues

### Suite without --bench stores results in DB
`brokkr suite pbfhogg` (no `--bench`) calls `bench_all` which stores results. May not be worth fixing — suite is inherently a benchmarking operation.

### Suite builds without host features
`bench_all.rs` calls `cargo_build` without host features from `brokkr.toml`. Individual commands correctly include them via `BenchContext::new`. Pre-existing.

### ~~Standalone extract commands use hardcoded Copenhagen bbox~~ DONE
Standalone extract variants now use dataset bbox via `ctx.bbox`, same as `Extract { strategy }`.

### validate_since tautology
`cli.rs`: `s[..10].len() == 10` is always true when `s.len() == 19`. The recursive `validate_since(&s[..10])` call works but is unnecessarily clever. Dead code in the check.

### check does not really forward args raw to cargo test
`brokkr check` help says extra args are forwarded raw to `cargo test`, but every invocation runs clippy first. That means `brokkr check -- --help` and single-test workflows are blocked by clippy failures and do not behave like a clean cargo-test passthrough. The help text should be tightened or the command split.

---

## CLI flattening follow-ups

### `test` and `list` are generic top-level names
These only work for litehtml/sluggrs but are natural names users might try in any project. `brokkr test` from pbfhogg gives a project-gating error, which could confuse users expecting it to run `cargo test` (that's `brokkr check`). No immediate fix needed — the error message now includes the current project and is clear.

### Help output lacks section headers
With 55+ top-level commands, `--help` is a wall of text. `display_order` groups by project but there are no visual separators. Clap's `next_help_heading` could inject section headers like "Visual Testing Commands:", "Litehtml Commands:", etc.

---

## CLI sync backlog

### pbfhogg: new flags on existing commands

New flags that could warrant additional `bench commands` variants or verify coverage:

- `tags-filter`: `-i/--invert-match`, `-e/--expressions`, `-t/--remove-tags`
- `getid`: `-I/--id-osm-file`, `--remove-tags`, `--verbose-ids`
- `diff`: `--summary`, `--quiet`, `--output`
- `inspect`: `-e/--extended`, `-g/--get`, `--json`
- `extract`: `--config` (multi-extract from JSON), `--clean`, `--set-bounds`
- `cat`: `--clean`
- `check-refs`: `--show-ids`
- `derive-changes`: `--update-timestamp`, `--increment-version`
- `tags-count`: `-M`, `-s`

### elivagar: missing `verify` integration

Elivagar now has a `verify` subcommand for PMTiles output validation. Not wired into brokkr — should be added as a verify command.

### elivagar: new `run` flags not exposed in benchmarks

The following elivagar flags are not forwarded through `bench self`, `hotpath`, or `profile`:

- `--tile-format` (mvt/mlt) — MLT is a new tile format, benchmarking it matters
- `--tile-compression` (gzip/brotli) — compression strategy affects perf
- `--compress-sort-chunks` — LZ4 compression of sort spill data
- `--in-memory` — keep tile blob in RAM
- `--locations-on-ways` — PBF format flag
- Memory budgets (`--sort-budget`, `--way-budget`, `--rel-budget`, `--assemble-budget`) — tuning knobs, lower priority

No schema changes needed: `bench_self.rs` already stores flags as `meta.*` kv pairs in `run_kv` and the full command line in `cli_args`. New flags just need CLI plumbing + `KvPair::text("meta.<flag>", ...)` entries in the metadata vec.
