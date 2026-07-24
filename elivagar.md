# elivagar - corpus redesign contract

Companion: `brokkr.md`. This is what **elivagar** must become, written at the
contract level: what it sheds, what it keeps, and what it must expose. It says
nothing about how the code is structured.

## Why

Two problems, and neither of them is "who is allowed to decode":

1. **The end-user binary carries development tooling.** The six `corpus`
   subcommands (`check`, `bless`, `render`, `render-manifest`, `rings`,
   `mutate`) plus `regress` - no end user of a map-tile producer runs any of
   the seven, and all of it is built on the shared adjudication machinery:
   the canonical hash, the render core, the calibration instruments. (Only
   three of the seven ever touch a committed reference - check, bless,
   render-manifest; render, mutate, rings, and regress are reference-free -
   so the criterion is who runs it and what it is built on, not whether it
   reads a baseline.)
2. **That tooling is debt elivagar developers carry.** The canonical hash, the
   digest fold, the comparability policy, the render core, and the calibration
   suite are development tooling, and development tooling for every project in
   this family lives in brokkr.

brokkr resolves this by linking the elivagar crate and decoding archives
in-process through it. How brokkr manages that dependency is brokkr's build
concern and outside both contracts.

What this deliberately does **not** claim: oracle independence. The gate still
reads archives through elivagar's decoder, linked in. Producer-independent
adjudication remains the job of the Node oracles (earcut, boundary-line,
ring-grouping), which this redesign does not touch.

## The invariant elivagar must hold

**elivagar must stop knowing that a reference exists.** No corpus directory,
no manifest, no committed baseline, no "matches the reference" verdict, no
calibration machinery. The binary answers intrinsic questions about the
archive in front of it and nothing else.

The knowledge line is one-directional: brokkr may know everything about
elivagar - its types, its readers, its writers - while elivagar knows nothing
about brokkr, and nothing elivagar does changes meaning based on what brokkr
will do with the answer.

## What elivagar sheds

Subcommands removed from the binary:

- **the whole `corpus` namespace** - `check`, `bless`, `render`,
  `render-manifest`, `rings`, `mutate`.
- **`regress`** - the two-archive semantic diff is the tier-3 attribution
  instrument, development tooling by the same criterion, and it shares the
  canonical decode and canonicalization with the digest. It moves with that
  machinery rather than surviving as its one leftover consumer in the product
  binary.

Code that leaves the crate - it becomes brokkr source, not a shared library:

- the canonical semantic tile hash (`streaming_tile_hash`) and the
  canonicalization rules: everything that defines "same tile".
- the digest fold: leaf runs, per-zoom rollups, root, buckets mode.
- the contract gating policy - which provenance fields gate comparability and
  which are diagnostic. That partition is adjudication policy, not archive
  schema. The schema stays here; the policy leaves.
- the canonical SVG render core, the classifyRings port, and the style
  machinery.
- the `mutate` archive-rewriter (the calibration instrument).
- the mutation tests that pin the streaming hash and the detail decoder to
  one equivalence relation. They test the gate, so they live with the gate.

One correction recorded here rather than under "keeps", where an earlier
draft wrongly listed it: **`compare-tiles`**. The elivagar CLI has no such
subcommand and never did - what exists is a cargo example
(`examples/compare_tiles.rs`) that brokkr builds and runs on demand. It was
never in the end-user binary, so there is nothing to shed from the CLI; the
example itself retires once brokkr's `compare-tiles` goes native over the
linked crate (`brokkr.md`, which also draws the user-facing line between it
and `regress`). Reference prose claiming an `elivagar compare-tiles`
subcommand is stale and gets corrected when this lands.

## What elivagar keeps

The binary keeps the operations an end user or operator of a tile producer
actually runs:

- **`run`**, **`ocean-build`** - produce the archive and the durable ocean
  artifact.
- **`inspect`** - read provenance and layout back. Provenance *emission* also
  stays in the producer: only the writer can author the block, and it is a
  property of the archive, not of any gate.
- **`verify`** - the intrinsic verdict: "is this archive well-formed",
  computable from the archive alone. The line to hold: elivagar may still say
  "this archive is malformed"; it may never again say "this archive fails to
  match a reference".
