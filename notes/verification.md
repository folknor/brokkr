# Verification findings

Standing list of gaps in what `brokkr verify` proves or how it scales. Current
gaps only; remove an entry when it is resolved.

## extract membership is not adjudicated

`verify extract` gates order and content on shared IDs only. Which elements
each strategy includes is reported, not judged, because pbfhogg contracts no
per-strategy membership against osmium (pbfhogg ticket filed with the germany
Berlin-bbox counts: simple 628 pbfhogg-only relations; complete 873
osmium-only relations; smart 222,799 pbfhogg-only nodes plus the same 873
relations; zero content mismatches in all three). Once pbfhogg states its
inclusion rules, either gate the explained classes the way `verify merge`
exempts its delete set, or add small fixtures with literal expected
memberships.

## extract source preservation is unchecked

Nothing proves every element pbfhogg's extract emits exists unchanged in the
input, so a corrupted or invented element only pbfhogg emits passes. Diffing
output against input in text or OSC writes every input-only element, the whole
dataset at planet scale; a bounded check needs a pbfhogg primitive that
reports only one side (old-only and modified) of a diff.

## multi-extract regions are adjacent strips only

`Bbox::strips` makes adjacent strips, so overlapping or identical regions with
distinct outputs and a deliberately empty region are never constructed (a way
crossing a strip boundary still lands in several outputs, and a strip can
still come out empty, but neither is guaranteed). Nothing proves pbfhogg took
its single-pass path rather than the per-region fallback. Complete-ways and
smart under `--config` are not checked; they run the per-region path.
