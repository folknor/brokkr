# Hygiene - measurement and storage

Hygiene findings from the hunt for the shared benchmark harness (`measure.rs`, `harness_mod/`), the sidecar profiler, `results.db`/`sidecar.db`/`gate.db`, `results`/`sidecar`/`invalidate`, `git.rs` and `artefacts.rs`, plus the per-project copies of the measurement loop. Siblings: `hygiene-validation.md` (VAL), `hygiene-platform.md` (PLT), `hygiene-projects.md` (PRJ). Entries record what hunters reported; nothing has been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## MEA-005 - Small store helpers still copied

Reported by: measurement-storage, piners, ratatoskr.

Residue: table/column helper copies remain in `history.rs`, `visual/db.rs` and piners, along with piners' `as_i64` copies.

## MEA-007 - Provenance-prefix classes outside the results DB

Reported by: measurement-storage.

Residue: the `RunRow` API still takes prefixed key strings, and config parsing and the ratatoskr gate still handle prefixes themselves.

## MEA-016 - Measurement resources grow without bound

Reported by: measurement-storage, ratatoskr.

Residue: the sidecar retention choice is pending, and sample/marker/counter and output-capture buffers stay unbounded.

## MEA-017 - Measurement tests that prove nothing

Reported by: measurement-storage.

Residue: the weak artefact-drop test in `src/artefacts.rs` remains, and the gate store's `open_mem` still bypasses file opening and WAL setup.

## MEA-018 - Stale claims in measurement docs and code

Reported by: measurement-storage, pbfhogg-nidhogg, elivagar, ratatoskr.

Residue: the duplicated parent-path paragraph in `src/artefacts.rs` remains.

## MEA-020 - Dead fields, columns and migrations in the stores

Reported by: measurement-storage.

Residue: `ArtefactDir.armed` and the `GateEntry` dead-code allowance remain.
