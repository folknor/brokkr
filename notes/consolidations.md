# Consolidations

One decision answered in more than one place, from the design hunt. Each entry
is the question, every site that answers it, whether those sites already
disagree, and where the single owner should live. Siblings: `structural.md`
(STR), `bugs.md` (BUG), and the hygiene documents (`hygiene-*.md`), where design
findings about mechanisms those documents already held were added to the
existing entries instead. Entries record what hunters reported; nothing has
been verified.

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

## CON-002 - "Did this run pass, fail, or blow its budget, and does that stop brokkr?" - lane-local answers

Reported by: check-exec.

One deadline verdict and one freeze/snapshot/kill sequence now serve every libtest lane, and the parallel lane's separate timeout outcome is gone. Residue:
- Pass/fail/completeness is still decided per lane: serial and cargo-threaded in `check_cmd/output.rs`, `parallel::report_runs`, `test_cmd::run_one`, and `nextest_lane` mapping its own outcomes.
- The lane verdicts still re-parse reconstructed human text (`cargo_filter::parse_test_output*`), while `accounting.rs` reconciles the same runs from the typed `ObsEvent` stream with its own reducer, so the lane verdict and the audit have two evidence bases that can disagree.
- `nextest_lane` records `StreamEnded { Eof }` unconditionally, including after an engine error.

Owner: an `ExecutionOutcome` reduced from the observation stream (shared with accounting) and one `lane_verdict(&ExecutionOutcome) -> Pass | Fail | Incomplete | BudgetBlown | Stopped`, with the reconstructed text display-only.

## CON-005 - "Is this lane cargo-threaded, and therefore unattributable?"

Reported by: check-plan, check-exec.

`test_threads: Option<u32>` is a three-state mode in disguise. `None` or `Some(1)` means serial under the watchdog, `Some(0)` means libtest's default, and `Some(n >= 2)` means n threads. The predicate `matches!(test_threads, Some(n) if n != 1)` ("cargo-threaded") is written independently at:
- `config_parts/parser.rs::validate_complete_test_threads` (load time). It re-resolves `test_threads` through its own `effective_test_threads` walk of the `extends` chain, and re-classifies lanes as serial via `e.parallel.is_none() && e.harness != Nextest && !e.doc_only`. That makes it a second implementation of both profile `extends` resolution (`profile::resolve_profile_chain`) and lane classification (`prepare::lane_kind`).
- `prepare.rs::serial_unattributable`: `!doc_only && matches!(test_threads, Some(n) if n != 1)`.
- The `phase.rs` doc-only refusal: `process_isolation || threads != 1`.
- `output.rs`, three sites, plus `effective_test_threads` parsed again in `parallel.rs` and twice in `output.rs`. Two functions are named `effective_test_threads`, one on argv and one on profiles.

They agree today. A suspected gap: `parser.rs`'s refusal of `isolation = "process"` together with `test_threads != 1` checks each profile definition alone, not the merged chain. A child that sets `test_threads = 4` and `extends` a parent with `isolation = "process"` passes load validation and resolves to an isolated lane carrying 4 threads. That is probably inert, since the isolated lane ignores it, but it is the drift a per-definition copy invites.

Owner: `enum Threads { Watchdogged, CargoDefault, Cargo(NonZeroU32 > 1) }` with `is_cargo_mediated()`, carried on the resolved lane type (filed in `structural.md`). Profiles are resolved once at load so the parser asks the resolved lane.

## CON-006 - A sweep's selection filters have five parallel encodings, and "does this lane run `#[ignore]`d tests?" has three answers

Reported by: check-plan.

A sweep's filters live as `libtest_args` (flattened argv, which also carries `--include-ignored`), `name_filters`, `cargo_test_filters` (argv `--test X` pairs), `qualified_skips` and `declared_filters`. `declared_filters` is the provenance copy, added because the argv forms are "flattened past recovery". All five are built in parallel by three constructors (`shaping_from`, `build_resolved_sweep`, `sweep_from_check_entry`), and each consumer re-parses the argv back into meaning:
- "Does this lane name a target?": `output.rs::has_target_selector` scans `cargo_test_filters` with `starts_with`, while `phase.rs`'s doc-carrier check uses `cargo_test_filters.is_empty()`.
- `coverage.rs::filter_liveness` re-reads `declared_filters` and `qualified_skips`, and trusts a comment that `libtest_args` never contains `--exact`.
- "Does this lane lift `#[ignore]`?": `prepare.rs::lane_runs_ignored`/`args_run_ignored` look at libtest args plus forwarded args, recognise `--include-ignored` and `--ignored`, and skip `--skip` values. `prepare_profile` first sets `record.include_ignored` from `args_run_ignored(sweep.libtest_args)` without the forwarded args, then overwrites it with `prepared.include_ignored`, but only for prepared lanes, so unprepared lanes carry the first answer. `coverage.rs::filter_liveness` checks `libtest_args.iter().any(|a| a == "--include-ignored")`, with no `--ignored` and no forwarded args. `output.rs` does the same string test, and `nextest_lane.rs` maps the flag itself.

