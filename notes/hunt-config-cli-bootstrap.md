I did not edit, build or run anything. Five findings are concrete bugs, including one that writes history.db into the current directory and one that hides the ids `history <id>` needs. Several other findings are copies that have already diverged. The biggest structural problems are: config is parsed several times per invocation, the top-level key list has three owners, "which command runs where" has two owners, the `.brokkr/` state root is spelled at about 90 sites, and brokkr has no mechanical rule against bypassing its own output and subprocess channels.

## First: what this build can enforce today

brokkr's own `brokkr.toml` has only three `[[textlint]]` rules: `docs-cite-no-line-numbers`, `docs-never-cite-notes` and `comments-never-cite-notes`. There is no `[header]`, `[gremlins]`, `[lints]`, `[[script_check]]`, `[[dependency_rule]]` or `[manifest]`. Your brief assumed those exist; they don't, so brokkr doesn't use most of its own engines on itself. `Cargo.toml [lints.clippy]` denies about 30 lints, but none of `print_stdout`, `print_stderr`, `exit` or `disallowed_methods`, and there is no `clippy.toml`. So:

- **Available now:** textlint, clippy `disallowed-methods`/`disallowed-macros` through a new `clippy.toml` (clippy already runs in `check`), unit tests, and enums in serde types.
- **Not available:** anything that crosses repos. `pbfhogg`, `elivagar` and `nidhogg` names used here have no enforceable pairing.

## 1. One value, one owner

- **brokkr.toml is parsed and validated 3–5 times per invocation.** The call sites are `parse_cli`, the `disable_dir` peek, each per-command `detect_optional()`, `project::detect()`, `record_history` at exit, and `bench_cmd` internally. Each call reloads the user layer and runs `Box::leak` again for `Other`. Two docs claim the opposite and are false today: `project::detect` ("read and parsed exactly once") and `Project::Other` ("leaked once at startup"). History also re-reads the config *after* a run that may have lasted 25 minutes.
  - Fix: detect once in `main` and thread a `Detection` (or `Option`) through. A `OnceLock` cache inside `project` would make a second parse impossible to write.
- **The top-level key universe has three owners.**
  - `load()` calls each `parse_*` function.
  - `parse_hosts` has its own hard-coded skip list of 24 keys.
  - `docs/brokkr.toml.md` "Reserved top-level keys" lists 16 and has already diverged: it lacks `ratatoskr`, `piners`, `dellingr`, `mogwai`, `quarantine`, `clippy`, `lints` and `bin`.
  - Side effect: a host literally named `test`, `check` or `bin` cannot be configured.
  - Fix: one `const SECTIONS: &[(&str, parser)]` table driving both parsing and the host skip, plus a test that the doc list equals it (or the doc stops restating it).
- **The "which command belongs to which project" map has two owners.** One is `cli/visibility.rs TABLE` ("must be kept in agreement with those call sites"). The other is the scattered per-handler checks: `project::require`, the `cmd_pmtiles_stats` inline gate, and the `'X' is only available for litehtml/sluggrs` match arms. Only pmtiles-stats has an agreement test.
  - Fix: make TABLE the gate by checking `visible_in` in dispatch before any handler runs, and delete the per-handler checks. The gating message would then have one spelling; today there are two shapes, "in X projects" and "for litehtml/sluggrs projects".
- **Project name ↔ variant mapping is duplicated.** It lives in `config::load`'s string match and in `Project::name()`, and the hand-written `Project` lists in tests already miss `Saehrimnir` and `Mogwai`.
  - Fix: a `Project::ALL` constant and a round-trip test.
- **`"denmark"` is the default dataset about 30 times in `cli/schema.rs`**, plus literal `"denmark"`/`"raw"` in `bootstrap.rs` for `PmtilesWriter`/`NodeStore`. The default variant also varies by command (`raw` vs `indexed`).
  - Fix: a per-host `default_dataset` in config, or one const. A textlint rule on `default_value = "denmark"` in `src/cli/**` would hold it.
- **"No dataset" has three spellings in `run_measured` callers:** `""` (dellingr, mogwai, piners), `"n/a"` (sluggrs hotpath), and `"denmark"` (the pmtiles-writer/node-store microbenches).
  - Fix: `Option<&str>`.
