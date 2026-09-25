I found about 60 hygiene findings in this scope, and several are already broken today rather than risks. brokkr's `brokkr.toml` enforces very little: three textlint rules and rustdoc. There is no `[[dependency_rule]]`, `[header]` or `[lints]` section. Cargo.toml denies `unwrap_used` but not `expect_used`, `panic` or `indexing_slicing`. So almost none of the rules this scope relies on are held by anything mechanical.

I did not edit, build or test anything. The findings come from reading the code and grepping. Where I say "confirmed", I traced it in the code. Paths below are relative to `/home/folk/Programs/brokkr/`.

## Bugs found along the way (all confirmed)

1. **`BenchConfig.mode` and `BenchConfig.brokkr_args` are never set to anything but `None`** (checked all ~35 construction sites). Yet `format_result_line` and `build_run_info` read `config.mode`, not the harness's `measure_mode`. As a result:
   - the `[result]` line never prints `mode=`;
   - `sidecar_meta.mode` is NULL for every run, so `brokkr sidecar`'s "mode:" line never appears.

   (`src/harness_mod/types_run.rs`, `src/harness_mod/format_sidecar.rs`)
2. **`brokkr lock`'s "last marker" is blind for `--hotpath` and `--alloc` runs.** The writer puts `.sidecar-status` next to the FIFO: `SidecarFifo::status_path` uses `with_file_name`, and `run_hotpath_capture` gets `paths.scratch_dir` (default `data/scratch`). The reader looks at `project_root/.brokkr/.sidecar-status` (`src/main_parts/commands.rs`). The ratatoskr bench gate has the same problem. It fails silently.
3. **Comments claim the opposite of what happens when kv keys collide.** `build_row` orders kv as metadata, then env, then `prev.*`, then stderr counters. `insert_kv_row` uses `INSERT OR IGNORE`, so the first one wins. The comment "runtime counters win on the unlikely key collision" is false: metadata and env beat counters.
4. **A fresh `results.db` has a different schema from a migrated one.** `idx_runs_uuid` is only created in `migrate_uuid`. `run_migrations` returns early on a fresh database, and `SCHEMA` never creates the index.
5. **Recording failures lose the whole measurement.** `record_result` inserts first and prints `[result]` only afterwards. If the insert fails, the timing is never printed and `store_sidecar` never runs, so the `/proc` trajectory is gone too.
6. **`--hotpath`/`--alloc` never keep sidecar data on failure or `brokkr kill`.** `run_hotpath_capture` returns `Err` and drops the data it holds. `run_hotpath` discards the runs it already collected when `f(i)?` fails. `run_external_*` does keep it (under `dirty`). `measure.md` claims all modes keep it.
7. **`sync --bench` (bench_gate.rs) is a third, drifted copy of the measurement loop.**
   - It records `cargo_profile: Release` even when it built debug.
   - It stores `iterations: Vec::new()` for best-of-N, and the comment "Single measured run" is false.
   - It never sets `measure_start_epoch`, so `prev.gap_seconds` includes the run's own duration. That is exactly the bug `types_run.rs` documents fixing.
   - It never attaches captured env.
   - It passes `run_info: None`.
   - On failure it collects sidecar runs and then drops them.
