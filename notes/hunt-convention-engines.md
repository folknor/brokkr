I didn't edit anything, run any builds or run any tests. Every finding below comes from reading the scope files, `brokkr.toml`, the three reference docs, and the parts of `config_parts/parser.rs`, `config_parts/schema.rs`, `check_cmd/phase.rs`, `scope.rs`, `rustflags.rs`, `output.rs` and `error.rs` that the scope calls into. Findings are grouped by your eight questions and are not ranked. Each one ends with how it could be enforced ("Enforce:").

## 1. One value, one owner

- **The tracked-file walk.** `gremlins::tracked_files` is the file list for header, textlint and manifest too, which is why those three modules import `gremlins`. `scope.rs` has its own `git_paths`/`ls-files`. One `check` run can run `git ls-files` up to five times. The two walks already treat non-UTF-8 paths in opposite ways (see Q6). Enforce: a single corpus module whose result is computed once per run and passed to every engine; a `[[dependency_rule]]`-style module rule could forbid engines from spawning `git` themselves.
- **"Which extensions count as docs."** Gremlins scans `rs/toml/md/js/sh`, case-sensitive (`SCANNED_EXTENSIONS`). `scope::PROSE_EXTENSIONS` is `md/markdown`, case-insensitive. So `.markdown` and `README.MD` count as prose for the markdown-only shortcut, and that shortcut runs gremlins, but gremlins never scans them. The gremlins extension list is also written out in CLAUDE.md and `docs/commands/check.md`. Enforce: one shared, case-folded table, with a test asserting every prose extension is scannable.
- **`cargo metadata`.** It is invoked from 8 modules, each with its own deserializer: `deps::CargoMetadata`, `dependency_rules::CargoMetadata`, the `Value`-based one in `build.rs`, plus the runnables, bench, direct_runtime and nextest ones. In one `check` run, the dependency_rules and publish_cycle phases each spawn `cargo metadata --no-deps` back to back. The two deserializers already differ: `dependency_rules` requires `optional`, while `deps` defaults every optional field. `run_metadata` in `deps/mod.rs` and `dependency_rules::check` are the same function written twice, down to the error string. Enforce: one metadata module plus a text rule banning `"metadata"` argv outside it.
- **The argv shown to the operator is a second copy.** `phase.rs` prints `cargo_line(... "cargo metadata --format-version 1 --no-deps (dependency rules)")` as a string literal, separate from the argv actually passed. The two agree today; nothing keeps them in step. Enforce: render the line from the argv slice.
- **Host triple.** It is computed three times: in `deps::host_triple` (fallible, runs with cwd `"."`, not the project root), in `rustflags::host_triple` (cached in a `OnceLock`, raw `Command`), and in `bench_cmd/stamp.rs`. Enforce: `deps` should call the `rustflags` one; a text rule can forbid a `"-vV"` literal outside one module.
- **Dependency-kind vocabulary** (`None` means normal, plus `"dev"`/`"build"`). It is interpreted separately in `dependency_rules` (`DependencyKind::from`, `parse_config_kind`, `as_str`: three spellings in one file), `publish_cycle` (`KIND_*` constants plus its own match), `duplicate_version`/`focus` (`kind.is_none()`) and `native_code` (`== Some("build")`). Enforce: a single serde enum on the shared metadata type.
- **Dependency-table names.** `manifest::is_dependency_table_name` has 3 names; `workspace_dep::collect_from` has 5, including the `dev_dependencies`/`build_dependencies` aliases. So `sort_dependencies`, `declared_deps` and `version_align` ignore underscore-alias tables that `workspace_dep` reads. Two TOML parsers (`toml_edit` and `toml`) read the same manifests. Enforce: one constant, and a shared manifest reader.
- **"Reverse Normal-edge adjacency plus BFS to the workspace"** is written twice (`duplicate_version::run`/`via_workspace` and `focus::emit_traces`/`chains_to_workspace`). The only thing keeping them in step is a comment in `focus.rs` ("matches the duplicate_version blame filter"). The `workspace_set` construction is repeated in 6 phase files. Enforce: methods on `CargoMetadata`.
- **`TEXTLINT_PRESET_FIELDS`** (`parser.rs`) is a hand-written list of 16 names that mirrors `TextlintRule`'s fields minus 3. No test checks the two against each other: a new rule field left off the list is silently rejected in presets. Enforce: a parity test, or a `Preset` struct derived from the same field set.
- **Stale thresholds.** `STALE_DAYS`/`ABANDONED_DAYS` are also written as "~8 months / ~2 years" in the `StaleEvent` doc and in `deps.md`. They are approximate words, so there is no divergence today.
- **Deps summary.** `deps::run` has a literal `phases_run` list of 8 names, and the `findings` sum is written out by hand next to the phase calls. `deps.md`'s "adding a phase" recipe doesn't mention either. Enforce: build both from a phase table.