- **`.brokkr` state root is spelled at about 90 sites in about 40 files.** `resolve::results_db_path` and similar exist, but `commands.rs` hand-builds `.brokkr/ratatoskr`, `.brokkr/dellingr`, `.brokkr/piners/corpus` and `.brokkr/.sidecar-status`, and `preflight` builds `.brokkr/hash_cache`.
  - Fix: a `StateDir` type with named accessors, held by a textlint rule forbidding `".brokkr` outside that module.
- **XDG/HOME resolution is re-implemented per site:**
  - `config_parts/user.rs` (`XDG_CONFIG_HOME`)
  - `history.rs::db_path` and `format_sidecar.rs` (`XDG_DATA_HOME`)
  - `history_cmd::record_history`, which restates the rule as a precondition using `var_os` while `db_path` uses `var`
  - `HOME` lookups in `lockfile`, `hold`, `guard`, `rustc_guard` (twice) and `deps/focus`
  - `CARGO_HOME` fallback in `bench_cmd/stamp` and `rustflags`
  - None treats an empty XDG value as unset (bug below). Fix: one `dirs` module plus `disallowed-methods` on `std::env::var`/`var_os` elsewhere. `rustc_guard` is a separate bin and may need a documented copy.
- **Archive retention for the tilegen store has two pruners:** `elivagar/dispatch.rs::prune_output_dir` (`OUTPUT_RETENTION = 5`) and `commands.rs::clean_archives` (`--keep` default 2). Their read_dir → mtime-sort → skip logic is identical. Both docs describing `OUTPUT_RETENTION` still say `<dataset>-<commit>.pmtiles` "per dataset", which is stale (now per dataset and variant).
- **Values parsed twice:**
  - `--osc-range`: `validate_osc_range`, then again in `resolve::resolve_osc_range`.
  - `--compression`: `validate_compression`, then again in `pbfhogg/mod.rs`.
  - Fix: clap value parsers that return typed values rather than validated `String`s.
- **Mode exclusivity and run count are checked in two places.** `--bench/--hotpath/--alloc` exclusivity and `runs >= 1` are checked in `resolve_mode` after parse, and `--runs >= 1` again in `cmd_run`. A clap `ArgGroup` plus `value_parser!(usize).range(1..)` would make both unrepresentable.
- **`tilegen_tmp` appears in three sites** (`elivagar/bench_self.rs`, `elivagar/commands.rs`, `elivagar/dispatch.rs`) and clean doesn't use it (bug below). `ocean-build_tmp`, `.ingest_tmp`, `.tilegen_tmp` and `.pbfhogg-external-join-` are other tools' naming conventions with a single copy here. They are forced duplicates across a repo boundary and nothing keeps them in step.
- **Other duplicated constants and texts:**
  - `rustflags-` prefix: `commands.rs` and `check_cmd/output.rs`.
  - Fallback of 4 CPUs: `tools.rs` and `cpu_topology.rs`.
  - `/proc/version` parsing: `env.rs` and `history_cmd.rs`.
  - io_uring kernel params: `env::check_uring_blocked` and `preflight::uring_checks`. env's doc claims "checks the same", unenforced, and env never reports the 16 MB memlock bar.
  - `wc`'s default of 800 is restated in its help doc comment and in CLAUDE.md.
- **Exit codes have no table.** 1, 3 (regress), 10 (partial), 124 (the only named one, `WATCHDOG_EXIT_CODE`), 130 (twice in `main` as a literal), and 2/127 in rustc_guard. Passthrough child codes are forwarded verbatim via `ExitCode(out.code)`, so a child exiting 10, 124 or 130 reads as partial, watchdog or interrupt. Fix: an `exit` module with named constants and remapping of child codes.
- **`worktree_keep(&config::hostname()?)` and the `cwd != project_root` → `parent_build_root` derivation** are each repeated three times in `bootstrap.rs`, even though `Detection.build_root` already carries that value.

## 2. Values nobody can find, change or trust