These agree only because a complete claim, the only kind that runs coverage, refuses forwarded args, and profiles cannot emit `--ignored`. Owner: one `LaneFilters { skips: Vec<Filter>, only: Vec<Filter>, targets: Vec<TargetName>, include_ignored: bool }`, where each `Filter` carries its origin (`FilterOrigin::Profile(name) | Entry(name)`; today `DeclaredFilter.origin` is rendered text "because the two sources have no common address type", and coverage's `FilterLedger` keys on `origin` plus the rendered `label`). `include_ignored` is computed once with the forwarded args folded in. Argv is rendered only at the cargo boundary, and `declared_filters` stops being a second copy.

## CON-007 - "Does this lane carry doctests?"

Reported by: check-plan.

- `accounting::lane_runs_doctests`: DocOnly, or Serial with `doctests && !has_target_selector(cargo_test_filters)`.
- The complete-profile carrier check in `phase.rs`: `doc_only || (harness == Libtest && parallel_budget.is_none() && !process_isolation && cargo_test_filters.is_empty() && doctests)`.

These are two predicates written differently (`is_empty` against `has_target_selector`, an explicit field ladder against `lane_kind`) deciding the same question. The accounting doc calls its function "the one definition the plan's carrier flag and the strict shim's rustdoc admission share", but the carrier refusal in `phase.rs` is a third reader that does not use it. Owner: `lane_runs_doctests`, with the phase check calling it.

## CON-008 - "Are two sweeps the same build?" - two keys, one hashed through `Debug`

Reported by: check-plan.

`ResolvedSweep::build_shape_key` is the configured key, used for coverage grouping, accounting's `shape_id` and universes. `selection::EffectiveKey` is the effective key, used for clippy and rustdoc dedupe and for `ExecProjection` in `brokkr test`. `build_shape_key` keeps `build_packages` (its own doc says "KNOWN GAP ... phase-contract conflation"), while `EffectiveKey` keeps support requests. Universes are keyed by configured shape, so two lanes of one configured shape with different effective selections would share a universe. That is safe only because a complete claim refuses `-p`.

`BuildShapeKey` is also a bare 7-tuple: `(Vec<String>, Vec<String>, Vec<String>, Vec<(String,String)>, Vec<String>, Option<SweepProfile>, EffectiveUnification)`. Its three `Vec<String>` siblings can be swapped without a type error. `accounting::shape_id` hashes `format!("{:?}", key)` and persists the result in the on-disk plan, so identity depends on the `Debug` output of every component, which any derive change or field rename silently alters. That is harmless today because nothing reads a plan after its run, but it becomes a bug the moment a cross-run reader appears.

Owner: selection.rs mints both keys, a `DeclaredShape` (policy grouping) and an `EffectiveBuild` (dedupe and artifact identity), each a named struct with an explicit, stable encoding, or a `ShapeId` newtype minted only from one. `profile.rs` stops knowing about build identity.

## CON-009 - "Where can a package-qualified skip be applied?"

Reported by: check-plan.

This is decided in `profile::resolve_single` (isolated or nextest), in `profile::run_shaping` (isolated only, by design, since ad-hoc lanes are libtest), and implicitly by which lanes read `qualified_skips` (prepare's `list_binary`, coverage's `filter_liveness`, the nextest filterset). The rule is "lanes with per-test attribution", which is a property of the lane's execution kind. Owner: `LaneExec::applies_qualified_skips()`, asked by both resolvers.

## CON-010 - "Which invocation packages does this phase admit?" - install versus everything else

Reported by: check-plan.

`selection::admit` produces `Admission { kept, notes }` with typed rules. `InstallSelection::build` does its own `cfg.install.iter().filter(|p| cli.contains(p))` and produces no notes. One question, two answers, two output shapes. Owner: `admit`, with an `AdmissionRule::NotInstallPackage`.

## CON-011 - The obligations of a complete claim are spread over a dozen comparisons

Reported by: check-plan.

`certifies == Some(Certifies::Complete)` is evaluated at about six runtime sites (`phase.rs` five times, `install_shape::install_feature_applies`) and about six parse sites. `LaneInputs.certifying` and `AccountingPlan.certifying` are bools derived from it. Each site encodes one obligation of the claim: refuse `-p`, refuse trailing args, enumerate the universe, run install-feature in gate mode, refuse unattributable lanes. The `reject_scoped_complete` doc already speaks of "the certifies permission table", but no table exists, only scattered comparisons. Owner: a `Claim` type with named obligation methods (`refuses_scoping()`, `enumerates_universe()`, `requires_attribution()`, `runs_install_feature(mode)`).

## CON-014 - "Does this allow suppress this diagnostic?" and "which lint namespace is this name in?"

Reported by: check-report, config-cli.

The namespace is decided by string prefix in five places: `reaches_test_build` (`!cargo:: && !clippy:: && !rustdoc::`), `is_cargo_lint` (`cargo::`), `is_rustdoc_lint` (`rustdoc::`, in phase.rs), `sited_match` (strips `clippy::` from either side) and `code_allowed` (exact match, with no `clippy::` normalisation). There are also prefix tests inside `test_phase_allow_flags`. The matching rules differ. `allow = ["needless_return"]` is applied by clippy via `-A`, using rustc's own normalisation. An `allow_exact = "needless_return@src/x.rs"` matches the diagnostic code `clippy::needless_return` through the strip, while `code_allowed` would not. The difference is masked today only because `code_allowed` is consulted just for `cargo::` and `rustdoc::` codes, which have no unqualified spelling, and nothing keeps that true. `cmd_check` and `cmd_clippy` also receive `allow` and `allow_exact` as two loose slices, unbundled from `LintsConfig` in `bootstrap.rs` (`clippy_cfg.as_ref().map_or(&[][..], |c| &c.allow)` twice for each).

Owner: `LintName { tool: Rustc | Clippy | Rustdoc | Cargo, name }`, parsed at config load (for `allow`/`allow_exact`) and at ingestion (for `code`). `reaches_test_build`, "which phase judges this" and `LintName::matches(&self, code)` each get one owner, and `&LintsConfig` is passed whole.

## CON-015 - "Is this diagnostic about something the workspace does not own?"

Reported by: check-report.

The text path uses `cargo_filter::ClippyDiagnostic::is_foreign_manifest_warning`: an absolute `Cargo.toml` path that is not under `project_root`. The JSON path uses `attribute_manifest_lints` plus `is_dependency_warning`: a package id that is not among `cargo metadata`'s `workspace_members`, with a synthetic `"manifest+<path>"` marker minted inline for non-members. They disagree by construction. A path dependency inside the project root but excluded from the workspace is foreign to the JSON path and owned to the text path. A workspace member outside the root (`members = ["../x"]`) is the reverse. The test-build failure render (`output.rs` `filter_clippy_in_tree`) uses the text rule, while clippy and rustdoc use the JSON rule. The marker string is the third value of an implicit `enum Owner { Member(id), WorkspaceRoot, NonMember(path) }` encoded as a string prefix. Owner: a real `Owner` classification fed by metadata and used by both renderers. That requires the test-build path to stop being text-scraped (see the rustc text readers in `structural.md`).

## CON-022 - "How does a named config entry shadow another of the same name?" - two layering implementations

Reported by: config-cli.

- `config_parts/compose.rs::merge_entries`: works on raw TOML across include-graph layers. A later layer's named `[[array]]` entry replaces an earlier one, and unnamed entries always survive.
- `config_parts/user.rs::merge_named`: works on parsed `Vec<TextlintRule>`/`Vec<ScriptCheck>`. The user layer comes first, and a project entry replaces a user entry of the same name.

They agree today. But the user layer is really "the lowest-precedence include, restricted to three keys", and it is applied *after* parsing. That has two consequences. First, the user file's `[textlint_preset.*]` resolve only against the user's own rules, and the project's presets only against the project's, so neither layer can use the other's presets, unlike includes, which merge before preset resolution. Second, `check`'s no-config branch has to call `config::load_user()` itself and hand-assemble a 15-tuple, a third site that applies the user layer. Owner: `compose()`, with the user file as its bottom layer and the key allow-list enforced there. `merge_named` and `apply_user_layer` go away, and the no-config path composes a normal `DevConfig` from the user layer alone.

## CON-023 - "Does `[<host>] features` apply to this build?"

Reported by: config-cli.

Every `BenchContext` build now merges host features itself, and `docs/brokkr.toml.md` states which other commands apply them. Residue: `run`, `install`, `check`, `clippy`, `test` and `bench` keep their own feature selection and do not share `BenchContext` or a command descriptor, so whether host features apply is still a per-path decision rather than one policy.

## CON-024 - "Which run does this id name?" - two stores, two policies, three selection APIs

Reported by: measurement, deps-misc.

`db/run_store.rs` now owns resolution and selection across both stores; `results`, `sidecar` and `invalidate` go through it, `invalidate` selects uncapped and deletes by exact uuid. Residue: the provenance formatter in `sidecar_fmt.rs` still calls the string-prefix query helpers, which survive only for it; once it takes a resolved `RunId` they can go.

## CON-026 - "What is this command called?" - `id()` against `result_command()` in all three map-data projects

Reported by: map-projects.

Each command enum has both `id()` and `result_command()`, and they diverge: elivagar `tilegen`/`self` and `pmtiles-writer`/`pmtiles`; nidhogg `nid-ingest`/`ingest`; pbfhogg's `result_command()` is `id().to_owned()`. `bench_self.rs` and `bench_all` also file rows, so the results-DB name for the same work is chosen in several places. One test, `cat_variants_share_result_command`, guards one case pairwise. Owner: either a single `CommandIdentity { cli, results }` per variant, or renaming the results label to match the CLI id with a results-DB migration, which removes the axis entirely. The legacy names exist only because historical rows used them.
