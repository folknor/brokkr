# Bugs

Defects turned up by the design hunt, plus any that the hygiene documents had
already described inside a broader entry; those were moved here whole. Siblings:
`consolidations.md` (CON), `structural.md` (STR), and the hygiene documents
(`hygiene-*.md`). Entries record what hunters reported; nothing has been
verified. Entries marked *suspected* are ones the hunter flagged without
demonstrating them.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

## BUG-001 - `lockfile::status()` probes with an exclusive flock, and the guard then refuses real compiles

Reported by: process-control.

`status()` checks whether anyone holds `brokkr.lock` by trying `LOCK_EX | LOCK_NB`. When nobody holds the lock, the probe itself briefly becomes the holder. During that window the guard (`rustc_guard.rs::lock_is_held`, which probes `LOCK_SH | LOCK_NB`) gets `EWOULDBLOCK`, concludes a hold is live and reads `auth=`. After a normal release `auth` is empty because the file was truncated. The guard then returns `Refused { why: "a brokkr hold is starting up ..." }` and exits 1. So a foreign cargo's rustc fails whenever it races an idle `brokkr lock`, `brokkr kill` or `brokkr strays`, or a `stray::current_ownership()` call made outside a hold (`brokkr strays` runs unlocked). If the refused invocation is a `--print` that `classify` does not recognise as a query, the refusal can be cached in `.rustc_info.json`, which is the poisoning the guard's comments warn about.

Two probes answer "is the lock held?" and they disagree in what they do to the lock. The hunter's fix is one shared `LOCK_SH|LOCK_NB` probe, used by `status()`, the guard and `wait_for_flock`, and living in a module both binaries include. A shared probe conflicts only with an exclusive holder, which is the question being asked.

## BUG-002 - `guard --install`/`--remove` rewrite the global cargo config non-atomically

Reported by: process-control.

`guard.rs::do_install` and `do_remove` write `$CARGO_HOME/config.toml` with `std::fs::write(config_path, doc.to_string())`. Every cargo on the machine reads that file. A kill or a full disk at the wrong moment leaves it truncated, and a symlinked `~/.cargo/config.toml` (a dotfile manager) is written through rather than kept as a link. `atomic_write.rs`'s header calls truncate-then-fill "a bug" and says that module is the one copy of the safe sequence. `toolchain.rs` renaming `rust-toolchain.toml` in a foreign checkout is the same kind of edit. Only `download_toml.rs`'s edit of `brokkr.toml` goes through `atomic_write`. The hunter proposes a `foreign_file::edit(path, |doc| ..)` helper that resolves symlinks, preserves permissions, writes atomically and refuses on parse failure, so files brokkr edits but does not own have a single path.

## BUG-003 - The stray reaper kills brokkr's own parent cargo; the orphan reaper exempts ancestors

Reported by: process-control.

`stray.rs`'s test `descendant_expansion_never_crosses_into_brokkrs_own_subtree` asserts that in `cargo(10) -> brokkr(20, us) -> cargo(30)` the outer cargo is a stray **and is killed**. Expansion stops at brokkr, but the root itself is killed. So `cargo run --bin brokkr -- check`, or any brokkr started under a cargo (a script check run by an outer brokkr's cargo, an xtask), SIGKILLs its own parent cargo at the first fresh hold. `test_orphans::select` makes the opposite choice on purpose ("Exempt: this process, its ancestors and its descendants"). Neither reaper mentions the other, and a test on each side asserts its own answer. Even if killing the parent is intended (it does hold the target-dir lock), the decision should be stated once, in one exemptions type that both reapers use.

## BUG-004 - An empty `$HOME` puts the global lock at a cwd-relative path

Reported by: process-control (this hunt), and earlier by the hygiene hunt's process-control and config-cli-bootstrap hunters.

`$HOME/.brokkr` is built independently in `lockfile::lock_path`, `lock_service::socket_dir`, `hold::cargo_config_overrides`, `test_orphans::runs_dir`, `worktree.rs`, `test_runner/harness_shim.rs`, and twice in the guard (`brokkr_dir`, `lock_is_held`). `user_dirs::home()` treats an empty value as unset. Every one of these sites uses `env::var("HOME")` or `var_os` instead, which accept `""` and resolve `.brokkr` against the cwd. In a sandbox with an empty `$HOME` the global lock becomes `./.brokkr/brokkr.lock`, so the mutex splits per cwd. In the guard it is relative to rustc's cwd, so brokkr and the guard lock different files. Nothing rejects an empty value. This is the drift `user_dirs.rs`'s header says that module was created to end. The hunter's owner is `user_dirs::brokkr_state_dir()` with typed accessors (`lock_file()`, `compile_lock()`, `socket_dir()`, `test_runs_dir()`, `nextest_env()`, `worktrees()`), included by the guard binary as well.