- There is no single list of this scope's tunables. They live where first needed: `OUTPUT_RETENTION` (elivagar dispatch), `DEFAULT_KEEP` (worktree_record), `MIN_FILTER_LEN` (parser), `STATUS_WIDTH`, `DEADLINE_POLL_INTERVAL` and `SIGTERM_FORWARD_BUDGET` (output), `JDK_MAJOR` and `OSMOSIS_VERSION` (tools), `INDEX_MIN_SECTIONS` (man), `DEFAULT_THRESHOLD` (wc).
- **Stringly config validated at use rather than at load**, so a typo only surfaces if that phase runs (and never under the markdown-only shortcut or `--textlint`):
  - `VersionAlign.granularity`: checked in `manifest.rs` at phase time.
  - `TextlintRule.region`: checked in `lex.rs`.
  - `DependencyRule.kinds`: its own doc says "Validated when the phase runs".
  - `QuarantineEntry.category`, `LitehtmlConfig.mode`.
  - Fix: serde enums.
- **`worktree_keep = 0` is silently remapped to the default** (at use, in `DevConfig::worktree_keep`), while `parallel.budget = 0` is refused at parse. The same class of value gets two policies.
- **`DriveConfig` lacks `deny_unknown_fields`**, so a typo under `drives.*` is silently ignored. Every sibling struct denies unknown fields.
- **Host sections fail open.** A misspelled hostname section silently resolves to default paths and no host features (`resolve_paths`, `host_features`). `host_features` also swallows a `hostname()` error that other callers propagate. Checkable: warn when `hosts` is non-empty and none matches.
- **No injection point for the process environment.** Tests reach `std::env` directly (see §5), and `captured_env_pairs` reads `std::env::vars()` itself.

## 3. One channel, one implementation

- **`output.rs` makes two false claims:**
  - "All output goes to stdout (stderr reserved for panics only)". `sidecar_msg`, `record_history`, `man::write_stdout` and `tools::download_file` all write stderr.
  - "every prefixed printer has to go through [the renderer]". Only `emit()` does; `bench_msg`, `verify_msg`, `hotpath_msg`, `download_msg`, `lock_msg` and the per-project `*_msg` functions `println!` directly, bypassing both the run log and status-line clearing.
- **The "10-char prefix column"** is broken by `[download] `, `[litehtml] `, `[sluggrs] `, `[ratatoskr] `, `[sidecar] ` and `[history] `. A test over a prefix table would hold it.
- **`output::history_msg` is `#[allow(dead_code)]`,** while `record_history` hand-writes `eprintln!("[history] warning: ...")`. There is also no `warn`-level rule: record_history, man and tools each pick their own.
- **tools.rs reports the same event on different channels:** Osmosis downloads print via `verify_msg`, JDK/Planetiler via `bench_msg`; `download_msg` exists and isn't used.
- **About 220 direct `println!`/`eprintln!` calls outside `output.rs`** (sidecar_fmt 46, test_cmd 21, env 14, …). Some are legitimate data output. Enforceable with `disallowed-macros` plus a per-file `#[allow]` on data printers.
- **Subprocess spawning has at least four shapes:**
  - `output.rs`: `run_captured_with_env_and_deadline`, `spawn_captured`, `run_passthrough_in`.
  - Raw `Command` in `commands.rs::forward_cargo`, `env::read_tool_version`/`read_git_rev`, `tools::download_file`/`head_url`/`check_curl`, `preflight::check_binary`, `wc::rust_files`.
  - `run_passthrough_in`'s own doc says bare `Command::status()` "silently gave up the SigtermGuard, the OOM marking and the lockfile PID publication", which is exactly what `forward_cargo` (fmt) still does.
  - `output.rs` also depends on `crate::ratatoskr::process::send_signal_pgrp`, a generic module reaching into a project module.
  - Fix: `disallowed-methods = ["std::process::Command::new"]` with an allow in one process module.

## 4. Errors

