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

## MEA-002 - `BROKKR_MARKER_FIFO` is spelled in four places and across a repo boundary

Reported by: measurement-storage.

`types_run` ×2, `format_sidecar`, `bench_gate`. It should be a const on `sidecar.rs`, which owns the protocol. The emitter side (pbfhogg `src/debug.rs`) is a necessary second copy across a repo boundary; only README "Sidecar conventions" keeps them in step.

Enforcement proposed: textlint on `"BROKKR_MARKER_FIFO"` outside its owner.

## MEA-004 - UUID generation still has inline copies

Reported by: measurement-storage, small-benches.

`db` exports `generate_uuid`, `short_uuid` and `RunId::short()`, and results, sidecar, litehtml and sluggrs use them. Residue: `harness_mod/types_run.rs` still generates dirty-run uuids inline from `/dev/urandom`, and `ratatoskr/gate_store.rs` has its own UUIDv4 generator. A textlint ban on `/dev/urandom` must exempt the lock-token code, which needs randomness that is not a uuid.

## MEA-005 - Small store helpers still copied

Reported by: measurement-storage, piners, ratatoskr.

Residue: `has_table` exists in five helpers and `has_column` in three plus one inline scan; `as_i64` is duplicated in piners; percentile interpolation is duplicated between `sidecar_fmt::print_field_stat` and the harness formatter. (The `pbfhogg` project default is now documented as a compatibility fallback; piners shares one clock conversion and keeps two timestamp formats on purpose; `prev.gap_seconds` compares whole seconds from both sources.)

## MEA-007 - Provenance-prefix classes outside the results DB

Reported by: measurement-storage.

`db::types::KvClass` now classifies kv keys once for the results DB (load, compare, both detail views, writes). Residue: `meta.`/`env.` prefixes are still formatted or stripped by hand in the ratatoskr gate paths (`ratatoskr/gate.rs`, `ratatoskr_sync/bench_gate.rs` or their successors), `config_parts/parser.rs` and the legacy migrations; `RunRow` still takes kv pairs as prefixed strings rather than the split `meta`/`env`/`predecessor`/`thread_summary`/`counters` the proposal named.

## MEA-009 - The sidecar-view hint is hand-written three times

Reported by: measurement-storage.

`results_cmd` ×2 and `single.rs`; each list differs and none mentions `--stalls`/`--compare`. Could be generated from clap.

## MEA-010 - Measurement tunables are scattered and not injectable

Reported by: measurement-storage.

`SAMPLE_INTERVAL_US` (sidecar.rs), `SIDECAR_BACKUP_COPIES` (format_sidecar.rs), the invalidate limit `100_000` (claims "don't silently truncate" and then does), the 500-char preview in `parse_kv_stderr`, `hotpath-report.json`; nothing lists the set. `run_sidecar` reaches the clock, `/proc` and signals directly; the `/proc` parsers are fused to `fs::read`, so tests can read only the test process's own `/proc`. An unknown `cargo_profile` is reported at every read (`map_stored_row`) rather than at write; the binary hash is taken at record time.

## MEA-013 - Store errors swallowed into defaults

Reported by: measurement-storage, ratatoskr.

`store_sidecar(...).ok()` on interrupt and failure paths; `query_meta` falls back to `(0,1)`, `query_run_info`/`has_data`/`resolve_latest` to defaults; `open_sidecar_db(...).ok()` in `results_cmd` and `sidecar_cmd`, so a corrupt or locked DB reports "no sidecar.db found"; the v3→v4 sidecar migration ignores all `ALTER` errors, not just duplicate-column; `previous_run_kv` drops lookup errors by design and reports nothing. ratatoskr's gate: `serde_json::to_string(..).unwrap_or_else(|_| "{}")` then parsed back with `unwrap_or_default()` in `evaluate_against_baseline`; `missing_baseline_error` queries use `unwrap_or_default()`/`unwrap_or(0)`.

Design hunt (other-projects): `bench_gate.rs`'s round trip means a serialization failure becomes an empty map and the run then fails with "missing `sidecar.x`", blaming the data rather than the encoder, though `GateRun::from_parts` exists to skip the round trip. Proposed: keep the `Map`s in the in-memory row and serialize only at the DB edge.

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