8. **The external timing path throws away precision it already has.** `run_external_inner` has an exact `Duration` but floors it to ms (`elapsed_to_ms`, and a test asserts the truncation) and sets `elapsed_us: None`. Meanwhile `us_to_ms` says rounding toward "faster than it was" is "the one error worth avoiding". Best-of-N on this path compares truncated milliseconds.
9. **`--compare` fails to pair rows that measured the same thing.** `brokkr_args` includes argv[0] verbatim (`capture_brokkr_args`), and `normalize_brokkr_args` strips only `--commit` and `-v`. So `brokkr` vs `~/.cargo/bin/brokkr`, or `--bench 3` vs `--bench 5`, never pair.
10. **A pinned gate baseline can silently move.** `GateDb::lookup_baseline` resolves an ambiguous prefix with `ORDER BY created_at DESC LIMIT 1`. If a later row shares the pinned prefix, the newer row becomes the baseline.
11. **Ambiguous sidecar prefixes merge sessions.** `query_samples`, `query_markers` and `query_counters` use `LIKE prefix%` with no ambiguity check. `query_meta` and `query_run_info` pick an arbitrary row.
12. **`brokkr invalidate "" -f` deletes every row.** There is no value parser on the UUID argument. `_` is also a LIKE wildcard in every prefix match: uuid, commit, and `command`/`mode`/`dataset` in `build_query_sql` and `query_compare`. `--grep` switched to `instr` for exactly this reason; nothing else did.
13. **Integer metadata can never be filtered.** `--meta`/`--env` compare `value_text` only, so a `meta.*` value stored as Int is never matched and nothing says so. The `--grep` expression coalesces all three value types.
14. **An empty `XDG_DATA_HOME` puts backups inside the project.** `sidecar_backup_dir` treats `""` as set, so backups go to `./brokkr/sidecar-backups/` relative to the working directory. That untracked directory would then make the next run's tree dirty. The XDG spec says empty means unset.
15. **`--where` fails open.** A malformed condition is silently ignored and every sample is printed (`if let Ok(..) = parse_where_cond`). An unknown field name filters every sample out, and an unknown `--fields` entry is silently dropped.
16. **Commit identity width has no owner.** `git rev-parse --short` has a variable length. Passing a longer hash to `--commit` or `--compare` then matches nothing.

## 1. One value, one owner

- **`XDG_DATA_HOME`/`XDG_CONFIG_HOME` resolution is written four times**: `format_sidecar.rs::sidecar_backup_dir`, `history.rs::db_path`, `history_cmd.rs::record_history` (using `var_os`, which disagrees on non-UTF-8 values), and `config_parts/user.rs::user_config_path`. None treats empty as unset. *Enforceable:* one `dirs` module, plus a textlint rule banning `env::var("XDG_` outside it.
- **`.brokkr` is spelled about 20 times** across `src/`. `results.db` and `sidecar.db` have helpers in `resolve_parts`, but `BenchHarness::new_with_lock` and `store_sidecar` rebuild the paths themselves. `output-channels.md` says both "are resolved identically … by `BenchHarness::new_with_lock`", which is not true. *Enforceable:* a textlint on `join(".brokkr")` outside `resolve`.
- **`BROKKR_MARKER_FIFO` is spelled in 4 places** (types_run ×2, format_sidecar, bench_gate). It should be a const on `sidecar.rs`, which owns the protocol. The emitter side (pbfhogg `src/debug.rs`) has to be a second copy across a repo boundary. Nothing keeps the two in step except `README` "Sidecar conventions".
- **The mode strings `"bench"`/`"hotpath"`/`"alloc"`** live in `MeasureRequest::mode_label`, bench_gate `Some("bench")`, and pbfhogg's `if alloc {"alloc"} else {"hotpath"}`. They reach the harness as `Option<String>`. *Enforceable:* have the harness take `MeasureMode`.
- **"At least one run" is owned twice**: `resolve_mode` rejects 0, and the four harness loops each re-raise "benchmark requires at least 1 run". bench_gate's `--bench 0` gets a third message. *Enforceable:* `NonZeroUsize` in `MeasureMode` and `BenchConfig.runs`, plus a clap range parser.
- **`BenchConfig.runs` duplicates `MeasureRequest::runs()`.** Every writer copies it by hand.
- **UUID generation is duplicated**: `db::types::generate_uuid` and an inline `/dev/urandom` block in `store_sidecar`.
- **Short-UUID slicing `[..8.min(len)]` is spelled 6 or more times** despite `types::short_uuid`.
- **`percentile` is copied** into `sidecar_fmt::print_field_stat`, whose comment says "same as harness::percentile" with nothing to hold it.
- **The legacy project default `'pbfhogg'`** appears twice (schema `DEFAULT` and `map_stored_row`).
- **`has_table` is duplicated** (migrate.rs, gate.rs), and the has-column closure appears twice inside `run_sidecar_migrations`.
- **Wall-clock-to-epoch is done three ways**: `wall_clock_epoch`, `gate::unix_now`, and SQLite `datetime('now')`. `prev.gap_seconds` subtracts a Rust clock from an SQLite timestamp.
- **Each sample field is spelled in about 7 places**: the `Sample` struct, both DDLs (sidecar.rs and dead migrate v7→v8), `store_run`, `query_samples` twice, `sample_field_value`, the full-JSON format string, and the measure.md table. The full JSON omits `i`. *Enforceable:* one field table, macro-generated.
- **Provenance-prefix lists disagree**: compare `{meta, env, prev}`, single/details `{meta, threads}`, `load_children` `{env}`. As a result `threads.*` appear as `counters:` noise on hotpath pairs, and compare's `env.` exclusion is dead because env was already stripped.
- **`brokkr_args` tokens are re-parsed with different rules** in `normalize_brokkr_args` and `format_brokkr_args_summary`, from a string `format_cli_args` cannot round-trip (it only quotes spaces). Store argv as a JSON array instead.
- **The sidecar-view hint is hand-written three times** (results_cmd ×2, single.rs). Each list differs and none mentions `--stalls`/`--compare`. It could be generated from clap.