## 2. Values nobody can find, change, or trust

- **Config checked when the phase runs, not at load.** Textlint regexes, `region`, the `join_wrapped_use` conflicts and zero-line windows are only checked in `textlint::compile`, at scan time. So are dependency_rule `kinds` (the schema doc even says "Validated when the phase runs"), `version_align.granularity` (checked inside `manifest::scan`) and every header/manifest glob. A typo therefore surfaces mid-run, after earlier phases have run. It never surfaces at all if the phase is skipped by `skip_phases`, and under `--textlint NAME` only the selected rules are compiled. `region`, `kinds` and `granularity` are `String`s, while `MatchMode`/`Stream`/`Stage` are serde enums, which shows the fix. `brokkr.toml.md` claims the `join_wrapped_use`+`except` error is "rejected at load time"; that is false. Enforce: serde enums, plus compiling every rule in the parser.
- **Tunables with no config surface:** `DISPLAY_CAP = 200` in textlint (it truncates silently, with no ellipsis), `TOOLCHAIN_CRATES`, `STALE_DAYS`, `ABANDONED_DAYS`, `SUPPORTED_SCHEMA`, and gremlins' `SCANNED_EXTENSIONS` (a project can't add `.py`/`.yml`). `[deps]` has exactly one knob. Nothing lists what can be tuned in this scope.
- **No injection point for the clock or the process.** `ccu::now_julian_day` reads `SystemTime` directly, and `try_run` spawns `ccu` directly. So the dedup logic and the `OutdatedComplete` marker have no test, and a pre-epoch clock silently becomes day 1970 with negative ages. `focus::tildify` reads `HOME` directly.
- **Hard-coded developer path.** `ccu`'s `INSTALL_HINT` hard-codes `~/Programs/check-updates/ccu`, one developer's directory layout, and the module doc cites the same path as where the JSON contract lives.

## 3. One channel, one implementation

- `deps` JSON output and `focus` JSON call `println!` directly (`deps/mod.rs::render_json`, `focus.rs`). That is defensible for NDJSON, but no rule marks where it is allowed. Enforce: a textlint rule forbidding `println!`/`eprintln!` in `src/**` except `output.rs`, with an `allow_marker`.
- Focus JSON drops `prefix_note` ("not in host-filtered graph", "substring matches"). A JSON consumer can't tell a fallback result from an exact one.
- The `ccu` failure names only the `outdated` phase (`PHASES[0]`), although it also skips `stale`. The text renderer prints "outdated skipped (ccu)" and says nothing about stale.
- Green lines are inconsistent. Textlint, dependency rules and script-check report counts "so ok is falsifiable"; header, manifest and gremlins print a bare `ok`. That same policy is applied in some phases and not others.
- The gremlins failure hint says "rerun with `--fix-gremlins` to rewrite all banned chars". Codepoints banned through config `ban` are scan-only, so for them the hint is false.

## 4. Errors

