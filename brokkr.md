# brokkr - corpus redesign contract

Companion: `elivagar.md`. This is what **brokkr** must become, written at the
contract level: what it owns, what it guarantees, and what it consumes from
elivagar. It says nothing about how the code is structured.

## The invariant brokkr must hold

**brokkr owns the gate.** Every line of adjudication code, every baseline
semantic, every verdict. It links the elivagar crate and decodes archives
in-process through elivagar's readers; how that dependency is managed is
brokkr's build concern and outside this contract.

The knowledge line is one-directional: brokkr may know everything about
elivagar; elivagar knows nothing about brokkr, reads no baseline, and learns
no verdict. After this redesign, brokkr is the only place that knows a
committed reference exists.

## What moves in

The commands move whole - not dissolved into a shell protocol, but rewritten
as native brokkr code over the linked crate. The user-facing spellings
already exist (the 2026-07-24 wrapper landing) and survive unchanged; they
stop shelling out:

- `brokkr pmtiles-corpus check | bless | render | render-manifest | rings |
  mutate`
- `brokkr regress` (including `--overlay` attribution)
- `brokkr compare-tiles` - a brokkr command already, but today implemented by
  building an elivagar cargo example (`examples/compare_tiles.rs`) and
  shelling to it. It goes native over the linked crate and the example is
  retired; it was never an elivagar subcommand, whatever stale reference
  prose says.

with the existing resolution front end - `--dataset` / `--variant` /
`--commit` / `--file`, durable-archive names, corpus directory anchored at
the target project's git root - kept verbatim.

With both two-archive comparators under one roof, the user-facing line
between them is this contract's to draw. **`regress`** is the canonical
instrument: strict decode, exhaustive, elivagar archives only, attribution
and overlay - for "did my output change, and where". **`compare-tiles`** is
the lenient instrument: sampled, aggregate feature counts, tolerant of wire
fields elivagar does not emit (it rides the tolerant mode of the linked
decoder - the gate's strict mode rejects foreign fields on purpose, see
`elivagar.md`) - for cross-producer bake-offs, and it proves nothing about
bytes. Bit-identity claims use neither: `cmp` over complete
deterministic archives, as ever.

What becomes brokkr source, taken from elivagar:

- the canonical semantic tile hash and the canonicalization rules.
- the digest fold, both modes (leaves and buckets).
- the contract gating policy over elivagar's provenance schema.
- the canonical SVG render core, the classifyRings port, and the style
  machinery.
- `mutate`, the calibration instrument, over elivagar's writer API.
- the regress semantic diff and overlay renderer.
- the compare-tiles sampler, going native from the retired cargo example.
- the calibration suite (see Calibration below).

## What brokkr owns: the adjudication semantics

- **What "same tile" means.** The canonical hash and what it absorbs (gzip
  bytes, layer/feature/attribute ordering, key/value-table permutation,
  component ordering) versus what it covers (addressing, layer names,
  versions, extents, ids, bit-exact attributes, geometry).
- **The fold.** Leaf runs (maximal same-zoom spans sharing one hash,
  zoom-split then merged), per-zoom rollups, the root; buckets mode for
  planet-scale baselines (z0-z7 tiles hashed individually, z8-z14 rolled up
  under their z7 ancestor, with a bucket root as the integrity guard). Both
  representations are pure arithmetic over per-tile hashes, and both are
  brokkr's.
- **The gating policy.** Which provenance fields gate comparability (input
  identity, the config contract) and which are diagnostic, warn-only (input
  names, `build.*`, `effective`). Applied **symmetrically at compare time**:
  brokkr applies its current policy to both the stored contract and the fresh
  archive's block, so a policy change never silently re-interprets one side
  against the other. Policy changes are calibration events.
- **The style** and its recorded identity.
- **The verdicts and exit codes.**

## The baseline: state in elivagar's repo, owned by brokkr code

The baseline stays where it is: `corpus/<dataset>/` plus `corpus/style.toml`
in **elivagar's repository**, visible and top-level. Rotation atomicity (one
commit couples code change and baseline change) and review prominence (the
corpus diff is the human gate) both require it; see `elivagar.md`. brokkr is
the sole reader and writer.

This is the opposite kind of artifact from brokkr's measurement stores
(`results.db`, `sidecar.db`, `history.db`): those are records, not source of
truth, and stay out of git. The baseline is source of truth, lives in git,
and is text.

Per file:

- **`leaves`** - the ground truth: per-tile hashes as maximal same-zoom runs.
  Committed as plain text, **never compressed** - a gzipped baseline diffs as
  binary, and the by-tile rotation diff is the review. If a dataset's leaves
  ever outgrow git, that dataset switches to buckets mode; de-diffing the
  leaves is never the answer.
- **`digest`** - committed, **not optional**. It carries two duties: the
  human-readable per-zoom summary in a rotation diff, and the baseline's
  self-integrity guard - `check` recomputes the rollups and root from the
  committed leaves (the bucket root from the committed bucket rows in
  buckets mode) and refuses a damaged baseline (hand edit, merge damage)
  before any archive is read.
