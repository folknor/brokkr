# Corpus gate migration - handoff

Status as of commit `e0f95fd`. Everything below is about relocating elivagar's
corpus adjudication into brokkr per `brokkr.md` and `elivagar.md` (the two
contract docs in this repo root - read them first; they are the spec).

The suite is green (1223 tests): `brokkr check` passes. The gate is functionally
complete and linked against the real elivagar crate.

## Ground rules (do not relearn these the hard way)

- **Bash: no `;` chaining, no `|` pipes, no redirects (`<` `>`).** One command per
  call. The only sanctioned pipe is `review`. Violating this gets the call
  denied in dontAsk mode. `cd X; git ...` is a chain - use `git -C <dir> ...`
  instead. A `<->` inside a commit message is read as a redirect and denied.
- Never `sed`/`find`/`awk`/complex bash. Use `rg` (single command) and the Read
  tool. Never read/write `/tmp`.
- Use `brokkr check` / `brokkr test -p brokkr <filter>`, never raw `cargo`
  (except `cargo install --path .`, which is allowlisted).
- After a code change: update docs, `brokkr check`, commit on master (no
  branches), `cargo install --path .`. Commit lockfiles if changed.

## Architecture of what was built

- `../elivagar` is a **path dependency** (`Cargo.toml`); `protohoggr = "0.4"` too.
  `scratch/elivagar` is a SEPARATE read-only inspection checkout (does not affect
  the build).
- **`src/elivagar/eliv.rs`** is the seam: it re-exports elivagar's public API
  (`pmtiles_reader`, `pmtiles_writer`, `tile_detail`). Everything in `corpus/`
  imports from here, not from `elivagar::` directly. If elivagar's API shifts,
  adapt this one file. `next_zoom_boundary` is brokkr-owned here (shed with
  regress).
- **`src/elivagar/corpus/`** is the gate:
  - `mod.rs` - the `check` pipeline (baseline integrity -> comparability -> walk
    -> subordinate staleness; exit 0/1/2/3), `bless`, `render_manifest`,
    `render_tile`, `rings`, `compute` (the digest walk). `Outcome` maps to the
    process exit code.
  - `canonical.rs` - **the frozen streaming tile hash** (`streaming_tile_hash`,
    domain `elivagar-stream-tile-v1`), a verbatim port that reproduces the
    committed `corpus/*/leaves` bit-for-bit. ALSO holds the **detail canonical
    form** (`detail_tile_hash`, `compare_detail_features`,
    `compare_detail_components`) - regress's "same tile" definition, shared with
    the render. ALSO holds the streaming/detail equivalence tests + a ported MVT
    fixture encoder in its `#[cfg(test)]` module.
  - `digest.rs` - fold, `digest`/`leaves` text formats, baseline self-integrity.
  - `diff.rs` - localized leaf/zoom/bucket deltas.
  - `contract.rs` - comparability gating policy (which provenance fields gate).
  - `manifest.rs` - the hard-tiles ledger.
  - `mutate.rs` - the mutate calibration instrument + the mutation-isolation
    calibration test.
  - `render.rs` - canonical SVG render core, classifyRings, `dump_ring_grouping`.
  - `style.rs` - the render style + `matches_toml`.
- Dispatch: `src/elivagar/cmd.rs::corpus()` calls the native gate and prints via
  `emit_corpus`.

**The streaming hash is FROZEN.** Any change to what it covers/absorbs or its
`elivagar-stream-*` domain strings invalidates every committed leaf and is a
corpus-rotation + full-recalibration event, never a quiet edit.

## How to read elivagar code that was shed

The corpus namespace and `regress` were deleted from elivagar at commit
`0129ef3` ("shed the adjudication layer"). To read the originals:

    git -C /home/folk/Programs/brokkr/scratch/elivagar checkout 0129ef3~1 -- <path>

then Read the file, then clean up:

    git -C /home/folk/Programs/brokkr/scratch/elivagar rm -f --quiet <path>

Useful originals: `src/regress.rs` (the streaming hash, the detail decode, the
comparators, the full diff engine, the residual matcher, overlay), `src/corpus.rs`
(check/bless/mutate/compute), `src/corpus/render.rs`, `src/corpus/style.rs`,
`src/corpus/overlay.rs`, `src/regress/tests.rs` (equivalence tests). The current
elivagar (`scratch/elivagar/src/tile_detail.rs`, `pmtiles_reader.rs`,
`pmtiles_writer.rs`, `provenance.rs`) is the live public surface.

## Acceptance stack: CLOSED (dev run against elivagar `0129ef3`, 2026-07-24)

Items 1-3 below are done. The elivagar dev ran the full stack against the
unchanged committed `corpus/denmark/` baseline (`a40c077` rotation, never
re-blessed - it was the parity oracle throughout):

