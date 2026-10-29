# Library harness: design and slice plan (brokkr#46)

Transient chain document for bz brokkr#46. Each link of the chain takes one
slice below, lands it green, updates this file in the same commit, and the
chain's last link deletes it and files the pbfhogg list. Durable text never
cites this file; the binding protocol is `docs/commands/library.md`
(`brokkr man library`), which grows by one section per slice and says only
what brokkr does at that commit.

## Where things live

- `docs/commands/library.md`: the binding protocol and the command, a man
  topic. It replaces the untracked draft `reference/library-harness.md`,
  which is folded into this file and deleted: a protocol pbfhogg writes a
  harness against has to be readable as `brokkr man library` from the
  pbfhogg tree, and reference/ files are not man topics. The doc documents a
  field of the describe document only once brokkr acts on it.
- `src/pbfhogg/library/`: the command (`mod.rs`), the describe document
  (`describe.rs`), and one module per later concern. The harness executable is
  reached only through `src/pbfhogg/binary.rs` (`LibraryHarness`), under the
  same one-owner textlint rules as the CLI handle.
- Config: `[pbfhogg.library]` (see R1).

## Rulings

Rulings marked (ledger) were reached before the chain and are restated here
with what this link settled about them; the rest are this link's. A later link
may argue against one with evidence, in a spar, and says so in its commit;
none is reopened quietly.

### R1 Config: `[pbfhogg.library]`, a conditional claim

`[pbfhogg.library.harness]` is a `HarnessConfig` (package, binary, features,
debug, profile), required inside `[pbfhogg.library]`. Later slices add keys
to `[pbfhogg.library]` (the gate's `require`, slice 5).

`pbfhogg` is not a reserved top-level key today: a `[pbfhogg]` table loads as
a legacy hostname section, so reserving the name would change what an
existing config means (AGENTS.md config contract). brokkr claims the
top-level `pbfhogg` table only when it carries a `library` key. A host table
holding `library` has never loaded (`HostConfig` denies unknown fields), so
the claim breaks no config; a `[pbfhogg]` table without `library` stays the
hostname section it always was. The claimed table admits only project keys
(`library`), so host fields beside `library` are an error naming the split.
The claimed table belongs to `project = "pbfhogg"`; in any other project it
is a load error like every other foreign project section. One predicate
decides the claim, applied to the final composed table (after includes
merge, never per layer), and both the host parser and the project-section
parser use it. `[host.pbfhogg]` is always a host and coexists with a
claimed `[pbfhogg]`. The user-wide layer refuses `pbfhogg` as it refuses
every other project key. Config is read from
the live tree, also under `--commit` (as every command).

### R2 One command, `brokkr library` (ledger)

Pbfhogg-only. Bare lists the scenarios of the harness built from the tree
under test (`--commit REF` builds it from a worktree). `SCENARIO` runs one as
a verification (slice 2), `--all` certifies (slice 5), `--bench N` measures a
single SCENARIO (slice 6), `--hotpath`/`--alloc` a single-call scenario
(slice 7). `--all` never measures (R16).

### R3 The harness build and handle

The harness is a bin of a pbfhogg workspace member named by
`[pbfhogg.library.harness]`, built with `harness_build::build_for_harness`
from the build root (the `--commit` worktree when given) under the profile
`--profile`/`--debug`/`--release`, else the config's, else release (the
library is measured in release; one build serves list, run and bench).
`src/pbfhogg/binary.rs` owns the build and returns a `LibraryHarness`
handle: it holds the brokkr lock and an `executable_guard` registration like
`PbfhoggBinary`, re-stats its executable before every spawn, and exposes no
path. Every flow that needs both the harness and the CLI (slice 4) runs both
cargo builds before it makes either handle, since a live handle refuses a
build.

### R4 Describe

`HARNESS --describe-harness`, run in the project root with
`BROKKR_MARKER_FIFO` and `BROKKR_HARNESS_ARTEFACT_DIR` removed from its
environment (an inherited one would otherwise reach it), must exit 0 within
60 s, write nothing to stderr, and print one describe document on stdout of
at most 1 MiB (captured with a sentinel byte past the limit and refused when
longer, since the bounded capture truncates silently). Anything else fails naming what happened. The harness writes
nothing anywhere in describe mode (producer obligation; brokkr does not
check the file system).

### R5 Strict JSON envelope

