## Hygiene hunt: pbfhogg / nidhogg command layers (`src/pbfhogg/`, `src/pbfhogg_mod/`, `src/nidhogg/`, `src/osc.rs`)

The most consequential pattern is at the top of section 6: several pbfhogg verify checks cannot fail on content, and in the default quiet mode the FAIL lines they print are thrown away. Findings are grouped by your eight questions and not ranked. Each one says what could hold the fix mechanically. Line numbers are left out; symbols are named. Nothing was built or run, and every "confirmed" below means confirmed by reading the code.

### 1. One value, one owner

- **Cargo package names.** `Project::cli_package()` in `src/project.rs` owns `"pbfhogg-cli"` and `"nidhogg"`. The literal is still restated at about 15 pbfhogg sites (`cmd.rs`, `dispatch.rs`, `verify.rs`, `bench_all.rs`, `bench_allocator.rs`, and three times in `pbfhogg_mod/download_modes.rs`) and about 8 nidhogg sites (`dispatch.rs`, `cmd.rs`, `commands.rs::package`). *Enforce:* a textlint rule forbidding `"pbfhogg-cli"` in `src/**/*.rs` except `project.rs`, or have `BenchContext::new` derive the package from `req.project`.
- **Default dataset and variant.** `default_value = "denmark"` appears about 30 times in `src/cli/schema.rs`, `default_value = "indexed"`/`"raw"` about 20 times, and `bootstrap.rs` hardcodes `"denmark"`/`"raw"`. nidhogg `RunApi`/`RunTiles` hardcode variant `"raw"` and take no `--variant` flag at all. A host with no `denmark` gets a confusing resolution error. *Enforce:* one const, or better a config-owned default, plus a textlint ban on the literal in `schema.rs`.
- **Compression rules: two validators that already disagree.**
  - `cli/validation.rs::validate_compression`: zlib levels 1-9 only, zstd must be a non-negative `u32`, bare `zlib` rejected.
  - `pbfhogg/mod.rs::parse_compressions`: any `i32`, bare `zlib`/`zstd` expand to default levels. Its test `negative_level_is_valid_i32` asserts `zstd:-1` is accepted, which the other validator refuses.
  - The defaults `zlib:6`/`zstd:3` are also spelled in schema `default_value` strings.
  - *Enforce:* a `Compression` type with `FromStr` and a clap `ValueEnum`-style parser, so a string spelling can't be written.
- **Merged-PBF cache key: two implementations, already diverged.**
  - `dispatch.rs::ensure_merged_pbf` keys on `{stem}-snap{key}-osc{seq}-bench-merged`. Its own comment says the extra fields prevent "silent wrong-file reuse".
  - `bench_commands.rs::ensure_merged_pbf` uses `{stem}-bench-merged` and never rebuilds. So `suite pbfhogg` still has the bug the other copy fixed.
  - The dispatch file also spells the key twice (the dry-run synthesis and `ensure_merged_pbf`).
  - *Enforce:* one function; a test asserting the dry-run path equals the real path.
- **GETID fixture IDs.** `commands.rs::GETID_BENCH_IDS` and `verify_getid_removeid.rs::IDS` are identical copies.
- **Inspect-tags min-count.** `INSPECT_ALL_TAGS_MIN_COUNT` is a constant in `commands.rs`, but `bench_blob_filter.rs` spells `"999999999"` inline.
- **Extract strategy → flags mapping, four copies.** `commands.rs` (`Extract` and `MultiExtract`), `bench_extract.rs::strategy_args`, `verify_extract.rs`. They have diverged: `commands.rs` emits `-b=<bbox>`, `bench_extract` emits `-b <bbox>`. Rows under the same `extract` command therefore never pair in `results --compare`, which keys on `cli_args`.
- **Multi-extract strip geometry and config JSON.** Duplicated verbatim between `commands.rs` (`MultiExtract` arm) and `verify_multi_extract.rs`.
- **bbox parsing.** `resolve_bbox`/`validate_bbox` own validation. Four other places re-parse: `commands.rs` MultiExtract, `verify_multi_extract.rs`, `nidhogg/client.rs::shrink_bbox`, and `bbox_to_api`, which does no numeric check at all. `bench_all.rs` reads `ds.bbox` raw and never validates it.
- **nidhogg highway filter list.** `["motorway",...,"residential"]` is spelled 6 times: `client.rs` five times, `query.rs::FALLBACK_QUERY` once.
- **nidhogg server paths and URLs.**
  - `.brokkr/nidhogg.pid` is spelled in both `server::serve` and `server::stop`.
  - `http://localhost:{port}` is in 5 URL builders, and `health_check` builds its URL ad hoc instead of going through them.
  - The 6s readiness timeout exists twice: once as `30 × 200ms` in `poll_for_ready` and once as the literal "6s" in the error text.