## BUG-005 - The free-space gate probes the config dir's `target` for `check`, `test` and every post-detection command

Reported by: config-cli (this hunt); the root mismatch was reported earlier by the hygiene hunt's process-control, pbfhogg-nidhogg and config-cli-bootstrap hunters.

`Detection` carries `project_root` (the config dir: `.brokkr/`, data, results) and `build_root` (the cwd: cargo, git, `target/`). Both are `PathBuf`, and `bootstrap.rs` rebinds them under each other's names. `run`, `install`, `wc`, `deps` and `clippy` bind a local named `project_root` to `d.build_root`. `check` uses `project_root = d.build_root, state_root = d.project_root`. `test` takes the lock and run log on `project_root` and runs cargo on `build_root`. The path reaches `LockContext.project_root` (a `&str`), and `disk_gate::check` probes `<that>/target`. So the gate measures `<cwd>/target` for run/install/deps/clippy, but `<config dir>/target` for `check` (`acquire_cmd_lock_opt(project, &state_root, "check")`), for `test`, and for every post-detection locked command (`clean`, `download`, `serve`, ...). `bench_cmd` says `project_root` is "the project root everywhere else" while fmt also passes `build_root`. Under the one-level-up layout that `Detection` exists to support, the config dir has no `target/`. For the two heaviest build commands the gate then fails open, or measures the parent's disk, but never the disk cargo is about to fill. The structural fix (distinct root types) is filed in `structural.md`.

## BUG-006 - A hang snapshot on a cargo-threaded sweep snapshots cargo, not the test binary

Reported by: check-exec.

`run_libtest_parallel` hard-codes `Leader::TestBinary`. `check_cmd/output.rs` calls it with `"cargo"` as the program for `test_threads >= 2` sweeps, so on a cargo-threaded sweep the hang snapshot is taken of cargo itself, not the test harness. `Leader` exists precisely to prevent this, and the serial path gets it right through `Launch`. The underlying duplication (two runners sharing about 80% of their body) is recorded in the hygiene validation document's two-runners entry.

## BUG-007 - A nextest-lane timeout fails the sweep but does not stop the run

Reported by: check-exec.

`nextest_lane::run_nextest_engine` records a timed-out test as `TestResult::TimedOut` in the journal and then returns `Ok(false)`, an ordinary red sweep. Every libtest lane turns a blown budget into `Err(Reported)`, which ends the run, and the documented contract is that exceeding the per-test cap "fails the run and STOPS it". After a nextest-lane timeout, `run_test_phase` carries on to the next sweep. The engine's own fail-fast stops only that lane. The decision behind this (four lane-local verdicts) is filed in `consolidations.md`.

## BUG-008 - The serial shim's runner check ignores a sweep-env `CARGO_HOME`, so the shim can override a configured runner

Reported by: check-exec.

Two functions decide whether a target runner, a foreign build target or a rustdoc override is configured: `output.rs::serial_shim_fallback` (may the shim replace the runner?) and `direct_runtime::refuse_configured_runner` (would direct execution bypass one?). They differ:
- Config chain: the serial check uses `config_paths(root)`, the direct check `config_paths_under(root, env)`. The serial check therefore cannot see a runner configured under a sweep-env `CARGO_HOME`, and the shim then silently overrides that runner. This is exactly the bypass the check exists to prevent.
- Empty values: `var_os(..).is_some()` (empty counts as set) against `.filter(|v| !v.is_empty())`.
- The cfg evaluation is copy-pasted, and `CARGO_TARGET_<HOST>_RUNNER` is spelled with `format!` in three places (`streaming_run_libtest`, `serial_shim_fallback`, `refuse_configured_runner`).

The single owner the hunter proposes is filed in `consolidations.md`.

## BUG-009 - Per-binary timing keys collide when a bin and a test target share a name

Reported by: check-exec.