- **`DevError::Subprocess` Display is wrong for spawn failures.** When `code` is `None` it prints "killed by signal", and a spawn failure also has `code: None`. So a missing `curl` renders as `curl killed by signal: No such file or directory`. A real signal death in `forward_cargo` renders "killed by signal: killed by signal 9". Needs a `Spawn` variant or an `Option<signal>`.
- **`From<io::Error>` loses the subject.** Blanket `?` produces `io: No such file or directory` naming no path; examples are `HistoryDb::open` `create_dir_all`, `cmd_run` `create_dir_all(scratch)`, `resolve::file_size_mb`, `preflight::cached_xxh128` metadata and `wc`. Removing the blanket `From` would force context at every site.
- **Parse errors don't name the file.** `From<toml::de::Error>` is used by `config::load` (`toml::from_str(&text)?`), so a parse error never says which `brokkr.toml` (cwd or parent). The user layer does wrap with its path.
- **Misclassified errors:** `From<serde_json::Error>` maps to `DevError::Build` (for example the adoptium API JSON in tools), and `hostname()` failure maps to `Config`.
- **Swallowed errors:**
  - `Cleaner::dir`/`file` count a path as "removed" whatever `remove_*` returned.
  - `HistoryDb::query` uses `.filter_map(Result::ok)`.
  - `tools` `git pull --ff-only` failures are dropped ("tolerate").
  - The preflight hash-cache write is best-effort.
  - `clean_scratch` treats `kill(pid, 0) == -1` as dead, including EPERM, so it can delete a live foreign-uid process's join dir.

## 5. Tests that prove nothing or depend on the host

- **Process env mutated in parallel test binaries:** `config_parts/tests.rs::empty_env_override_disables_the_layer` sets `BROKKR_USER_CONFIG`, and `history.rs::db_path_uses_xdg_data_home` sets `XDG_DATA_HOME` with a "single-threaded" SAFETY comment that is false under libtest. Any concurrent detect/load_user sees the mutated value. Fix: make path resolution take an env accessor and delete the `set_var` calls.
- **`history::fresh_db_schema_version` cannot fail.** `test_db()` writes `SCHEMA_VERSION`, then the test reads it back. `test_db` also duplicates `open()`'s setup, so `run_migrations` and `open` are never exercised.
- **`HarnessConfig::binary_name` is `#[cfg(test)]`-only.** Its doc says it exists "to lock the defaulting rule into the schema", but production re-implements the rule, so the test pins a copy.
- **Host-utility tests:** `output.rs` tests need `/bin/true`, `/bin/sleep` and `/bin/echo`. `preflight::check_binary` and `tools::check_curl`/`check_build_tool` need `which` at runtime.
- **Three scratch allocators exist** despite CLAUDE.md calling `test_scratch` "the one scratch-dir allocator": `preflight` `tree_hash_tests` uses `target/` plus a timestamp (leaked forever), and `resolve_parts/tests.rs` uses `cwd/.brokkr/test-artifacts`, i.e. brokkr's own state dir, depending on the cwd. Fix: a textlint rule forbidding `CARGO_MANIFEST_DIR`/`test-artifacts` in tests.
- **Visibility/project tests** enumerate `Project` by hand (§1).

## 6. Guards and claims that have stopped holding

False today:
- **README.md:**
  - It documents a `preview` command, a `--wait` flag on "all commands", and a `[hostname.preview]` table. None exist, and `HostConfig` is `deny_unknown_fields`, so the README's config example fails to load.
  - It also says `project` is one of 4 names; `check` is "gremlin scan + dependency rules + clippy + tests"; `test` "always builds release" (`[test] debug` exists); and it cites `notes/sidecar.md`.
  - The `docs-never-cite-notes` rule only covers `docs/**/*.md`, so README and CLAUDE.md are unguarded.
- **Help text:**
  - `check` long_about says "Three phases in order: gremlin scan, clippy, then tests" and mentions an enforced `--test-threads=1`.
  - `deps` says "v1 ships `duplicate_version`".
  - `man` help lists the agnostic topics as "check, clippy, deps, config, measure, run, output-channels", which omits `bench` and `clean`.
  - `PmtilesCorpusCommand` doc says it's "a thin wrapper over `elivagar corpus`… elivagar owns… the exit-code contract", contradicting CLAUDE.md's native gate, and gives two different exit-code lists in one file.
  - `history` help hard-codes `~/.local/share/brokkr/history.db`.
  - `History.id` says ids are "shown in the leftmost column of the default view"; `format_history` prints no id (bug below).
- **Config docs:**
  - `Harness`/`CheckEntry.harness` say nextest runs "under the project's own `.config/nextest.toml`"; CLAUDE.md says that file is never opened.
  - `parse_test` doc claims detection of `consumer_features` inside `[test]`; the code only checks `sweeps`.
  - `parse_check`'s error says "See CLAUDE.md for examples"; there are none there.
  - `ParallelBinaries::resolved_budget` claims to be "resolved at config load"; it's called in `profile.rs`. `brokkr env` prints `cache_domain_cores()` (None → "not detected") while the budget silently falls back, so "reports the same figure" fails exactly when detection fails.