- **Scratch output names.** `"multi-extract"` and `geocode-{dataset}` are each spelled in `output_kind`, in `build_body`, and in `cleanup_output`. `OutputKind::ScratchDir("multi-extract")` claims a `-{dataset}` suffix that is never used, which is why `cleanup_output` needs a special case. nidhogg spells `bench-`, `run-` and `hotpath-ingest-output` inline, while the dead `scratch_output_dir()` lists only the first.
- **`brokkr.toml` writers.** `download_core.rs` has 7 appenders plus 2 line-based rewriters, each calling `project_root.join("brokkr.toml")` and hand-formatting TOML with no escaping. The repo already owns a comment-preserving writer pattern (`toml_edit` in `guard.rs` and `piners/pins_write.rs`).
- **Dataset filename conventions.** `{key}-{date}.osm.pbf` and `-with-indexdata.osm.pbf` are spelled at 5 sites: `raw_pbf_path`, `indexed_pbf_path`, `run_as_snapshot`, `run_refresh`, `promote_snapshot`.
- **Planet origin string.** `"planet.openstreetmap.org"` appears 3 times in `download_core.rs`.
- **Date handling.** `download_core.rs` hand-rolls `days_to_civil`/`civil_to_days` and uses `SystemTime::now()`, while `header.rs` uses libc `gmtime` for the same "what is today" question.
- **Verify check names.** `cmd.rs::verify_name` claims to "match the labels used by `verify all`", but `verify_all.rs` spells its own label strings. It also passes `5` regions to multi-extract, duplicating the CLI default `"5"`.
- **`"Sort.Type_then_ID"`.** Spelled 3 times in `verify.rs`.
- **Inspect-output parsers, three independent ones.** `verify.rs::parse_ordered`, `verify_multi_extract.rs::parse_inspect_counts`, `verify_renumber.rs::parse_total_relations`, with `strip_commas` duplicated between the last two. The format contract is with pbfhogg's `inspect.rs`, and only a test comment records it.
- **UTF-8 path helpers, five variants.** `pbfhogg::path_strs`, `commands::path_to_string`, the `CommandContext::*_str` family, `nidhogg::client::path_str`, and inline `to_str().ok_or_else` in many places. Their messages disagree; some name the path, some don't.
- **"MB" means two things.** `file_size_mb` is decimal; `nidhogg/ingest.rs` uses 1,048,576 and `verify_renumber::fmt_size` uses binary units.
- **nidhogg port resolution.** `nidhogg/cmd.rs::resolve_port` applies `PORT` env > `[host].port` > `DEFAULT_PORT` (3033). It re-derives the hostname itself instead of using `ResolvedPaths.hostname`, and swallows hostname errors. `docs/brokkr.toml.md` lists `port = 3033` with no meaning and never mentions `PORT`. The same `PORT` name is also the contract brokkr passes into the child (`server.rs`, `bench_tiles.rs`), so an unrelated `PORT` in the operator's shell silently retargets brokkr.

### 2. Values nobody can find, change, or trust