## 2. Values nobody can find, change or trust

- **The tunables are scattered and none is configurable or injectable**: `SAMPLE_INTERVAL_US` (sidecar.rs), `SIDECAR_BACKUP_COPIES` (format_sidecar.rs), the invalidate limit `100_000` (it claims "don't silently truncate" and then does), the preview length `500` in `parse_kv_stderr`, and `hotpath-report.json`. Nothing lists the set.
- **`run_sidecar` reaches the clock, `/proc` and signals directly.** The `/proc` parsers are fused to `fs::read`, so the tests can only read the test process's own `/proc`.
- **Configuration is validated at the moment of use**: an unknown `cargo_profile` is reported as an error at every read (`map_stored_row`). The binary hash is taken at record time.
- **Every open writes.** Each `ResultsDb::open` or `SidecarDb::open` runs migrations, `CREATE`, and an unconditional `user_version = N`. That means read-only commands (`results`, `sidecar`, `invalidate`) migrate and write without the lock. A database from a newer brokkr gets its version silently downgraded. The same applies to gate.db. piners' stores open `READ_ONLY` for queries, so a better policy already exists in the tree.

## 3. Output channel

- **The `[result]` prefix has two owners**: `emit_result_lines` goes through `output::result_msg` (renderer plus run log), while `force_emit_result_lines` uses raw `println!("[result]  …")`. `record_result` also does `println!("{short}")`.
- **`print_run_info` writes `eprintln!("[sidecar] …")` and `eprintln!("[error] …")` directly**, so the same `[error]` class lands on stderr here and on stdout through `output::error`.
- **`bench_msg` prints to stdout, bypassing the run log; `sidecar_msg` goes to stderr.** "stored in results.db" and "stored in sidecar.db" therefore land on different streams.
- **`artefacts::emit_clean_hint` hard-codes the `[ratatoskr]` prefix** in a module shared with piners.
- **`sidecar_marker_json` escapes JSON by hand** (only `\\`, `"` and `\n`). A marker name containing a tab or other control character, which comes from an untrusted FIFO, produces invalid JSONL. serde_json is already in use two functions away.
- **Some significant events are silent**: FIFO lines dropped because the timestamp or counter value won't parse (the counts are never reported), and hotpath JSON failures that still record a "hotpath" row with no profile (`run_hotpath_capture` logs and continues).

## 4. Errors

- **Swallowed errors**:
  - `store_sidecar(...).ok()` on the interrupt and failure paths;
  - `query_meta` falls back to `(0,1)`, `query_run_info`/`has_data`/`resolve_latest` fall back to defaults;
  - `open_sidecar_db(...).ok()` in both results_cmd and sidecar_cmd, so a corrupt or locked database reports "no sidecar.db found";
  - the v3→v4 sidecar migration ignores all `ALTER` errors, not just duplicate-column;
  - `git::check_clean` turns git failures into "dirty tree".
