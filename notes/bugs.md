# Bugs

Defects surfaced by the hygiene hunt (eleven read-only scopes). Hygiene findings live in `hygiene-validation.md` (VAL), `hygiene-platform.md` (PLT), `hygiene-measurement.md` (MEA) and `hygiene-projects.md` (PRJ). Nothing here has been verified unless stated; each entry records what the reporting hunter(s) read in the code, and some are explicitly marked "likely" or "inferred" by them.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## BUG-016 - A SIGKILLed brokkr can orphan serial-lane test binaries

Reported by: test-execution (remainder after two fix waves).

Graceful kill, Ctrl-C and `kill --hard` now take the whole tree down. Remaining: when brokkr dies by SIGKILL from anything else (e.g. the OOM killer), only its direct child dies via `PR_SET_PDEATHSIG`; in the serial lanes that child is cargo, so the test binary under cargo can survive. Closing it needs a cargo runner wrapper (so the test binary gets its own PDEATHSIG) or a persisted process-group record reaped at the next acquisition.

## BUG-047 - Drifted measurement-loop copies record wrong provenance (remainder)

Reported by: measurement-storage, ratatoskr, piners.

Profile, features, iterations, measurement start, captured env, run info and `dirty` sidecar retention are fixed for sync bench and piners. Remaining: `gate.db` has no features column (`db/gate.rs` schema); sync bench still drops the sidecar data of an iteration whose child succeeded but whose log write or `summary.json` parse then failed. The loop duplication itself is MEA-001.

## BUG-048 - `run_distribution` stores integer milliseconds

Reported by: measurement-storage (remainder).

External and hotpath runs now record exact `elapsed_us` and round. `run_distribution` still stores whole milliseconds by schema, so nidhogg API queries under 0.5 ms record 0; it needs a microsecond distribution.