- **Denmark-only fixtures on a dataset-parameterized surface.** `SUITE_EXTRACT_BBOX` (Copenhagen, `bench_commands.rs`), `GEOCODE_TEST_QUERIES` (Danish cities), `query.rs::FALLBACK_QUERY` (Copenhagen bbox), and the GETID IDs ("known to exist in Denmark PBFs"). On another dataset the extract suite measures near-empty work and getid verify diffs two empty outputs.
- **nidhogg timeouts with no injection point.** `ITERATIONS=5` and `STARTUP_TIMEOUT=30s` in `bench_tiles.rs`; server stop 5s and ready 6s in `server.rs`; curl `--max-time 30` and `--connect-timeout 2` in `client.rs`. Nothing lists these knobs, and tests can't shorten them.
- **Configuration validated at the moment of use.**
  - `nidhogg serve` turns a data_dir resolution error into "tiles-only" via `.ok()`.
  - `bench_all` finds its OSC through `get_default_osc_entry`, which silently returns `None` when more than one OSC is configured, then prints "skipped, no osc file". It also bypasses hash verification that `resolve_*` performs; `get_pbf_entry` for the raw variant does the same.
  - `cli_adapter.rs` does `CatTypeFilter::parse(s).ok()`, so a bad value becomes an unfiltered `cat`. `tags-filter --input-kind` is compared as a string (`== Some("osc")`).
  - `Extract`/`MultiExtract` `--strategy` is a `String` compared against `"all"` and parsed late; `DiffFormat` already shows the `ValueEnum` pattern that would fix this.

### 3. One channel, one implementation

- **Direct writes outside the output channel.** `nidhogg/ingest.rs` and `update.rs` use `eprint!`/`print!`. They also capture and then dump, so "progress output" appears only after the process exits. *Enforce:* clippy `print_stdout`/`print_stderr` in `[lints.clippy]`, with an allow only in `output.rs`.
- **13 raw `Command::new` spawns bypass the spawn choke points** where `hold::stamp` runs. `hold.rs` claims the boundary is "*every* child brokkr starts under a hold". The raw sites:
  - `nidhogg/bench_tiles.rs`: the server and curl, spawned under the lock.
  - `nidhogg/server.rs`: the server and `pkill`.
  - curl in `nidhogg/client.rs` (3) and `nidhogg/bench_api.rs` (2).
  - `chmod` in `verify_readonly.rs`, osmosis in `verify_merge.rs::run_osmosis` (which also rebuilds `CapturedOutput` by hand), and `which` in `verify.rs`.
  - *Enforce:* `clippy.toml` `disallowed-methods = ["std::process::Command::new"]`, with `#[allow]` only in `output.rs`.
- **Six curl invocations, six flag policies.**
  - `client::curl_get`/`curl_post` use `--fail-with-body --max-time 30`.
  - `bench_api::run_curl_timed` has neither, so a fast HTTP 500 is timed as a success.
  - `health_check` has only `--connect-timeout`, so a server that accepts and hangs blocks `poll_for_ready` forever.
  - `curl_get_tile` has no timeout.
- **Lock labels are inconsistent.** pbfhogg labels bench mode `"run {id}"` (`dispatch.rs::run_pbfhogg_wallclock`). nidhogg uses `"bench nid-ingest"`, a hand-typed `"run nid-ingest"`, and `"hotpath …"`. `VerifyHarness` uses the literal project `"pbfhogg"` instead of `Project::name()`.
- **Error variants chosen ad hoc.** `verify_geocode` and `verify_readonly` report verify failures as `DevError::Config`, while `verify_batch` uses `Verify`. I/O failures in `ensure_merged_pbf` and `bench_*` ("failed to remove cached merged PBF: {e}", "failed to create scratch dir: {e}") are also `Config`, and neither names the path.

### 4. Errors

