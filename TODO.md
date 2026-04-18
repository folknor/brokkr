# TODO

## Structural debt (last audited 2026-04-17)

The high-impact items have been worked off. What's listed below is
either speculative, subjective, or low-ROI and has been left
deliberately.

### Low ROI - not worth doing yet

#### `--clean` only emits `version` attr (brokkr cat)
`brokkr cat --clean` hardcodes the emitted attr as `version`. pbfhogg's
real flag accepts multiple attrs (`version|changeset|timestamp|uid|user`,
comma-separated or repeated). Combinations like `--clean version --clean
uid` aren't reachable from brokkr. Would need to change brokkr's
`clean: bool` into `clean: Option<Vec<String>>` to bench the cost of
stripping each attr independently.

#### `-b=<val>` vs `-b <val>` in extract suite rows (cosmetic)
After the build_args unification, suite presets `extract-simple`/`extract-complete`/
`extract-smart` emit `format!("-b={bbox}")` as a single token; pre-refactor
suite emitted `"-b"` then `"12.4,55.6,12.7,55.8"` as two tokens. Clap accepts
both, but the `cli_args` column text differs between old and new rows. A user
filtering with `--grep "-b 12.4"` would miss new rows (would need `--grep -b=12.4`
or just `--grep 12.4`). Not a behaviour bug, just a textual shift in stored argv.

#### `-R` flag position in tags-filter suite rows (cosmetic)
`tags-filter-way` / `tags-filter-amenity`: old suite emitted `tags-filter pbf -R
w/highway=primary -o out`; the enum emits `tags-filter -R pbf w/highway=primary
-o out`. Same semantics (clap is order-independent for boolean flags), but the
`cli_args` column text differs between old and new rows.

#### Hotpath scratch filenames renamed
Old code used ad-hoc `hotpath-merged`, `hotpath-altw`, `hotpath-extract-{simple,complete,smart}`.
The unified `build_args` normalises to `hotpath-{cmd.id()}-output.{ext}`, so
`hotpath-merged` becomes `hotpath-apply-changes-output`, etc. The cli_args
column reflects the new names; historical rows keep the old names.

#### Scratch output filename pattern coupled to command id
`bench-<id>-output.osm.pbf` / `hotpath-<id>-output.osm.pbf` in
`scratch_output_path`. Works today because benches run sequentially with
cleanup between; becomes a problem if two benches of the same command
want distinct outputs within one suite run.

#### `PbfhoggCommand::metadata` takes `&CommandContext`, others don't
Pbfhogg's metadata reads runtime observations from `ctx.params`; elivagar
and nidhogg don't have any. The signature difference reflects a real
semantic difference, not duplication.

#### Harness builder methods split across types
`BenchHarness::with_cargo_features` / `with_brokkr_args` / `with_measure_mode`
vs `BenchContext::with_request` (which wraps two of them). Different
abstraction levels - `with_request` is a convenience for `MeasureRequest`
callers; removing it would add boilerplate to every measured-command
dispatch.

#### `cmd.rs` layouts differ per project
pbfhogg: top-level cmd dispatch + per-bench helpers + verify dispatch.
elivagar: small per-subcommand fns. nidhogg: mixes both. Each reflects
that project's feature shape; no clean unification.

### Speculative - wait for the trigger

#### v12→v13 `DELETE FROM run_kv WHERE key IN (…)` list
30 hardcoded meta key names. The TODO suggested generating from a
code-level list, but historic migrations are frozen - changing code
would not affect v12 databases, so the list has to stay literal.
Future migrations can add their own DELETE statements as needed.

#### Migration tests copy `V3_SCHEMA` verbatim
If a schema change ever lands that modifies v3-era columns (extremely
unlikely - v3 is pre-brokkr-v1), the copy would need updating. Not
worth the indirection until that happens.

#### Cumulative migration tests force cascade updates
Each new migration test runs the full chain from `V3_SCHEMA`, so adding
a v16 forces edits to the `migrate_v3_to_v4` and `migrate_v11_to_v12`
test assertions. Per-migration tests that start at the precise prior
version would fix this but require redesigning `tests::test_db` to
accept a starting schema version.


## Punted

