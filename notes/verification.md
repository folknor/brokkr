# Verification findings

Standing list of gaps in what `brokkr verify` proves or how it scales. Current
gaps only; remove an entry when it is resolved.

## diff's listing content is never compared with osmium

`verify diff` holds pbfhogg's text diff to osmium through the summary's
class counts only (`compare_diff_classes` in `src/pbfhogg/verify_diff.rs`,
with a tolerated version-comparison deviation); the line counts are
informational. The listing itself is compared only with pbfhogg's own
unindexed rerun, so a fault both of its diff paths share (a wrong element
reported as modified, wrong detail in a block) passes as long as the class
counts reconcile. This holds buffered and under `--direct-io` alike.

## multi-extract never constructs identical or deliberately empty regions

`Bbox::strips` makes adjacent strips, and the mixed config appends the
dataset's polygons, which overlap the strips only where a polygon happens to
(a way crossing a region boundary still lands in several outputs, and a region
can still come out empty, but neither is guaranteed). Identical regions with
distinct outputs and a deliberately empty region are never built. Nothing
proves pbfhogg took its single-pass path rather than the per-region fallback.
Complete-ways and smart under `--config` are not checked; they run the
per-region path.
