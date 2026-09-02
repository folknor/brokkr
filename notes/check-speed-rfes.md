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

## RFE 2: make the nextest harness lane selectable

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