### `diff-snapshots` per-side variant selection (punted 2026-04-11)
`Command::DiffSnapshots` exposes a single `--variant` flag and resolves both
`--from` and `--to` with the same variant via `build_diff_snapshots_context`.
That rejects asymmetric snapshot pairs where one side has `pbf.indexed` but
the other only has `pbf.raw`.

**Why punted**: no concrete use case has surfaced. Both auto-population paths
(`brokkr download <region> --as-snapshot <key>` and `brokkr download <region>
--refresh`) generate `pbf.raw` *and* `pbf.indexed` automatically via
`pbfhogg cat`, so brokkr-managed snapshots are always symmetric. The
asymmetric case only arises with hand-edited brokkr.toml entries or
third-party archives. The TODO note was a code-reviewer observation, not a
user-need report.

**Original design decision (Q3 of the snapshot feature spec)**: explicitly
went with a single `--variant` flag (default `indexed`), with the reasoning
"YAGNI on the split flags - no known use case for asymmetric variants, and
the error-if-missing behavior on the receiving snapshot is the right default.
Add `--variant-from` / `--variant-to` later only when a concrete need shows
up." Today's review (2026-04-11) confirmed that decision still holds - the
pbfhogg dev's roadmap doesn't include any workflow that intentionally
produces asymmetric snapshots.

**What was done instead**: improved the error message that fires when the
asymmetric case is hit (commit 36cb9f3). The new error names the available
variants on the snapshot, suggests `--variant <X>` as a one-shot workaround,
and points at `brokkr download <region> --as-snapshot <key>` as the proper
fix (which auto-generates the missing variant). Closes the
first-time-user papercut without committing to any particular per-side flag
shape.

**Trigger condition for revisiting**: someone files a bug or feature request
with a concrete asymmetric use case. Examples that would qualify: regular
benchmarks against archive.org weekly dumps (raw-only) compared against
brokkr-managed snapshots (raw + indexed); a pbfhogg testing workflow that
deliberately wants to diff `--from raw --to indexed` for some reason; a
third party releasing snapshots in a non-standard variant set.

**If revisited**: the original feature request walkthrough lists the most
plausible CLI shape - `--variant-from` / `--variant-to` overriding `--variant`
on each side independently. But "wait for a real use case to inform the
shape" was the principled call; the use case might suggest a different shape
(e.g. `--variants from=X,to=Y`).

---

## Won't fix

### Inconsistent path-to-string conversion
All paths are constructed from known UTF-8 components, so `.display().to_string()` won't corrupt in practice. 100+ occurrences across 30+ files - not worth the churn.

### Hand-rolled UUID via `/dev/urandom`
10 correct lines in `src/db.rs`. Not worth adding the `uuid` crate as a dependency.

### `#[allow(clippy::too_many_arguments)]` proliferation
Functions genuinely need many parameters. `BenchContext` and `HarnessContext` cover the common cases; remaining allows are the pragmatic choice.

### `Project::Other` `Box::leak`
`config.rs`: `Project::Other(Box::leak(...))` leaks the project name
string once at startup. Removing the leak requires dropping the `Copy`
derive on `Project` (or switching to a `'static` interner), both of
which cascade through every `fn foo(project: Project)` signature for
no runtime benefit - `Project` is constructed exactly once per process.

### Suite without `--bench` stores results in DB
`brokkr suite pbfhogg` (no `--bench`) calls `bench_all` which stores
results. Suite is inherently a benchmarking operation - there's no
meaningful "measure without storing" mode to preserve.

### `test` and `list` are generic top-level names
These only work for litehtml/sluggrs but are natural names users might
try in any project. The project-gating error message now includes the
current project and the expected one, which is good enough; renaming
would churn every litehtml/sluggrs invocation in CI and docs.

---

## Backlog

### `--grep` is substring match, not real grep

`brokkr results --grep X` currently compiles to
`cli_args LIKE '%X%' OR brokkr_args LIKE '%X%'`. That's SQL substring
match - no regex, no word boundaries, no inversion, only `%` / `_` as
wildcards. The flag name `--grep` is aspirational.

Upgrade path: register a `REGEXP` scalar function on the rusqlite
connection (via `Connection::create_scalar_function` using the `regex`
crate) and switch the generated SQL to `REGEXP ?`. Users then get
`--grep "zstd:[1-3]"`, `--grep "direct-io.*uring"`, `--grep "^pbfhogg
apply-changes"`, etc. Also consider accepting `--grep` multiple times
(clap `Vec<String>`) with AND semantics so
`--grep apply-changes --grep zstd:1` works naturally.