Every document either side writes is one JSON object, UTF-8, no byte order
mark, nothing after it but whitespace. Unknown keys, duplicate keys, missing
required keys and wrong types are refused naming the path into the document
(`scenarios[2].roles[0].owner`). Integers are JSON integers in the stated
range (no fraction, no exponent). Strings in identifiers and one-line text
carry no control characters. A `protocol` field (integer) is in every
document the harness writes; brokkr implements 1 and refuses any other value
naming both. The protocol stays 1 for the whole chain: fields land with the
slice that acts on them, and v1 is final when the chain closes.

Implementation (sparred): serde derive cannot enforce this. A derived struct
also accepts a positional JSON array, `Option` collapses an absent key into
`null`, `flatten` loses the duplicate guarantee, and its errors carry line
and column, not a document path; parsing into `serde_json::Value` first
collapses duplicate keys. So brokkr parses each document with its own
visitor into a tree that keeps every object member in order (integers kept
as integers, any fraction or exponent refused at the token), then extracts
typed values by hand with a path, refusing a duplicate key, an absent
required key, a present forbidden one and a wrong type, each naming its
path. The module (`src/pbfhogg/library/json.rs`) is shared by every
document of the protocol. One accepted exception: a number beyond a double's
range (`1e400`) is refused by serde_json itself, at a line and column, not a
path; carrying a path for it would need serde_json's `arbitrary_precision`
feature crate-wide, which changes every other JSON reader in brokkr.

### R6 Identifiers and the reserved namespace

Scenario, call, role and assertion ids match `[a-z0-9][a-z0-9_.-]{0,63}`.
Every call id is therefore a valid FIFO call id (docs/commands/measure.md).
`setup` is reserved as a call id (it names the harness's own preparation).
Assertion ids starting `brokkr.` are reserved for results brokkr adds
(`brokkr.stream`, `brokkr.cli`, `brokkr.seal`; finding 12): they are keyed by
call, shown as `<call>/brokkr.stream`, and can never collide with a harness
id. Capability ids (slice 5) use the id grammar and end in `.v<N>`
(`cancel.pre-decision.v1`); a capability whose meaning changes gets a new id.

### R7 The describe document, as slice 1 lands it

```json
{
  "protocol": 1,
  "scenarios": [
    {
      "id": "repeat-cat",
      "description": "cat the input twice in a row",
      "deadline_s": 600,
      "roles": [
        {"id": "input", "kind": "pbf", "source": "dataset", "variant": "indexed"},
        {"id": "out-a", "kind": "pbf", "source": "output", "owner": "a"},
        {"id": "out-b", "kind": "pbf", "source": "output", "owner": "b"}
      ],
      "calls": [
        {"id": "a", "label": "cat input -> out-a"},
        {"id": "b", "label": "cat input -> out-b"}
      ],
      "assertions": [
        {"id": "a.outcome", "call": "a"},
        {"id": "b.outcome", "call": "b"}
      ]
    }
  ]
}
```

- `scenarios`: one or more, ids unique.
- `description`, call `label`: one line, 1 to 200 bytes.
- `deadline_s`: 1 to 86400, the wall from spawn to exit brokkr gives one
  process of the scenario (slice 2 enforces it).
- `roles`: ids unique within the scenario; may be empty. `kind` is `pbf`,
  `file` or `directory`; `source` is `dataset`, `generated`, `output` or
  `scratch`:

  | source | kinds | owner | other keys |
  |---|---|---|---|
  | dataset | pbf | absent | `variant` required (a dataset variant name, grammar-checked here, resolved at binding) |
  | generated | pbf, file, directory | absent (setup owns it) | |
  | output | pbf, file, directory | a call id of this scenario | |
  | scratch | directory | a call id of this scenario | |

  pbf-only dataset roles are a deliberate v1 narrowing. `kind: pbf` on an
  output or generated role declares a PBF in pbfhogg digest's domain (the CLI
  equality of slice 4 accepts only those).
- `calls`: one or more, ids unique, none `setup`.
- `assertions`: the inventory. Ids unique within the scenario, none
  starting `brokkr.`; `call` is a call id of this scenario or `null` for a
  scenario-level assertion. May be empty.
- Ownership (ledger): every writable role (output, scratch) has exactly one
  owner by construction (`owner` is one call id); generated roles are owned
  by setup; dataset roles are read-only and shareable. Each role gets its own
  path at binding.

Fields added by later slices (each documented when it lands): call `stream`
(slice 3), call `cli` (slice 4), assertion `covers` (slice 5), scenario
`hotpath` and `isolation` (slice 7).

### R8 Listing

`brokkr library` prints, per scenario, its id, call count, deadline and
description, then one line per call (id, label) and one per role (id, kind,
source, variant or owner). Data output on stdout; build narration through
`output`.

