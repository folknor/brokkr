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

## BUG-043 - Config tests read the developer's real user-wide config

Reported by: the wave-4 reviewer, laterally.

`compose()` now always folds in the machine's user-wide `brokkr.toml` (XDG-resolved, `BROKKR_USER_CONFIG` overrides), so the compose, provenance and dellingr/mogwai `config::load` fixture tests are no longer hermetic: a developer whose user config defines textlint rules can fail them. The test in `config_parts/tests.rs` that sets `BROKKR_USER_CONFIG` with `set_var` is process-global and races other tests on the parallel lane unless it holds the process-global test lock. Pass the user layer into `compose` explicitly (or give tests an override that is not the process env).

## BUG-044 - *Suspected:* glob warnings may land in `check --json` stdout

Reported by: the wave-4 reviewer, laterally.

Unmatched-scope globs now warn through `output::warn`, which writes to stdout. `check --json` promises stdout carries only the trailer; confirm whether these (and the existing check warnings) are routed away under `--json`.

## BUG-045 - *Suspected:* a legacy gate.db profile spelling blocks every gate run against it

Reported by: the wave-4 reviewer, laterally.

The ratatoskr gate identity now refuses a baseline row whose profile string `CargoProfile::from_db` cannot parse. Earlier code stored `"debug"`/`"release"`; if any older row holds another spelling, every gate run against that baseline becomes incomparable. Check what existing `gate.db` rows hold and accept the legacy spellings if needed.

## BUG-046 - The results v20 backfill has no migration test

Reported by: the wave-4 reviewer, laterally.

`migrate_v19_to_v20` copies `stop_marker` into `stop_marker_requested` and clears the fired column; no test asserts that on a pre-v20 database.

## BUG-042 - `brokkr env` parses the user config itself and hides parse errors

Reported by: a wave-4 fixer, laterally.

The user layer now composes beneath project includes (so a user rule can reference a project preset), but `env.rs`'s `user cfg:` line still parses the user file on its own and swallows errors. A valid user rule that names a project preset fails that private parse, and the line then reports the layer as absent. It should report from the same composed load the rest of brokkr uses.
