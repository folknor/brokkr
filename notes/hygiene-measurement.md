# Hygiene - measurement and storage

Hygiene findings from the hunt for the shared benchmark harness (`measure.rs`, `harness_mod/`), the sidecar profiler, `results.db`/`sidecar.db`/`gate.db`, `results`/`sidecar`/`invalidate`, `git.rs` and `artefacts.rs`, plus the per-project copies of the measurement loop. Siblings: `hygiene-validation.md` (VAL), `hygiene-platform.md` (PLT), `hygiene-projects.md` (PRJ), `bugs.md` (BUG). Entries record what hunters reported; nothing has been verified.

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

## MEA-002 - `BROKKR_MARKER_FIFO` is spelled in four places and across a repo boundary

Reported by: measurement-storage.

`types_run` ×2, `format_sidecar`, `bench_gate`. It should be a const on `sidecar.rs`, which owns the protocol. The emitter side (pbfhogg `src/debug.rs`) is a necessary second copy across a repo boundary; only README "Sidecar conventions" keeps them in step.

Enforcement proposed: textlint on `"BROKKR_MARKER_FIFO"` outside its owner.

## MEA-003 - Mode and run count have several owners

Reported by: measurement-storage, config-cli-bootstrap.

Mode strings `"bench"`/`"hotpath"`/`"alloc"` live in `MeasureRequest::mode_label`, bench_gate `Some("bench")`, and pbfhogg's `if alloc {"alloc"} else {"hotpath"}`, reaching the harness as `Option<String>`. `BenchConfig.runs` duplicates `MeasureRequest::runs()` and every writer copies it by hand. "At least one run": see PLT-026.

Enforcement proposed: the harness takes `MeasureMode`; `NonZeroUsize` in `MeasureMode` and `BenchConfig.runs`.

## MEA-004 - UUID generation and short-UUID slicing are duplicated

Reported by: measurement-storage, small-benches.

UUIDv4: `db::types::generate_uuid`, an inline `/dev/urandom` block in `store_sidecar` (`harness_mod/types_run.rs`), `litehtml/mod.rs::generate_run_id`, `sluggrs/mod.rs::generate_run_id`. `[..8.min(len)]` short-UUID slicing is spelled 6+ times (three in litehtml/sluggrs) despite `types::short_uuid`.

Enforcement proposed: textlint banning `"/dev/urandom"` outside `db/types.rs` and `hold.rs`.

## MEA-005 - Small store helpers and clocks copied

Reported by: measurement-storage, piners, ratatoskr.

`percentile` copied into `sidecar_fmt::print_field_stat` ("same as harness::percentile", unheld); legacy project default `'pbfhogg'` twice (schema `DEFAULT`, `map_stored_row`); `has_table` in `migrate.rs`, `gate.rs` and three more across the repo (piners), `has_column` twice (including twice inside `run_sidecar_migrations`), `as_i64` twice; wall-clock-to-epoch three ways (`wall_clock_epoch`, `gate::unix_now`, SQLite `datetime('now')`), and `prev.gap_seconds` subtracts a Rust clock from an SQLite timestamp; the piners stores use `datetime('now')` and the lint store a hand-rolled `now_rfc3339`.

## MEA-006 - Each sidecar sample field is spelled in about seven places

Reported by: measurement-storage.

The `Sample` struct, both DDLs (`sidecar.rs` and the dead migrate v7→v8), `store_run`, `query_samples` twice, `sample_field_value`, the full-JSON format string, the `measure.md` table. The full JSON omits `i`; the `measure.md` table says `vsize` is bytes while the code stores kB.

Enforcement proposed: one field table, macro-generated.

## MEA-007 - Provenance-prefix lists disagree

Reported by: measurement-storage.

compare `{meta, env, prev}`, single/details `{meta, threads}`, `load_children` `{env}`. `threads.*` show up as `counters:` noise on hotpath pairs; compare's `env.` exclusion is dead because env was already stripped.

## MEA-008 - Invocation argv is stored as a string and re-parsed with different rules

Reported by: measurement-storage, config-cli-bootstrap.

`normalize_brokkr_args` and `format_brokkr_args_summary` re-parse tokens with different rules from a string `format_cli_args` cannot round-trip (it quotes only spaces). History stores argv joined unquoted; `capture_brokkr_args` shell-quotes. Pairing failures: BUG-049.

Enforcement proposed: store argv as a JSON array.

## MEA-009 - The sidecar-view hint is hand-written three times

Reported by: measurement-storage.

`results_cmd` ×2 and `single.rs`; each list differs and none mentions `--stalls`/`--compare`. Could be generated from clap.

## MEA-010 - Measurement tunables are scattered and not injectable

Reported by: measurement-storage.

