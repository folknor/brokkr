# Hygiene - measurement and storage

Hygiene findings from the hunt for the shared benchmark harness (`measure.rs`, `harness_mod/`), the sidecar profiler, `results.db`/`sidecar.db`/`gate.db`, `results`/`sidecar`/`invalidate`, `git.rs` and `artefacts.rs`, plus the per-project copies of the measurement loop. Siblings: `hygiene-validation.md` (VAL), `hygiene-platform.md` (PLT), `hygiene-projects.md` (PRJ). Entries record what hunters reported; nothing has been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## MEA-001 - The measurement loop exists several times and the copies differ

Reported by: measurement-storage, ratatoskr, elivagar.

`run_external_inner`, `run_external_with_kv_raw`, ratatoskr `bench_gate.rs`, plus `run_hotpath` and `run_hotpath_capture`. Tie-breaking, failure handling, sidecar storage and `ok_codes` differ (the kv path drops `ok_codes`). `run_hotpath_capture` is a free function, so nine call sites re-pass the lock (always `Some`), `stop_marker` and `scratch_dir`, which the harness already holds. Best-of-N can compare a marker-span iteration against a wall-clock one, and `meta.timing_source` records only the winner's source (ratatoskr). elivagar's `bench_self`/`bench_pmtiles`/`bench_node_store` are a second implementation of their commands (PRJ-019). Consequences: BUG-046, BUG-047.

Hunter's recommendation (measurement-storage): rewrite as a single `Measurement` that owns the loop, FIFO, lock/child pid, typed `MeasureMode`, `NonZero` runs, the clock and the sidecar lifecycle; each path plugs in only its per-iteration timing strategy.

Design hunt (measurement): the per-loop decisions are failure classification, best-of-N (`pick_best` + `bench_ordering_key` in three loops, a hand-written `us < best` in `run_external_inner`, kept "consistent" by comments), `best_run_idx` for the sidecar, the `measure_start_epoch` stamp (two loops call `mark_measure_start`, three set it inline, the foreign loop calls a hook), where a failed iteration's sidecar goes (`store_sidecar(None, ..).ok()` six times), and how row and sidecar are committed (three record-then-store blocks with the same "row is lost; trajectory need not be" fallback, plus a deferred `PendingSidecar`/`record_with_pending`/`commit_sidecar` protocol a caller must follow by hand). `run_hotpath` parks failed captures in a thread-local (`FAILED_CAPTURE`) because the closure contract cannot return them, while the external loops have the data directly. Consistency between loops is pinned per loop by tests in `harness_mod/tests.rs`, which is `include!`d into the same namespace so tests reach private helpers. Proposed: one `MeasurementLoop` parameterised by `WallSource { External, SelfReported, Closure, MarkerSpan }` and sidecar on/off, returning `Measurement { iterations: Vec<Iteration { wall, ending, sidecar, stderr }>, winner }`; recording is a separate `harness.record(measurement, post)` that always does row-then-sidecar with the dirty fallback, so the deferred-commit protocol and the thread-local parking slot disappear (a failed iteration is a value in the list) and ratatoskr plugs in as `MarkerSpan` instead of borrowing private hooks; one loop test suite replaces the per-loop pins.

## MEA-002 - `BROKKR_MARKER_FIFO` is spelled in four places and across a repo boundary

Reported by: measurement-storage.

`types_run` ×2, `format_sidecar`, `bench_gate`. It should be a const on `sidecar.rs`, which owns the protocol. The emitter side (pbfhogg `src/debug.rs`) is a necessary second copy across a repo boundary; only README "Sidecar conventions" keeps them in step.

Enforcement proposed: textlint on `"BROKKR_MARKER_FIFO"` outside its owner.

## MEA-003 - Mode and run count have several owners

Reported by: measurement-storage, config-cli-bootstrap.

Mode strings `"bench"`/`"hotpath"`/`"alloc"` live in `MeasureRequest::mode_label`, bench_gate `Some("bench")`, and pbfhogg's `if alloc {"alloc"} else {"hotpath"}`, reaching the harness as `Option<String>`. `BenchConfig.runs` duplicates `MeasureRequest::runs()` and every writer copies it by hand. "At least one run": see PLT-026.