- **`verify all` swallows real errors and relabels them.** `pbfhogg/cmd.rs::verify` resolves the OSC and bbox with `.ok()`. A hash mismatch, an ambiguous-OSC error, or a malformed bbox all become "SKIPPED (no --osc provided)" or "SKIPPED (no --bbox provided)". The flag is actually `--osc-seq`, and the bbox comes from config. The suite then exits 0 with 5 of 12 checks skipped.
- **Setup failures handled two ways.** Single `verify merge` narrates an osmosis setup failure; `verify all` drops it silently (`ensure_osmosis(...).ok()`).
- **Curl failure detail discarded.** `verify_geocode::run_single_geocode` reduces any curl failure to "curl request failed". `verify_readonly::run_geocode_check`/`run_query_check` turn it into a bare `FAIL`. `health_check` turns "curl not installed" into "server not running", and `serve` then reports "did not start within 6s".
- **Other silent fallbacks.** `build_diff_snapshots_context` swallows the file-size error. `geocode.rs` defaults lat/lon to 0.0. `bench_api::report_response_stats` defaults size to 0 and count to 0. `bench_planetiler` defaults node/way/relation counts to 0.
- **Panics on reachable paths.**
  - `unreachable!()` in `bench_extract::strategy_args` and `bench_blob_filter::command_args` on an unknown name.
  - `unreachable!()` in `pbfhogg/cmd.rs::verify` for the elivagar and nidhogg variants. This relies on the caller's match order.
  - The `unreachable!()` block in `main_parts/bootstrap.rs` covering every pbfhogg command. This holds only while `as_pbfhogg()` returns `Some` for each of them; `MultiExtract` already moved out once.
  - *Enforce:* make `as_pbfhogg` an exhaustive match over a pbfhogg sub-enum.
- **Destructive step before its precondition check.** `download_core.rs::promote_snapshot` with `replace` deletes the old snapshot files and strips their TOML blocks *before* checking that the scratch artifact exists.
- **`brokkr.toml` edits are non-atomic and non-transactional.** Each append is a read, push, write. `run_refresh` rotates the primary data out, then appends a header, hashes, appends raw, builds, runs `cat`, and appends indexed. A failure anywhere after the rotate leaves a dataset with no primary.
- **First-time `brokkr ingest` is impossible.** `nidhogg/cmd.rs::ingest` resolves the output dir through `resolve_nidhogg_data_dir`, which errors if the directory doesn't exist. That makes the `create_dir_all(data_dir)` in `nidhogg/ingest.rs` dead code.

### 5. Tests that prove nothing

- `commands.rs::supports_hotpath_includes_tool_commands` tests a function that returns `true` unconditionally.
- `nidhogg/commands.rs`: the tests `package_none_for_api`, `api_needs_server_not_build`, `ingest_needs_build_not_server`, `tiles_needs_build_not_server` and `metadata_is_empty_post_v13` cover methods with no production caller (see section 8), or a constant `Vec::new()`.
- `verify_diff.rs::denmark_numbers_reconcile` restates the D formula inline instead of calling `compare_diff_classes`, so a regression in the real function passes.
- Misnamed tests:
  - `client.rs::explicit_level_with_defaults_off_still_works` actually tests `query_url` (copy-pasted from the compression test).
  - `bench_commands.rs::extract_variants_use_hardcoded_bbox` asserts only the strategy mapping, not the bbox.
- `download_modes.rs::run_rotation` allocates scratch under `current_dir()/.brokkr/test-artifacts/rotation-<pid>-<nanos>`. That depends on cwd and the wall clock, bypasses the mandated `src/test_scratch.rs` allocator, and leaks the directory when `rotate` panics. *Enforce:* a textlint rule on `test-artifacts` or on `current_dir()` inside `#[cfg(test)]`.
- No tests exist for the `verify_check_refs.rs` parsers, even though their doc comments already contain sample input, nor for `bench_tiles::is_ready_line`.

### 6. Guards and claims that have stopped holding

