# Verification findings

Standing list of gaps in what `brokkr verify` proves or how it scales. Current
gaps only; remove an entry when it is resolved.

## check-refs FAILs nodes in ways wherever a missing node is referenced twice

Measured on bygg, `verify check-refs` on the denmark `ways-only` snapshot:
pbfhogg reports 49285109 missing node refs in ways, osmium 60832065. pbfhogg
counts distinct missing IDs, osmium every reference, and pbfhogg's report
carries no occurrence figure for this class (it does for relation members),
so the ways-only comparison and the with-relations nodes-in-ways comparison
FAIL on any input where a missing node is referenced more than once; denmark
has none missing, so it passes. Waits on pbfhogg#45 (report the occurrence
count, or document the deviation); with an occurrence figure the check should
compare it as it does for relation members.

## multi-extract never constructs identical or deliberately empty regions

`Bbox::strips` makes adjacent strips, and the mixed config appends the
dataset's polygons, which overlap the strips only where a polygon happens to
(a way crossing a region boundary still lands in several outputs, and a region
can still come out empty, but neither is guaranteed). Identical regions with
distinct outputs and a deliberately empty region are never built. Nothing
proves pbfhogg took its single-pass path rather than the per-region fallback.
Complete-ways and smart under `--config` are not checked; they run the
per-region path.
