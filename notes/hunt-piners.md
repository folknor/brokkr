I found about 60 hygiene findings in the piners scope, plus several real bugs. They are grouped below by your eight questions, with the bugs first. Nothing was edited, built or run.

## Bugs found along the way

- **`corpus --bless` exits 0 when the harness failed.** In `/home/folk/Programs/brokkr/src/piners/cmd.rs`, the bless branch calls `bless::apply` and returns `Ok(())` before `run_pass` is checked. So a harness that exits 1, exits 2 or dies on a signal still gets its surviving dispositions stamped into `pins.toml`, and brokkr exits 0. The exit-code sections of `docs/commands/corpus.md` and `docs/commands/lint-corpus.md` say otherwise.
- **`lint-corpus --bless` can stamp a missing tool into every pin.** In `/home/folk/Programs/brokkr/src/piners/lint/cmd.rs`, bless stamps `r.disposition` without regard to `tool_error`. If `pine-lint` is missing from PATH, every probe comes back `lint_error`; `--bless` then writes `expected = "lint_error"` into every pin and exits 0.
- **A pinned break can never pass.** The docs say a probe can pin `expected = "compile_fail"` so the gate catches it when it starts compiling. But the harness exits 1 on any compile/runtime break, and `cmd.rs` fails the run on any non-zero exit. So a gated run containing that probe always fails, whatever the pin says. Two things own pass/fail here (the harness exit code and the gate) and they disagree. Lint has the same shape: a pinned `piners_error`/`lint_error` passes the gate, but `tool_error` still fails the run.
- **The runtime ceiling can be silently disabled.** `CorpusDb::estimated_wall_ms` takes the newest run whose ids cover the selection, with any non-null `wall_ms`. That includes failed runs, runs with forwarded harness args, and runs built with a different profile. One fast-failing `--all` run (harness exits 2 after a second) then bounds every later selection at about 1s.
- **An older run store blocks runs.** `enforce_runtime_ceiling` opens `runs.db` read-only, which skips migrations. On a database older than v4, `SELECT selector, wall_ms` fails, and only `--force` (which skips the ceiling and later opens the database read-write) gets past it. `corpus-results` also only ever opens read-only.
- **A duplicate harness line aborts ingest with an error that names no probe.** `disposition` and `trade_diff` have primary keys, so a repeated probe line gives "UNIQUE constraint failed" with no probe id. Meanwhile `gate::evaluate` and `bless::apply` quietly keep the last line (`BTreeMap` collect).
- **The lint TV anchor ignores scope.** `--reanchor` filters the fingerprint by the current `--warnings`/`--all-stages`, but `lints.toml` doesn't record which scope was used. A later run with a different scope reports false "TV advisory" divergences. `expected` has the same problem across scopes.
- **Registry writes are not atomic and not locked.** `pins.toml` and `lints.toml` are written with `std::fs::write` in `bless.rs`, `reseed.rs`, `lint/cmd.rs::write_registry` and `lint/reseed.rs`. A kill mid-write truncates the registry, even though an atomic helper already exists (`elivagar::corpus::digest::write_atomic`). Both `--reseed` paths also write without taking the lock that `--bless` holds, so the two writers can lose each other's updates.
- **Comments can be lost on a failed read.** `lint/cmd.rs::write_registry` reads the existing file with `.ok()`, so a read error falls back to writing from scratch.

## 1. One value, one owner