`SAMPLE_INTERVAL_US` (sidecar.rs), `SIDECAR_BACKUP_COPIES` (format_sidecar.rs), the invalidate limit `100_000` (claims "don't silently truncate" and then does), the 500-char preview in `parse_kv_stderr`, `hotpath-report.json`; nothing lists the set. `run_sidecar` reaches the clock, `/proc` and signals directly; the `/proc` parsers are fused to `fs::read`, so tests can read only the test process's own `/proc`. An unknown `cargo_profile` is reported at every read (`map_stored_row`) rather than at write; the binary hash is taken at record time.

## MEA-011 - Every store open migrates and writes

Reported by: measurement-storage, piners, ratatoskr.

Each `ResultsDb::open`/`SidecarDb::open` runs migrations, `CREATE`, and an unconditional `user_version = N`, so read-only commands (`results`, `sidecar`, `invalidate`) migrate and write without the lock, and a DB from a newer brokkr is silently downgraded. Same for `gate.db`. piners' stores open `READ_ONLY` for queries (a better policy already in the tree, though with its own problem: BUG-093). No store sets `busy_timeout`, relying on rusqlite's implicit default unstated. `results.db` version writers: BUG-101.

## MEA-012 - Harness output channels

Reported by: measurement-storage.

See PLT-004 for `[result]`'s two owners, `print_run_info`'s stderr writes and the stdout/stderr split of "stored in …" lines. Also silent: FIFO lines dropped because the timestamp or counter won't parse (counts never reported); hotpath JSON failures still record a "hotpath" row with no profile (`run_hotpath_capture` logs and continues). Marker JSON escaping: BUG-056.

## MEA-013 - Store errors swallowed into defaults

Reported by: measurement-storage, ratatoskr.

`store_sidecar(...).ok()` on interrupt and failure paths; `query_meta` falls back to `(0,1)`, `query_run_info`/`has_data`/`resolve_latest` to defaults; `open_sidecar_db(...).ok()` in `results_cmd` and `sidecar_cmd`, so a corrupt or locked DB reports "no sidecar.db found"; the v3→v4 sidecar migration ignores all `ALTER` errors, not just duplicate-column; `previous_run_kv` drops lookup errors by design and reports nothing. ratatoskr's gate: `serde_json::to_string(..).unwrap_or_else(|_| "{}")` then parsed back with `unwrap_or_default()` in `evaluate_against_baseline`; `missing_baseline_error` queries use `unwrap_or_default()`/`unwrap_or(0)`.

## MEA-014 - Row provenance precedence is decided per writer

Reported by: measurement-storage.

`cargo_profile` is spelled at each writer while `cargo_features` has a harness default. Precedence disagrees: features config-over-harness; mode/brokkr_args harness-over-config; the `with_brokkr_args` doc claims config wins. See BUG-041, BUG-047.

## MEA-015 - Store mechanics invented per site

Reported by: measurement-storage.

`insert` uses hand-written `BEGIN`/`COMMIT` strings (a failed `COMMIT` leaves the transaction open) while `delete` uses `unchecked_transaction`. Two ms-rounding policies (floor in `elapsed_to_ms`, nearest in `us_to_ms`; BUG-048). The compare pair key is a tab-joined string split back apart; `pair_key_tabs_in_values_still_bleed` documents the resulting bug instead of fixing it - a tuple key would make it unrepresentable.

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

## MEA-019 - `ON DELETE CASCADE` with foreign keys never enabled

Reported by: measurement-storage.

`delete_by_uuid_prefix` keeps a hand-maintained list of child tables instead. Enforcement proposed: `PRAGMA foreign_keys=ON`, or a test enumerating every table with a `run_id` column.

## MEA-020 - Dead fields, columns and migrations in the stores

Reported by: measurement-storage.

- `BenchConfig.mode`/`brokkr_args` never `Some` (BUG-041); `ArtefactDir`'s `armed` field and empty `Drop`; `run_hotpath_capture`'s `Option<&LockGuard>` always `Some`.
- Fresh-schema columns `runs.extra` and `runs.metadata`, read only by the v2→v3 migration.
- Write-only: `sidecar_summary.{vm_hwm_kb, sample_count, marker_count, wall_time_ms}`; `print_run_info` recomputes wall time from samples at one-second resolution instead.
- Migration v7→v8 creates tables v8→v9 drops.
- The whole v0→v18 migration chain (~2,000 lines incl. tests) is dead only if no `results.db` below v18 still exists; the hunter could not verify that from this repo (checking `user_version` on every host's databases would settle it).
- `#[allow(dead_code)]` on `StoredRow` and `GateEntry` hides which fields are unused.

## MEA-021 - The gate store (`db/gate.rs`)

Reported by: ratatoskr, measurement-storage.

`run_migrations` is a no-op scaffold ("Future migrations go here"; schema still v1); `unix_now()` reads the clock directly; `gate_runs.script` stores the canonical absolute path while the doc says "absolute or repo-relative", so moving the checkout breaks every pinned baseline via the script-identity check; `value_kind` (`bench_gate.rs`) and `kind_of` (`gate.rs`) are identical. Prefix resolution: BUG-050. Open-path coverage: MEA-017.