**Verify checks that cannot fail on content (confirmed by reading):**
- `check_sorted` and `compare_sort_feature` return `Result<bool>`. Every caller writes `harness.check_sorted(...)?;` and discards the bool: `verify_sort`, `verify_extract`, `verify_merge`, `verify_derive_changes`, `verify_cat`, `verify_tags_filter`, `verify_getid_removeid`, `verify_add_locations`.
- `verify_tags_filter` prints FAIL on a diff and returns `Ok`.
- `verify_getid_removeid` does the same, and its "complement test" asserts nothing.
- `verify_extract` never fails on a diff ("expected").
- `verify_add_locations`: `report_diff` always returns `Ok`, and optional variants print "FAILED" and return `Ok`.
- Why this matters: `run_check` discards the buffered detail on `Ok`. In the default quiet mode those FAIL lines are never shown, and the summary says `PASS`.
- *Enforce:* return a `#[must_use]` verdict enum, not a bool, and make every check return `Result<Verdict>` folded by `run_check`.

**Other fail-open guards:**
- `VerifyHarness::diff_pbfs` ignores the exit status. A crashed `pbfhogg diff` with empty stdout reads as "identical".
- `verify_merge` treats `diff_path.exists()` as proof the diff ran, but `target/verify/merge/` persists between runs, so a stale OSC from the previous run passes. The same stale-output risk applies to `osmosis_out`/`osmconvert_out` and to the multi-extract strip files; `VerifyHarness::subdir` never clears anything.
- `osc.rs::parse_osc_text` returns an empty diff for any non-OSC input (for example an empty file), and `verify_merge` then prints "element-identical PASS". *Enforce:* require the `<osmChange` root to have been seen.
- `harness::run_variants` with an empty list returns `Ok`. `brokkr api --query typo --bench` records nothing and exits 0.
- `verify_check_refs` with-relations mode does not apply the `integrity_ok → 0` handling that ways-only mode has, so a clean dataset reads as "could not parse counts".
- `bench_tiles::is_ready_line` is keyed on the substring "listening" in nidhogg's stderr. The contract is unenforced across the repo boundary.
- `server::is_nidhogg_process` is `cmdline.contains("nidhogg")`, and only guards the SIGKILL; the SIGTERM goes to an unverified PID read from the pid file. `stop()` falls back to `pkill -f "nidhogg serve"` host-wide. Compare the starttime and pidfd identity rule that `lockfile`/`kill` enforce.

**Locking and the rustc guard:**
- **nidhogg `verify readonly` runs `cargo_build` with no lock held.** `bootstrap.rs` takes no lock for the nidhogg verify variants, so this is an unserialized build carrying no hold capability, which an installed guard will refuse. *Enforce:* make `build::cargo_build` take `&LockGuard`.
- **The stray reap and guard-staleness warning are skipped on bench and verify paths.** CLAUDE.md says every locked command runs them, hooked in `context::acquire_cmd_lock_opt`. `BenchContext::with_build_config`, `HarnessContext::new`, `VerifyHarness::new` and `BenchHarness` call `lockfile::acquire` directly, so every pbfhogg/nidhogg bench and pbfhogg verify skips both. *Enforce:* move both into `lockfile::acquire`, or a textlint ban on `lockfile::acquire(` outside `context.rs`.