- **Registry file names** are spelled more than once. `PINS_FILE` is defined in both `registry.rs` and `reseed.rs`, and `cmd.rs` uses a bare `"pins.toml"`. `LINTS_FILE` is in `lint/registry.rs` and `lint/reseed.rs`, plus a bare `"lints.toml"` in `lint/cmd.rs`. The marker names `strategy.pine`/`tv_trades.csv` are constants in `reseed.rs` but literals in `registry::verify_probe`, and `lint/registry.rs` copies the `"strategy.pine"` label onto lint snippets, where it is wrong. A text rule could hold this.
- **The `.brokkr/piners/corpus` path has three owners:** `ARTEFACT_PARENT` plus `"corpus"` in `cmd.rs`, `resolve_parts/runtime.rs::corpus_runs_db_path`, and a literal in `main_parts/commands.rs::clean_artefact_trees`. The lint tree is separate again.
- **`BROKKR_HARNESS_ARTEFACT_DIR`/`BROKKR_TEST_BIN_DIR` are spelled as literal pairs** at five production sites (piners `cmd.rs` and `measured.rs`, plus ratatoskr `service_test.rs`, `list_smoke.rs` and `bench_gate.rs`). A shared constant or builder, backed by a text rule, would hold this.
- **The "no `[piners.harness]`" error message is duplicated word for word** in `cmd.rs` and `measured.rs`, and the same is true of the whole load → lint → select → verify → feeds pipeline.
- **The gate verdict is computed twice.** `gate::evaluate` and `ingest::insert_disposition` each compute it (the comment says "matches gate::evaluate"). They already disagree: a harness line for an unselected probe is ignored by the gate but stored as `gate_ok = 0`, so `corpus-results` shows DEVIATES for a run that passed.
- **The selector JSON has four owners:** `cmd::selector_json` and `lint/cmd.rs::selector_json` write it; `format::fmt_selector`, `measured::selector_label` and `query::selection_covered` read it. `selector_label` says it "mirrors" `fmt_selector`, but it drops keywords when `--probe` is also given, where `fmt_selector` shows both. Lint's run table prints the raw JSON with every id, which is the problem `fmt_selector` was written to fix.
- **Disposition labels are strings in several places.** Corpus: `DISPOSITION_LABELS`, the match arms and fields in `report.rs::summarize`/`Summary`, and `format_summary`. Lint: `DISPOSITION_LABELS`, string literals in `diff::classify`, and `== "piners_error"` twice in `lint/cmd.rs`. The `const _: () = assert!(len == 8)` in `report.rs` only checks the count, so it cannot catch a rename. An enum with serde would make a bad spelling unrepresentable.
- **Run-id precedence disagrees between the two query commands:** `corpus_query` uses `q.run.or(q.run_id)`, lint `query` uses `q.run_id.or(q.run)`. Lint also re-defaults `limit == 0` to 20, so `-n 0` means 20 rows there and 0 rows in `corpus-results`.
- **Timestamps come in two formats:** the corpus store uses SQLite `datetime('now')`, the lint store uses the hand-rolled `now_rfc3339` in `lint/mod.rs`.
- **The 270 s ceiling is restated** in three comments in `cmd.rs`, in the CLI help (`~270s`, `--over 269`) and in `corpus.md` (twice). Per your documentation rule, those should be reworded rather than repeat the number.

## 2. Values nobody can find, change, or trust

- **`RUNTIME_CEILING_MS` is a const with no config key and no injection point.** Its refusal path, the `--force` bypass and the no-database path have no tests; only `estimated_wall_ms` is tested.
- **The harness exit-code contract (0/1/2)** exists only as a `match` in `cmd.rs` and as prose in the docs.
- **Config is checked late, on the path that happens to run:**
  - A missing `[piners.harness]` is only noticed after full hashing and the ceiling check.
  - A missing `pine_lint_bin` shows up as `lint_error` on every probe, which `--bless` then pins.
  - Nothing stops `[piners] registry_dir` and `[piners.lint] registry_dir` from being the same directory, in which case each command loads the other's file as a keyword file and fails to parse.
- **`measured.rs` ignores `[piners.harness] debug`** (`profile_override.unwrap_or(false)`), which `cmd.rs` honours. It also hard-codes `cargo_profile: CargoProfile::Release` for a dev build and patches around it with `meta.profile=dev`.

## 3. One channel, one implementation