- **`svg`**, **`diag`** - debugging renders and dumps of an archive you
  hold. Keeping `svg` leaves two SVG renderers alive in two repos - the
  viewer-flavored one here (grids, layer filters, distinct auto colors), the
  canonical render core in brokkr - and that duplication is accepted, not
  waved off as "unrelated". They serve different masters and must not
  converge: the canonical core answers to cross-host byte-identity and
  MapLibre semantics (the 500-ring clamp) and may never grow viewer
  conveniences; `svg` answers to a developer's eyeball and may never be used
  for adjudication. The rule that keeps the footgun latent: corpus tiles are
  judged from the committed canonical renders (or a fresh `brokkr
  pmtiles-corpus render`), never from `elivagar svg` output.

Production-side properties that sound gate-adjacent stay, because they are
properties of the product:

- provenance emission (`Input` / `Config` / `Build` / `Effective`).
- the within-run total record order (paint order) - determinism of the
  artifact itself.
- `OCEAN_POLICY_VERSION` and artifact-key validation. A stale ocean artifact
  must fail a production run loudly whether or not any gate exists.

## The library surface elivagar must expose

brokkr compiles elivagar in and needs, as public API:

- archive open and addressed-tile iteration (the PMTiles reader), including
  run-level access to raw compressed payloads - mutate copies runs verbatim
  and only splits the one it edits.
- the tile decoder: decompression plus MVT decode into the detail
  structures, with **selectable strictness**. Strict mode errors on unknown
  wire fields - the gate's requirement, so foreign structure can never
  silently skip past the hash. Tolerant mode skips them - compare-tiles'
  requirement, since competitor archives carry wire fields elivagar never
  writes. One decoder, one switch, both consumers named here; brokkr rolls
  no foreign-MVT decode of its own.
- typed provenance: the embedded block decoded into schema types, **in
  full** - no projection, no gating. Policy is the caller's.
- the PMTiles writer's run-level surface - the same `add_run` path assemble
  writes production archives through - plus **one mutate-only affordance**:
  setting the metadata blob verbatim rather than authoring it. That setter's
  sole consumer is brokkr's calibration tool. It is the one slice of this
  surface `run` never exercises, and it is priced as such: one method, not a
  parallel writer.

Guarantees on this surface:

- **Pure functions** of the archive plus explicit arguments. No repo state,
  no side channels.
- **No adjudication.** No hashing, no verdicts, no policy. The API reports
  what the archive contains; deciding what that means is the caller's.
- **Failures are on the archive's own terms** - unreadable, absent block,
  unknown schema. "Does not match" is not a concept on this surface.
- Changes to this surface, the decoder, or the encoder are elivagar's to make
  freely - but each one is a recalibration trigger for the gate, and
  detecting it is brokkr's obligation, discharged by mechanism rather than
  vigilance (see `brokkr.md`, Calibration). elivagar does not coordinate.

The debt this leaves is the honest kind: elivagar maintains a decode/write API
for one known consumer, built on readers and writers it must maintain anyway
for `run`, `verify`, and `inspect` - the verbatim-metadata setter above being
the one named exception. Everything adjudicative - hash, fold, policy, render,
calibration - stops being elivagar's problem.

## State stays in this repo; code does not

The committed baseline - `corpus/<dataset>/` (`contract.json`, `digest`,
`leaves`, `manifest.toml`, `tiles/*.svg`) and `corpus/style.toml` - remains in
elivagar's repository, at its current visible top-level location. Two reasons,
both hard requirements:

- **Atomic rotation.** An output-changing landing and its baseline rotation
  must be one commit; that is only possible if code and baseline share a repo.
- **Review prominence.** The corpus diff is the human gate. It stays a
  browsable, first-class artifact - never a dotdir, never hidden from the PR
  diff. (Stated honestly for buckets-mode datasets: their digest rows are
  opaque hashes, so the review value there is carried by the SVG diffs and
  tier-3 attribution, not the leaves diff - see `brokkr.md`.)

But elivagar-the-program never reads or writes a byte of it. brokkr is the
sole reader and writer of these files. elivagar-the-repo hosts state;
elivagar-the-binary does not know it exists.