**Docs and comments that are false today:**
- `docs/projects/pbfhogg.md` names `build_hotpath_args()` and `result_variant()`, neither of which exists. It also says to add CLI fields in `src/cli.rs`; they live in `src/cli/schema.rs`.
- Verify subcommand counts: `docs/projects/pbfhogg.md` says "11 commands + all" and CLAUDE.md says "verify (11 commands + all)". There are 12. Per your documentation rule this should be reworded, not re-counted.
- `docs/projects/nidhogg.md` says status is "PID file under the host's scratch dir". Status is actually an HTTP health check, and the PID file is `.brokkr/nidhogg.pid`.
- `docs/projects/pbfhogg-vs-elivagar.md` claims pbfhogg has one build kind, a uniform `run_external_ok` path, no external baselines, and "stderr kv → results.db: never". All false: `run_external_ok` scrapes stderr kv into results.db (`types_run.rs::run_external_inner`); read/write/merge use `run_external_with_kv`; osmpbf and planetiler rows are self-reported via `run_internal`; the suite runs osmium, osmpbf and planetiler baselines.
- `docs/commands/output-channels.md` contradicts itself. Its table says `run_external_ok` records stderr kv, while the per-command pbfhogg table and the decision guide say no command routes kv into results.db. nidhogg `tiles` is a stderr-kv path (`parse_kv_lines`) and is absent from the doc.
- The `VerifyPbfArgs` help text says OSC "still resolve[s] from the dataset's primary chain", but `resolve_verify_osc` is snapshot-aware.
- `download_core.rs::rotate_dataset_to_snapshot` says its limitation is "documented in CLAUDE.md". It is not.
- `dispatch.rs::run_wallclock_core` claims argv construction is "centralised here", but the same build_args + io + compression assembly is repeated in dry-run, run and hotpath.
- `commands.rs` says Hotpath mode "prepends the binary path (matching the format expected by `run_hotpath_capture`)". Dispatch slices it off again (`hotpath_args[1..]`), and dry-run needs a fake binary only to satisfy this.
- `bench_tiles::ChildGuard::new` says the server is "spawned in its own process group"; `spawn_server` and `Drop` both say it is not.
- `bench_all.rs::parse_stderr_blocks` claims planetiler shares it. Planetiler has its own parser with a different unknown-key policy.
- `nidhogg/dispatch.rs` says ingest follows "the standard build+run_external pattern (like pbfhogg)"; it uses `run_internal`.
- `verify_merge.rs` is headed "4-tool comparison", but osmosis and osmconvert outputs are only printed, never compared.
- `CommandParams.jobs` doc says diff/diff-snapshots only, but inspect, tags-filter and apply-changes use it too. The `osc_range` doc says `LO-HI`; the validator requires `LO..HI`.
- `commands.rs` headers still reference the removed `bench_build_geocode_index.rs`, `hotpath.rs` and "26 commands"; `bootstrap.rs` says "28 commands".
- `docs/commands/clean.md` claims clean never parses names back. It prefix-matches `geocode-*`, sweeps every `*.pbf`, and parses `.pbfhogg-external-join-<pid>`, checked with a bare `kill(pid,0)`.
- Clean's nidhogg arm removes `.ingest_tmp`/`.tilegen_tmp`, which the nidhogg module never creates, and misses the `*-ingest-output` dirs it does create. The pbfhogg arm misses `*.osc.gz` scratch outputs and `multi-extract/`.
- Operator-facing text references transient plan artifacts: `verify_renumber.rs` prints "notes/renumber-planet-scale.md", and `download_modes.rs` comments cite "Q5:" and "the C3 refresh feature". The existing `comments-never-cite-notes` rule covers comments only (`region = "comment"`), so string literals slip past it.

### 7. Policy invented per call site

- **Scratch cleanup on the error path.** pbfhogg run mode cleans up on failure; the bench and hotpath paths return early via `?` before `cleanup_output`. nidhogg bench cleans the ingest dir every run; hotpath never does between runs. `verify_renumber` deletes its outputs on success; the other verifies keep them.
- **Generate indexed PBF via `cat`: three copies that have diverged.** `run_refresh` passes `--type node,way,relation`; `run` and `run_as_snapshot` don't.
- **OSC resolution: four policies.** `resolve_single_osc` (dispatch), `resolve_default_osc_path` (merge-bench; not snapshot-aware), `get_default_osc_entry` (suite; unverified), and `resolve_verify_osc`.
- **Two timing policies for the same HTTP query.** nidhogg `bench_api` uses curl's `time_total` truncated to whole ms via `as i64`, so sub-ms queries record 0 even though `elapsed_us` exists. `verify_batch` wraps curl spawn in `Instant`.
- **Ambient dependencies reached directly.** `SystemTime::now()` in `download_core::today`; env `PORT` in `resolve_port`; host tools `curl`, `which`, `pkill`, `chmod` and `osmium` on PATH. `verify_readonly`'s "restore" is `chmod -R u+w`, which grants write to files that never had it, and a Ctrl-C mid-test leaves the index read-only because there is no RAII guard.
- **Side effects in dry-run.** `commands.rs::build_body` for `MultiExtract` writes `multi-extract-config.json` and creates a directory. `--dry-run` calls it, contradicting "Does NOT … process execution" and the "validate without building or running" contract. That JSON also embeds `output_dir.display()` without escaping.
- **Unbounded input.** `osc.rs` reads the entire decompressed OSC into one `String`. A planet or europe daily diff is large; the doc comment's "double-digit megabytes (the Denmark daily diff)" is dataset-specific.
- **Cross-module reach.** `nidhogg/bench_tiles.rs` calls `crate::ratatoskr::process::send_signal`. *Enforce:* a textlint rule banning `crate::ratatoskr` in `src/nidhogg/**`. `[[dependency_rule]]` can't express this because it works on crates, and brokkr is a single crate.
- **Lint deny rules opted out site by site.** `too_many_arguments` is `deny` in `Cargo.toml` but is `#[allow]`ed at roughly 15 sites in scope, the 10-positional-argument `BenchContext::new` among them.

