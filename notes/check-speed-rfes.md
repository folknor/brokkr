# Five RFEs from measuring `brokkr check` on nautilus_trader (2026-09-02)

A session spent decomposing the cost of `brokkr check` on the largest config in
use produced these five feature requests. They are ordered by expected payoff.
The measurements are the motivation, so they come first; every number is from
warm runs on the same tree, same day, and the external tooling used to obtain
them lives in the PRs workspace (`scripts/time_script_checks.py`,
`scripts/stamp_output.py`).

This updates the picture in todo.md's "Baseline: `brokkr check` on
nautilus_trader (2026-07-22)" section, which recorded 8m11s for 3 sweeps.

## The measured baseline

Warm `brokkr check` (tier1: 4 sweeps, `test_threads = 0`): **4m29s**. Warm
`brokkr check --gate` (tier1 + a process-isolated serial lane + the coverage
audit): **7m18s**. The split, reconstructed by timestamping output lines
externally and via `brokkr clippy --sweep` and per-hook timing:

    test run, default sweep     217s   81% of check
    script_check (20 hooks)      34s   docs-conventions 9.8, formatting-rs 9.0,
                                       typos 6.6, dst-conventions 4.8, rest ~4
    test run, cli+ffi+live      ~10s
    clippy, all 4 shapes         ~2s   warm incremental
    gremlins/textlint/manifest/
    dependency/publish_cycle     ~0s
    gate-only (serial lanes,
    coverage enumeration)      ~170s   170 per-test cargo spawns + --list passes

The default sweep's 26.4k tests have a serial sum of 1228.8s against the 217s
wall: about 5.7x effective concurrency on a machine with far more to give. The
floor is the binary boundary - cargo runs test binaries sequentially and
`test_threads` parallelizes only within one. The slow set is idle waiting, not
compute: 350 tests at 1s or more carry half the serial cost, and the top of the
`--timings` list is heartbeat deadlines, reconnect ladders, and 5.0s
order-window ceilings being waited out. That is precisely the profile where
fan-out pays (broadarrow's `parallel = { budget = 24 }` took its test phase
from ~50s to ~23s on the same reasoning).

## RFE 1: fan-out for sweeps that carry `test_exclude_packages` [DONE]

**Implemented 2026-09-02, with a different mechanism than proposed below.**
The selection-argv fan-out sketched here was shown unsound in design review:
cargo's feature resolution follows the root unit set, not the package
selection argv, so `--workspace --exclude a --test T` can still resolve a
different graph than the prebuild (dev-dep features of packages whose targets
are not among the roots drop out). What landed instead: the fan-out executes
the prebuilt test binaries **directly** under a reconstruction of cargo's
launch contract (`src/check_cmd/direct_runtime.rs`), never re-entering cargo
- which keeps the per-binary unit, budget, watchdog and timing store intact,
works on every selection shape, and retires the runner-side need for the
unification pin (the prebuild-side `auto` promotion is unchanged; explicit
`feature_unification = "selected"` is no longer refused on parallel sweeps).
Configured target runners refuse the lane; `--no-run`/`--target`/`--config`/
`--manifest-path`/`--target-dir` after `--` are rejected on parallel sweeps.
Smoke: `scripts/smoke-parallel-direct.py`. nautilus can now put
`parallel = {}` on its default entry.

### The original (superseded) proposal

The `parallel` key is the built cure for the binary-boundary floor, and the
sweep that needs it most cannot use it. nautilus' `default` entry selects
`--workspace --exclude nautilus-pyo3 --exclude nautilus-cli`, and the fan-out's
whole-workspace unification promotion requires the selection to be exactly the
whole workspace - so each fanned-out `cargo test -p <pkg> --test <t>` would
resolve a much narrower feature graph than the prebuild (64-bit model, no
adapter features) and the lane degenerates into serialized rebuilds, the
documented failure the promotion exists to prevent.

The rejected alternative first: pinning `feature_unification = "workspace"` on
the entry is not a fix, because workspace mode resolves as if every member were
selected, excluded members included - pyo3 would donate `python` across
common/core/model and the sweep would stop being the CI-parity shape it exists
to be. The consuming repo also cannot commit a `.cargo/config.toml` pin the way
broadarrow did, because the checkout is upstream's.

