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

## PLT-005 - Operator text formatting is invented per site

Reported by: config-cli-bootstrap, ratatoskr, piners, check-core, process-control, measurement-storage, small-benches, elivagar.

Residue (cosmetic, deliberately not taken): the 10-char prefix column is broken by longer tags (`[download] `, `[litehtml] `, `[sluggrs] `, `[ratatoskr] `, `[sidecar] `, `[history] `), and `(s)` plural hedges remain in ratatoskr and piners although `output::count` exists.

## PLT-019 - Build profile is a bare string or bool

Reported by: process-control, ratatoskr, test-execution, piners.

Residue: the DB writers that record `Release` regardless of the actual profile are unchanged.

## PLT-058 - build_purpose and check_cmd depend on each other

Reported by: the wave-4 reviewer, laterally.

- `build_purpose/selection.rs` (about 2.4k lines) is mostly check/test lane eligibility and needs check's `Lanes`, `LaneIdx`, `Phase`, `TestBinary`, `cargo_selection_args` and `install_feature_applies`, while `check_cmd` glob re-exports `selection` and `environment` back. `ReproLine::parallel_binary` is check-lane repro rendering. Keep lane eligibility in `check_cmd`; move only package/feature request construction (`ResolutionRun`, `SupportRequest`, projection) into `build_purpose`.
- `build_purpose/features.rs` and `selection.rs` still wrap their bodies in an inner module (`routing`, `plan`), a leftover of the include! era; flatten them.
- `Features::{Verbatim, Joined, Projected}` keeps three renderings; Verbatim and Joined differ when a value contains whitespace. `sweep_args` always uses `Purpose::Check`; the real env axis is runtime-env yes/no, not Clippy vs Check.
- `Profile::sweep()` panics on `Profile::Bench` (unreachable by construction); a typed `profile_for_sweep` would remove the panic.
- nidhogg `run_curl_body`/`bench_api`: a non-deadline ending with no exit status (Interrupted, WaitFailed) becomes `DevError::Verify` instead of reading as an interrupt; `CapturedOutput::check_success` already maps these.
- `cargo_json` skips unknown diagnostic levels with no trace at all.
- The 12 Natural Earth S3 versionIds and XXH128 pins were never exercised by the gate; run `download-natural-earth` once on a networked host.

## PLT-057 - Natural Earth treats a present `.shp` as a complete layer

Reported by: a wave-4 fixer, laterally.

`src/elivagar/download_natural_earth.rs` skips a layer when its `.shp` exists without checking the other shapefile components (`.shx`, `.dbf`, `.prj`), so an interrupted extraction is never repaired.