- **Code comments and module docs:**
  - `bootstrap.rs` says "Pbfhogg measured commands: 28 commands"; `as_pbfhogg` handles 17. Its `unreachable!()` list depends on `as_pbfhogg` returning `Some` for exactly those 17 variants: one `None` (as `MultiExtract` already returns) would panic.
  - `resolve_pmtiles_by_commit` doc uses the old `<dataset>-<commit>` name.
  - `env::EnvInfo` says "the `dev env` subcommand".
  - `visibility.rs TABLE` claims "Sorted by name" and isn't (mogwai, install, guard/strays/wc are out of order). Duplicates aren't tested either.
  - `validate_meta_filter`/`validate_env_kv` claim "exactly one `=`" and don't check it.
- **CLAUDE.md:**
  - `DevError` list omits `ExitCode` and `Interrupted`.
  - The `Project` enum list omits Brokkr, Saehrimnir, Dellingr, Mogwai and Other.
  - tools.rs is said to do "osmium" discovery; it doesn't (it does osmosis, JDK, planetiler and tilemaker).
- **clean:** its doc says it removes only "brokkr-designated… or constructed" names, yet it deletes other tools' tmp names.

Guards that fail open:
- The `comments-never-cite-notes` rule keys on the literal `notes/`. About 40 comments cite transient IDs instead (`S3-06`…`S3-35`, `TODO #5`, "request 2", "plan 1/plan-3", "feature 6 `when`"). The `man/render.rs` S-IDs collide with mogwai's tracker namespace by the file's own admission.
- The per-profile doctest rule in `validate_complete_universe` counts `parallel.is_none()` as a serial lane, while the global rule counts `parallel || nextest` as unable to run doctests. A complete profile whose non-curated sweeps are all nextest passes load with doctests never running. These are two implementations of one rule, already diverged.
- `man::TOPICS` covers all 29 docs today, but no test enumerates `docs/**/*.md`, so a new doc silently won't appear in `man`.
- tools.rs caches forever by existence of a version-marker file: `ensure_jdk` returns if `.jdk-version` exists, so bumping `JDK_MAJOR` does nothing.

## 7. Policy invented per call site