Enforcement proposed: the harness takes `MeasureMode`; `NonZeroUsize` in `MeasureMode` and `BenchConfig.runs`.

Design hunt (measurement, other-projects): after the request the mode becomes `Option<String>` (`with_measure_mode(req.mode_label())`), stored on `BenchHarness.measure_mode`, cloned into `RunRow.mode` and `RunInfo.mode`; `BenchConfig.mode` is a dead fallback (`effective_mode` = harness or config; every writer sets `None`; its doc admits it caused two bugs); the DB tests insert `"mmap"`, `"zlib"`, `"release"` into the column; a `HarnessContext` built without `with_request` silently writes `mode = NULL`. `hotpath_feature(alloc: bool)`, `hotpath_alloc_note(alloc: bool)`, `MeasureRequest::is_alloc()` and `hotpath_features()` are called by pbfhogg, elivagar, nidhogg, dellingr, mogwai, sluggrs, piners and main_parts, each computing `alloc` itself, and several use `hotpath_feature(alloc)` as a label, conflating feature name and recorded mode. Proposed: `enum RecordedMode { Bench, Hotpath, Alloc }` via `MeasureMode::recorded()`, a non-optional harness field, `NonZeroUsize` runs, and a `ProfiledMode { Hotpath, Alloc }` with `.feature()`, `.label()`, `.note()` owned by `measure.rs`; delete `BenchConfig.mode` and `BenchConfig.brokkr_args`.

## MEA-004 - UUID generation and short-UUID slicing are duplicated

Reported by: measurement-storage, small-benches.

UUIDv4: `db::types::generate_uuid`, an inline `/dev/urandom` block in `store_sidecar` (`harness_mod/types_run.rs`), `litehtml/mod.rs::generate_run_id`, `sluggrs/mod.rs::generate_run_id`. `[..8.min(len)]` short-UUID slicing is spelled 6+ times (three in litehtml/sluggrs) despite `types::short_uuid`.

Enforcement proposed: textlint banning `"/dev/urandom"` outside `db/types.rs` and `hold.rs`.

Design hunt (measurement): run ids, sidecar `dirty` aliases and uuid prefixes all travel as `&str`. Proposed: a `RunId` minted once with `.short()`, and `RunRef { Prefix | Alias }` resolved once per command (the resolution question is filed in `consolidations.md`).

## MEA-005 - Small store helpers and clocks copied

Reported by: measurement-storage, piners, ratatoskr.

`percentile` copied into `sidecar_fmt::print_field_stat` ("same as harness::percentile", unheld); legacy project default `'pbfhogg'` twice (schema `DEFAULT`, `map_stored_row`); `has_table` in `migrate.rs`, `gate.rs` and three more across the repo (piners), `has_column` twice (including twice inside `run_sidecar_migrations`), `as_i64` twice; wall-clock-to-epoch three ways (`wall_clock_epoch`, `gate::unix_now`, SQLite `datetime('now')`), and `prev.gap_seconds` subtracts a Rust clock from an SQLite timestamp; the piners stores use `datetime('now')` and the lint store a hand-rolled `now_rfc3339`.

Design hunt (measurement): `db/format/details.rs` hides `project` when it equals the literal `"pbfhogg"` - a display default tied to one project in the shared formatter.

## MEA-006 - Each sidecar sample field is spelled in about seven places

Reported by: measurement-storage.

The `Sample` struct, both DDLs (`sidecar.rs` and the dead migrate v7→v8), `store_run`, `query_samples` twice, `sample_field_value`, the full-JSON format string, the `measure.md` table. The full JSON omits `i`; the `measure.md` table says `vsize` is bytes while the code stores kB.

Enforcement proposed: one field table, macro-generated.

## MEA-007 - Provenance-prefix lists disagree

Reported by: measurement-storage.

compare `{meta, env, prev}`, single/details `{meta, threads}`, `load_children` `{env}`. `threads.*` show up as `counters:` noise on hotpath pairs; compare's `env.` exclusion is dead because env was already stripped.