`parallel.rs` builds the binary label as `format!("{}/{}", package, target)` twice (`BinaryRun.label` and `run_parallel_sweep::label_of`) and uses it as the `binary_timings` store key. The label drops the target kind, so `[[bin]] foo` and `tests/foo.rs` in one package both key as `pkg/foo`, and each reads and overwrites the other's cost. The hunter rates it low severity, since allocation is advisory. `TestBinary::label()` is a second label spelling. The proposed key is a typed `BinaryKey { package, kind, target }`, or the planning unit, with the label derived from it.

## BUG-010 - gremlins never scans a file whose name is not UTF-8

Reported by: check-report (this hunt); also reported earlier by the hygiene hunt's convention-engines hunter.

`gremlins::tracked_files` drops non-UTF-8 paths silently (`if let Ok(s)`). Such a file is therefore never scanned by gremlins, header, textlint or manifest. That is a gap in exactly the phase that hunts deceptive bytes, since the filename itself can be the deceptive bytes. `scope::dirt` / `scope::classify_status` treat the same case as `Code`, meaning "cannot vouch" (fail-closed), while `git_paths` drops it. The hunter's policy is to fail closed: an unreadable path is a finding. The surrounding question (one tree view per run) is recorded in the hygiene platform document's git-walk entry.

## BUG-011 - `script_check` on `stream = "both"` cannot match `exact`, and `last-line` reads stderr

Reported by: check-report.

`script_check::evaluate` decodes stdout and stderr lossily and, for `Stream::Both`, joins them with `\n`. `MatchMode::Exact` on `both` can therefore match a sentinel only when stderr is empty and the configured `expect` ends where stdout does. `last-line` on `both` reads stderr's last line, not the last line the script printed. Interleaving is lost. The hunter suggests either documenting this or capturing a merged stream.

## BUG-012 - The sidecar records a child it lost track of as a clean exit

Reported by: measurement.

`sidecar::run_sidecar_with_deadline`, when `try_wait` errors and the final `wait` also fails, builds `ExitStatus::default()` under the comment "synthesize a failure status". On Unix that status is exit 0, so the run is recorded as a clean success and its wall time is stored.

## BUG-013 - The sidecar joins its pipe drains without a bound, so a measured binary that leaves a grandchild hangs brokkr

Reported by: measurement.

`sidecar::drain_pipe` runs `read_to_end` on a thread that is joined unconditionally after the child exits. A measured binary that exits normally but leaves a grandchild holding stdout or stderr (a daemon, a helper) hangs brokkr with the lock held. That is the same hang `output::settle_drains` (`DRAIN_SETTLE`) was written to fix in the other supervisor. The process-group SIGKILL happens only on stop, deadline or shutdown, never on natural exit. The two supervisors are filed in `structural.md`.

## BUG-014 - `brokkr results --compare` silently drops `--meta` and `--env`

Reported by: measurement.

`CompareFilter` has no `meta` or `env` field. `results_cmd` builds it without them, and clap does not conflict them with `--compare`. An A/B selected by `--env K=1` therefore compares unfiltered rows. A uuid query likewise silently ignores `--command`, `--mode`, `--dataset` and `--grep`. The doc comment on `query_compare` records that `--grep` was once dropped the same way, because the filter is defined twice. The hunter's proposed type-level fix (views that carry only the filters they use) is in `structural.md`.

## BUG-015 - A `--stop` run records the configured marker, not whether it fired

Reported by: measurement.

The row's `stop_marker` stores the configured spelling, not the marker that fired, and not whether any marker fired at all. A `--stop X` run whose marker never appeared records `stop_marker = X` beside a full-length wall, which is indistinguishable from a run that stopped. `stop_marker: Option<&str>` is also re-interpreted on every FIFO drain by `stop_match`, which accepts three spellings. The hunter proposes parsing it once into `StopSpec { Exact | SpanEnd | Lenient }` and recording the resolved marker as part of how the child ended (filed in `structural.md`).

## BUG-016 - `verify multi-extract` still splices the directory into JSON with `display()`

Reported by: map-projects (this hunt); the duplication was reported earlier by the hygiene hunt's pbfhogg-nidhogg hunter.

