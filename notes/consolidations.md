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

## CON-008 - "Are two sweeps the same build?" - two keys, one hashed through `Debug`

Reported by: check-plan.

`ResolvedSweep::build_shape_key` is the configured key, used for coverage grouping, accounting's `shape_id` and universes. `selection::EffectiveKey` is the effective key, used for clippy and rustdoc dedupe and for `ExecProjection` in `brokkr test`. `build_shape_key` keeps `build_packages` (its own doc says "KNOWN GAP ... phase-contract conflation"), while `EffectiveKey` keeps support requests. Universes are keyed by configured shape, so two lanes of one configured shape with different effective selections would share a universe. That is safe only because a complete claim refuses `-p`.

`BuildShapeKey` is also a bare 7-tuple: `(Vec<String>, Vec<String>, Vec<String>, Vec<(String,String)>, Vec<String>, Option<SweepProfile>, EffectiveUnification)`. Its three `Vec<String>` siblings can be swapped without a type error. `accounting::shape_id` hashes `format!("{:?}", key)` and persists the result in the on-disk plan, so identity depends on the `Debug` output of every component, which any derive change or field rename silently alters. That is harmless today because nothing reads a plan after its run, but it becomes a bug the moment a cross-run reader appears.

Owner: selection.rs mints both keys, a `DeclaredShape` (policy grouping) and an `EffectiveBuild` (dedupe and artifact identity), each a named struct with an explicit, stable encoding, or a `ShapeId` newtype minted only from one. `profile.rs` stops knowing about build identity.

## CON-010 - "Which invocation packages does this phase admit?" - install versus everything else

Reported by: check-plan.

`selection::admit` produces `Admission { kept, notes }` with typed rules. `InstallSelection::build` does its own `cfg.install.iter().filter(|p| cli.contains(p))` and produces no notes. One question, two answers, two output shapes. Owner: `admit`, with an `AdmissionRule::NotInstallPackage`.

## CON-011 - The obligations of a complete claim are spread over a dozen comparisons

Reported by: check-plan.

`certifies == Some(Certifies::Complete)` is evaluated at about six runtime sites (`phase.rs` five times, `install_shape::install_feature_applies`) and about six parse sites. `LaneInputs.certifying` and `AccountingPlan.certifying` are bools derived from it. Each site encodes one obligation of the claim: refuse `-p`, refuse trailing args, enumerate the universe, run install-feature in gate mode, refuse unattributable lanes. The `reject_scoped_complete` doc already speaks of "the certifies permission table", but no table exists, only scattered comparisons. Owner: a `Claim` type with named obligation methods (`refuses_scoping()`, `enumerates_universe()`, `requires_attribution()`, `runs_install_feature(mode)`).

## CON-014 - "Does this allow suppress this diagnostic?" and "which lint namespace is this name in?"

Reported by: check-report, config-cli.

A borrowed `LintName`/`LintTool` in `config_parts/schema.rs` now owns namespace classification, test-build eligibility and blanket/sited matching. Residue: `allow`/`allow_exact` in config and `code` on diagnostics are still strings parsed at each use rather than at load and ingestion (needs an owned representation built in `parse_lints`), and `cmd_check`/`cmd_clippy` still receive `allow` and `allow_exact` as two loose slices rather than `&LintsConfig`. Also, `SitedAllow::parse` claims to validate relative paths but accepts absolute ones.

## CON-015 - "Is this diagnostic about something the workspace does not own?"

Reported by: check-report.

The text path uses `cargo_filter::ClippyDiagnostic::is_foreign_manifest_warning`: an absolute `Cargo.toml` path that is not under `project_root`. The JSON path uses `attribute_manifest_lints` plus `is_dependency_warning`: a package id that is not among `cargo metadata`'s `workspace_members`, with a synthetic `"manifest+<path>"` marker minted inline for non-members. They disagree by construction. A path dependency inside the project root but excluded from the workspace is foreign to the JSON path and owned to the text path. A workspace member outside the root (`members = ["../x"]`) is the reverse. The test-build failure render (`output.rs` `filter_clippy_in_tree`) uses the text rule, while clippy and rustdoc use the JSON rule. The marker string is the third value of an implicit `enum Owner { Member(id), WorkspaceRoot, NonMember(path) }` encoded as a string prefix. Owner: a real `Owner` classification fed by metadata and used by both renderers. That requires the test-build path to stop being text-scraped (see the rustc text readers in `structural.md`).

## CON-023 - "Does `[<host>] features` apply to this build?"

Reported by: config-cli.

Every `BenchContext` build now merges host features itself, and `docs/brokkr.toml.md` states which other commands apply them. Residue: `run`, `install`, `check`, `clippy`, `test` and `bench` keep their own feature selection and do not share `BenchContext` or a command descriptor, so whether host features apply is still a per-path decision rather than one policy.

## CON-024 - "Which run does this id name?" - two stores, two policies, three selection APIs

Reported by: measurement, deps-misc.

`db/run_store.rs` now owns resolution and selection across both stores; `results`, `sidecar` and `invalidate` go through it, `invalidate` selects uncapped and deletes by exact uuid. Residue: the provenance formatter in `sidecar_fmt.rs` still calls the string-prefix query helpers, which survive only for it; once it takes a resolved `RunId` they can go.

## CON-033 - A sweep's lane kind and filters exist twice: typed values and argv projections

Reported by: the wave-6 reviewer, laterally.

Resolved sweeps now carry typed `LaneExec`, `Threads` and `LaneFilters`, but the old fields (`libtest_args`, `name_filters`, `cargo_test_filters`, `qualified_skips`, `process_isolation`, `parallel_budget`, `test_threads`, `declared_filters`) survive as compatibility projections because readers remain: `check_cmd/nextest_lane.rs` builds its filterset from the argv projections and `test_threads`, `parallel.rs` derives threads from `libtest_args`, and `accounting/plan.rs::lane_runs_doctests` tests targets through `cargo_test_filters`. `profile.rs`'s `lane_exec()`/`lane_filters()` fall back to rebuilding from the projections when the typed value is absent, so a sweep whose two forms disagree answers from whichever path it hits (the `brokkr test` filter clearing had exactly that bug). Migrate the readers, then delete the projections and the fallbacks so the typed values are the only source. Related: `sidecar.db`'s own `command` column was not renamed when results.db migration v21 moved elivagar and nidhogg rows to their CLI ids, so historical sidecar sessions keep the old names.
