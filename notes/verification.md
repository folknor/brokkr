# Verification findings

Standing list of gaps in what `brokkr verify` proves or how it scales. Current
gaps only; remove an entry when it is resolved.

## apply-changes failure path holds the difference set in memory

`verify_merge::verify_pbfhogg_vs_osmium` parses the `pbfhogg diff --format
osc` between the two outputs into per-section `BTreeSet`s (`osc::OscDiff`)
and copies classified IDs into report vectors. On a pass the set is tiny; on
a catastrophic regression at planet scale it is as large as the regression.
Streaming classification with counters and bounded samples, and a disk-backed
membership test for a large input delete set, would bound it.

## osmosis is outside the reference-run path

Every `run_tool` reference run dies with brokkr and has its SIGKILL
classified as an environment outcome. osmosis runs through its own launcher
(`verify_merge::run_osmosis`, a captured `Invocation`), so a brokkr SIGKILLed
mid-merge leaves it writing into `target/verify/apply-changes/`, and an
OOM-killed JVM reaches brokkr as the wrapper script's exit code, not a
signal. It is context only, so no verdict depends on it.

## Human inspect reports scale with ordering segments

`print_inspect` and `check_sorted` capture the human `pbfhogg inspect` report
in memory. Its ordering line lists every segment, and a non-standard ordering
adds every segment's block range, so a noncanonically ordered file of planet
size could exceed the capture cap. Their callers pass outputs a command was
required to sort, plus osmium outputs and getid outputs (a getid output keeps
its input's order); a capture-limit error there fails the check rather than
passing it, but reads as a harness error. Moving them to `inspect --json`
read from a file, as `inspect_counts` now is, would remove it.

## Bench getid and getparents fixtures are Denmark IDs

`GETID_BENCH_IDS` / `GETPARENTS_BENCH_IDS` in `src/pbfhogg/commands.rs` are
fixed IDs that exist in Denmark. The verify check no longer uses them; the
bench presets do, so on another dataset those benches measure a mostly empty
selection. `pbf_frames::walk` could supply dataset-derived IDs, at the cost of
changing what the bench rows measure.