- **Downloads:** curl has three call shapes (`run_curl`, `download_file`, `head_url`) with no `--max-time`, no retry and no checksum. They run under the global lock, so a stalled network wedges every brokkr on the host. JDK, planetiler, tilemaker and shortbread are "latest"/HEAD and unpinned, so comparison baselines drift silently. tilemaker does a network `git pull` on every run.
- **Hash cache (`preflight`):**
  - The read-modify-write through a fixed `hash_cache.tmp` loses updates under concurrency (atomic rename doesn't prevent that).
  - Every tree-file miss rewrites the whole cache, which is quadratic.
  - Deleted files are never pruned, so the cache grows without bound.
  - The key is `path.display()`: relative and absolute paths duplicate, and a tab or newline in a path corrupts it.
  - The key uses whole-second mtime, so a same-size rewrite within one second serves a stale digest to a pinning system.
- **PATH lookup via `which`** is re-implemented three times (`preflight::check_binary`, `tools::check_curl`, `tools::check_build_tool`), and `wc::rust_files` duplicates `gremlins` `git ls-files` byte for byte.
- **Secrets:** `clippy --env KEY=VALUE`, passthrough args and similar land verbatim in the global `history.db` (`raw_args`) and in `brokkr_args`. `capture_env` guards `*` but nothing else is scrubbed. Separately, history stores argv joined with spaces unquoted, while `capture_brokkr_args` shell-quotes: two renderings of one argv.
- **Watchdog exits bypass history:** `watchdog::exit(124)` skips `record_history`, so exactly the runaway runs are missing from `brokkr history`.
- **Misattributed lock name:** `acquire_cmd_lock(..., "run")` is used for `passthrough`.

## 8. Code that is no longer load-bearing

Each item names what shows it is dead:
- `Project::is_builtin`: carries `#[allow(dead_code)]` and has no callers.
- `Project::Saehrimnir` and `Project::Brokkr`: no behaviour distinct from `Other` except an exhaustive match in `env.rs`.
- `output::history_msg`: `#[allow(dead_code)]`.
- `preflight::Check::{File, DiskSpace, KernelParam}`: the doc admits "not yet constructed by any caller".
- `ProfileDef.description`: carries `#[allow(dead_code)]` and waits on a "future `brokkr profiles`" command.
- `Snapshot.osc`: "not consumed by any current command".
- `tools::ensure_osmosis(workspace_root)`: marked `#[allow(unused_variables)]`.
- `history::run_migrations`: an empty placeholder.
- `#[allow(clippy::too_many_arguments)]` on one-argument `HistoryDb::insert`.
- `passthrough`: hidden and self-described as "deprecated - use `run`".
- `#[allow(dead_code)]` on whole structs (`PbfEntry`, `Dataset`, `TilegenConfig`, `HostConfig`, `LitehtmlFixture`, `SluggrsSnapshot`, `ResolvedPaths`) hides which fields are genuinely unread.
- Compatibility aliases with nothing to show they still matter: `sha256` on `PbfEntry`/`OscEntry`/`PmtilesEntry` (and a second digest field name, `xxhash` vs `Dataset.xxh128`), `[clippy]` as a `[lints]` alias, and the `[ratatoskr.harness].sweep`, `[check]` table and `[test.sweeps]` refusals.

## Bugs found along the way

1. **An empty `XDG_DATA_HOME`/`XDG_CONFIG_HOME` resolves relative to cwd.** `history.db` then lands in `./brokkr/history.db` (the same happens to sidecar backups), and the user config is read from `./brokkr/brokkr.toml`. Per the XDG spec, an empty value means unset.
2. **`history --until 2026-03-05` excludes that whole day.** It's a string compare against `'YYYY-MM-DD HH:MM:SS'`, and the timestamps are UTC while the user supplies an unspecified timezone. `validate_since` also accepts month 13.
3. **The ids `history <id>` needs are never shown.** `format_history` doesn't print them.
4. **Routine `clean` wipes the wrong elivagar directory.** It removes `scratch_dir` (default `data/scratch`) but reports it as "tilegen_tmp"; the real `data/tilegen_tmp` is never cleaned unless a host points scratch there. Nidhogg's cleanup is gated on an unrelated `scratch_dir.exists()`.
5. **Broken error message.** `validate_check_entry`'s `parallel.budget = 0` text has a missing line continuation, leaving a run of about 14 spaces mid-sentence.
6. **Misattached doc comments.** `resolve_features`' doc is glued onto `profile_override`, and the "Cross-check that every sweep name…" doc sits on `parse_quarantine`. `profile_override`'s doc also says ratatoskr-only, but it serves `test`, `service`, `corpus` and `lint-corpus` too.
7. **Outside my scope:** `profile.rs` documents that `test_threads >= 2` and `0` "bypass the watchdog for a whole-sweep timeout", which contradicts CLAUDE.md's "every test brokkr runs gets 20s… no exception but `--timeout`".

The main files: `/home/folk/Programs/brokkr/src/main_parts/bootstrap.rs`, `/home/folk/Programs/brokkr/src/main_parts/commands.rs`, `/home/folk/Programs/brokkr/src/config_parts/parser.rs`, `/home/folk/Programs/brokkr/src/config_parts/schema.rs`, `/home/folk/Programs/brokkr/src/config_parts/user.rs`, `/home/folk/Programs/brokkr/src/project.rs`, `/home/folk/Programs/brokkr/src/cli/visibility.rs`, `/home/folk/Programs/brokkr/src/cli/schema.rs`, `/home/folk/Programs/brokkr/src/output.rs`, `/home/folk/Programs/brokkr/src/error.rs`, `/home/folk/Programs/brokkr/src/history.rs`, `/home/folk/Programs/brokkr/src/history_cmd.rs`, `/home/folk/Programs/brokkr/src/preflight.rs`, `/home/folk/Programs/brokkr/src/tools.rs`, `/home/folk/Programs/brokkr/src/env.rs`, `/home/folk/Programs/brokkr/src/resolve_parts/schema.rs`, `/home/folk/Programs/brokkr/README.md`, `/home/folk/Programs/brokkr/docs/brokkr.toml.md`.