- **Unreadable files are silently skipped.** `let Ok(content) = read_to_string(..) else { continue }` appears in gremlins scan and fix, header, textlint and manifest, and manifest also skips files that fail to parse. A non-UTF-8 file therefore passes the header check. A Latin-1 `.md` containing a raw 0xA0 byte (exactly the garbage gremlins exists to catch) is invisible to gremlins. Nothing is logged. Enforce: report unreadable in-scope files as violations.
- **`workspace_dep` failures are swallowed.** If the root manifest fails to read or parse, the phase returns an empty set and "no findings". If a member manifest fails to parse, its inherited deps are lost, producing false "unused" findings.
- **Errors that name no subject.** `From<serde_json::Error>` produces `DevError::Build("json: ...")`, so a cargo-metadata schema mismatch reads "build: json: missing field ... line 1 column N", with no phase and no command. `cargo metadata` failures use `DevError::Build` rather than `Subprocess`, which loses the exit code.
- **User error reported as a build failure.** `brokkr deps nosuchcrate` returns `DevError::Build`, so the user's typo prints as "build: no package matching ...".
- **`deps` can change what it audits.** `deps` runs a full-resolve `cargo metadata` with no `--locked`/`--offline`, so it can rewrite `Cargo.lock` or go to the network. `deps.md` says it "Audits `Cargo.lock`".

## 5. Tests that prove nothing

- `header::current_year_is_sane` reads the wall clock and asserts `2024..2100`. It depends on the host clock and can't catch a real regression. `header::scan` has no test at all.
- No test covers the git-backed scan entry points (gremlins, header, textlint and manifest `scan`), so the wiring between globs, `exclude` and the walk is untested. Only the pure helpers are tested.
- Nothing asserts that every `GREMLINS` table entry has a `replacement()` arm. Both lists cover the same codepoints today, but a table entry added without an arm would be flagged and never fixed. Enforce: a loop test.
- There is no parity test for `TEXTLINT_PRESET_FIELDS` (see Q1).
- Wording: the test `gates_or_together_any_hit_suppresses` and `check.md` ("Multiple gates AND together") describe the same behaviour with opposite words.

## 6. Guards and claims that have stopped holding

**Checks that fail open:**
- `globs::build_set` uses `Glob::new`, whose default `literal_separator(false)` lets `*` match `/`. So `crates/*/src/**` (the preset example in `brokkr.toml.md`) also matches deeper paths, while the doc implies only `**` crosses directories. Enforce: a one-line `GlobBuilder` change, since this module is already the single owner.
- `manifest.adapter_group`: if `marker` matches no comment group, the check silently no-ops. It also compares dependency *keys*, so `foo = { package = "adapter" }` gets past it; `dependency_rules` explicitly guards against exactly that rename. `forbidden_in` names are never checked against real packages, whereas dependency_rules errors on an unknown `from`. These are two implementations of one rule with opposite validation policies.
- `version_align`: `find_dep_version` finds no version for `{ workspace = true }`, so the check does nothing in any member that inherits its deps. It only works on the root manifest.
- Entries that match nothing are silent: gremlins `exclude`, textlint `exclude`, header `exempt`, manifest `exclude`/`shape_exclude`, `workspace_dep_ignore`, and dependency_rule `except`. That is inconsistent with the parser's own rule that dead preset config is an error.
- Textlint's file count is aggregated across rules, so one rule's `paths` glob going dead is hidden whenever any other rule matches files. The docs promise that "a shrinking count" gives it away; that holds only per rule.
- Header and manifest have no count at all, so a `[header].paths` that matches nothing is simply green.
- `gremlins::tracked_files` drops non-UTF-8 paths silently. `scope::classify_status` treats the same case as "cannot vouch", which is fail-closed.
- Gremlins doesn't scan the repo's own 6 `scripts/*.py` files.

