# Verification findings

Standing list of gaps in what `brokkr verify` proves or how it scales. Current
gaps only; remove an entry when it is resolved.

## multi-extract fails on an unsorted input for a reason outside pbfhogg

`verify multi-extract --snapshot unsorted` (denmark, 2026-11-03) fails before
comparing anything: an extract of an input without `Sort.Type_then_ID` does
not claim it either, and `pbfhogg diff` refuses such a strip with exit 2. The
strip order verdict is also the sorted-input one. `verify extract` meets the
same input by judging the header claim and comparing through osmium-sorted
copies; multi-extract could do the same per strip. On such an input the
single-pass path declines and both sides run the per-region fallback, so the
check would compare that fallback with itself.

## multi-extract regions are adjacent strips only

`Bbox::strips` makes adjacent strips, so overlapping or identical regions with
distinct outputs and a deliberately empty region are never constructed (a way
crossing a strip boundary still lands in several outputs, and a strip can
still come out empty, but neither is guaranteed). Nothing proves pbfhogg took
its single-pass path rather than the per-region fallback. Complete-ways and
smart under `--config` are not checked; they run the per-region path.
