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

## BUG-039 - pbfhogg's run path bypasses the shared run-mode dirty-tree policy

Reported by: a wave-3 fixer, laterally.

Run mode's dirty-tree allowance now lives in `context::dirty_tree_allowed`, applied by the request-aware context constructors, and nidhogg, elivagar, mogwai and dellingr route through it. Still bypassing it, passing `req.force` or a literal `true` straight to the constructors: pbfhogg (`dispatch.rs`, `cmd.rs`), sluggrs `hotpath.rs`, piners `measured.rs` and the generic hotpath in `main_parts/commands.rs`. They behave correctly today, but a change to the rule would miss them.

## BUG-040 - A `--stop` that never fired is invisible in `brokkr results`

Reported by: the wave-3 reviewer, laterally.

Results schema v20 stores the requested marker (`stop_marker_requested`) apart from the one that fired (`stop_marker`), but nothing reads the requested column, so a run asked to stop at X that ran to completion shows nothing in the single view (only `--grep` over `brokkr_args` finds it). Also check `ratatoskr_sync/bench_gate.rs`: it records through `record_result`, which passes no fired marker, so a `--stop` that fires there (if that path forwards `--stop` at all) records requested-but-not-fired.

## BUG-041 - `results` uuid views are capped by `--limit` with no notice

Reported by: the wave-3 reviewer, laterally.

`--compare` now prints how many pairs the cap cut; a uuid-prefix lookup is cut at the same default of 20 silently.
