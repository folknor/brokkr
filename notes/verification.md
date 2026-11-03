# Verification findings

Standing list of gaps in what `brokkr verify` proves or how it scales. Current
gaps only; remove an entry when it is resolved.

## the apply-changes checks FAIL on unsorted input at a documented refusal

`verify diff` (and, by the same first step, `verify merge` and `verify
derive-changes`) runs pbfhogg `apply-changes` on the input, which refuses a
base without `Sort.Type_then_ID` by contract (exit 2, "requires a sorted
base PBF"); `pbfhogg diff` refuses such input too. On the denmark `unsorted`
snapshot `verify diff` reports this as a FAIL of pbfhogg, measured on bygg;
merge and derive-changes were not run. These checks follow neither the
osmium-sorted-copy route the extract, multi-extract and getid checks take nor
a cannot-compare refusal naming the unsorted input.

## multi-extract never constructs identical or deliberately empty regions

`Bbox::strips` makes adjacent strips, and the mixed config appends the
dataset's polygons, which overlap the strips only where a polygon happens to
(a way crossing a region boundary still lands in several outputs, and a region
can still come out empty, but neither is guaranteed). Identical regions with
distinct outputs and a deliberately empty region are never built. Nothing
proves pbfhogg took its single-pass path rather than the per-region fallback.
Complete-ways and smart under `--config` are not checked; they run the
per-region path.