Design hunt (measurement): `KvPair.key: String` carries five classes as prefixes - `meta.*`, `env.*`, `prev.*`, `threads.*`, bare runtime counters - plus `peak_rss_kb`, which `build_row` promotes to a column and removes; `db::write::insert_hotpath` writes `threads.*` `OR IGNORE` so stderr wins; `ratatoskr/gate.rs`, `ratatoskr_sync/bench_gate.rs`, `db/migrate.rs` and `config_parts/parser.rs` each format or strip `meta.`/`env.` themselves. So "counter" means different things to compare (counts `threads.*`) and single/details (counts `prev.*`). Proposed: a `KvClass` classifier in `db/types.rs` used once at load and once at write, with `StoredRow` carrying `meta`, `env` (already done as `captured_env`), `predecessor: Option<PreviousRun>`, `thread_summary` and `counters`, `RunRow` taking the same split, and prefixing happening only in `db::write`.

## MEA-008 - Invocation argv is stored as a string and re-parsed with different rules

Reported by: measurement-storage, config-cli-bootstrap.

`normalize_brokkr_args` and `format_brokkr_args_summary` re-parse tokens with different rules from a string `format_cli_args` cannot round-trip (it quotes only spaces). History stores argv joined unquoted; `capture_brokkr_args` shell-quotes. Pairing failures: BUG-049.

Enforcement proposed: store argv as a JSON array.

Design hunt (measurement): `db/format/compare.rs::normalize_brokkr_args` is a second CLI grammar, with hand-kept knowledge of `--commit`, `--verbose`/`-v`, `--bench/--hotpath/--alloc [N]` and `=N`, which must agree with clap and nothing checks; a value that happens to be `--commit` eats the next token. Proposed root fix: record the arm structurally at write time - the harness knows the measure mode, the commit and the runs count - as an `arm_key` (normalized argv minus provenance flags) computed from the parsed `Cli`, not from the joined string, and pair on it.

## MEA-009 - The sidecar-view hint is hand-written three times

Reported by: measurement-storage.

`results_cmd` ×2 and `single.rs`; each list differs and none mentions `--stalls`/`--compare`. Could be generated from clap.

## MEA-010 - Measurement tunables are scattered and not injectable

Reported by: measurement-storage.

`SAMPLE_INTERVAL_US` (sidecar.rs), `SIDECAR_BACKUP_COPIES` (format_sidecar.rs), the invalidate limit `100_000` (claims "don't silently truncate" and then does), the 500-char preview in `parse_kv_stderr`, `hotpath-report.json`; nothing lists the set. `run_sidecar` reaches the clock, `/proc` and signals directly; the `/proc` parsers are fused to `fs::read`, so tests can read only the test process's own `/proc`. An unknown `cargo_profile` is reported at every read (`map_stored_row`) rather than at write; the binary hash is taken at record time.

## MEA-011 - Every store open migrates and writes

Reported by: measurement-storage, piners, ratatoskr.

Each `ResultsDb::open`/`SidecarDb::open` runs migrations, `CREATE`, and an unconditional `user_version = N`, so read-only commands (`results`, `sidecar`, `invalidate`) migrate and write without the lock, and a DB from a newer brokkr is silently downgraded. Same for `gate.db`. piners' stores open `READ_ONLY` for queries (a better policy already in the tree, though with its own problem: BUG-093). No store sets `busy_timeout`, relying on rusqlite's implicit default unstated. `results.db` version writers: BUG-101.

Design hunt (measurement): each store does its own open - `ResultsDb::open`, `SidecarDb::open`, `GateDb::open`, `HistoryDb::open_typed`, `litehtml/db.rs`, `sluggrs/db.rs`, `piners/lint/db.rs`, `piners/corpus_db` - each with its own WAL pragma, migration runner and `user_version` stamp; only history sets a `busy_timeout`. `results.db` has three schema owners (`ResultsDb`, litehtml `MechanicalDb`, sluggrs) sharing one `user_version`, and the migrate tests carry regression cases for litehtml having stamped it. Proposed: a `Store` trait or helper owning open, pragmas, busy timeout and versioned migrations, one `user_version` owner per file, and litehtml and sluggrs in their own files rather than guests in `results.db`.

## MEA-012 - Harness output channels

Reported by: measurement-storage.

