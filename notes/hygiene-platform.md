# Hygiene - platform and cross-cutting

Hygiene findings from the hunt for process control (lock, hold, guard, strays, shutdown, toolchain, worktrees, builds, runnables, `bench`), config/CLI/bootstrap (config parsing, CLI schema, project detection, output, errors, history, tools, preflight, env, man), and every theme that recurred across scopes. Where a theme has sites inside project code, they are listed here rather than in `hygiene-projects.md`. Siblings: `hygiene-validation.md` (VAL), `hygiene-measurement.md` (MEA), `hygiene-projects.md` (PRJ). Entries record what hunters reported; nothing has been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## PLT-002 - brokkr enforces very little of its own convention on itself

Reported by: every hunter (check-core, process-control, config-cli-bootstrap, measurement-storage, pbfhogg-nidhogg, ratatoskr, piners, small-benches).

Residue: no broader print/panic/indexing/expect denies were added.

## PLT-004 - Output bypasses the output channel

Reported by: config-cli-bootstrap, process-control, measurement-storage, ratatoskr, piners, test-execution, pbfhogg-nidhogg, elivagar, convention-engines.

- `output.rs` claims "All output goes to stdout (stderr reserved for panics only)" and "every prefixed printer has to go through [the renderer]". Only `emit()` does; `lock_msg`, `bench_msg`, `hotpath_msg`, `verify_msg`, `download_msg`, `verify_summary`, `history_msg` and the per-project `ratatoskr_msg`, `corpus_msg`, `lint_msg`, `sluggrs_msg` etc. `println!` directly, skipping the run log added in 4db2821, status-line clearing and (for `lock_msg`) quiet mode. So the reap's "SIGKILL sent to …", drain messages, "lock acquired after …", all `guard`/`strays` output, ratatoskr and piners output never reach the run log.
- stderr writes: `sidecar_msg`, `record_history` (`eprintln!("[history] warning: ...")`), `man::write_stdout`, `tools::download_file`, `lockfile::publish`/`invalidate_mutable_metadata`/`invalidate_metadata` (`eprintln!("[lock] warning: …")`), `print_run_info` (`eprintln!("[sidecar] …")`, `eprintln!("[error] …")`), nidhogg `ingest.rs`/`update.rs` (`eprint!`/`print!`, captured then dumped so "progress" appears only after exit). "stored in results.db" (stdout) and "stored in sidecar.db" (stderr) land on different streams.
- `[result]` has two owners: `emit_result_lines` via `output::result_msg`, `force_emit_result_lines` raw `println!`; `record_result` also `println!("{short}")`.
- ~220 direct `println!`/`eprintln!` outside `output.rs` (sidecar_fmt 46, test_cmd 21, env 14, …); some are legitimate data output (deps JSON, `outline`, query tables in `corpus_query.rs`/`lint/query.rs`).
- `inspect.rs`, `diag.rs`, `svg.rs`, `bench_pmtiles.rs`, `bench_node_store.rs` each hand-roll capture → `print!`/`eprint!` → `check_success`; their headers claim output is "streamed directly".
- `compare_tiles` mixes `bench_msg` census lines with raw `println!`; `pmtiles::run` prints errors with `println!`.

Enforcement proposed: clippy `print_stdout`/`print_stderr` or `disallowed-macros` with a per-file `#[allow]` for data printers; or a textlint rule `println!\("\[` outside `src/output.rs`.

## PLT-005 - Operator text formatting is invented per site

Reported by: config-cli-bootstrap, ratatoskr, piners, check-core, process-control, measurement-storage, small-benches, elivagar.

- The "10-char prefix column" is broken by `[download] `, `[litehtml] `, `[sluggrs] `, `[ratatoskr] `, `[sidecar] `, `[history] `. `artefacts::emit_clean_hint` hard-codes `[ratatoskr]` in a module shared with piners.
- Warnings written as fake tags or plain text: ratatoskr `"  [warn] ..."` five times plus inside `missing_baseline_error`; piners `corpus_msg("warning: ...")` in `report.rs`, `bless.rs`, `cmd.rs`; stale-guard and guard-status problems as `lock_msg("WARNING: …")` rather than `output::warn`; no warn-level rule (record_history, man, tools each pick their own).
- Levels inconsistent: non-fatal worktree retention/bookkeeping failures are `output::error` while the overage is `output::warn`; toolchain "moved aside" is quiet-suppressible `build_msg`; `print_run_info`'s `[error]` goes to stderr while `output::error` goes to stdout.
- `(s)` plural hedges throughout ratatoskr ("gate(s)", "script(s)", "iteration(s)", "row(s)") and piners although `output::count` exists; `service_suite` hand-rolls plurals.
- One flow, several prefixes: the ratatoskr bench loop mixes `[bench]` and `[ratatoskr]`; the build line is `[harness]`; tools.rs reports downloads via `verify_msg` (osmosis) and `bench_msg` (JDK/planetiler) while `download_msg` goes unused.
- The "NOTE: alloc profiling" banner exists eight times and has diverged (`--` vs `-` in nidhogg/elivagar).
- Duration formatting: `fmt_wall` vs `lockfile::format_duration` (the latter used by the watchdog); service prints `412ms`, sync `0.4s`.