The proposed shape: fan out using the sweep's own selection argv. An invocation
of `cargo test --workspace --exclude a --exclude b --test <target>` keeps the
selection - and therefore the resolution - identical to the prebuild, no pin
needed, so the no-op-rebuild property holds by construction. The unit becomes
the target name rather than the binary: two packages sharing an integration
target name run in one invocation, which only coarsens the fan-out, and lib
unit-test binaries have no selection-preserving spelling (`--lib` under
`--workspace` means every package's lib tests) so they stay sequential. That is
an acceptable partition here: the slow set is overwhelmingly integration
binaries, and the lib remainder lands in the same serial complement the
enumerate-the-parallel-side rule already prescribes.

Expected win, from the measured distribution: the default sweep's floor drops
toward max(serial/budget, slowest binary), roughly 50-70s against today's 217s.
Check lands around 2 minutes.

## RFE 2: a nextest-engine execution lane [REFRAMED 2026-09-02; LANE REBUILT]

**Rebuild landed same day.** The lane now runs exactly as reframed below:
synthesized config under the state dir (retries 0, no default-filter, the
20s watchdog as slow-timeout + terminate-after, max-fail all to match the
other lanes' enumerate-everything ethos), `.config/nextest.toml` never
opened, NEXTEST_PROFILE never read, profile `test_threads` mapped onto the
engine's in-flight count, `isolation = "process"` refused as redundant
rather than conflicting. The smoke's load-bearing scenario writes a foreign
nextest.toml whose default-filter and retries would hide a failing test and
asserts the run stays red - the file gets no vote. The pin/bless/drift
machinery, the boundedness refusal and the DefaultFiltered bucket are
deleted.

**Audit landed same day - RFE 2 is complete.** A `certifies = "complete"`
profile may reference nextest sweeps: the lane's ran-set comes from the
engine's own listing under the sweep's real filters (the same code that
shapes its run), and the pair unit is (binary-id, test) shape-wide whenever
any lane of a shape is nextest - the finer-key-wins rule from the B51
analysis, with the unit built through nextest's own RustBinaryId
construction so libtest claims and engine claims cannot drift. Smoke: the
gate scenarios in scripts/smoke-nextest-lane.py, including a pair only the
engine's claim covers (an id-mapping break turns the gate red) and the
orphan path. The serial lane's ~170s collapse is now unblocked: migrate the
gate's isolated serial lane to a harness = "nextest" entry, and check the
B51 acceptance delta (+7, 91 -> 98) on the first migrated run.

**Migration finding, fixed 2026-09-02**: the first nautilus gate run hit
B51 = 98 exactly and collapsed the serial lanes ~170s -> 2.2s, but orphaned
39 lib-binary pairs on a binary-id normalization drift: nautilus' lib
crates declare crate-type = ["rlib", "staticlib", "cdylib"], cargo reports
the lib target's kind as `rlib`, and brokkr's from_parts call passed it raw
(pkg::rlib/target) where the engine normalizes every lib-like kind to the
bare package name. Fixed by mirroring nextest's own normalization before
building the unit, and the smoke's fixture lib now declares the multi
crate-type so this exact drift turns the gate scenario red forever after.
Nothing changes in nautilus' config; the same gate run should now read
check complete.

**The correction, which invalidates this RFE as originally written.** The
original text below argued the lane on CI parity: upstream supports only
nextest, so run their runner their way. That framing was wrong about the
goal, it echoed a pre-existing wrong sentence in check.md's nextest section
("Motivation is CI parity, and only parity"), and the first implementation
faithfully built it - reading `.config/nextest.toml`, honoring the
default-filter, and then inventing a pin/bless/drift apparatus to police a
foreign config that should never have had a vote. The user killed it at the
root: brokkr.toml is the ONLY authority over what a gate runs and claims -
that is the entire point of brokkr standing over a foreign checkout - and
upstream's CI machinery is of no interest at all.

What nextest actually is here: a fast process-per-test **executor**. An
engine, not a policy source. The rebuilt lane runs the linked engine on a
brokkr-synthesized config: selection from the sweep and profile
(packages/excludes/features/skips/only, the translation already built),
retries zero, no default-filter, no test groups, no setup scripts, and
slow-timeout + terminate-after set to brokkr's own per-test watchdog value -
the hang kill the other lanes have, per process, for free. Never opens
`.config/nextest.toml`, never reads NEXTEST_PROFILE. Everything the first
cut built to police the foreign config dies with it: the boundedness
refusal, the default-filter pin, the bless flow, the DefaultFiltered
bucket, the run-extra-args rejection. The coverage audit shrinks to the
existing model - universe from a full listing, ran-set from the lane,
narrowing only ever from brokkr.toml skips and quarantines, keyed
(binary-id, test).

What the lane is FOR, measured on nautilus: the gate's `isolation =
"process"` serial lane is ~170 sequential `cargo test -- --exact` spawns,
~170s of gate-only overhead. Process-per-test under the engine is that
lane's isolation guarantee executed concurrently - it collapses to seconds,
configured from brokkr.toml alone. The in-process parallel lanes stay the
default everywhere else: the shared-process lane is the detector for the
process-global-state class (it produced four real upstream findings in the
nautilus workspace), and the original text's claim that it "is not a
detector there" is retracted along with the parity frame.

### The original proposal (kept as the record of the misread)

The groundwork is in (`src/check_cmd/nextest.rs`, the coverage key, the
disposition classifier); the lane is not. nautilus is the consumer where the
documented reservation - process-per-test retires the in-process
global-state detector - does not bind, because upstream documents nextest as
the only supported runner and states the suite relies on its per-test process
isolation. The stricter in-process lane is not a detector there; it is a source
of quarantines for tests upstream never promised would pass that way (three
FLAKE-PM entries, two misfiled logger tests, and this session's find: a
busy-yield test whose wall time ordered 7.2s / 19.0s / one 20s-watchdog kill
purely by machine load).

What it buys beyond parity: process-per-test dissolves the binary-boundary
floor (subsuming RFE 1 for this consumer) and replaces the gate's serial lane,
which today is 170 individual `cargo test -- --exact` spawns. The ~170s of
gate-only overhead is mostly this lane plus enumeration.

## RFE 3: run `[[script_check]]` entries concurrently within a stage

Twenty hooks, 34s summed, and the wall time is the sum because they run one
after another. Four hooks carry 30s of it. They are independent by
construction - each is `sh -c` in the same cwd with no ordering contract
beyond declaration-order reporting - so running a stage's entries concurrently
and reporting in declaration order (buffered per entry, the way the parallel
test lane already buffers per binary) turns 34s into roughly the slowest hook,
~10s.

One caveat to design for: an entry may mutate the tree on failure
(nautilus' markdown-tables phase rewrites files the way a formatter hook
does). Concurrent with read-only siblings that is benign on a green run and
only racy on a red one, but a `mutates = true` key that serializes such an
entry after the others would make the property explicit instead of lucky.

## RFE 4: per-phase timings

`--timings` already surfaces the per-test half and was the key that unlocked
this analysis - but it is easy to miss (it appears in the docs only inside the
`--limit` and `--triage` descriptions), and there is nothing at phase
granularity. A run that ends `check complete in 11m28s` supports no theory
about where the minutes went; the doc note that says to read the per-phase
timings before explaining a slow gate describes output that does not exist.

The ask: an elapsed figure on each phase's completion line (`textlint: ok (37
rules, 2933 files) in 0.4s`, `script-check: ok (20 checks) in 34.1s`, per-sweep
build and run figures in the test phase, coverage enumeration), and a
`phase_timings` object in the `--json` summary - additive under schema 1.
Per-hook figures on the script_check line would have answered in one run what
took an external harness here.

## RFE 5: opt-in timestamps on every output line

A `--stamps` flag (or env var) prefixing each output line with elapsed time
since command start. This is the generic fallback that makes any future cost
question answerable without a dedicated feature, and it is what this session
had to reconstruct externally by tailing the output file with a watcher.

The wrinkle that makes it worth building inside brokkr rather than leaving to
watchers: the watcher approach fails on buffering. A sweep's raw cargo stream
is captured and re-emitted after the sweep completes, so an external
timestamper sees a two-minute silence and then everything at once - the stamps
have to be taken at capture time, where only brokkr stands. Stamping brokkr's
own `[run]`/`[test]`/`[warn]` lines alone would already cover the phase
boundaries; stamping captured lines at capture would cover the rest.

## RFE 6: a per-entry doc-only sweep (from the nautilus review) [DONE]

**Implemented 2026-09-02**: `[[check]] doc_only = true` - the entry runs
`cargo test --doc` with its selection/features/unification/profile, serial,
regardless of `[test] doctests`. The doctest twin a parallel workspace sweep
needs: nautilus pairs `parallel = {}` on `default` with a `doc_only` twin of
the same selection and keeps B42 (doctest parity) closed. Guard rails from
design review: no entry filters (doctests are not enumerable, so liveness
could never be audited; profile-inherited filters refused under a complete
claim - compose the twin as its own filterless lane), forwarded target
selectors refused (`--doc` is exclusive), and under a complete claim with a
doctest obligation at least one carrier must be workspace-shaped (`packages`
empty; `test_exclude_packages` allowed and reported) so a package-scoped
twin cannot green-light a workspace-wide subtraction. A referenced doc-only
entry satisfies the complete profile's doctest obligation under
`doctests = false` (quarantine then stale). Doc-only sweeps sit outside the
coverage pair audit, reported on the coverage line. Smoke: the doc-twin +
red-doctest scenarios in `scripts/smoke-parallel-direct.py`.

## Adoption notes for nautilus (2026-09-02)

- `parallel = {}` on `default` + the doc twin is the one-edit adoption; the
  nextest-engine lane arrives later as the serial-lane replacement, once the
  rebuilt synthesized-config lane and its (binary-id, test) audit exist (see
  RFE 2's reframing - the original "waits on the audit wholesale" sequencing
  was written against the parity-shaped lane and its foreign-config
  machinery, now dead).
- First parallel runs: the fan-out overlaps binaries, so wall-clock tests
  (the B100 class; `test_failed_connect_tears_down_websockets_before_retry`
  already at 15.7s) see cross-binary load for the first time and may trip
  the 20s watchdog. Read `--timings` after the first warm run, expect a
  possible round of serial-lane relocations, and tune no budget until the
  weight store has two runs behind it.

## Adoption results (2026-09-02, same tree as the baseline)

Landed in the consuming config: `parallel = { budget = 24 }` on `default`, the
`doc_only` twin as its own filterless gate lane, and a parallel-free
`default-serial` twin for the process-isolated serial profile (see the findings
below for why that third entry exists). All measured warm:

    brokkr check          4m29s -> 2m21s
    brokkr check --gate   7m18s -> 5m23s
    default sweep tests    217s -> 82s   (claims 1-4, pole
                                          nautilus-polymarket/exec_client 61.5s)

The ledger is unchanged across the adoption: 4 shapes, 32976 pairs, 32815 run,
0 orphaned; the doc lane runs 69 doctests the parallel sweep can no longer
carry. No watchdog trips and no new flakes across four parallel runs.

The cache-domain default budget was arithmetically wrong for this suite, not
merely conservative: at budget 6 the pole binary's proportional share is
6 x 193/1229 = 0.94, floored to 1, so both warm runs converged to `claims 1-1`
with betfair/exec_client serializing its whole 193s and the sweep at ~330s -
slower than not fanning out. A suite spread over ~150 binaries needs the
budget to clear `total_serial / pole_serial` before the pole can claim a
second slot; worth a line in the `parallel` docs, since the failure reports
itself as a clean green run.

Two findings against the new code, both reproduced - **both fixed
2026-09-02** (test-phase refusals are now voiced before the summary line, and
the timing store is keyed by entry name so profiles share warmth; the budget
floor got its line in the `parallel` docs):

- **The parallel x isolation refusal is silent.** A profile with
  `isolation = "process"` referencing a sweep that declares `parallel` is
  refused as documented, but the run prints nothing - `[error] check failed
  in 41.2s` with no line naming the sweep, the conflict, or that a refusal
  fired. In a gate composed of lanes it presents as tier1 passing and the run
  dying between lanes with no diagnostic. The remedy on the consumer side is
  a parallel-free twin entry of the same build shape (clippy dedupes it);
  the remedy in brokkr is to print the refusal.
- **The parallel timing store is keyed by the lane-qualified label, not the
  entry.** `default` (bare check) and `tier1/default` (under the gate's lanes
  profile) warm separately: the gate's first parallel run re-paid the whole
  count-weighted warm-up (`claims 1-3`, betfair serialized, 195.6s) minutes
  after plain check had converged to 82s on the identical entry. Keying on
  the entry name (or build shape) would let every profile that runs an entry
  share its warmth.