See PLT-004 for `[result]`'s two owners, `print_run_info`'s stderr writes and the stdout/stderr split of "stored in …" lines. Also silent: FIFO lines dropped because the timestamp or counter won't parse (counts never reported); hotpath JSON failures still record a "hotpath" row with no profile (`run_hotpath_capture` logs and continues). Marker JSON escaping: BUG-056.

## MEA-013 - Store errors swallowed into defaults

Reported by: measurement-storage, ratatoskr.

`store_sidecar(...).ok()` on interrupt and failure paths; `query_meta` falls back to `(0,1)`, `query_run_info`/`has_data`/`resolve_latest` to defaults; `open_sidecar_db(...).ok()` in `results_cmd` and `sidecar_cmd`, so a corrupt or locked DB reports "no sidecar.db found"; the v3→v4 sidecar migration ignores all `ALTER` errors, not just duplicate-column; `previous_run_kv` drops lookup errors by design and reports nothing. ratatoskr's gate: `serde_json::to_string(..).unwrap_or_else(|_| "{}")` then parsed back with `unwrap_or_default()` in `evaluate_against_baseline`; `missing_baseline_error` queries use `unwrap_or_default()`/`unwrap_or(0)`.

Design hunt (other-projects): `bench_gate.rs`'s round trip means a serialization failure becomes an empty map and the run then fails with "missing `sidecar.x`", blaming the data rather than the encoder, though `GateRun::from_parts` exists to skip the round trip. Proposed: keep the `Map`s in the in-memory row and serialize only at the DB edge.

## MEA-014 - Row provenance precedence is decided per writer

Reported by: measurement-storage.

`cargo_profile` is spelled at each writer while `cargo_features` has a harness default. Precedence disagrees: features config-over-harness; mode/brokkr_args harness-over-config; the `with_brokkr_args` doc claims config wins. See BUG-041, BUG-047.

Design hunt (measurement): `BuildConfig.profile: &'static str` (`"release"`/`"dev"`) while `CargoProfile` exists; the `CargoProfile::Dev` doc records the drift this already caused ("older dev rows in those series carry the wrong profile"). Features likewise: `BenchContext::with_build_config` joins `build_config.features` into the harness default, `BenchConfig.cargo_features` can override it, and `HarnessContext` (no build) leaves it unset. Proposed: `cargo_build` returns `BuiltArtifact { path, profile: CargoProfile, features: FeatureSet, target: TargetKind }`, the harness takes the artifact (or an explicit `ExternalBuild::{Java, CMake}`) and stamps the row from it, and writers stop naming profiles.

## MEA-015 - Store mechanics invented per site

Reported by: measurement-storage.

Inserts now use a real transaction and the compare pair key is a typed `PairKey`; both ms conversions round to nearest. Residue:
- `sidecar::run_sidecar_with_deadline` computes `SidecarSummary.wall_time_ms` with `as_millis()` (floor) while the results row rounds, so one run's two stored walls can differ by a millisecond.
- Wall time is still a pair of integers held in step by comments (`elapsed_ms`/`elapsed_us` on `BenchResult`, `RunRow`, `StoredRow`; `Distribution` with its optional `us`; parallel `iterations`/`iterations_us` vectors). Proposed: `enum Wall { Micros(i64), LegacyMillis(i64) }` with a derived nearest `.ms()` and `.ordering_key()`, iterations as one `Vec<Wall>`, the DB splitting the two columns at its boundary.

## MEA-016 - Measurement resources grow without bound

Reported by: measurement-storage, ratatoskr.