Enforcement proposed: a prefix table with a width test; textlint bans on `\[warn\]` literals outside `output.rs` and `\(s\)` in string regions; one banner constant.

## PLT-006 - Subprocess spawning bypasses the choke points

Reported by: process-control, config-cli-bootstrap, ratatoskr, pbfhogg-nidhogg, convention-engines, elivagar.

Residue: a raw osmosis capture remains in `src/pbfhogg/verify_merge.rs`.

## PLT-019 - Build profile is a bare string or bool

Reported by: process-control, ratatoskr, test-execution, piners.

Residue: harness profile resolution is still bool-based at the ratatoskr/piners callers, and the DB writers that record `Release` regardless are unchanged.

## PLT-021 - `brokkr.toml` is parsed more than once per invocation

Reported by: config-cli-bootstrap, process-control.

Residue: `git::collect` still recomposes config for callers that only pass a repository path (`collect_with_config_paths` is the provenance-carrying form); `history_cmd.rs` re-runs `project::detect()` after startup and then calls `git::collect`, and other modules call `git::collect` too.

## PLT-023 - "Which command runs in which project" has two owners

Reported by: config-cli-bootstrap, small-benches, elivagar.

Residue: handler-side `project::require` checks remain in `elivagar/cmd.rs`, `litehtml/cmd.rs` and `sluggrs/cmd.rs` (the `verify` nested-variant checks are deliberate and commented).

## PLT-024 - Project name ↔ variant mapping is duplicated

Reported by: config-cli-bootstrap, ratatoskr, pbfhogg-nidhogg.

Residue: `Project::Other` still uses `Box::leak` in `config_parts/parser.rs`. Removing it means changing `Project::Other`'s `&'static str` (which keeps `Project` `Copy`) in `project.rs` together with the parser.

## PLT-025 - Configuration is validated at use instead of at load

Reported by: config-cli-bootstrap, process-control.

Residue: project-specific string validation is still at use time.

## PLT-030 - External tools: unpinned, unverified, re-discovered per site

Reported by: config-cli-bootstrap, elivagar.

Residue: nidhogg's separate curl timeout contract and mutable map-data URLs remain.

## PLT-037 - Secrets and personal data can reach durable logs

Reported by: config-cli-bootstrap, check-core.

Residue: raw script-check output is rendered unsanitized in `check_cmd/conventions.rs`.

## PLT-053 - Malformed external JSON is skipped or misclassified

Reported by: the wave-1 reviewer and a wave-2 fixer, laterally.

`ContractViolation` now covers cargo metadata. Residue: `cargo_json.rs` and two parsers in `build.rs` silently skip malformed JSON lines, so cargo output can be dropped unnoticed.

## PLT-054 - history.db argv redaction over-redacts

Reported by: the wave-2 reviewer, laterally.

`history::redact_argv` lets a bare positional whose text contains `session`/`token`/`auth` swallow the next argument, and `sensitive_name`'s substring match also hits `author`, `oauth` and `authority`. `history::redact_secret_argv` (used for results.db) is the narrower credential-names-only rule; history should adopt it or share its predicate.

## PLT-055 - The guard-decision probe builds into the shared target dir

Reported by: the wave-2 reviewer, laterally.

`scripts/guard-decision-probe.py` runs a plain `cargo build --bin brokkr-rustc-guard` into `target/` inside check's script_check stage, without check's cargo env (`CARGO_CACHE_RUSTC_INFO=0`, `[lints] allow` rustflags). A cold `target/debug` compiles the full dependency set under the phase ceiling. A dedicated target dir, or reusing the artifact check already builds, would bound it.

## PLT-051 - Decisions and leftovers from the command-spec wave

Reported by: the wave-9 reviewer.

Residue: the exhaustive dispatch tables in `command_input.rs` (the bootstrap ones are commented as deliberate one-arm-per-command routing).