- **Piners output bypasses the shared output path.** `corpus_msg` and `lint_msg` are bare `println!`: no run log, no renderer lock, no quiet mode. Warnings are written as ad-hoc `corpus_msg("warning: ...")` in `report.rs`, `bless.rs` and `cmd.rs` instead of `output::warn`.
- **Tables are printed with raw `println!`** in `corpus_query.rs` and `lint/query.rs`.
- **Counts use the `(s)` hedge throughout**, although `output::count` exists to replace it.
- **Operator text names the wrong command:** `query.rs::resolve_diff_columns` errors say `results --columns`, a leftover from before `corpus-results` split off.
- **Some significant events are not logged:**
  - Reseed re-hashes every feed group, even under `--probe`, and its summary line doesn't report whether any feed hash changed.
  - Lint throws away validator stderr: `RunMeta.stderr` is always `""`, although its doc says it holds captured stderr.
- **A stored field misreports bless runs:** `gated: !args.no_gate` records `gated=yes` for runs where the gate was ignored (both stores).

## 4. Errors

- **The spawn-failure path names the wrong cause.** Every non-`Interrupted` error from `run_captured_with_env_and_deadline` is recorded as "harness failed to spawn", and that branch never calls `clear_child_pid`.
- **Some unrecognised values fail open silently.** An unknown `TvDiag` severity is dropped by `filter_map` in `tv_anchor`, and `LintRegistry::lint` never validates it; a serde enum would fix this. `report.rs` drops malformed dense-na sites and unparsable lines with only a stdout warning; that is intended, but those drops are never recorded in `runs.db`.
- **`measured.rs` uses `MeasureMode::Run => unreachable!`** for a dispatch contract, and refuses `--bench` at runtime instead of via clap conflicts.
- **Some flag combinations are silently ignored or widened:**
  - `--verify-only --keyword x` widens to the whole universe.
  - `--reseed` ignores `--no-gate`, `--force` and `--release`.
  - In `corpus-results`, `--where` without `--diffs`, `--over` without `--runtimes`, and `--sql`/`--trend` alongside other flags are ignored. `--columns` without `--diffs`, by contrast, is rejected, so the policy is inconsistent.

## 5. Tests that prove nothing, or depend on the environment

- **Eleven piners tests use `std::env::temp_dir()` with pid-named directories**, in `bless.rs`, `reseed.rs` (4), `lfs.rs`, `registry.rs`, `lint/reseed.rs`, `lint/registry.rs` and `lint/db.rs`. That breaks the no-/tmp rule and bypasses `test_scratch`'s uniqueness guard, whose module header claims every test already uses it. `lint/db.rs` even comments that this keeps "/tmp literal out of source". Outside piners, `db/*` and `harness_mod/tests.rs` do the same. A text rule banning `temp_dir()` in `src/**` (except `test_scratch.rs`) would hold this.
- **The bless tests never read the file they write.** They delete the directory and then assert only on the in-memory registry.
- **`lint/db.rs::record_run_roundtrips_and_renders` has assertions that can't fail.** `runs.contains("diverge_one") || runs.contains("bracket")`: the first side is a probe name, never in the runs table. `!runs.is_empty()`: an empty table renders as `"(none)"`.
- **The hand-rolled date code (`now_rfc3339`/`civil_from_days`) has no tests**, and `reanchor` reads the clock directly.
- **Stale test comments:** the `ingest.rs` test says `runtime_ms` is "store-only… not yet on any canned query row", and `ProbeLine.runtime_ms` says "not yet rendered". `--runtimes` now renders it.
- **`cmd.rs` and `lint/cmd.rs` have no tests at all.** Uncovered: fail-reason mapping, selector JSON, bless on failure (the bug above), ceiling enforcement.

## 6. Guards and claims that have stopped holding

These are false today:
- CLI help for `corpus` says "parity tiers … do not fail the run yet (baseline work is deferred)". The gate does fail the run.
- CLI `--runtimes` help says it "shares the pre-run ceiling's per-probe estimate". So do the `RUNTIME_CEILING_MS` doc and the comment above the ceiling call in `cmd.rs` ("sum of each probe's most recent recorded runtime"). The ceiling uses the superset wall now.
- `docs/projects/piners.md` says "The one command is `brokkr corpus`"; `lint-corpus`/`lint-results` also exist.
- The `piners-config` topic summary in `man.rs`, and CLAUDE.md, list a `feeds` config key that the doc says does not exist. `docs/brokkr.toml.piners.md` doesn't document `[piners.lint]`.
- `lint-corpus.md` claims:
  - `path` is "relative to the registry's snippet tree"; it is relative to `corpus_root`.
  - The advisory reads "agree but TV-divergent, anchored Nd ago"; the real line has no age and isn't limited to agreeing probes.
  - TV "times out at 10s"; brokkr passes `Duration::MAX`.
  - The store has an `outcome` column; it doesn't.
  - "`brokkr clean` spares it"; `clean` never looks at the lint directory at all.