### R9 The run directory and its reservation (slice 2; replaces the ledger's parent hold)

- Role paths live in a per-run directory brokkr allocates under the host
  scratch directory, `<scratch>/library/<scenario>/run-N/`, outside any
  private directory, with this layout, created by brokkr before the launch:
  `request.json`, `cwd/` (empty), `artefacts/` (empty), `roles/`. Each
  output, scratch or generated role binds `roles/<role id>`, whose initial
  state is: output (any kind) absent; scratch an empty directory; generated
  file or pbf absent; generated directory an empty directory. Dataset roles
  bind the resolved dataset file. `evidence/` is created by brokkr only after
  the harness exited and the layout check ran.
- Every launch of the run (the harness, each CLI replay, each digest)
  reserves the **whole run directory** as Write coverage, besides its own
  inputs (dataset files, Read), its outputs and its private directory, and
  lends that lease to its child. Coverage is a new `LaunchSpec` input,
  reservation only: it is not an output, so `check_placement` (which reads
  every `writes` entry as a publication destination and checks its parent's
  mount against the private directory) never sees it. Slice 2 adds it to
  `LaunchSpec` for the harness launch; slice 4 gives `PbfhoggBinary` a
  run-scoped invocation context (cwd, coverage, private placement in
  `run-N/`) for replays and digests, leaving every other caller as it is,
  and updates the placement rule in docs/projects/pbfhogg.md "Launch
  reservations and the private directory" in the same commit. Coverage is
  never spelled as a fake `PbfArgv` output (that would negotiate output flags
  on a read-only digest). The lease store never compares a lease's
  resources with each other, so coverage over the lease's own outputs,
  disposables and private directory is no conflict; release removes only
  `Private` resources, so the run directory survives it.
- Launches of one run are strictly serial: at most one admitted launch at a
  time, and `Launched::finish()` (the lease released, not just the child
  reaped) completes before the next is admitted; a release failure stops the
  run there, failed, with its evidence kept. The global lock spans
  allocation, every launch, every comparison and the final removal.
- Why not the ledger's `ParentHold` (a parent reservation with child
  admission under it and two inherited descriptors per child): sparred in
  this link, neither side could name a race it closes that this does not.
  Between launches the global lock excludes every cooperating command
  (`clean` takes it); during a launch the child holds a lease on the whole
  directory, so a brokkr that dies leaves the directory reserved for as long
  as the child lives; between launches no child exists to mutate it. Leases
  on the role paths alone would not do: a surviving child would leave the
  request, the cwd and the artefact siblings unreserved. The parent-hold
  hierarchy and multi-descriptor inheritance are therefore dropped, and no
  slice builds them. This holds only with the serial-launch rule below; a
  later slice that wants two live launches in one run reopens it.
