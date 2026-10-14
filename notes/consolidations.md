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

## CON-014 - "Does this allow suppress this diagnostic?" and "which lint namespace is this name in?"

Reported by: check-report, config-cli.

A borrowed `LintName`/`LintTool` in `config_parts/schema.rs` now owns namespace classification, test-build eligibility and blanket/sited matching. Residue: `allow`/`allow_exact` in config and `code` on diagnostics are still strings parsed at each use rather than at load and ingestion (needs an owned representation built in `parse_lints`), and `cmd_check`/`cmd_clippy` still receive `allow` and `allow_exact` as two loose slices rather than `&LintsConfig`. Also, `SitedAllow::parse` claims to validate relative paths but accepts absolute ones.

## CON-015 - "Is this diagnostic about something the workspace does not own?"

Reported by: check-report.

A metadata-backed `OwnerIndex` in `src/diagnostic.rs` tells members, the root manifest and non-members apart, and the test-build text renderer uses it. Residue: the JSON clippy/rustdoc phase in `check_cmd/phase.rs` still classifies with `is_dependency_warning` and mints the synthetic `"manifest+<path>"` package id for manifest lints; it should classify through `OwnerIndex`.

## CON-023 - "Does `[<host>] features` apply to this build?"

Reported by: config-cli.

Every `BenchContext` build now merges host features itself, and `docs/brokkr.toml.md` states which other commands apply them. Residue: `run`, `install`, `check`, `clippy`, `test` and `bench` keep their own feature selection and do not share `BenchContext` or a command descriptor, so whether host features apply is still a per-path decision rather than one policy.

## CON-033 - Residual second sources beside the typed lane and command identity

Reported by: the wave-6 and wave-7 reviewers, laterally.

The sweep argv projections and the duplicate `harness` field are gone; every reader uses `LaneExec`/`LaneFilters`. Residue: `sidecar.db` has its own `command` column, which results.db migration v21 did not rename, so historical sidecar sessions keep `self`, `pmtiles` and `ingest` while new ones use the CLI ids.

## CON-034 - Two dependency-warning rules for one question

Reported by: the wave-7 reviewer, laterally.

`check_cmd/phase.rs::is_dependency_warning` (a `members` map plus `package_id`) and `Diagnostic::is_dependency_warning` (the metadata-backed `OwnerIndex`, which also classifies by manifest path and site) can disagree on the same warning, so the clippy/rustdoc phases and the text filter (`filter_clippy_with_owners`, test phase) may differ; `attribute_manifest_lints` duplicates `OwnerIndex::classify_path`'s manifest rule. Fold the phases onto `OwnerIndex`. (This is the remaining half of the ownership consolidation filed above.)

## CON-035 - Transitional string facades with no end date

Reported by: the wave-7 reviewer, laterally.

Compatibility paths kept while call sites migrate: `lockfile.rs` re-exports for the old process helpers; `profile::DeclaredFilter`; `cargo_json::DiagnosticEvent` (an alias of `Diagnostic`); and `preflight::digest`'s `compute_xxh128`, `compute_xxh128_tree`, `cached_xxh128` and `verify_file_hash` string spellings, still used by piners, pbfhogg download, the bench stamp and the harness. `FileDigest` also carries a redundant `normalized: String` only to `Deref<Target = str>`, a stringly escape hatch around the typed digest. Migrate the callers and delete the facades.
