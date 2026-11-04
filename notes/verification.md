# Verification findings

Standing list of gaps in what `brokkr verify` proves or how it scales. Current
gaps only; remove an entry when it is resolved.

## cat, tags-filter, add-locations-to-ways and check-refs FAIL on unsorted input

Measured on bygg, `verify all` on the denmark `unsorted` snapshot: `cat` and
`tags-filter` fail at `pbfhogg diff` (exit 2, "Old PBF does not declare
Sort.Type_then_ID") because they diff pbfhogg's output, which rightly claims
no sort on unsorted input, directly rather than through an osmium-sorted copy.
`add-locations-to-ways` fails the same way on osmium's output; its stated
domain is sorted input, yet the sparse engine takes unsorted input and so goes
unverified there, while the external engine refuses it. `check-refs` fails
because `osmium check-refs` refuses out-of-order input ("Node IDs out of
order") and its counts then do not parse. None follows the osmium-sorted-copy
route extract, multi-extract and getid take, nor the refusal route merge,
derive-changes, diff and renumber take (`verify_input_order`).

## multi-extract never constructs identical or deliberately empty regions

`Bbox::strips` makes adjacent strips, and the mixed config appends the
dataset's polygons, which overlap the strips only where a polygon happens to
(a way crossing a region boundary still lands in several outputs, and a region
can still come out empty, but neither is guaranteed). Identical regions with
distinct outputs and a deliberately empty region are never built. Nothing
proves pbfhogg took its single-pass path rather than the per-region fallback.
Complete-ways and smart under `--config` are not checked; they run the
per-region path.
