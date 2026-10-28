# Verification findings

Standing list of gaps in what `brokkr verify` proves or how it scales. Current
gaps only; remove an entry when it is resolved.

## Bench getid and getparents fixtures are Denmark IDs

`GETID_BENCH_IDS` / `GETPARENTS_BENCH_IDS` in `src/pbfhogg/commands.rs` are
fixed IDs that exist in Denmark. The verify check no longer uses them; the
bench presets do, so on another dataset those benches measure a mostly empty
selection. `pbf_frames::walk` could supply dataset-derived IDs, at the cost of
changing what the bench rows measure.
