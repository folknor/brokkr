# Library harness internals

How brokkr's side of the pbfhogg library harness (`src/pbfhogg/library/`,
`LibraryHarness` in `src/pbfhogg/binary.rs`) is built, and why. The protocol
itself, binding on brokkr and on the pbfhogg harness alike, is
`docs/commands/library.md` (`brokkr man library`); this file records the
invariants the implementation rests on and the alternatives that were
weighed and rejected, so a later change does not reopen them unknowingly.
Each holds until someone argues it down with evidence and changes this file
and the code together.

## Protocol stability

Protocol 1 is final. While it was being built, a describe or artefact field
landed only with the brokkr change that acted on it, and brokkr refused
every field it did not implement, so no producer ever met an intermediate
dialect. Now a change to the schema or to what brokkr makes of it comes with
a new protocol number; the producer-defined domains library.md names stay
open without one. The strict reader still refuses unknown keys, so that
rule is what keeps a newer harness from being half-read by an older brokkr.

`isolation` is part of version 1 although nothing times it yet: the library
against CLI timing it is for needs nothing more from the harness (a call's
`entry_us` and `return_us` exist already, and the boundary it lacks is the
CLI's), so declaring it now lets that feature arrive without a protocol
change. What brokkr does with it today is validate and list it.

## The strict JSON reader

`json.rs` reads every protocol document with its own visitor into a tree
that keeps every object member in order, integers as integers. serde derive
cannot enforce the closed schema: a derived struct accepts a positional
array, `Option` folds an absent key into `null`, `flatten` loses duplicate
detection, and errors carry a line and column instead of a document path;
reading into `serde_json::Value` first collapses duplicate keys. One accepted
exception: a number beyond a double's range is refused by serde_json itself,
at a line and column, since carrying a path for it would need serde_json's
`arbitrary_precision`, which would change every other JSON reader in brokkr.

## The run directory and its reservation

Every launch of a run (the harness, each counterpart replay, each digest)
reserves the whole run directory as write coverage
(`LaunchSpec::with_coverage`, reservation only: no output, so the private
directory's placement never compares its mount, and the release keeps it),
launches are strictly serial (each `Launched::finish`, the lease released,
before the next is admitted), and the global lock spans allocation, every
launch, every judgment and the final removal. Between launches the lock
excludes every cooperating command; during one the child's lease covers the
whole directory, so a brokkr that dies leaves it reserved for as long as the
child lives.

This replaced a parent hold with child admission under it and two inherited
lease descriptors per child: no race was found that the hierarchy closes and
this does not. Leases on the role paths alone would not do, since a
surviving child would leave the request, `cwd/` and the artefacts
unreserved. The equivalence holds only with serial launches: a change that
wants two live launches in one run reopens it.

Release comes before judgment, always: nothing in the run directory is read
until the launch that could have changed it is released, and a release that
finds the lease held leaves the directory untouched (some process may still
change it). Every run-scoped CLI launch goes through
`PbfhoggBinary::run_in`, whose `measure::Supervised` outcome keeps an
admission or release that did not establish a safe state apart from a run
that failed; any admission error counts as unsafe, a conflict refusal
included, since the lease may exist and its release be in doubt.

The interrupt cleanup runs after the command's guards dropped
(`src/main_parts/bootstrap.rs`), so the lock no longer protects the
directory then; that cleanup never removes a kept run directory, and only
routine `brokkr clean` does, through the gate.

The marker FIFO lives in the project's `.brokkr/`, never in the run
directory, so the layout stays fixed.

## Profiled runs

A profiled run's hotpath report goes in the run directory, not under
`.brokkr/` as a launch disposable as other profiled launches do: a
disposable has the `Private` disposition, which the release removes, so it
would have to be read before the release, breaking release before
judgment. In the run directory the coverage reserves it, the release keeps
it, the layout check admits it only in a profiled run, a failure keeps it as
evidence, and a pass removes it with the directory.

`db::hotpath_data_from_json` accepts any object with a section in it, a
nameless or unmeasured entry included, so `profile.rs` judges the raw report
first: a called function with a nonnegative total of the mode's dimension,
and every entry named with a call count that fits an i64 (larger ones would
be dropped silently by the conversion).

Profiling is activated by `HOTPATH_OUTPUT_PATH` in the launch environment,
never by the build: brokkr removes the profiling variables from describe
mode and from every launch it does not profile, so a harness built with
the hotpath features (by `--hotpath`, or by configured features) still
writes no report and prints nothing there.

## Call streams and capture

The capture keeps every FIFO record (marker, counter, sealing emission,
terminal record) in one arrival-ordered list, so no index can disagree with
the order; markers and counters are projections of it. Sealing emissions
count against the counter budget and terminal records against the marker
budget, as budgets only: they are never markers. Loss is split by cause
(`CaptureLoss`), and a call stream reads only the causes that may lose a
FIFO record. The census of tagged call ids is kept before any budget drops a
record and is bounded at the declared calls plus one, so an overflow proves
an undeclared id. Every FIFO drain is bounded in bytes, measured runs
included, because an unbounded drain let a child writing without pause
starve the supervisor's deadline and stop checks.

Proven stream defects fail on a lossy capture too: loss cannot manufacture
a sealing value the snapshot contradicts, a name the snapshot lacks, an
observation above the final value, or a sink end from an unsealed call.

## CLI equality

A `SCENARIO` run always builds the CLI, release with default features,
before either handle exists, since a live handle refuses a build. A lazy
build (describe first, then drop the harness handle and build the CLI only
when a counterpart exists) was rejected: two handle lifetimes over one run
for a near no-op cargo run. There is no digest capability probe: a pbfhogg
commit that has the harness member postdates `pbfhogg digest`.

## Identity and recording

brokkr-json-v1 is brokkr's own canonical form, not RFC 8785, whose numbers
are doubles; plan and context integers reach 2^64-1. `tree-v1` frames every
entry at fixed width because a newline-framed draft collided structurally (a
link target and a file name each holding a newline);
`preflight::compute_tree_digest` is not used, since it refuses an empty
directory and is a pinning tool.

The identity is the rows' arm qualifier and `meta.library.identity`. It
carries the harness features because `cargo_features` is not a pairing key.
Agreement with iteration 0 is part of each process's verdict, so a
disagreeing process fails like any verification, with the baseline as
evidence. Call rows carry the snapshot under `snapshot.`, so no harness
counter name can overwrite or impersonate a brokkr key.

Capture budgets are per process, unlike every other measurement's shared
budget: a shared one would make a later process unverifiable for reasons
not its own. The bound is memory instead: each trajectory is stored as soon
as it is judged. Publication order is trajectories with the session
metadata, then the final metadata, then the results.db group transaction,
scenario row last, which publishes.

The requirement digest is not stored on measured rows: a measured run
takes one SCENARIO, which applies no requirement list, so the digest would
claim a certification context that never existed. A certification is a JSON
record under `.brokkr/library-certifications/`; SQLite (no reader) and
results.db (measurements only) were rejected for it. Its digest covers the
required ids only, so a changed meaning is a new id and a changed wording is
visible in the record, never hidden behind an equal digest.

`sidecar --compare` stays process-only, and `--durations` pairs START/END
over a whole call stream; both are adjudicated at their code sites in
`src/sidecar_cmd.rs`.

## Not built

Library against CLI timing (timed counterpart runs, boundary matching,
recording the declared policy differences, a comparison view, never folded
into `--compare`) waits on the pbfhogg CLI marking its command's entry and
return on the FIFO; it has a brokkr bz ticket of its own, blocked on the
pbfhogg one for that marker pair.
