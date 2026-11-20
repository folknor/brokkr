# Hygiene findings

Current gaps only: remove an entry when it is resolved, and delete the file
once none remain. An entry closed with no action owes a comment at the code
sites it names, so the finding is not hunted again.

## Operator path lines are not all on `output::path`

`output::path` (src/output/paths.rs) prints a project path relative to the invocation directory. Some output lines use it; most `DevError` text and the remaining `.display()` sites (install steps, guard, reservations, registry errors, fixture lint, nextest lane, rustflags, and others) still print absolute. Nothing enforces the helper: `.display()` is also correct for argv, stored values and foreign paths, so a textlint ban would need per-site exemptions. Options: migrate per module as touched, or a typed message representation that keeps paths as `Path` until rendering.

## `kill --hard` and a leader killed without its group

When a recorded leader is gone, `kill --hard` sweeps its group's members by
acquisition cohort. A live leader whose pgid lookup was lost, so it was
signalled through its pidfd alone, counts as delivered, and its group never
enters that sweep. Race-only; the cohort marker would also support a wider
sweep of every process carrying this acquisition's `BROKKR_EXEC`, in any
group.