- **`previous_run_kv` drops lookup errors** by design, but reports nothing.
- **`run_variants` wraps benchmark failures in `DevError::Verify`**, which is the wrong class.
- **`sidecar_cmd` uses `.expect` on a clap guarantee** and `run_compare` indexes `uuids[0]`/`[1]`. Both are allowed only because `expect_used` and `indexing_slicing` aren't denied.

## 5. Tests that prove nothing

- **`env_fingerprint_disambiguates_comma_in_value` cannot fail on the bug it names.** With the old `,` joiner the two fingerprints still differ, because of the trailing `=` from `("narenas:1","")`. The doc comment's own `MALLOC_CONF=a,b=1` example is the case that would have caught it.
- **`parse_kv_stderr_elapsed_ms_takes_precedence_over_total_ms`** is named for a rule that its own second assertion disproves (last one wins). `output-channels.md` states that same false rule: "if both appear, `elapsed_ms` wins".
- **`read_proc_io_tolerates_extra_lines` tests nothing about extra lines.** The sidecar `/proc` tests depend on the host `/proc` (`/proc/self/io` is denied in some sandboxes).
- **`drop_without_finalize_preserves` cannot fail**: `ArtefactDir::drop` is empty and the `armed` field is never read.
- **The gate tests' `open_mem` skips `GateDb::open`**, so migrations and the open path are untested.
- **Five in-scope test modules (write.rs, query.rs, sidecar.rs, harness tests, migrate.rs) use `std::env::temp_dir()`**, i.e. `/tmp`, some with fixed names and no pid. That contradicts `test_scratch.rs`'s claim that "every test … writes under `target/test-tmp/`". About 12 files do this crate-wide. *Enforceable:* textlint `std::env::temp_dir` in `src/**/*.rs`.
- **The same 15-lint `#![allow(...)]` block is pasted into every test module**, including lints that aren't denied (`expect_used`, `panic`, `approx_constant`, `useless_vec`).

## 6. Guards and claims that no longer hold

- **`results.db` is described as both git-tracked and gitignored.** `measure.md` says "gitignored" in one place and "git-tracked" in another. `output-channels.md`, the `db/sidecar.rs` header and the `gate.rs` header say "committed/tracked". Nothing checks this.
- **Other false statements in `measure.md`**:
  - `run_internal` is described as "(N runs, min/avg/max)"; it is best-of-N.
  - "Sidecar data is stored even when the child fails" is false for hotpath/alloc and sync.
  - The field table says `vsize` is in bytes; the code stores kB.
  - "iterations collected by every loop" is false for sync.