`pbfhogg/commands.rs::multi_extract_plan` builds the extract config with `serde_json`. Its comment says the directory "used to be spliced in with `display()`, which broke on a `"` or `\`". `pbfhogg/verify_multi_extract.rs::run` still builds the same JSON with `format!` and `multi_dir.display()`. It also lacks the bench path's `is_finite` check, and it computes the longitude strips with its own copy of the arithmetic. The strip geometry and config JSON are otherwise duplicated verbatim between the two files. `verify_multi_extract.rs` also builds a `multi_args` value and immediately overwrites it, under a misleading comment.

## BUG-017 - `OutputKind` is wrong for `MultiExtract`, and `OutputKind::None` would promote an empty path

Reported by: map-projects (this hunt); the naming duplication was reported earlier by the hygiene hunt's pbfhogg-nidhogg hunter.

`output_kind()` returns `ScratchDir("multi-extract")`. `scratch_output_path` and `cleanup_output`'s generic arm turn that into `<scratch>/multi-extract-<dataset>`, but the plan actually writes `<scratch>/multi-extract`. `cleanup_output` keeps working only because a hard-coded special case runs before the generic match. `"multi-extract"` and `geocode-{dataset}` are each spelled separately in `output_kind`, `build_body` and `cleanup_output`, and `BuildGeocodeIndex`'s `build_body` builds `geocode-<dataset>` itself instead of calling `scratch_output_path`. `OutputKind::None => PathBuf::new() // Should not be used.` is reachable: `promote_artifact` calls `scratch_output_path` without checking the output kind, so `--as-snapshot` on a command with no output would try to promote `""`. On the nidhogg side, `bench-`, `run-` and `hotpath-ingest-output` are spelled inline, while the dead `scratch_output_dir()` lists only the first.

## BUG-018 - *Suspected:* run mode records results rows for nidhogg `api`/`tiles` and elivagar's external tools

Reported by: map-projects.

`nidhogg/dispatch.rs` sends `MeasureMode::Run` for `Api`/`Tiles` straight into `cmd::bench_api`/`bench_tiles`, and `elivagar/dispatch.rs::run_elivagar_external` does the same for Planetiler and Tilemaker. Those are the bench paths: `HarnessContext::new(..., req.force, ...)`, with the dirty-tree gate and, unless the harness keys off `req.mode`, a results row. Three dispatchers state the opposite rule ("Run mode never stores results, so dirty tree is always fine"). The hunter asks that someone verify whether the harness suppresses the row in Run mode. Either way, whether a run is recorded is decided per dispatcher rather than by the mode.

## BUG-019 - `merge-changes` rows record the PBF as their input

Reported by: map-projects.

`InputKind::OscOnly` is used by `merge-changes`, but `run_wallclock_core` still calls `file_size_mb(&cmd_ctx.pbf_path)` and records the PBF as `input_file`, for a command that never reads it. Results rows for `merge-changes` therefore carry the wrong `input_file` and `input_mb`.

## BUG-020 - `snapshot_key_to_iso_date` accepts February 31

Reported by: map-projects.

The day check is `(1..=31).contains(&d)`, independent of the month. The three calendar implementations this sits among are recorded in the hygiene projects document's dataset-registry entry.

## BUG-021 - The lint registry accepts a pin that can never pass

Reported by: other-projects.

`lint/registry.rs::lint()` accepts any label for which `is_disposition` is true as a pin's `expected`, so `expected = "lint_error"` or `"piners_error"` loads cleanly. But `lint_corpus` fails every run that has a tool error "whatever the pin says", and bless refuses to stamp one. So the registry admits a pin that can never be satisfied. Nobody owns the question "is this a pinnable disposition?". The hunter's shape is `enum LintDisposition { AgreeClean, AgreeFlagged, Divergent(Signature), ToolError(Tool) }`, with the pin type taking only the pinnable subset. That would also remove the `Option<Signature>` "Some only when divergent" invariant on `lint::diff::LintOutcome` and `lint::ProbeResult`.

## BUG-022 - piners comparability reads a malformed or legacy selector as "debug, no args" and compares on it

Reported by: other-projects.

`corpus_db::compare::context_of(selector)` reads `debug` and `harness_args` out of the stored `selector` JSON using `unwrap_or(Value::Null)`, and `.unwrap_or(true)` for `debug`. A malformed or legacy selector silently reads as "debug, no args", so two incomparable runs compare as comparable. The `true` default is also a read-back copy of the parity default profile. The hunter's fix is to make the profile and forwarded args typed columns on the run row, written from the same `RunContext` struct that produced them.