- **`contract.json`** - the provenance snapshot captured at bless time,
  stored **whole**: gated and diagnostic fields both. Diagnostics (which
  commit blessed, build flags) are the record's value; gating happens at
  compare time under current policy, not at storage time. One member brokkr
  *authors* rather than captures: the style record (path and hash). tilegen
  embeds no style, brokkr owns the style file, and "which style the committed
  tiles were rendered with" is reconstructible from nothing else, so it is
  persisted explicitly here. The style member sits outside the
  gated/diagnostic partition entirely: it is consumed by the staleness step,
  never by comparability - a stale style must not read as an incomparable
  archive.
- **`manifest.toml`** - stays a file in the corpus directory, **not** in
  `brokkr.toml`. It is curated, append-only, review-bearing baseline state
  (the hard-tiles ledger, curator notes), not tool configuration - and
  `brokkr.toml`'s dataset blocks are host-scoped while the corpus is
  dataset-scoped and host-independent by construction.
- **`tiles/*.svg`** - the canonical renders of the manifest tiles, browsable
  in a file manager and diffable in a PR. Because `check` byte-compares fresh
  renders against these files on whatever host runs it, **cross-host byte
  identity is a stated obligation on the render core**, not an observed
  property of its output: integer-only path emission, deterministic iteration
  order everywhere, fixed color and number formatting. Any render-core change
  that cannot hold this guarantee is a design rejection, because a
  formatting-nondeterministic renderer turns `check`'s staleness step into a
  spurious `svg stale` on every other machine.
- **`corpus/style.toml`** - the corpus-global canonical style, one per corpus
  root, shared across datasets.

## `check`: the verdict pipeline

Ordering is a guarantee, not an optimization detail, and it follows one
precedence rule: **a condition short-circuits the content walk only if it
invalidates the walk's meaning.** Baseline damage and archive
incomparability do; baseline staleness does not - the canonical hash reads
no style, so a dirty style file has no bearing on the content verdict and
must never mask it. The cheap-before-expensive guarantee narrows to what it
is entitled to: *refusals* (steps 1 and 2) never pay for the walk; staleness
is not a refusal of the walk.

1. **Baseline integrity.** Recompute the committed digest from the committed
   rows - rollups and root from `leaves` in leaves mode; the bucket root
   from the committed bucket rows in buckets mode, a weaker guard by
   construction since there are no full leaves to recompute from. A damaged
   baseline cannot judge anything: refuse, exit 3, before any archive is
   read. Subject: the baseline.
2. **Comparability.** Read the archive's typed provenance through the linked
   crate (a metadata read - cheap, no tile walk) and compare against the
   stored contract under current gating policy. Diagnostic fields warn only.
   A gated mismatch is exit 2 with the field named. An unreadable archive or
   absent/invalid block is a refusal on the archive's own terms, also exit
   2, with the cause named. An incomparable archive makes the walk
   meaningless: short-circuit. Subject: the incoming archive.
3. **The walk.** Compute per-tile canonical hashes over the linked decoder,
   fold, and diff against the committed leaves (or bucket rows). A mismatch
   is exit 1 and names the changed zooms and tiles (leaves mode) or buckets
   (buckets mode). This verdict is style-independent and is never skipped
   for a comparable archive against an intact baseline.