`sidecar.db` has no retention and every dirty or failed run adds a UUID; each stored run makes a full-DB backup (three generations kept); captured child stdout/stderr and FIFO markers are buffered in memory with no cap (ratatoskr's bench path too).

## MEA-017 - Measurement tests that prove nothing

Reported by: measurement-storage.

- `env_fingerprint_disambiguates_comma_in_value` cannot fail on the bug it names (with the old `,` joiner the fingerprints still differ because of the trailing `=` from `("narenas:1","")`); the doc comment's own `MALLOC_CONF=a,b=1` example would have caught it.
- `parse_kv_stderr_elapsed_ms_takes_precedence_over_total_ms` is named for a rule its second assertion disproves (last wins); `output-channels.md` states the false rule ("if both appear, `elapsed_ms` wins").
- `read_proc_io_tolerates_extra_lines` tests nothing about extra lines.
- `drop_without_finalize_preserves` cannot fail: `ArtefactDir::drop` is empty and `armed` is never read.
- The gate tests' `open_mem` is a hand copy of the DDL that skips `GateDb::open`, so migrations, open and WAL setup are untested (also reported by ratatoskr).

## MEA-018 - Stale claims in measurement docs and code

Reported by: measurement-storage, pbfhogg-nidhogg, elivagar, ratatoskr.

- `results.db` is described as both gitignored and git-tracked in `measure.md`; `output-channels.md`, the `db/sidecar.rs` header and the `gate.rs` header say committed/tracked. Nothing checks. `gate.db` is described as committed but uses WAL, so completeness depends on a clean close (ratatoskr).
- `measure.md`: `run_internal` "(N runs, min/avg/max)" (best-of-N); "Sidecar data is stored even when the child fails" (false for hotpath/alloc and sync); "iterations collected by every loop" (false for sync); `vsize` bytes (kB).
- `output-channels.md` contradicts itself: the harness table says `run_external_ok` scrapes stderr kv; the pbfhogg/elivagar tables and decision guide say "stderr kv: no"/"captured and discarded"; nidhogg `tiles` is a stderr-kv path (`parse_kv_lines`) absent from the doc; example benches' "stderr … discarded" is false on elivagar's `bench all` path.
- `artefacts.rs` header: N is "the smallest positive integer such that run-N does not exist" (code uses highest+1; `allocate`'s doc says gaps aren't filled); the parent-path paragraph is written twice.
- `sidecar.rs` header: "bulk-inserts everything to SQLite" (the harness does) and "Zero I/O during the benchmark" (writes `.sidecar-status` every drain).
- `previous_run` "what last touched this machine" (only this project's stored rows; dirty runs and other projects invisible).
- Stale "variant" wording in `types_run` field docs, both `with_request` docs ("`variant` columns"), the `StoredRow` doc, and pbfhogg's dispatch comment.
- `RowData.counters` describes the removed identity-counters feature; `request.rs` `ResultsQuery.grep`/`grep_v` say two sources (three); migrate v7→v8 "Fresh databases already have these tables" (false); `exit_code_from_status`'s doc sits on `clamp_u32`; `parse_kv_lines` has two stacked summaries.
- `hotpath.md`/dellingr dispatch premise that rows carry `n/a`: PLT-027. `dellingr.md` says the marker FIFO lives in `.brokkr/dellingr/`; for `--bench`, `run_external_inner` creates it in `db_dir` (small-benches). `dellingr.md` gives a three-part pair key; `db/format/compare.rs::pair_key` has five (`hotpath.md` is correct).

## MEA-020 - Dead fields, columns and migrations in the stores

Reported by: measurement-storage.

- `BenchConfig.mode`/`brokkr_args` never `Some` (BUG-041); `ArtefactDir`'s `armed` field and empty `Drop`; `run_hotpath_capture`'s `Option<&LockGuard>` always `Some`.
- Fresh-schema columns `runs.extra` and `runs.metadata`, read only by the v2→v3 migration.
- Write-only: `sidecar_summary.{vm_hwm_kb, sample_count, marker_count, wall_time_ms}`; `print_run_info` recomputes wall time from samples at one-second resolution instead.
- Migration v7→v8 creates tables v8→v9 drops.
- The whole v0→v18 migration chain (~2,000 lines incl. tests) is dead only if no `results.db` below v18 still exists; the hunter could not verify that from this repo (checking `user_version` on every host's databases would settle it).
- `#[allow(dead_code)]` on `StoredRow` and `GateEntry` hides which fields are unused.

Design hunt (other-projects): the artefact retention decision is split - `keep_on_success` is fixed at allocation, ratatoskr's `PreserveDataDir::OnSuccessToo` frontmatter is folded into that bool by each caller, and piners decides "keep despite success because diagnosis ran" by calling `finalize_failure` on a run that did not fail. Proposed: `Retention { OnFailure, Always }` plus `finalize(Outcome::{Success, Failure, KeepEvidence})`, and either removing `armed` or making `Drop` do something observable (log that the dir was kept unfinalized).