### 8. Code that is no longer load-bearing

- **`nidhogg/commands.rs`:** `package()`, `needs_build()`, `needs_server()`, `build_args()` and `scratch_output_dir()` have no production caller (grep shows tests only). They sit under `#[allow(dead_code)] // methods prepared for future dispatch unification`, while dispatch hand-writes `["ingest", pbf, out]` three times. `metadata()` always returns empty. `result_command()` (`"ingest"`) and `id()` (`"nid-ingest"`) are two names for one thing.
- **`PbfhoggCommand::supports_hotpath()`** is `true` for every variant, so the refusal branches in `run_pbfhogg_hotpath` and dry-run are dead.
- **Unused parameters.**
  - `bench_commands::run`: `index_type` is always `None`; its `osc_path` and `scratch_dir` `Option`s are always `Some`.
  - `pbfhogg/cmd.rs::verify(_project)` and nidhogg `status/query/geocode(_project_root)` take arguments they never use.
- **Dead assignment.** `verify_multi_extract.rs` builds `multi_args` and immediately overwrites it, under a misleading comment.
- **Duplicate extract benchmarks in the suite.** `bench_extract.rs` duplicates the `extract-*` presets in `bench_commands` (hardcoded bbox) with a different argv. `suite pbfhogg` runs both, producing two extract row families that can never pair.
- **Legacy argv kept only for row continuity.** "Hotpath legacy" in `commands.rs`: `inspect tags` drops `--type`/`--min-count` in hotpath mode, and `apply-changes` forces `--compression zlib`. It exists "so hotpath result rows don't shift", which means bench and hotpath measure different workloads under the same command name.
- **Outside scope, noticed:** `Project::is_builtin` is `#[allow(dead_code)]` with no caller.

### What the build can already hold

The repo's own `brokkr.toml` uses textlint for docs only; it has no `[[script_check]]` and no `[[dependency_rule]]`. Each finding above could instead be held by one of these existing mechanisms:

- **textlint** (regex on lines, path-scoped): `"pbfhogg-cli"`/`"denmark"` literals, `lockfile::acquire(` outside `context.rs`, `crate::ratatoskr` in `src/nidhogg/`, `notes/` in string literals.
- **`clippy.toml`:** `disallowed-methods` (`Command::new`) plus `print_stdout`/`print_stderr`.
- **Types:** a `#[must_use]` verify verdict, a `Compression` type, `ValueEnum`s for strategy, cat type and input kind, and a `&LockGuard` parameter on `cargo_build`.
- **Tests:** assert the dry-run merged path equals the real one; parser tests from the doc-comment samples; one `InspectReport` parser.

These cannot be enforced mechanically: the cross-repo output contracts (pbfhogg `inspect` and `diff` text, nidhogg's "listening" line and shutdown kv). They need either a structured output mode in those tools or a pinned fixture test on each side.