Caveats: regex metachars (`.`, `*`, `+`, etc.) in user input become
significant - `--grep "version 1.0"` would match "version 120". Cache
the compiled regex to avoid per-row `Regex::new`. Adds a dep on the
`regex` crate (not currently in the tree).

### Counter diffs in `brokkr sidecar --compare`
Include counter values at matching phase boundaries in the comparison table. Currently `--compare` only shows duration, peak anon, and disk read.

### Captured env: filter flag short form
`brokkr results --env PBFHOGG_USE_NEW_PATH=1` works. Since the only
project-defined prefix in current use is `PBFHOGG_`, supporting a bare
`--env USE_NEW_PATH=1` that auto-resolves against the single common
prefix in `capture_env` would be nice. Low priority; the full name is
always accepted.

### Sidecar: result+sidecar persistence is not atomic

The benchmark result row is committed first, then sidecar rows are inserted in separate per-run transactions. If sidecar storage fails after the result is committed, the DB has a result with partial/no sidecar data. Not catastrophic (partial data is better than none), but could be wrapped in a single transaction.

### Make default binary configurable per-project in brokkr.toml

Currently `find_executable` infers the expected binary name from `BuildConfig.bin` or `BuildConfig.package`. This should be configurable in `brokkr.toml` (e.g. a `default_bin` field per project) so projects with multiple binaries can declare which one brokkr should run by default.

### `Worktree` has no `Drop` impl

If the process panics or is killed (SIGKILL/SIGTERM) inside a `--commit` benchmark, the worktree at `.brokkr/worktree/<hash>` is left behind. Mitigated: `Worktree::create` cleans up stale worktrees at the same path before creating a new one. A `Drop` impl would require interior mutability or an `Option` wrapper - probably not worth the complexity.

### `--mem` systemd-run wrapping

Pre-rewrite elivagar invocations had `--mem 8G` support via systemd-run
for per-run memory caps. Nothing equivalent survives after the
`Command::Passthrough` consolidation. Could be promoted to a
project-agnostic flag (e.g. on `ModeArgs` / `Passthrough`) so any
measured command can cap child RSS.

---

## History command enhancements

### Capture brokkr's own output
All brokkr output goes through `output::*` helpers (`build_msg`, `bench_msg`, `error`, etc.). Add a tee layer that copies prefixed lines into a global buffer, flushed to a nullable `output TEXT` column at end of invocation. Cap at 64KB. Does NOT cover passthrough subprocess output (`brokkr run`, `brokkr serve`) which uses `Stdio::inherit()` - capturing that would require piped tee threads and changes live output UX. Schema v2 migration alongside `error_tail`.

### Capture stderr tail on failure
On non-zero exit, capture the last 4KB of stderr into a nullable `error_tail TEXT` column. Requires schema v2 migration. Only stored on failure - success path stays lightweight. The `history <id>` detail view would display it, and `brokkr history --failed` could show a one-line preview.

### `--json` output
Useful for scripting (jq, dashboards, CI analysis) instead of only human-formatted lines.

### `--from-last-success` / `--failed` + `--rerun <id>`
Fast recovery loop: find last failed command and re-execute it exactly.

### `--sort slow|recent` and `--top-slowest N`
Makes performance triage easier without external tooling.

---

## CLI

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
- `extract`: `--clean`, `--set-bounds`
- `check-refs`: `--show-ids`
- `derive-changes`: `--update-timestamp`, `--increment-version`
- `tags-count`: `-M`, `-s`

### elivagar: memory-budget run flags not exposed

`--sort-budget`, `--way-budget`, `--rel-budget`, `--assemble-budget` -
tuning knobs that aren't forwarded through `bench self` / `hotpath` /
`alloc`. Lower priority than the structural flags which are already
wired (tile-format, tile-compression, compress-sort-chunks, in-memory,
locations-on-ways). No schema changes needed: `bench_self.rs` stores
flags as `meta.*` kv pairs in `run_kv` and the full command line in
`cli_args`. New flags just need CLI plumbing + `KvPair::text("meta.<flag>",
...)` entries in the metadata vec.