- **`output-channels.md` contradicts itself**: the harness table says `run_external_ok` scrapes stderr kv, while the pbfhogg and elivagar tables say "stderr kv: no" and "stderr captured and discarded".
- **The `artefacts.rs` header** says N is "the smallest positive integer such that run-N does not exist". The code uses highest+1, and `allocate`'s own doc says gaps are not filled. The header paragraph about the parent path is also written twice.
- **The `sidecar.rs` header** says the module "bulk-inserts everything to SQLite" (the harness does that) and "Zero I/O during the benchmark" (it writes `.sidecar-status` on every drain).
- **The `git::check_clean` comment** says `*.md` and `brokkr.toml` "don't change what the built binary does". But brokkr's own man pages are compiled into the binary with `include_str!`, and `brokkr.toml` carries host features and `capture_env`.
- **`previous_run` says it answers "what last touched this machine".** It only sees stored rows in this project's DB. Dirty runs and other projects are invisible.
- **The schema declares `ON DELETE CASCADE` but foreign keys are never enabled**, so `delete_by_uuid_prefix` keeps a hand-maintained list of child tables. *Enforceable:* `PRAGMA foreign_keys=ON`, or a test that enumerates every table with a `run_id` column.
- **Stale "variant" wording** remains in the `types_run` field docs, both `with_request` docs ("`variant` columns"), the `StoredRow` doc, and pbfhogg's dispatch comment.
- **Stale docs for removed features**: `RowData.counters` still describes the removed identity-counters feature; `request.rs` `ResultsQuery.grep`/`grep_v` say two sources when there are three; migrate v7→v8's "Fresh databases already have these tables" is false. `exit_code_from_status`'s doc comment is attached to `clamp_u32`, and `parse_kv_lines` has two stacked summaries.
- **Leftover files leak and can mislead**: `.sidecar-<pid>.fifo` and `.sidecar-status` are left behind after a SIGKILL (Drop doesn't run), and `clean` never sweeps them. A stale status file can show a dead run's marker. `$XDG_DATA_HOME/brokkr/sidecar-backups/` is missing from `clean.md`'s store table.

## 7. Policy invented per call site

- **The measurement loop exists three times** (`run_external_inner`, `run_external_with_kv_raw`, bench_gate), plus `run_hotpath` and `run_hotpath_capture`. Tie-breaking, failure handling, sidecar storage and `ok_codes` differ in each (the kv path drops `ok_codes`).
- **`run_hotpath_capture` is a free function**, so nine call sites re-pass the lock (always `Some`), `stop_marker` and `scratch_dir`, all of which the harness already holds.
- **`cargo_profile` is spelled at each writer**, while `cargo_features` has a harness default. The precedence rules also disagree:
  - features: config wins over harness;
  - mode/brokkr_args: harness wins over config;
  - the `with_brokkr_args` doc claims config wins.
- **Transactions**: `insert` uses hand-written `BEGIN`/`COMMIT` strings (a failed `COMMIT` leaves the transaction open), while `delete` uses `unchecked_transaction`.
- **Two ms-rounding policies** coexist (floor in `elapsed_to_ms`, nearest in `us_to_ms`).
- **The pair key is a tab-joined string** that gets split back apart. A test (`pair_key_tabs_in_values_still_bleed`) documents the resulting bug instead of fixing it. A tuple key would make it unrepresentable.
- **Unbounded resources**:
  - sidecar.db has no retention, and every dirty or failed run adds a new UUID;
  - each stored run makes a full-database backup, three generations kept;
  - captured child stdout/stderr and FIFO markers are buffered in memory without a cap.
- **brokkr's rustc-wrapper guard compares against a gate that doesn't exist here.** The shared sidecar module depends on `crate::ratatoskr::process::send_signal_pgrp`, a project module. *Enforceable:* a `[[dependency_rule]]`.
- **SQLite busy handling**: none of these stores set `busy_timeout`, so they rely on rusqlite's implicit default without saying so.

## 8. No longer load-bearing

- **Dead fields**: `BenchConfig.mode` and `BenchConfig.brokkr_args` (never `Some`), the `armed` field and empty `Drop` in `ArtefactDir`, and `run_hotpath_capture`'s `Option<&LockGuard>` (always `Some`).
- **Dead columns in the fresh schema**: `runs.extra` and `runs.metadata`, read only by the v2→v3 migration.
- **Write-only data**: `sidecar_summary.{vm_hwm_kb, sample_count, marker_count, wall_time_ms}` are never read. `print_run_info` recomputes wall time from samples at one-second resolution instead.
- **A dead migration step**: v7→v8 creates tables that v8→v9 drops.
- **The whole v0→v18 migration chain** (about 2,000 lines including tests) is dead only if no results.db below v18 still exists. I can't verify that from this repo; checking `user_version` on every host's databases would settle it.
- **`#[allow(dead_code)]` on `StoredRow` and `GateEntry`** hides which fields are unused.

## Structural recommendation

The root cause is that the harness is a bag of loops rather than one measurement engine. I'd rewrite it as a single `Measurement` that owns the loop, the FIFO, the lock/child pid, `MeasureMode` (typed), `NonZero` runs, the clock and the sidecar lifecycle. Every path, including sync and hotpath, would plug in only its per-iteration "how to time it" strategy. Put the store layer (open read-only vs writable, migrations with a newer-version guard, prefix resolution with ambiguity errors, UUID and short-UUID) in one module shared by results, sidecar and gate. Generate the sample-field registry from one table.

Most findings above then become unrepresentable, and the rest can be held by a few textlint rules: `temp_dir`, `join(".brokkr")`, `env::var("XDG_`, and `"BROKKR_MARKER_FIFO"` outside its owner.