## BUG-023 - ratatoskr frontmatter typos are silently ignored

Reported by: other-projects (this hunt), and earlier by the hygiene hunt's ratatoskr hunter.

`discover::parse_frontmatter` drops unparseable values without a word. `expected: ignore` runs the ignored reproducer as `Pass`. `ceiling: 5min` or `ceiling: 5y` falls back to the 60s `DEFAULT_CEILING`, so the script gets a different wall limit than it asked for. `preserve_data_dir: on-success-too` or `maybe` falls back to the default. An unknown `fixtures:` value is ignored, and the script then runs with no mock and no warning. The test `malformed_ceiling_falls_back_to_default` locks the ceiling behaviour in. The hunter wants parsing to return `Result<ScriptInfo, FrontmatterError>` and fail discovery, or at minimum to warn.

## BUG-024 - litehtml `visual-status` prints every non-PASS status twice

Reported by: other-projects (this hunt), and earlier by the hygiene hunt's small-benches hunter.

`sluggrs::format_status_columns` carries a fix with the comment "only ever an annotation, never a repeat ... used to render `FAIL_THRESHOLD (FAIL_THRESHOLD)`". `litehtml::format_status_columns` still has `s => format!(" ({s})")`, so `brokkr visual-status` in a litehtml project prints `FAIL_THRESHOLD (FAIL_THRESHOLD)`, `ERROR (ERROR)` and so on. The two visual layers being near-clones is recorded in the hygiene projects document.

## BUG-025 - An unnamed `brokkr bench` refuses to run on a clean `disable_toolchain` checkout

Reported by: deps-misc.

`bench_cmd::is_dirty` runs a raw `git status --porcelain` under the lock. While the lock is held, `disable_toolchain` has moved `rust-toolchain.toml` aside, so the raw status sees a deletion plus an untracked file. On every `disable_toolchain` project an unnamed `brokkr bench` therefore refuses on a spotless checkout. `git::toolchain_exclusions` was written to prevent exactly this failure in the harness. The check also counts `.brokkr/bench/` itself as dirt unless it is gitignored. The several definitions of "dirty" are recorded in the hygiene platform document.

## BUG-026 - `env::read_git_rev` abbreviates with `--short` and runs in the process cwd

Reported by: deps-misc.

It uses `git rev-parse --short HEAD`, the variable-width abbreviation that `git.rs`'s header says brokkr abolished in favour of `SHORT_HASH_LEN`. It also runs in the process cwd, not in `project_root`.

## BUG-027 - `git_dependency` drops the requested `?rev=` when a fragment is present

Reported by: deps-misc.

`GitDependencyEvent { rev, branch, tag }`: `rev` holds the resolved commit (from the fragment), while `branch` and `tag` hold the requested ref. A `?rev=X` request with resolved fragment `Y` reports `rev=Y` (`fragment.or(query_rev)`) and loses `X` entirely. The hunter's shape is `enum GitRef { Branch, Tag, Rev, DefaultBranch }` for the request, with the resolved commit in its own field.

## BUG-028 - `harness_abort` is detected from the raw outcome in two places and from the validated disposition in a third

Reported by: other-projects.

`report.rs` rendering and `integrity.rs::harness_aborts` compare `p.outcome == HARNESS_ABORT`, the raw outcome. The gate uses the validated disposition. A malformed `harness_abort` line that carries a tier is therefore treated as an abort by the first two and not by the gate. The disposition vocabulary itself is recorded in the hygiene projects document's piners-vocabulary entry.

## BUG-029 - *Suspected:* a typo in `-p` under a sweep with features gets no drop announcement

Reported by: check-plan.

`features::project` returns the identity when any selected package is not a workspace member. That is correct for cargo's own error, but it means a typo in `-p` gets no drop announcement at all. Depending on cargo's check order, the user may see cargo's feature error before the unknown-package one.

## BUG-030 - *Suspected:* `continuation::suspects` may drop terminations on a partial lane

Reported by: check-plan.

`continuation::suspects` treats only `unavailable` lanes (and superseded ones) as blind. A `partial` lane, one with unlisted binaries, can also only name suspects on an unattributed binary's stream, and that stream has no binary. The hunter asks someone to check whether those terminations are dropped.