**Claims that are false today:**
- `script_check.rs` module doc says "The child is given `BROKKR_CARGO=1`". `run_one`'s own doc says that override was removed, and the code passes an empty env. The same module doc lists a "style" phase that doesn't exist.
- `textlint.rs` header: "Four bounded capabilities... These are the *only* two predicates: no arbitrary multiline". The engine now also has `region`, `join_wrapped_use`, `skip_after`, `only_if_file_matches(_above)` and four context windows. CLAUDE.md's textlint line lists only the old four predicates.
- `lex.rs`: "(and, later, logical-line joins)". Joins already exist.
- `deps/mod.rs`: "v1 phases: `duplicate_version`". There are 8 phases.
- `deps.md`:
  - It says "serde-tagged like `CheckEvent` in `src/cargo_json.rs`", but `CheckEvent` was removed.
  - It says "Shells out to `cargo metadata` once per run". It actually runs twice, plus `rustc -vV`.
  - It says "Dispatch lands in `src/main.rs`"; dispatch is in `main_parts/bootstrap.rs`.
  - It says the `OutdatedComplete` marker is something "ccu emits"; brokkr emits it.
  - It says the summary lists "the phases that ran". `phases_run` includes outdated and stale even when ccu was skipped, so "ran 8 phases" is false in that case.
- `check.md` manifest section: "today `sort_dependencies`". There are about 10 checks.
- `DependencyRule` doc: "a direct Cargo dependency that must not exist". It predates the allow polarity.
- `header.rs`/docs call it a "required file header", but `scan` accepts the text anywhere in the file (`contains`).

**This repo's own rules:**
- CLAUDE.md applies the no-line-numbers rule to "notes/, docs/ and reference/ alike", but `docs-cite-no-line-numbers` covers `docs/**` only, and only `.rs:N`. `notes/todo.md` cites `src/test_runner.rs:520` and `src/test_runner.rs:21` today.
- CLAUDE.md says "nothing durable may cite notes/". `README.md` line 303 cites `notes/sidecar.md`, and no rule covers README, CLAUDE.md or `scripts/`.
- Enforce: widen `paths` in the three existing rules.

## 7. Policy invented per call site

- **Spawning.** `ccu` (raw `Command`), `gremlins::tracked_files`, `scope::git_paths` and `rustflags::host_triple` bypass `output::run_captured*`. That means no `hold::stamp`, no `oom::protect_child` and no shutdown poll.
- **No timeouts.** `ccu` has none, so network trouble hangs `brokkr deps` indefinitely. `script_check::run_one` passes `Duration::MAX`, so one hung script kills the whole run through the phase watchdog (exit 124) instead of failing that entry.
- **Unbounded capture.** `script_check` holds stdout and stderr fully in memory, so a chatty script can grow memory without bound.
- **Four path-pattern dialects in one scope:** globset (header, textlint, manifest), directory prefix (gremlins `exclude`), trailing-`*` prefix (`workspace_dep_ignore`), and a literal `"*"` wildcard (`dependency_rule.from`).
- **Textlint's `skip_after` and `allow_marker`** each have two implementations, one per-line and one for the join pass (`skip_after_suppresses`, `use_marker_suppresses`). Comments keep them aligned.
- **Two names for one behaviour.** `except_above` and `require_above` (and the two `_below` fields) behave identically; the docs say the names differ only to show intent. That is two configuration names for one behaviour, each documented four times in `schema.rs`.

## 8. Code that is no longer load-bearing

- `ccu::PHASES[1]` is never read.
- The `(None, None) => false` arm in `dependency_rules` can't be reached, because the parser requires exactly one of `forbid`/`allow`. An `enum Polarity` would make it unrepresentable.
- `DependencyKind::Unknown`: a dependency of unknown kind is silently skipped whenever `kinds` is set.
- `globs::matches` is a one-line pass-through.
- `workspace_root`'s `#[serde(default)]` plus the `is_empty` early return is a no-op path for a field cargo always emits, and it would silently disable the phase if it ever fired.

## Bugs I tripped over

- **`lex::use_statements` and precise-capturing bounds.** The engine treats any `use` identifier as a use statement. Rust 2024's `impl Trait + use<..>` bounds therefore start a bogus statement that runs to the next depth-0 `;`. At top level that swallows the following real `use` statement and reports it at the wrong line.
- **`deps::human_age`** can print "12mo" or "1y12mo": `(days % 365) / 30` reaches 12.
- **`focus::format_source`**: a non-crates.io `sparse+` registry falls through unnormalised. `deps.md`'s list of source labels omits that case.