- The `lfs.rs` header says "every path that hashes a pinned file first calls `ensure_materialized`". The lint `verify_probe` and lint reseed don't.
- The `ratatoskr/build.rs` header says it is ratatoskr-only, reads `[ratatoskr.harness]` and defaults to release. Piners and lint call it with debug defaults.
- The `migrate.rs::run_migrations` doc says a fresh DB gets "the v1 tables"; it gets the current ones.

Guards that fail open:
- The reseed exclusion `path == registry_dir` fails open when the configured path is spelled differently (symlinks, `..`).
- `clean` finds piners run dirs by parsing names (`starts_with("run-")`), against the constructed-name rule.
- The TV severity filter drops unknown values silently (see question 4).

## 7. Policy invented per call site

- **Two registry stacks, copied almost line for line:**
  - `Registry::load`/`lint` vs `LintRegistry::load`/`lint`.
  - `select.rs` vs `lint/select.rs`, which says it "mirrors".
  - `pins_write.rs` vs `lints_write.rs`: `set_value`, `sync_opt`, `sort_fields`, `rank`, `set_block_prefix`, `pin_value`, `parse_value`, `toml_str` (a third copy is `rustflags::toml_string`).
  - Reseed diff/carry-forward logic.
  - `grid` in `corpus_db/format.rs` and `lint/db.rs`.
  - `has_table` appears five times across the repo, `has_column` twice, `as_i64` twice.

  My recommendation is a single generic registry, selector, writer and run store with a per-corpus pin type. This is a rewrite, and I think the payoff is real.
- **Bless is implemented twice, with different rules.** Corpus refuses unknown labels and counts real changes; lint stamps anything, counts `!gate_ok` as "changed", and has no failure guard.
- **Directory walks differ:** corpus reseed uses `file_type()` (doesn't follow symlinks), lint reseed uses `is_dir()` (follows them, so a symlink loop recurses).
- **No time limits:** the harness and every validator call, including network `pine-lint --tv`, run with `Duration::MAX`.
- **Unbounded storage:** harness stdout is buffered whole in memory, and `runs.db` is append-only with full stderr and `trade_diff` rows and no retention.
- **Piners and lint reach into `crate::ratatoskr::build`.** Because brokkr is a single crate, `[[dependency_rule]]` can't express this boundary; a text rule forbidding `crate::ratatoskr` under `src/piners/**` could.
- **`lint/cmd.rs` rebuilds a `HarnessConfig`** by hand from `LintConfig`'s own package/binary/features/debug fields. The config could embed `HarnessConfig` directly.

## 8. Code that is no longer load-bearing

- `RunMeta.stderr` and the lint `run.stderr` column: always `""` (a single call site).
- `explicit_run_id` returns a `Result` that can never be `Err`, per its own comment.
- The lint `run_migrations` scaffolding has no migrations.
- The duplicated `project::require` in `measured.rs` (dispatch already checks it).
- Compatibility paths whose continued need I can't check from this repo: `report.rs` skipping a legacy `summary` line ("the harness no longer emits one") and `fmt_selector` accepting the legacy string `probe` shape. Whether they're dead depends on the piners harness and on existing `runs.db` rows.

## Other

- `measured.rs` has an unformatted line (`req.force,        req.stop_marker…`). Nothing in `brokkr.toml` enforces `fmt`.
- The repo's own enforcement is currently only three `[[textlint]]` rules. Most of the fixes above could be held by text rules (temp_dir, env-var literals, registry file names, cross-project imports) or by enums and a shared generic registry.