- Success removes the run directory under the lock through the reservation
  gate (which refuses while any lease, such as an orphan's, overlaps it). On
  failure it is kept: ordinary scratch that routine `brokkr clean` removes
  through the gate, and that the cleanup after an interrupt skips (that
  cleanup runs after the command's guards dropped,
  `src/main_parts/bootstrap.rs`, so the global lock no longer protects it).
- The private directory name is generated at launch (the
  `PRIVATE_DIR_PLACEHOLDER` substitution), after the request is written, so
  it is passed on the harness argv as `--private-dir DIR`, never in the
  request.

### R10 The run protocol (slice 2)

- `HARNESS --test-harness REQUEST --private-dir DIR`, cwd `run-N/cwd/`;
  environment `BROKKR_HARNESS_ARTEFACT_DIR` (`run-N/artefacts/`) and
  `BROKKR_MARKER_FIFO`, always: the harness's walls are `MarkerFifo::now_us()`
  readings, a clock that exists only on an opened FIFO, so slice 2 already
  attaches the FIFO, drains it and keeps the parsed stream as evidence;
  slice 3 judges it.
- Request: `{"protocol": 1, "scenario", "iteration", "bindings": {role:
  absolute path}}`. `bindings` names every role and nothing else.
  `iteration` counts the processes of one measured run from 0.
- Exit: 0 ran and every reported result passed; 1 ran and some reported
  result is fail or unevaluated; 2 could not run (a request it cannot honour,
  its schedule violating ownership); anything else, a signal, or the deadline
  is a failure. 0 with a non-pass or 1 with none is a protocol failure.
  Artefacts are read on 0 and 1 only; on 2 the run fails naming the harness's
  `refusal.json` if it wrote one (one line `reason`), else the exit.
- stdout and stderr must be empty on every exit (captured, bounded at 64 KiB
  each for the report; anything at all fails). Diagnostics go through
  `host::Diagnostics` into the artefacts.
- Producer obligations brokkr does not check (finding 13): the harness reads
  only the request, the bound input roles and the private directory, writes
  only its artefact directory, the roles its calls or setup own and the
  private directory; setup is deterministic and runs before any call; the
  call schedule honours ownership; it sets its own soft `RLIMIT_NOFILE` where
  a scenario's plan says so before any library work, and reports it in the
  call context; it never resets `SIGPIPE` to default. What brokkr does
  check: exit, empty stdout and stderr, the deadline, the artefact tree and
  its documents, the inventory, and, after the harness exits, the run
  directory against the layout of R9 (detection, not prevention: it sees
  what survived, never a read or a write undone before exit): `run-N/` holds
  exactly `request.json` (bytes unchanged), `cwd/` (the same inode, still an
  empty directory), `artefacts/`, `roles/` and nothing else (the launch's
  private directory is gone by then, removed with its lease); `roles/` holds
  only bound role names; a file or pbf role is absent or a regular file, a
  directory role is a directory whose contents are not judged.
- Artefact tree (finding 14): exactly `scenario.json` and `calls/` holding
  exactly one `<call id>.json` per declared call; every entry a regular file
  or that one directory, checked with `lstat` (a symlink, a special file or
  any other entry fails); each file at most 16 MiB; `scenario` and `call`
  fields equal the scenario and the file's call id.
- `scenario.json`: `protocol`, `scenario`, `plan` (R13), `assertions` (the
  scenario-level results).
- Slice 2 validates the value domain of `plan` and `context` (R13) and the
  plan's required structure; slice 6 canonicalizes and digests them.
- `calls/<id>.json`: `protocol`, `call`, `entry_us`, `return_us`, `seal_us`
  (0 to 2^63-1, `entry_us <= return_us <= seal_us`, `MarkerFifo::now_us()`
  at command entry, return, and when sealing ended), `outcome` (`ok`,
  `cancelled`, or `error` with `kind` and a one-line `message` of at most
  1000 bytes). Wall bound: `return_us - entry_us` and `seal_us - entry_us`
  are each at most the process wall brokkr observed (spawn to reap) plus
  1 s, else the run fails. The bound is relative on purpose: the readings
  share one clock per opened `MarkerFifo`, whose initialization offset
  cancels, while its absolute origin is not reliable (pbfhogg's
  `MarkerFifo::open` takes its `Instant` anchor before it samples the process
  age from /proc, so a preemption between the two counts twice; on the
  pbfhogg list). No positive lower bound holds (the FIFO can open just
  before entry); nonnegativity and order do. That the readings sit at the
  specified boundaries stays a producer obligation no consumer check can
  prove. Then `publications`, `cancel`,
  `seal`, `snapshot`, `stream`,
  `context` (a nonempty object, R13), `diagnostics` (finding 12: the captured
  `host::Diagnostics` events, each `{"level", "message"}`, reported and never
  judged by brokkr), `assertions` (this call's results).
- Results: `{"id", "status": "pass"|"fail"|"unevaluated", "detail"}`, detail
  at most 4000 bytes. Across the documents the results are exactly the
  inventory, each in the file its `call` names. `unevaluated` fails exactly
  as `fail` does in every mode; the report keeps them apart (ledger).
- Seal (finding 9): `seal.state` is `sealed` (with `outstanding_blocks` 0),
  `outstanding` (count) or `refused` (one-line `reason`). brokkr adds
  `<call>/brokkr.seal`: pass only for `sealed`; anything else fails the run
  in every mode, because the snapshot and the stream verdict exist only for a
  sealed call. `snapshot` is an object of counter name to integer 0 to
  2^64-1 when sealed, else `null`.
- Publications and cancel (ledger, pbfhogg list): `publications` lists every
  publication the call decided, ordinals 1.. without gaps, `decision`
  `authorized`/`refused_cancelled`/`refused_error`, `installed` bool;
  `cancel` is `null` or the outcome `Invocation::cancel()` returned
  (`before_decision`/`after_decision` with `ordinal`, or
  `no_publication_pending`). brokkr validates their shape and consistency
  (an `after_decision k` names an authorized k; every publication after a
  cancel's ordinal is refused) and holds no other meaning; order judgments
  are the harness's assertions.
- Verdict: a run fails on any build, describe or request failure, a bad exit,
  any output, a deadline kill, an invalid or incomplete artefact tree, an
  exit inconsistent with the results, or any result (harness or brokkr) that
  is not pass. A failed run is never a sample; its run directory (request,
  artefacts, captured stdout and stderr, the parsed stream) is kept and named.

### R11 Call streams (slice 3; ledger, findings 3, 9, 14)

- The extension applies to every tagged record in every capture; the
  process stream and every untagged record parse exactly as today, so CLI
  captures are untouched. Tagged producers exist (`MarkerFifo::call` and
  `CallSink` since pbfhogg 387daff5, exercised by pbfhogg's tests), and
  today a caller could make one emit a counter named `@x` or a marker
  `!sealed drops=0`; no pbfhogg emitter does. The reservation is deliberate:
  it changes no stored trajectory (stored records are already parsed and are
  not re-read), and the sink refusal that makes it a producer contract goes
  on the pbfhogg list. Timestamps
  and whole-line trimming are today's (`<i64> `, the line trimmed); a sealing
  VALUE is an i64 as a counter value is; the 512-byte line limit, tag and
  newline included, is the producer's as today.
- The artefact `stream` object: `{"state": "sealed"|"failed"|"open",
  "drops": <u64>}`; `open` is a sink that was never sealed (its call's
  `brokkr.seal` already fails the run).
- Verification capture has no storage cap of its own: the caps of
  docs/commands/measure.md "Trajectory capture limits" bound what a measured
  run stores, and a verification run that hits one has lost records, so its
  streams are incomplete. Completeness for the verdict and completeness of a
  stored trajectory (slice 6) are separate facts.
- In a tagged record, a body starting `@@` is a sealing emission
  `@@NAME=VALUE` (NAME nonempty, no `=` or newline; VALUE an i64), and a body
  starting `!` is a terminal record, exactly `!sealed drops=N` or
  `!failed drops=N` (N a u64 in decimal without sign or leading zeros); any
  other body with those starts is rejected like any malformed record.
  Producer side (pbfhogg list): a call sink refuses, as it refuses today's
  bad names, a tagged marker body starting `!` and a counter name starting
  `@`, so ordinary records can never take those forms.
- Sealing emits each snapshot name exactly once, zeros included, after the
  sink stopped admitting ordinary records and every callback inside it
  returned; then the terminal record; nothing after it.
- Completeness: the run's reader lost nothing (no rejected line, no fragment,
  no capture cap; slice 3 splits `capture_truncated` so reader loss is told
  apart from a storage cap or output cut), the call's terminal arrived
  exactly once and reads `!sealed drops=0`, nothing of the call arrived after
  it, and the artefact `stream` agrees (`sealed`, 0).
- A tagged record whose call id the describe does not declare fails the run
  (protocol failure: lines are whole or dropped, so no loss can make one).
- Time order: every record of a call has `entry_us <= ts <= seal_us`;
  sealing and terminal records have `return_us <= ts`. Violations fail that
  call's stream verdict.
- Verdict when complete: sealing emissions name exactly the snapshot's
  counters with equal values (each saturated to 2^63-1 as the wire carries
  it), and every ordinary `@NAME=V` observation of a snapshot counter has
  `V` at most its final value. Incomplete is unevaluated. brokkr adds
  `<call>/brokkr.stream` only for a call whose describe `stream` list is
  nonempty; for the others the verdict is reported and judges nothing.
- Phases: a call's markers between `entry_us` and `return_us` are its
  phases, the last ending at `return_us`; markers after return are its tail.

### R12 CLI equality (slice 4; findings 2, 7, 8)

- The counterpart is declared in the describe document on the call, so it is
  validated before anything runs and its reservations are known up front:
  `"cli": {"argv": [TOKEN...], "scratch": bool, "policy_differences":
  [one-line strings]}`.
- Tokens: a literal string, or a placeholder: `{"input": ROLE}` (the bound
  path of a dataset or generated file or pbf role), `{"files_in": ROLE}` (every
  regular file directly inside a generated directory role, sorted by bytes of
  name, one argument each; a non-regular entry or an empty directory fails),
  `{"output": ROLE}` (an output role of kind pbf this call owns: brokkr passes
  a fresh path of its own and compares the two). At least one output
  placeholder; no output placeholder twice.
- Paths (finding 8). Producer obligation: a literal names no path, and no
  input the counterpart reads names another path (a config file naming
  files is outside v1), so the counterpart reads and writes exactly its
  placeholders and its private directory. brokkr holds no pbfhogg CLI
  semantics and cannot prove that; it narrows and detects: a literal holding
  `/` or NUL, equal to `.` or `..`, or one of the flags brokkr negotiates
  (`--overwrite`, `--staging-dir`, `--tmp-dir`, alone or with `=`) is refused
  at describe; the replay runs in an empty cwd brokkr owns
  (`run-N/replay-<call>/cwd/`), checked afterwards to be the same empty
  directory. That detects a surviving write relative to cwd; it detects no
  read and nothing undone before exit, and the doc says so.
- Replay after the harness exited, outside every timed interval, through
  `PbfhoggBinary::invocation` (reservation, private directory and flag
  negotiation of docs/projects/pbfhogg.md) in its run-scoped context (R9),
  one launch at a time; must exit 0.
- Equality `digest-v1`: `pbfhogg digest FILE` with the same-commit CLI on
  both outputs, each exit 0 with exactly the four-line report
  (`xxh128 v<N> <32 hex>`, then `node`, `way`, `relation` lines `<kind>
  <count> <32 hex>`, lowercase hex, each newline-terminated, nothing else);
  equal when the four lines are equal. brokkr implements `xxh128 v1` only;
  any other algorithm or version fails naming it (a new canonical version is
  a brokkr change, never accepted unread). A
  digest refusal (exit 2) or any other output fails; no fallback to `diff`.
  brokkr adds `<call>/brokkr.cli`.
- Reuse key for a counterpart's digest across the iterations of one run: the
  CLI executable's XXH128, the token argv as declared (placeholders by role
  name, not resolved paths), the identity of every input role it reads (a
  dataset file's recorded digest; a generated role's content digest, XXH128
  of a file or the tree digest of a directory, computed by slice 4 after the
  harness exited and reused by slice 6; brokkr has no observation point
  between setup and the calls, so this rests on the producer obligation that
  nothing changes a generated role after setup), and the scratch flag. The initial output
  state is always absent (a fresh brokkr path every replay), so it is a
  constant of the key, stated. The library output is digested every
  iteration.

### R13 Plan, context and identity (slice 6; findings 4, 5)

- Producer obligation (finding 4): the plan carries every choice that
  affects the workload (each call's command and options, inputs by role,
  the Host each call gets, order, grouping and concurrency, setup and every
  generated recipe); each call's context carries every effective per-call
  setting (Resources figures, executor, page-cache policy, process actions,
  soft NOFILE at start). Neither carries anything run-specific: no path, no
  timestamp, no iteration, no pid. brokkr does not interpret either.
- Consumer checks (slice 2): the value domain below; `plan` has a `calls`
  object whose keys are exactly the declared call ids, each a nonempty
  object (so `{}` cannot pass for a plan); `context` is a nonempty object;
  no string anywhere in either, object keys included, contains the run
  directory's absolute spelling or a bound dataset path's. That is a narrow
  check and a restriction on the producer's value domain (it also refuses
  text that merely contains such a spelling), not proof of path
  independence: relative spellings, aliases and split strings evade it.
  Completeness stays the producer's obligation.
- Value domain: objects (keys unique, valid UTF-8), arrays, strings (valid
  UTF-8, no NUL), `true`, `false`, `null`, and integers in -2^63 to 2^64-1
  written without fraction or exponent. No other number, and not `-0`
  (serde_json reads it as a float; the reader keeps every such number as
  `Json::NotInteger`, which no read accepts, so a plan or context walk must
  refuse it too).
- Canonical form, `brokkr-json-v1` (a custom form, not RFC 8785, whose
  numbers are doubles): no whitespace; object keys sorted by their UTF-8
  bytes; arrays in order; integers in shortest decimal (no `+`, no leading
  zeros, `0` never negative); strings escape `"`, `\` and U+0000 to U+001F
  (as `\b \f \n \r \t` where one exists, else `\u00xx` lowercase), every
  other character literal. Digest: XXH3-128 seed 0 over the canonical bytes,
  lowercase hex, `plan-v1:<32 hex>` and `context-v1:<32 hex>`.
- Comparison identity of a scenario row: scenario id, plan digest, the
  identity of every dataset file bound to a role (the dataset digest brokkr
  already records), the content digest of every generated role after setup
  (XXH128 of a file, the tree digest of a directory), the build (features,
  profile), the host fingerprint today's pairing uses, and the map of every
  call id to its context digest (identical plans on a machine-probed Host
  can run under different effective Resources, which the scenario wall
  reflects). A call row inherits the whole scenario identity and adds its
  call id. The commit is never part of it. Plans and
  contexts are stored canonical (deduplicated by digest), not only their
  digests, so an unpaired comparison names the first differing field.
- Within one measured run every iteration must agree on plan digest,
  generated digests and each call's context digest, else the run fails
  naming the field.

### R14 Concurrency is a claim (ledger)

A concurrent group carries a required harness assertion that the calls'
lifetimes intersect; nothing claims parallel execution. Per-call walls of a
concurrent group are contention measurements; the scenario wall is the
headline.

### R15 The gate (slice 5; finding 11)

- `[pbfhogg.library.require]` maps capability id to its one-line meaning,
  in the live tree's config, also under `--commit`.
- Coverage: an assertion's `covers` lists capability ids; a call's `stream`
  list is the coverage of its `brokkr.stream` result. brokkr's other results
  cover nothing.
- `--all` runs every scenario, then classifies each required capability by
  this precedence: **failed** when any covering result is fail or
  unevaluated, or a declared covering assertion has no result (its scenario
  failed before reporting); else **passed** when at least one covering result
  passed; else **unsupported** (no declared assertion covers it, as on a
  commit that predates it). Unsupported is stored and shown apart, is never
  called a regression, and fails certification. Certification passes only
  when every scenario passed and every required capability passed. The
  applied requirement list and its digest are printed and stored with the
  run (slice 6 stores it on measured rows; an `--all` run stores a
  certification record, slice 5 decides where).

### R16 Recording (slice 6; finding 15)

- `--bench N` takes one SCENARIO; `--all` never measures. N whole
  processes, each fully verified before it counts; any failure fails the run
  and stores no row.
- The stored group comes from the iteration with the fastest process wall;
  ties go to the lowest iteration index (as `winning_index` does today).
  Iterations are indexed from 0, as the sidecar's `run_idx`.
- Rows: one scenario row (`library:<scenario>`, wall = that process's wall,
  iterations = every process wall in order) and one per call
  (`library:<scenario>/<call>`, wall = `return_us - entry_us` of the chosen
  iteration, iterations = that call's wall in every iteration in order).
  Every row carries the plan digest, its iteration and the scenario row's id;
  the group is written in one transaction (today's `ResultsDb::insert` writes
  one row per transaction; slice 6 adds the group write and its migration).
- Sidecar: every iteration's trajectory is stored under the scenario row's
  id with each call's `entry_us`, `return_us`, `seal_us` (sidecar.db
  migration), so the open final interval of a call stream closes at return.
  A failed run stores no row; its trajectories are stored under a fresh
  failure UUID that the failure report prints.
- `--compare` pairs scenario and call rows by the R13 identity; rows whose
  plan digests differ are shown unpaired with the first differing plan field.
  Lib against CLI is never folded into `--compare`.

### R17 Hotpath and CLI timing (slice 7; finding 6)

- `--hotpath`/`--alloc` only for a scenario whose describe sets `hotpath:
  true`, which brokkr also requires to have exactly one call (hotpath state is
  process-wide). One scenario row with the hotpath report; no call rows.
- Lib against CLI timing only for a scenario with `isolation: true` (one
  call, the first library work in a fresh process, with a `cli` counterpart),
  against a matched CLI boundary: a reserved process-stream marker pair the
  pbfhogg CLI emits at command entry and return (pbfhogg list). Until it
  exists brokkr shows no lib-vs-CLI timing. The feature itself (timed CLI
  counterpart runs, boundary matching, recording its policy differences, the
  comparison view) is slice 8, blocked on pbfhogg, and the chain closes
  without it, saying so.

## Slice plan

Each slice lands green with its callers, docs and tests; none leaves dead
code. Sizes are rough source-plus-tests lines.

1. **Listing** (this link). `[pbfhogg.library]` with its conditional claim,
   `brokkr library [--commit REF] [--profile/--debug/--release]`,
   `LibraryHarness` build and handle in `binary.rs`, describe capture,
   strict parse and validation of the R7 fields, the listing, the man topic
   `library`, config docs, a fixture harness script in tests. About 1400.
2. **Verification run.** `brokkr library SCENARIO` with
   `--dataset/--variant/--snapshot` binding, the run directory and its
   layout (R9), `LaunchSpec` coverage and the whole-directory reservation
   of the harness launch, the serial-launch rule, the wall bound, the
   interrupt-cleanup exemption and gated removal, the request,
   `--private-dir`, the FIFO attached and drained into evidence, the
   deadline, empty stdout/stderr, the artefact tree and documents (R10, the
   stream not yet judged), the plan and context domain checks (R13), the
   inventory, `brokkr.seal`, the post-run layout check, the verdict and kept
   evidence. About 2300; the largest slice, not splittable without a dead
   layer.
3. **Call streams.** The tagged-record extension in `parse_fifo_record`, the
   split of `capture_truncated` into reader loss and its other causes, call
   `stream` in describe, the per-call verdict and `brokkr.stream`, phases and
   tail in the report. About 1000.
4. **CLI equality.** The second build before handles, call `cli` in
   describe, the run-scoped `PbfhoggBinary` invocation context and the
   pbfhogg.md placement-rule update, generated-role content digests, replay,
   the digest runner and
   its parser, `brokkr.cli`, the reuse key. About 1200.
5. **The gate.** Assertion `covers`, `[pbfhogg.library.require]`, `--all`,
   classification, the certification record. About 700.
6. **Recording.** `--bench N`, `brokkr-json-v1` canonicalization and the
   plan and context digests (R13), iteration agreement, the results.db group
   write and migration, sidecar.db call boundaries and the closed final
   interval, `--compare` pairing. About 2200, one slice: a split would store
   trajectories whose call ends no view can show.
7. **Hotpath and close.** `hotpath` and `isolation` in describe,
   `--hotpath`/`--alloc`; then the chain's close: delete this file, file the
   pbfhogg list, comment on and close #46 naming slice 8 as not built. About
   500.
8. **Lib-vs-CLI timing** (R17). Blocked on the pbfhogg CLI marker pair; not
   part of the chain. About 800 once unblocked.

Protocol 1 is unstable until the chain closes, and compatibility with an
intermediate dialect is unsupported: each slice adds the describe
and artefact fields it acts on, so the brokkr of slice k refuses fields of
slice k+1 as unknown. No producer meets an intermediate dialect: pbfhogg
writes the harness from the pbfhogg list, filed at the close, against the
finished `docs/commands/library.md`; a `--commit` baseline of pbfhogg can
only carry a harness written after that.

## Landed

- Slice 1 (link 1): as planned. Where a later link starts: the command is
  `Command::Library` (Direct worktree policy, so `--commit` is a plain field;
  slice 6 may move it to `ModeArgs`), dispatched to
  `pbfhogg::library::list`; `LibraryHarness` in `src/pbfhogg/binary.rs`
  (add the test-harness spawn there, beside `describe`); `json.rs` is the
  reader every later document uses (`Field::root`, `as_object`, `required`,
  `optional`, `forbid`, `finish`); `describe.rs` is where each slice adds its
  describe fields (`stream`, `cli`, `covers`, `hotpath`, `isolation`), with
  the example in `docs/commands/library.md` kept equal to the one in
  `describe_tests.rs` by a test. The doc has five `##` sections; at eight it
  becomes section-addressed, so keep one subject per `##`.

## The pbfhogg list

Filed by the chain's last link with `bz new -p pbfhogg`, once settled.

- The harness member itself (`publish = false`, a bin), implementing the
  protocol in `docs/commands/library.md`; wired into pbfhogg's sweeps and
  lints, with the textlint rules that name the CLI as the only caller of
  `emit_writer_counters`/`emit_read_counters` widened, and pbfhogg#24's
  inherited-descriptor promise and its guards extended to it (it inherits
  brokkr's lease descriptors).
- `--describe-harness` and `--test-harness REQUEST --private-dir DIR` as
  specified.
- Invocation lifecycle Open, Draining, Sealed with `seal()` as the ledger
  states, documented as a binding pbfhogg library contract
  (docs/guide/advanced.md or an ADR) that the protocol can cite (finding 10).
- Sink sealing: per-call state cell, `MarkerFifo::call` refusing an issued
  id, sealing emissions and the terminal record (R11), per-sink drop counts,
  and refusing tagged marker bodies starting `!` and counter names starting
  `@`.
- `Invocation::cancel()` returning `BeforeDecision`, `AfterDecision` or
  `NoPublicationPending`, linearized with the publication decision.
- A reserving fd provider shipped by the library, for concurrent
  descriptor-heavy calls.
- A full-fidelity digest with a documented equality (closes the content
  blind spots; does not replace the harness's own assertions).
- The CLI entry and return marker pair (R17).
- `MarkerFifo::open`'s clock: it captures its `Instant` anchor before it
  samples the process age from /proc (`src/debug.rs`, `src/process.rs`), so a
  preemption between the two is counted twice in every reading. Sample the
  age first, or anchor both on one reading. brokkr's wall bound is relative
  (R10) and does not depend on it.

## Deferred, with size

Slice 8, lib-vs-CLI timing (R17), about 800, blocked on the pbfhogg CLI
marker pair; it stays a gap when the chain closes unless pbfhogg ships that.