4. **Staleness, reported after the content verdict.** Hash the live style
   file against the recorded style identity; re-render every manifest tile
   in-process and byte-compare against the committed SVGs (sound only under
   the render core's cross-host byte-identity obligation, above); flag
   orphaned tile files; surface unstyled-layer and ring-clamp warnings.
   Subject: the baseline - "re-render your corpus", nothing about the
   archive. An SVG mismatch is `svg stale` only under a passing digest;
   under a failing one the SVG diff is expected and already part of verdict
   1. Staleness masks nothing: with a content mismatch present the exit is
   1 and staleness is reported alongside; staleness alone is exit 3.

The masking hole this ordering closes, recorded because it survived one
draft of this document: with style staleness ahead of the walk, a developer
rotating the style while a code change had also - unintentionally -
regressed content would get exit 3, never run the walk, re-render and
bless, and the regression would enshrine in the new leaves with only human
diff review standing in the way. That is vigilance where this document
demands mechanism. The walk runs.

**Signals.** Exit 0 / 1 / 2 (pass / content mismatch / archive refusal)
remain the load-bearing caller contract, unchanged. Baseline trouble is
**exit 3**, pinned here because an unspecified signal is not a contract: 0
the archive passes, 1 the content changed, 2 the archive cannot be judged,
3 the baseline is the problem. Exit 3 has two flavors with different
precedence: baseline *damage* (step 1) invalidates the comparison and
short-circuits; baseline *staleness* (step 4) touches only the visual tier
and is subordinate to the content verdict. Never fold 3 into 2: "your
baseline is damaged or stale" must not read as "your archive is
incomparable". The old design lumped all of this into 2; this one splits
it.

## `bless`

Guards enforced from the typed provenance, unchanged in substance: refuse
dirty builds, non-locations archives, and non-MVT+gzip archives at the door.
Replacing an existing baseline requires `--rotate`. Bless writes
`contract.json` (whole block plus the style record), `leaves`, `digest`, and
re-renders the manifest SVGs in one command. The workflow is unchanged: the
bless lands inside the rotation commit, and the corpus git diff is the
review.

`--mode buckets` is the planet-scale form: the same per-tile hashes, folded
to z7-ancestor buckets. Its rotation reviewability is weaker by construction
(committed rows are opaque hashes; tile-level naming comes from tier-3
attribution against a comparand archive) and stays a recorded limitation.

## Calibration: now brokkr's suite

The oracle discipline moves wholesale and its home is brokkr:

- the mutate calibrands: `drop-tile`, `nudge-geometry`, `layer-version` must
  FIRE with the target tile named; `regzip` (byte-different,
  semantically equal) must CLEAR through both tiers.
- the mutation tests pinning the streaming hash and the detail decoder to
  one equivalence relation - moved from elivagar with the machinery they
  test.
- the ring-grouping differential oracle: `rings` output byte-equal to
  `scripts/validate/ring-grouping-oracle.mjs`, the independent Node
  implementation (unaffected by this redesign, still in elivagar's repo as a
  script).

Recalibration triggers, both directions of the boundary:

- any change to brokkr's gate code - the hash, the fold, the gating policy,
  the render core, the diff.
- any elivagar change touching its decoder, encoder, or provenance schema.
  elivagar does not announce these; detecting them is brokkr's obligation,
  discharged by mechanism, not vigilance. The mechanism has two legs:

  - **The standing gate is itself a drift detector.** The committed baseline
    was hashed by the decoder as of the last rotation; every `check` hashes
    the fresh side with the currently linked decoder. A semantic decoder
    drift that touches corpus content therefore surfaces as an ordinary
    exit-1 mismatch on the next landing. The residual human duty sits at
    rotation: a mismatch caused by decoder drift must not be blessed as an
    intended output change - adjudicating that is what the tier-2 SVG diffs
    and tier-3 overlays are for. This net has a named blind spot: drift in a
    path with no visual consequence - attribute, id, layer-version decode -
    shifts the hash while every render and overlay stays identical, leaving
    tier-2/tier-3 nothing to show a human. That subclass is exactly where
    the coverage obligation below is mandatory rather than
    belt-and-suspenders.
  - **The calibrand suite lives in brokkr's test suite** and re-runs against
    the currently linked elivagar whenever brokkr's tests run. It waits on
    nobody's memory.

The coverage obligation, stated because linking cannot catch it: a
*structural* change to elivagar's decode surface is a compile error in
brokkr - free detection. A *semantic* decoder drift - same types, different
bytes-to-values mapping - compiles clean and silently shifts every hash.
Beyond what the corpus's own content happens to exercise, the calibration
suite is the only mechanical backstop for that class, so its coverage is not
optional in shape: **every wire-level decode behavior the canonical hash
consumes must have a calibrand or mutation test that fails if that behavior
drifts.** A green suite that exercises less than the hash's decode surface is
a false floor.

Named limitation, so it reads as a limit and not an aspiration: coverage
*completeness* is the one link this design does not mechanize. Every other
trigger is a mechanism - structural drift fails the compile, covered
semantic drift fails a test, content drift fails the standing gate - but no
test detects a missing test: when the hash grows a new decode dependency,
adding its calibrand is a human act. This is the single place the
"mechanism, not vigilance" rule goes unsatisfied, and it guards the design's
own worst case - silent hash drift with no visual tell. The closing move, if
ever wanted: make the obligation derivable instead of remembered - the hash
is brokkr code now, so the decode branches it consumes are introspectable,
and a coverage check can fail when a decode dependency has no pinning
oracle. Until that exists, this limitation stands, named.

## Consequences

- **The gate requires brokkr.** There is no raw elivagar spelling of
  `check`, `bless`, `regress`, or `mutate` anymore; documentation that taught
  both spellings now teaches one. Anything that planned to reuse the gate
  without brokkr (the serve-path verification sketch in elivagar's planning
  notes) now either invokes brokkr or gets a deliberately extracted
  verifier - a decision for that work when it happens, recorded here so it is
  not discovered later.
- **`clean` scope is unchanged in spirit.** The baseline is source of truth
  in elivagar's repo; brokkr's `clean` never touches it. Only brokkr-created
  scratch is in scope.
- **The independent oracles are untouched.** The Node validators (earcut,
  boundary-line, ring-grouping) remain the only producer-independent
  detectors and stay authoritative for correctness-in-itself. This redesign
  relocates tooling and its debt; it neither strengthens nor weakens oracle
  independence, and the docs must not claim otherwise.
