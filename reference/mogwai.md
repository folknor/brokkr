# mogwai bench design

This page records why `brokkr mogwai` is shaped the way it is: a target
registry with no workload registry, a feature union, and counters that are
never fatal. Each section names the constraint a change must keep. How the
command is used is in `docs/projects/mogwai.md`.

## One row shape

Argv-shaped surfaces (the shipped bin) and harness-shaped surfaces (cargo
examples) share one row shape and one query surface. A registry that could
hold only argv is what once forced harness surfaces into a second "layer",
and harness surfaces are the majority of the eventual measurable surface. A
change must not reintroduce a mode or column that only one kind can fill.

`--bench` measures an external wall for both kinds. `--hotpath` and
`--alloc` run through `run_hotpath` and `capture_hotpath` in `src/measure/`,
which point the child at a report file and read it back.

## No workload registry

brokkr registers targets, never workloads. The surface to cover is the
product of commands, presets, windows, seeds, cells and flag axes, plus
every library-level loop with no command at all. That product can't be
enumerated, and a registry you must edit before asking a new question won't
survive years of questions. Harnesses take an argv for the same reason:
every surface is config-shaped, so an argument-free harness would need an
entry per shape, the same enumeration at one remove.

The predecessor workload registry had per-entry fields brokkr deliberately
doesn't accept:

| Field | Why it's absent |
|---|---|
| `timing`, `timing_reason` | They redefined what `elapsed` meant per entry: one row's was an external wall, another's internal phases with setup excluded, and no row said which. Markers plus `brokkr sidecar --durations` narrow the same way and keep the excluded setup visible as its own phase. |
| `expect_seconds` | The first stored row supersedes an estimate. The history is the expectation. |
| `runs` | A call-site flag everywhere else; how much time to spend is decided on the day. |
| `identity_counters` | It only controlled fatality; every counter is captured and diffed anyway. See "Counters are never fatal". |
| `{corpus}` and its token | Datasets under a different name, without the query surface. |
| `successor`, name-as-promise | Rows pair on the captured argv, which can't lie the way a name can. |
| `args` | The invocation belongs at the call site, captured verbatim. |

This design accepts one exposure: a harness can be invoked with an argv in a
shape nobody meant, and no registry entry prevents it. pbfhogg has the same
exposure across every command. The captured argv answers it instead of a
config constraint: a non-comparable invocation is visible in the row rather
than prevented, and reading rules turn that into a verdict.

## Feature union

A target's registered `features` are a union with what the mode and the
call site add, never replaced by them. The registration is often what makes
the target buildable: an instrumented example commonly declares
`required-features`, so dropping the list under `--bench` would fail the
build rather than produce a leaner one. The cost, documented for users, is
that a target registering `hotpath` has its `--bench` walls measured on an
instrumented build.

## Counters are never fatal

A moved counter is reported in `--compare`, never an error. A per-entry
declaration of identity-bearing counters would only control fatality, and a
gate that fires on the first legitimate win earns a bypass flag that then
gets passed out of habit: doing less work is what most optimization past the
free-lane stage is. Where a moved count does invalidate a series, such as a
seeded tape whose draw moved, mogwai bumps `TAPE_PROTOCOL_VERSION`, which is
unconditional and can't be waived per comparison.

Counters come from the winning run only, since averaging across iterations
would hide an iteration that differed rather than surface it.

## Serving path

`brokkr mogwai` doesn't measure mogwai's serving path or adapter yet. Both
are harness-shaped and must fit the target scheme when they are measured.
Excluding them on principle, as the retired workload registry did, would
write off the class mogwai's end state lives in.