- **e2e, existing archive**: native `check` on `denmark-locations-3344eaa.pmtiles`
  (pre-redesign producer) - exit 0, 1,296,998 tiles / 166,347 unique, matching the
  committed digest line, zero corpus diff. The `e0f95fd` render-sort fix cleared
  the staleness with no corpus changes.
- **e2e, fresh build**: `brokkr tilegen` at elivagar `0129ef3`, then native
  `check` - exit 0, same counts. New producer, new judge, old baseline all agree.
- **rings**: byte-equal (`cmp`) to `scripts/validate/ring-grouping-oracle.mjs`
  over all 1.3M tiles.
- **calibrands through the gate** (mutants of the fresh archive, tile
  `14/8764/5132`): `drop-tile`, `nudge-geometry`, `layer-version` each FIRED
  (exit 1, target named, old->new hash); `regzip` CLEARED (exit 0) through both
  the digest and SVG-staleness tiers.

Recalibration record written up on the elivagar side in
`reference/performance.md`. The "1 changed run(s)" cosmetic wording is confirmed
fixed. **Standing rule, still in force:** if `check` ever reports a stale tile,
chase it - do NOT re-render the corpus. The committed SVGs are the render's
parity oracle exactly as `leaves` is the hash's.

Note the through-the-gate calibrands were verified *once, externally*. The
in-suite standing test (`mutate.rs::mutations_are_isolated_and_regzip_is_...`)
still only covers mutation isolation over a synthetic archive; a hermetic
bless->mutate->check gate-level test would make the fire/clear property something
`brokkr check` re-verifies on every run rather than a one-time reading. Optional,
but it is the only acceptance leg with no standing guard.

### 4. The regress port - DONE

`src/elivagar/regress/` is native. Modules: `engine` (the three-pass
raw/canonical/detail blob-pair engine + report aggregation), `prepared`
(re-augments the wire-order `tile_detail` decode with the bboxes, digests and
structure signatures the old in-crate decoder carried inline), `compare` (the
`DiffSink` protocol, layer merge-join, id/anonymous grouping, tolerance
classification), `pairing` (exact / min-cost-matching residual / force-zip),
`geometry` (exact-integer bbox, Hausdorff, KD-tree, hole containment), `report`
(the bounded report + text/JSON rendering), `overlay` (`--overlay`). Pass 2 calls
`corpus::canonical::semantic_hash`, so regress and the gate share one definition
of "the same tile".

`compare-tiles` went native alongside it (`compare_tiles.rs`) - the lenient,
tolerant-decode sampling census; no build, no lock, no verdict.

The 16 ported tests (4 matcher incl. the brute-force oracle, 12 engine) are in
`regress/tests.rs`. The MVT/PMTiles fixture encoder was extracted to
`corpus/fixture.rs` and is now shared with `canonical.rs`'s equivalence tests -
which also closes the deferred "extract a shared `#[cfg(test)]` MVT encoder"
item below.

**Two deliberate deviations from the original, both flagged:**

- `dump_overlays` iterated `range.start..=range.end` over half-open ranges, so it
  rendered one extra tile past each differing range and spent an `--overlay-max`
  slot on it. Corrected to `..`.
- The `regress_*` counter FIFO emission is dropped. In elivagar those counters
  went to brokkr's sidecar from a child process; brokkr is the process that
  drains that FIFO. The same numbers are reported in-band (the `regress ...` line
  of the text report, `counters` in `--json`).

### 5. Deferred / optional

- Render byte-identity determinism unit test (two `Style::load` -> identical
  bytes). Deferred because it is redundant with the e2e staleness byte-compare
  (now confirmed). The shared fixture encoder now exists (`corpus/fixture.rs`),
  so the old objection about duplicating it no longer applies if anyone wants it.
- The hermetic gate-level calibrand (see the acceptance-stack note above) - the
  only acceptance leg with no standing in-suite guard.

## Calibration coverage obligation (standing)

Per `brokkr.md`, every wire-level decode behavior the canonical hash consumes must
have a calibrand or mutation test that fails if it drifts - it is the only
mechanical backstop for semantic decoder drift with no visual consequence. This is
a human-maintained completeness obligation (no mechanism detects a MISSING
calibrand). When adding decode dependencies, add the pinning test.

## Acceptance stack (the dev's definition of done) - ALL GREEN

- streaming/detail equivalence tests green   [DONE, in-suite]
- mutate calibrands fire/clear               [DONE e2e; in-suite guard covers
                                              isolation only - see above]
- rings byte-equal to the Node oracle        [DONE e2e]
- `pmtiles-corpus check` exit 0 against committed corpus/denmark on a fresh
  denmark build                              [DONE e2e, both producers]
