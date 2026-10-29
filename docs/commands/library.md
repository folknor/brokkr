# `brokkr library` and the library harness protocol

pbfhogg used as a library, not as its CLI. The harness is a bin of a pbfhogg
workspace member that links the library at the commit under test and runs
scripted sequences of library calls in one process; pbfhogg writes it to the
protocol below, and a pbfhogg commit without that member has nothing for this
command to build. `brokkr library` builds the harness per commit (so
`--commit` works), lists the scenarios it declares, runs one as a
verification, measures or profiles one and records it, and runs them all to
certify the capabilities brokkr.toml requires.

pbfhogg owns what a call does: the scenarios, the library calls they make
and the assertions about them that only the process itself can judge. brokkr
owns the build, the process and the checks a process boundary can make. It
knows no pbfhogg API semantics, only the identifiers the harness declares
and the shapes below.

This document is the protocol, binding on both sides. It describes protocol
version 1. The guarantee is the strict reader: a harness output this
document does not describe is refused, never guessed at or half read, so a
harness and a brokkr that disagree on the schema fail loudly. The domains
the producer defines stay open: the contents of plans and contexts,
snapshot counter names, held value kinds, marker names, diagnostic kinds
and keys, capability ids and result details.

## Usage

```
brokkr library [--commit REF] [--debug | --release | --profile NAME]
brokkr library SCENARIO [--dataset NAME] [--snapshot KEY] [--commit REF] [...]
brokkr library SCENARIO --bench [N] [--force] [...]
brokkr library SCENARIO --hotpath [N] | --alloc [N] [--force] [...]
brokkr library --all [--dataset NAME] [--snapshot KEY] [--commit REF] [...]
```

Bare, builds the harness and lists the scenarios it declares: per scenario
its id, call count, deadline (and `hotpath` and `isolation` when it is
declared so) and description, then one line per call (with
the capabilities its stream claims, and beneath it its CLI counterpart, with
its scratch flag, an expected refusal and a declared overwrite stance, and
its declared policy differences), role (a seeded one marked) and assertion. The listing is data on
stdout; build narration goes where `brokkr man output-channels` says.

With `SCENARIO`, runs that scenario once as a verification (see Running a
scenario): quiet on pass, one result line; loud on fail, every failure and
the kept run directory, exiting non-zero. A scenario the describe document
does not declare is refused, naming the ones it does. A run also builds the
pbfhogg CLI of the same tree, release with default features whatever
profile the harness gets, the reference its counterparts replay against
(see CLI equality); both builds finish before either is used.

With `SCENARIO --bench N` (bare `--bench` means the default count), runs
it as N whole processes, each verified as `SCENARIO` verifies one and
agreeing with the first, and records them (see Measuring a scenario).
`--force` lets it measure a dirty tree, which stores no row.

With `SCENARIO --hotpath N` or `--alloc N` (bare means one), profiles a
scenario the describe document declares `hotpath`: the same N verified
processes, of a harness built with the instrumentation feature, recorded
with the profile (see Profiling a scenario). `--force` works as for
`--bench`. `--bench`, `--hotpath` and `--alloc` exclude each other.

With `--all`, runs every scenario once, then classifies each capability
`[pbfhogg.library.require]` names and writes a certification record (see
Certification). It never measures. `--all` and `SCENARIO` exclude each other.

- `--commit REF` builds the harness from a git worktree at REF. Config,
  including `[pbfhogg.library]` (its `require` too) and the host's datasets
  and scratch, still comes from the current tree. A commit that predates the
  harness member fails its build.
- `--debug`, `--release`, `--profile NAME` choose the cargo profile,
  overriding the config's `debug`/`profile`. The default is release: the
  library is measured in release. One build, with the configured features,
  serves listing, verification, `--bench` and `--all`; `--hotpath` and
  `--alloc` build it with their feature added (see Profiling a scenario).
- `--dataset NAME` (default the shared default dataset, as every pbfhogg
  command) and `--snapshot KEY` (default `base`, the dataset's primary data)
  choose the files the scenario's dataset roles bind. There is no
  `--variant`: each dataset role names its own variant, resolved in the
  chosen dataset and snapshot as `brokkr man datasets` describes, its
  `xxhash` checked when one is configured. Both need `SCENARIO` or `--all`.

## `[pbfhogg.library]`

```toml
[pbfhogg.library.harness]
package = "pbfhogg-harness"   # the workspace member
binary = "pbfhogg-harness"    # its bin, when it is not named like the package
features = []                 # cargo features for the build
profile = "release"           # or debug = true

[pbfhogg.library.require]
"cancel.pre-decision.v1" = "a cancel before the publication decision refuses it"
"counters.v1" = "a sealed call's counters reach the FIFO whole"
```

`[pbfhogg.library.harness]` is required inside `[pbfhogg.library]` and has
the shape of `[piners.harness]` (`brokkr man piners-config`).

`[pbfhogg.library.require]` is optional: the capabilities `brokkr library
--all` certifies, each a capability id (see Identifiers; quoted, since it
holds dots) to its meaning, one line of 1 to 200 bytes. A key that is not a
capability id, or a meaning that is not one line, fails the config load,
whatever the command. Absent, `--all` refuses to run; empty, it requires no
capability, and certification rests on every scenario passing. The listing
and `SCENARIO` runs do not apply it. It is read from the current tree also
under `--commit`, so a baseline commit is certified against today's
requirements.

Nothing else is accepted in `[pbfhogg.library]` or its harness table.

`pbfhogg` is not a reserved top-level name: a `[pbfhogg]` table has always
been read as a hostname section, and still is. brokkr reads it as the pbfhogg
project section only when it holds `library`, which no host table could (the
host schema refuses unknown fields), so no config that loaded before loads
differently. The decision is made once, on the composed config. A claimed
`[pbfhogg]` holds nothing but `library`: host settings for a machine named
pbfhogg go in `[host.pbfhogg]`, which is always a host. The section belongs
to `project = "pbfhogg"`; in any other project it is a load error.

`brokkr install` does not install the harness: pbfhogg's `[bin] install`
names the CLI package only (`brokkr man run`).

## Running the harness

The harness is invoked in one of two modes:

- `HARNESS --describe-harness` prints the describe document.
- `HARNESS --test-harness REQUEST --private-dir DIR` runs one scenario (see
  The run protocol).

### Describe mode

brokkr runs it in the project root with `BROKKR_MARKER_FIFO`,
`BROKKR_HARNESS_ARTEFACT_DIR` and the profiling variables (see Profiling a
scenario) removed from its environment, and kills it at 60 seconds. Describe
mode never profiles, whatever the build. It must exit 0, write nothing to stderr, and print exactly one describe
document of at most 1 MiB on stdout. A longer output, any stderr, another
exit, a signal or the deadline fails the command, naming which. In describe
mode the harness writes nothing anywhere; brokkr does not check the file
system for that.

## Documents

Every document either side writes is one JSON object in UTF-8 without a
byte order mark, and nothing follows it but whitespace. Its schema is
closed:

- an unknown key, a duplicate key, a missing required key, a key the
  object's shape forbids, and a value of the wrong type are each refused;
- integers are JSON integers, without fraction or exponent (and never
  `-0`), in the range a field states;
- an object is never accepted in array form.

Each refusal names the path into the document, such as
`describe.scenarios[0].roles[2].owner`. The exceptions are text that is not
JSON at all and a number beyond the range of a double (`1e400`), which the
JSON parser itself refuses before any path exists; those name a line and
column instead.

Every document the harness writes carries `protocol`, an integer. This
brokkr implements 1 and refuses any other value, naming both.

A profiled run's hotpath report is not a protocol document: the hotpath
crate writes it in its own JSON, without `protocol`, and these rules do not
apply to it; Profiling a scenario says what brokkr requires of it.

### Identifiers

Scenario, call, role and assertion ids match `[a-z0-9][a-z0-9_.-]{0,63}`:
each is usable as a FIFO call id (`brokkr man measure`, Marker and counter
rules), a file name and a command-line word without escaping. `setup` is
never a call id: it names the harness's own preparation. Assertion ids
starting `brokkr.` are reserved for results brokkr adds itself.

A capability id is an identifier ending `.v<N>`, N a positive decimal
without a leading zero (`cancel.pre-decision.v1`); a capability whose
meaning changes gets a new id.

One-line text (a description or a label) is 1 to 200 bytes with no control
character.

## The describe document

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
        {"id": "a", "label": "cat input -> out-a", "stream": ["counters.v1"]},
        {"id": "b", "label": "cat input -> out-b"}
      ],
      "assertions": [
        {"id": "a.outcome", "call": "a"},
        {"id": "b.outcome", "call": "b"},
        {"id": "outputs-equal", "call": null, "covers": ["repeat.equal-outputs.v1"]}
      ]
    }
  ]
}
```

- `scenarios`: at least one, ids unique.
- `description`: one line.
- `deadline_s`: 1 to 86400, the wall one process of the scenario gets.
- `hotpath` (optional, default false): the scenario can be profiled (see
  Profiling a scenario). True requires exactly one call.
- `isolation` (optional, default false): the scenario is an isolated call
  (see Isolation scenarios). True requires exactly one call, and that call
  has a `cli` counterpart.
- `calls`: at least one, ids unique within the scenario, none `setup`. Each
  has a one-line `label`, and may have `stream`: a list of capability ids,
  each at most once, that the call's stream verdict stands for. A nonempty
  list makes brokkr add `<call>/brokkr.stream` (see The call stream), which
  covers those capabilities in a certification; absent and `[]` mean the
  same. It may have
  `cli`, its CLI counterpart, which makes brokkr add `<call>/brokkr.cli`
  (see CLI equality).
- `roles`: every path the scenario uses, ids unique within the scenario;
  possibly none.
- `assertions`: the scenario's assertion inventory, ids unique within the
  scenario; possibly none. `call` is required: a call id of this scenario,
  or `null` for an assertion about the scenario as a whole. `covers` is
  optional: the capability ids, each at most once, that the assertion's
  result stands for in a certification (see Certification); absent and `[]`
  mean the same.

### Roles

| `source` | `kind` | `owner` | `variant` | `seeded` |
|---|---|---|---|---|
| `dataset` | `pbf` | forbidden | required | forbidden |
| `generated` | `pbf`, `file`, `directory` | forbidden | forbidden | forbidden |
| `output` | `pbf`, `file`, `directory` | required | forbidden | optional |
| `scratch` | `directory` | required | forbidden | forbidden |

- A `dataset` role is a dataset file of the run in the named variant: read
  only, and shareable between calls. `variant` is a variant name
  (`[A-Za-z0-9_.-]`, 1 to 64 bytes). Dataset roles are PBFs only.
- A `generated` role is written by the harness's setup before any call and
  read by calls; setup owns it.
- An `output` role is written by its owner; a `scratch` role is its owner's
  run scratch. `owner` is a call id of this scenario, exactly one, so no two
  calls write one path.
- A `seeded` output role (`seeded: true`; default false) starts as its
  **seed**, which brokkr writes before the launch, so its call meets an
  existing destination: replacing one, or refusing to. The seed of role R
  is `brokkr-library-seed-v1 R` and a newline: a regular file holding
  those bytes for a `file` or `pbf` role, a directory holding only the
  regular file `seed` with those bytes for a `directory` role. It is no PBF;
  the role id in it makes one member's seed restored in another's place
  visible. A seed is reserved content: no call publishes a destination
  byte-identical to its role's seed (producer obligation), so a destination
  that holds it holds no published generation: it was never replaced, or a
  rollback put the seed back.
- `kind: pbf` declares a PBF file; one an output-expecting counterpart's
  output placeholder names must also be in `pbfhogg digest`'s domain
  (sorted, see CLI equality), or its comparison fails. A refusal
  counterpart digests nothing.

## Running a scenario

`brokkr library SCENARIO` holds the brokkr lock from before the run
directory exists until it is removed, and makes one harness launch, then
the launches of its calls' CLI counterparts (see CLI equality), strictly one
at a time: each is released before anything it could have changed is
judged, and before the next is admitted.

**The run directory.** brokkr allocates `<scratch>/library/<scenario>/run-N/`
(N one past the highest present) under the host's scratch directory and
lays it out before the launch:

- `request.json`, the request;
- `cwd/`, empty: the harness's working directory;
- `artefacts/`, empty: `BROKKR_HARNESS_ARTEFACT_DIR`;
- `roles/`: each `output`, `scratch` and `generated` role is bound to
  `roles/<role id>`. Output roles of every kind and generated file and PBF
  roles start absent, a seeded output role as its seed; scratch roles and
  generated directory roles start as empty directories.

Dataset roles bind the resolved dataset file. Every bound path is absolute
and canonical, the run directory's included.

**The request.**

```json
{"protocol": 1, "scenario": "repeat-cat", "iteration": 0,
 "bindings": {"input": "/abs/data/denmark.osm.pbf", "out-a": "/abs/.../roles/out-a"}}
```

`bindings` names every role of the scenario and nothing else. `iteration`
counts the processes of one run from 0: a verification makes one, a
measurement N. The
private directory is never in the request: its name is made at the launch,
after the request is written.

**The reservation.** The launch is reserved like every pbfhogg launch
(`brokkr man pbfhogg`, Launch reservations and the private directory): it
reads the bound dataset files, writes the bound role paths, and holds the
whole run directory as write coverage, so the lease the harness inherits
protects the request, `cwd/` and the artefacts too, for as long as any
process holds it. Its private directory goes in the run directory. The
launch is released (its lease free) before anything in the directory is
judged. Every later launch of the run covers the whole run directory the
same way.

## The run protocol

`HARNESS --test-harness REQUEST --private-dir DIR` runs in `cwd/` with
`BROKKR_HARNESS_ARTEFACT_DIR` (the `artefacts/` directory) and
`BROKKR_MARKER_FIFO` set. The FIFO is always attached: a call's wall
readings come from `MarkerFifo::now_us()`, a clock only an opened FIFO has.
`DIR` is the launch's private directory, for the library's staging and
scratch. The profiling variables are set only on a profiled launch and
removed from every other (see Profiling a scenario).

- **Exit.** 0: it ran and every result it reported passed. 1: it ran and
  some result it reported is `fail` or `unevaluated`. 2: it could not run
  the request (one it cannot honour, a schedule that would break role
  ownership); it may write `artefacts/refusal.json`,
  `{"protocol": 1, "reason": ONE_LINE}` (1 to 1000 bytes). Any other exit,
  a signal, or the scenario's `deadline_s` passing fails the run; at the
  deadline brokkr kills the harness's process group.
- **Output.** stdout and stderr stay empty on every exit; any byte fails
  the run, and an output still open after the harness exited cannot be
  shown empty and fails too. Diagnostics go through `host::Diagnostics`
  into the documents: a call's into its call document, setup's and the
  harness's own into `scenario.json`.
- **Producer obligations**, which brokkr does not check: the harness reads
  only the request, the bound input roles and the private directory, and
  writes only its artefact directory, the roles its calls or setup own, the
  private directory and, in a profiled run, its hotpath report; setup is deterministic and runs before any call;
  the call schedule honours ownership; a call changes its output roles only
  through its publications, and publications whose members share a role
  run one after the other in ordinal order, rollback included (disjoint
  ones are unconstrained); no call publishes its role's seed; it sets its own soft `RLIMIT_NOFILE`
  where a scenario's plan says so, before any library work, and reports it
  in the call context; it never resets `SIGPIPE` to its default; and no
  process it started reads or writes the run directory after it exits.
  What brokkr checks of the file system is detection under that last
  obligation: it sees what survived, never a read, nor a write undone
  before exit.

## The artefacts

Read on exit 0 and 1 only. `artefacts/` holds exactly `scenario.json` and
`calls/`, and `calls/` exactly one `<call id>.json` per declared call; any
other entry, a symlink, a special file, or a document over 16 MiB fails the
run. No file is followed through a symlink. Every document follows
Documents above.

**`scenario.json`**: `protocol`, `scenario` (the scenario's id), `plan`,
`assertions` (the scenario-level results) and `diagnostics`, setup's and the
harness's own events (never a call's), in the call documents' shape. Its
collection closes after setup and every call document; its `at_us` readings
are held to their range only, since no call reading anchors them.

**`calls/<id>.json`**:

- `protocol`; `call`, the file's call id.
- `entry_us`, `return_us`, `seal_us`: `MarkerFifo::now_us()` at command
  entry, at return, and when sealing ended; each 0 to 2^63-1, with
  `entry_us <= return_us <= seal_us`. `return_us - entry_us` and
  `seal_us - entry_us` must each be at most the process wall brokkr observed
  (spawn to the ending it saw) plus one second. The bound is relative on
  purpose: readings of one opened FIFO share its clock, whose origin is not
  reliable. That the readings sit at those boundaries is the harness's
  obligation.
- `outcome`: `{"status": "ok"}`, `{"status": "cancelled"}`, or
  `{"status": "error", "kind": ID, "message": ONE_LINE}` (an identifier,
  and 0 to 1000 bytes). Reported; the harness's assertions judge it.
- `publications`: every publication the call decided, in order, each
  `{"ordinal", "decision", "members"}`: ordinals 1, 2, ... without gaps;
  `decision` `authorized`, `refused_cancelled` or `refused_error`;
  `members` the destinations it covers, at least one, each
  `{"role", "effect"}`: `role` an output role this call owns, at most once
  per publication (an output set is one publication of several members, an
  output directory one member of a `directory` role); `effect` what that
  attempt left at the destination when it finished, relative to the
  destination just before it:
  - `none`: as it was, a rollback to absence included;
  - `installed`: the new generation is there, an incomplete rollback
    included;
  - `restored`: the previous generation was moved aside and put back;
  - `displaced`: the previous generation was moved aside and not put back,
    so the destination is absent. The moved-aside generation lives in the
    private directory and goes with the release; brokkr never sees it.

  Every member of a refused publication is `none`. See Publications on
  disk for what brokkr holds them against.
- `cancel`: `null`, or what `Invocation::cancel()` returned:
  `{"result": "before_decision", "ordinal": K}` (K a `refused_cancelled`
  publication), `{"result": "after_decision", "ordinal": K}` (K an
  `authorized` one), or `{"result": "no_publication_pending"}`. After a
  cancel's K, every publication is refused. brokkr checks this consistency
  and nothing else of order; order judgments are the harness's assertions.
- `seal`: `{"state": "sealed"}`, `{"state": "outstanding", "held":
  {KIND: N, ...}}`, or `{"state": "refused", "reason": ONE_LINE}` (1 to 1000
  bytes). `held` is the census at the seal attempt: every value the caller
  still held (a session, a buffer, a bound output, an escaped writer, ...),
  each counted once under one producer-defined kind, an identifier, N 1 to
  2^64-1; at least one kind. The harness keeps the census it took then,
  although it drops those values before writing the document.
- `snapshot`: when sealed, the call's final counters, name to an integer
  0 to 2^64-1, each name 1 to 400 bytes without `=` or a newline (so its
  sealing record fits a FIFO line); otherwise `null`.
- `stream`: `{"state": "sealed"|"failed"|"open", "drops": N}` (N 0 to
  2^64-1), the call sink's own account: how it ended, and every line it
  dropped, its terminal record included. It is `open` exactly when `seal` is
  not `sealed` (a sink is ended only by a successful seal of its call's
  invocation); anything else fails the run. The call stream says how brokkr
  holds it against the FIFO.
- `context`: a nonempty object, every effective per-call setting.
- `diagnostics`: the call's captured `host::Diagnostics` events, in capture
  order, each `{"at_us", "kind", "key", "message", "truncated"}`. `at_us`
  is `MarkerFifo::now_us()` when it was captured, 0 to 2^63-1, never before
  `entry_us` (an earlier event is setup's, which goes in `scenario.json`),
  and `at_us - entry_us` within the process wall brokkr observed plus one
  second, as for the other readings. `kind` is an identifier, the event
  kind's name in lower case (any kind, so a new one needs no brokkr
  change); `key` is 1 to 200 bytes; `message` is at most 4000 bytes, and a
  longer one is cut by the harness at a character boundary with `truncated`
  true. brokkr prints them, marking one captured strictly after `seal_us` as
  after the seal (equal readings establish no order), and never judges
  them. **The cut-off is the document**: a call's values can still report
  after its seal (a writer dropping, an unused output cleaning up), so the
  harness drops every value of the call, waits for every callback
  attributable to it to return, then closes and snapshots the call's
  collection, and only then writes the call document. An attributable event
  that misses that is a producer defect; it never moves to `scenario.json`.
- `assertions`: this call's results.

**Results**: `{"id", "status", "detail"}`, `status` `pass`, `fail` or
`unevaluated`, `detail` any text of at most 4000 bytes. Across the
documents the results are exactly the inventory: one per declared
assertion, in `scenario.json` for `call: null`, else in its call's
document. A missing, duplicate, undeclared or misplaced result fails the
run, naming it.

**Publications on disk.** Once the run directory passed its layout check,
brokkr replays each output role of each call from its initial state
(absent, or its seed) through the members naming it, in ordinal order:
`installed` leaves a published generation, `restored` and `displaced`
need a destination to move aside (one reported on an absent destination
fails), `displaced` leaves it absent, `none` leaves it alone. The state that
leaves must be what `roles/<role>` holds: absent; byte-identical to its
seed; or a published generation, present, of the role's kind and not its
seed. Any other fails the run, naming the call and the role. This proves
the destination is in the state the effects describe, never which
generation a published one holds: that a restore after an earlier
installation put back the right generation, and that a displaced
generation's backup survived, are the harness's assertions to make.

**Plan and context** are the harness's to define; brokkr interprets
neither. Each stays in a plain value domain: objects with unique keys,
arrays, strings without NUL, booleans, `null` and integers in -2^63 to
2^64-1 (no other number, and not `-0`). `plan` holds `calls`, an object
whose keys are exactly the declared call ids, each a nonempty object. No
string in either, object keys included, may contain the run directory's
absolute spelling or a bound dataset file's (as resolved, or canonical).
That is a narrow check on the value domain, not proof that the plan is free
of run-specific paths: relative spellings and split strings pass it.

Producer obligations, which brokkr does not check: the plan carries every
choice that affects the workload (each call's command and options, its
inputs by role, the Host each call gets, the order, grouping and
concurrency of the calls, setup and every generated input's recipe), and
each call's context every effective per-call setting (Resources figures,
executor, page-cache policy, process actions, the soft `RLIMIT_NOFILE` at
its start). Neither carries anything run-specific: no path, timestamp,
iteration or pid. A measurement's identity is built from them (see
Measuring a scenario), so a choice the plan leaves out lets two different
workloads pair. Calls the plan runs concurrently come with a harness
assertion that their lifetimes intersected; nothing claims they ran in
parallel.

## The call stream

Each call writes its markers and counters to the FIFO through a sink of its
own, tagged with its call id (`<ts> %<call> <name>`, `<ts> %<call>
@<name>=<value>`; `brokkr man measure`, Marker and counter rules). When the
call's invocation seals, its sink ends the stream with two more record
kinds, which only a tagged record can carry:

- a **sealing emission** `<ts> %<call> @@<name>=<value>` for every counter
  of the call's snapshot, zeros included, its value the snapshot's
  saturated to 2^63-1 as the wire carries it;
- then one **terminal record**, `<ts> %<call> !sealed drops=N` or
  `<ts> %<call> !failed drops=N`, N the lines the sink dropped before it.

Producer obligations: a sink is ended only by a successful seal of its
invocation, and an invocation that does not seal leaves its sink `open`
and writes neither kind; sealing begins after the sink stopped admitting
ordinary records and every callback into it returned; one thread stamps and
writes the sealing emissions and then the terminal record, in that order;
nothing of the call follows the terminal record.

A tagged record naming a call id the describe document does not declare
fails the run, whatever its exit: lines arrive whole or not at all, so no
loss can make one.

brokkr judges each call's records, in the order they arrived, against its
call document:

- **Proven defects fail**, whatever was lost: a record stamped outside
  `entry_us..seal_us`; a sealing or terminal record stamped before
  `return_us`, or before the sealing record ahead of it; an ordinary record
  stamped later than any sealing or terminal record, or arriving after
  sealing began; a name emitted twice; a second terminal record, or any
  record after it; a terminal record whose state or drops differ from the
  artefact's `stream`; no terminal record from a sink the artefact reports
  `sealed` or `failed`, although the reader lost nothing and the artefact
  counts no drop; a sealing emission naming a counter the
  snapshot lacks, or carrying another value; an ordinary counter of a
  snapshot name above its final value; any sealing or terminal record of a
  call that did not seal.
- **Otherwise incomplete is unevaluated**: a call that did not seal, a
  capture that may have lost a FIFO record (a rejected line, a read error,
  records left unread, the marker or counter limit), no terminal record, or
  one that reads `failed` or counts drops.
- **Otherwise complete**: it passes when every snapshot counter has its
  sealing emission, and fails naming the ones that have none.

For a call whose `stream` lists capabilities, brokkr adds the verdict as
`<call>/brokkr.stream`, with the reasons as its detail. For every call it
prints one line, pass or fail: the verdict (`(not judged)` when nothing
claims it), the call's phases and its tail. The phases are its markers
stamped from `entry_us` to `return_us`, in time order, each running to the
next and the last to `return_us`; the tail is its markers stamped after
`return_us`.

## CLI equality

A call may declare the pbfhogg CLI command whose outputs must equal its
own, or which must refuse the same input as it does:

```
{"id": "a", "label": "cat input -> out-a",
 "cli": {"argv": ["cat", {"input": "input"}, "-o", {"output": "out-a"}],
         "scratch": false,
         "policy_differences": ["the CLI raises its soft RLIMIT_NOFILE"]}}
```

- `argv`: 2 to 1000 tokens after the executable, the first a literal
  identifier, the subcommand. A token is a literal string or a placeholder
  naming one role of the scenario: `{"input": ROLE}`, the bound path of a
  dataset role or a generated `file` or `pbf` role; `{"files_in": ROLE}`, every
  entry of a generated `directory` role, one argument each, sorted by the
  bytes of its name; `{"output": ROLE}`, an `output` role of kind `pbf` this
  call owns, for which brokkr passes a path of its own, absent or holding
  the role's seed as the call's destination did, and compares the two
  files. A placeholder is the path alone: the argv spells any flag
  before it (`"-o"`). At least one output placeholder, none twice.
- A literal is 1 to 1000 bytes without a control character, and is refused
  when it holds `/`, is `.`, `..` or `--`, is `@brokkr-private-dir@`, or is
  one of the flags brokkr negotiates with the CLI (`--overwrite`, `-O`,
  `--staging-dir`, `--tmp-dir`, alone or with `=`). That is a syntactic
  narrowing, not proof: the producer's obligation is that no literal names
  a path or a negotiated flag in any spelling (short-option clusters,
  aliases), and that nothing the counterpart reads names another path, so
  it reads and writes exactly its placeholders and its private directory.
- `scratch` (default false): the command writes run scratch, so `--tmp-dir`
  is negotiated for it.
- `expect` (default `output`): `output` needs the replay to exit 0 with
  every output equal to the library's (below); `refusal` needs the call and
  the CLI both to refuse and publish nothing (see Refusal counterparts), and
  requires `exit`.
- `exit`: with `expect: "refusal"` only, the exit status the CLI refuses
  with, 2 to 127 but not 64. Never 0 or 1, which pbfhogg reserves for a
  completed command's answer; never 64, pbfhogg's usage error (`EX_USAGE`:
  the command line was refused before the command ran, which never refuses
  the input); and never 128 and up, the band a signal's status takes
  (pbfhogg exits 130 or 143 after a SIGINT or SIGTERM it caught).
- `overwrite` (default: negotiated): `true` passes `--overwrite` and needs
  the CLI to accept it (if not, the replay fails without running); `false`
  never passes it; absent, brokkr negotiates it as for every pbfhogg launch
  (`brokkr man pbfhogg`, Output overwrite), so a counterpart meant to refuse
  an existing seeded destination says `false`. With `false`, that no argv
  option or combination grants replacement either is the producer's
  obligation.
- `policy_differences` (default none): one-line texts, each how the CLI's
  process policy differs from the call's. brokkr prints them with the
  listing and the result and never judges them.

**The replay.** After the harness's documents are read, whatever its other
results, and only when the run directory passed its layout check, brokkr
replays each call's counterpart, in describe order, with the CLI of the
same tree; these are the steps for `expect: "output"` (Refusal
counterparts below has a refusal's):

1. Each library output an output placeholder names must be a regular file
   at `roles/<role>`; one that is absent or of another type fails the result
   without a replay.
2. A generated input must be a regular file, and a `files_in` directory a
   directory of regular files only, at least one, every name UTF-8; else the
   result is `unevaluated`, nothing replayed.
3. brokkr makes `replay-<call>/` in the run directory holding `cwd/` and
   `outputs/`, both empty but for the seed of each seeded output role at
   `outputs/<role>`, and runs the argv in `cwd/` with each output
   placeholder bound to `outputs/<role>`, through the same flag negotiation
   and reservation as every pbfhogg launch (`brokkr man pbfhogg`, Output
   overwrite, and Launch reservations and the private directory): the
   private directory goes in `outputs/`, and the lease covers the whole run
   directory. `BROKKR_MARKER_FIFO`, `BROKKR_HARNESS_ARTEFACT_DIR` and the
   profiling variables are removed from its environment, and it is killed at the scenario's
   `deadline_s`. It must exit 0.
4. Then each output is digested, library first: `pbfhogg digest FILE`, the
   same CLI, in the same `cwd/`, killed at `deadline_s`. Each must exit 0
   with its output closed and print exactly four lines, `xxh128 v<N>
   <hash>`, then `node`, `way` and `relation`, each `<kind> <count> <hash>`,
   every hash 32 lowercase hex digits, every count decimal without a leading
   zero, every line ending in a newline. brokkr implements `xxh128 v1` only:
   another algorithm or version fails naming it. A refusal (a file outside
   digest's domain: unsorted, mixed-kind blocks, inconsistent indexdata) is
   an exit other than 0 and fails; there is no fallback. The two outputs are
   equal when their reports are; a difference fails, naming the content
   hash and every kind line that differs.

After every one of these launches, released, whatever it came to, brokkr
checks without following a link what it could have changed before reading
anything below it again: the run directory is still a directory (if not,
the run stops there and nothing in it is read or written, evidence
included); `replay-<call>/` holds exactly `cwd/` (the same directory, still
empty) and `outputs/` (only the declared output names, each a regular
file); each library output is still a regular file (for a refusal, the
library side as step 1 of Refusal counterparts found it). A breach fails that
result, and every later counterpart is `unevaluated`, nothing more
launched. (The release itself, which runs only once no process holds the
lease, removes the launch's private directory by the identity the lease
recorded, even through a link a child put in an ancestor's place.) A
launch whose admission or release cannot be shown safe (a lease
still held after the child exited, a private directory that could not be
made, an overlapping reservation) stops the run as a held harness lease
does: the failure is reported and nothing in the run directory is read or
written again. The results read before it (the harness's, `brokkr.seal`
and `brokkr.stream`, judged from what brokkr already holds) are still
reported; no counterpart's is. A stop request interrupts the run, its directory kept, also
when it arrived while a release waited. The
capability probes the negotiation makes are not run launches: they never
touch the run directory, and are bounded as every probe is.

Producer obligation, which brokkr does not check: setup leaves every
generated role as the counterparts read it, and no process the
counterpart started touches the run directory after it exits (pbfhogg's
CLI starts no helper process).

`<call>/brokkr.cli` is `pass` when every output is equal (its detail lists
each with its counts) or the refusal held, `fail` on the first defect above
(every defect found stays in its detail), and `unevaluated` when nothing
could be replayed: the layout check failed, an input was missing, or an
earlier counterpart left a breach. Declared policy differences close its
detail. The run fails on anything but `pass`.

**Refusal counterparts.** A counterpart with `expect: "refusal"` is judged
whole-call, library side first, nothing digested:

1. The call returned an error (`ok` fails; so does `cancelled`, since the
   CLI has no cancel), no member of any of its publications was left
   `installed`, and every output role the call owns, not only those its
   placeholders name, is in its initial state on disk: absent, or
   byte-identical to its seed. Any defect fails the result without a
   replay.
2. The replay runs as in step 3 above and must end `Exited` with the
   declared `exit`; another status fails, and so does a signal or the
   deadline, which are never a refusal.
3. After its release, the checks after every launch run (above), and the
   library side must still be as step 1 found it, `roles/` still the
   directory it was: either change is a breach. Then every
   `outputs/<role>` must still be in its initial state, absent or its
   seed, else the result fails (the CLI changed a destination: a failed
   refusal, not a breach).

The detail on a pass says so with the exit and the first nonblank line of
the CLI's stderr. The status proves that the CLI refused, never why: exit 2
is any failure or refusal at run time (a command line refused before the
command ran exits 64, which no refusal declares), and a typo that still
parses, such as a wrong input path, can exit 2 as well. That the CLI
refused the same input for the reason the call did is the producer's
obligation. A pbfhogg build from before it split usage errors out (a
`--commit` baseline) exits 2 for a usage error too. Like every check
here it sees final states only: a write undone before exit (publish, then
put the old bytes back) passes.

## The verdict

After the launch is released, brokkr checks the run directory against its
layout: exactly `request.json` (bytes unchanged), `cwd/` (the same
directory, still empty), `artefacts/` and `roles/`, and in a profiled run
`hotpath.json` (absent or a regular file), nothing else (the private
directory went with the lease); in `roles/` only bound role names,
each absent or of its kind (a regular file for `file` and `pbf`, a
directory for `directory`), whatever its source, since a call that is
cancelled or fails before publishing leaves its output in its initial
state, absent or its seed. An entry
that is not the directory it should be (a symlink in place of the run
directory itself, `cwd/`, `roles/` or `artefacts/`) is a failure and is
never traversed (in place of the run directory, nothing below it is read
or written), and a directory that cannot be listed fails too. When any
counterpart was replayed, the same check runs again after the last of its
launches, admitting also the `replay-<call>/` directories made, so a CLI
that changed the request, left something in `cwd/` or beside the roles is
caught too. A stop seen
up to a passing run's removal interrupts it, its directory kept; the
removal commits the pass.

The run fails on a release that finds the lease still held (then nothing
in the run directory is read or written: some process may still change
it), a layout breach, any output, a bad exit or the deadline, a FIFO record
of an undeclared call, a profiled process's missing or invalid report, an
invalid or
incomplete artefact tree, publications that disagree with the output roles
on disk, an exit inconsistent with the harness's own
results (0 with a result that is not `pass`, 1 with none), or any result
that is not `pass`. `unevaluated` fails exactly as `fail` does; the report
keeps them apart.

brokkr adds one result per call, `<call>/brokkr.seal`: `pass` only for a
sealed call, since the snapshot exists only then; `<call>/brokkr.stream`
for a call that claims its stream (see The call stream); and
`<call>/brokkr.cli` for a call with a counterpart (see CLI equality). They
are reported once the call documents were read, except that no
`brokkr.cli` is reported after a counterpart launch that was not safe, and
a result brokkr adds never makes the harness's exit inconsistent.

On pass, brokkr prints each call's stream line, each diagnostic, each
counterpart's detail (its counts and declared policy differences) and one
result line, and removes the run directory through the reservation gate (a
refused removal is a warning,
and the directory stays). On fail, it prints every failure, results as
`<call>/<id>` or, for a scenario-level one, `<id>`, with any detail
indented beneath; harness-supplied text is printed with control characters
escaped. Unless the run directory was left untouched (a held lease, an
unsafe launch, or a run directory that is no longer one), it writes
`evidence/` into the run directory (`stdout`, `stderr`,
`stream.txt` with every FIFO record kept, in arrival order and in its wire
spelling, after a comment line per cause of capture loss, `failures.txt`),
and keeps the directory, naming it. Routine `brokkr clean` removes kept run
directories through the gate (`brokkr man clean`); the cleanup after an
interrupted command never does. An interrupted run, including one stopped
after the harness exited, keeps its directory and exits as interrupted.

## Measuring a scenario

`brokkr library SCENARIO --bench N` measures the scenario as N processes,
one after the other, under the brokkr lock, as every measured command is
(`brokkr man measure`): a dirty tree is refused unless `--force`, and then
the measurement runs and prints but stores no row; a clean tree, a
`--commit` worktree included, stores. `--all` never measures.

**Each process is a verification.** Process i gets its own run directory
and a request with `iteration` i, and is judged exactly as `SCENARIO`
judges a run, its directory removed on a pass and kept on a failure. The
first failure ends the measurement: no row is stored. Each process's
capture has the budgets a verification has; the FIFO, the call census and
the trajectory limits are per process, not shared across the N.

**Identity.** Once a process's documents are read and its run directory
passed its layout check, and before any counterpart is replayed, brokkr
identifies what it ran:

- its `plan` and each call's `context` in **brokkr-json-v1**, a canonical
  form of its own (not RFC 8785, whose numbers are doubles): no whitespace,
  object keys sorted by their UTF-8 bytes, arrays in order, integers in
  shortest decimal (never `-0`, no `+`, no leading zero), strings escaping
  `"`, `\` and U+0000 to U+001F (`\b \f \n \r \t` where one exists, else
  `\u00xx` in lowercase) and nothing else; digested as `plan-v1:` or
  `context-v1:` and the XXH3-128 (seed 0, 32 lowercase hex digits) of the
  canonical bytes;
- each dataset role's file, `xxh128:` and its XXH128, read once before the
  first process;
- each generated role as the harness left it: `absent`; for a `file` or
  `pbf` role `xxh128:` and its XXH128; for a `directory` role `tree-v1:`
  and the XXH3-128 over its entries sorted by the raw bytes of their path
  relative to it, each `<32 hex XXH3-128 of the path bytes><kind><value>`
  and a newline, kind `d` (no value), `f` (the file's XXH128) or `l` (the
  XXH3-128 of the link's raw target bytes). Nothing is followed through a
  link; any other entry, or an entry not of the role's kind, cannot be
  identified and fails the process;
- each seeded output role's seed recipe, `seed-v1:<kind>`;
- the features the harness was built with.

Every process must agree with the first on the plan, each call's context,
the generated and dataset identities, the seeds and the features; one that does not
fails, naming the first difference (a field of the plan or a context, or a
role), and its kept directory's `evidence/baseline.txt` holds the first
process's identity and documents beside its own, since the first
process's directory is gone. Producer obligation, which brokkr does not
check: setup leaves every generated role as the calls and counterparts
read it.

**CLI digests are reused.** A counterpart whose reuse key equals one an
earlier process of the same measurement passed under is not replayed: the
CLI executable's XXH128, the argv as declared (placeholders by role, never
resolved paths), the identity of every role an `input` or `files_in`
placeholder names, the scratch flag, the expectation with a refusal's exit,
the overwrite stance, and the seed recipe of every seeded output
placeholder (an output's state is otherwise absent at a fresh path, so a
constant of the key). Its
`replay-<call>/` is still made, `cwd/` and an empty `outputs/`, every
library output is still digested there and compared with the reused report,
and every check after a launch runs as for a replay (`outputs/` must stay
empty). A reused refusal launches nothing and makes no `replay-<call>/`;
its library side is judged as always. The result's detail says which
process's replay it reused.

**The rows.** When every process passed, brokkr stores one group of rows
under one uuid, in one transaction:

- the scenario row, `library:<scenario>`: its wall the chosen process's
  wall (spawn to the ending brokkr observed), its iterations every
  process's wall in order, its peak RSS the chosen process's;
- one row per call, `library:<call's scenario>/<call>`: its wall the chosen
  process's `return_us - entry_us`, its iterations that call's wall in every
  process in order, and the call's snapshot as counters, each
  `snapshot.<name>`, exact (a value past 2^63-1 as decimal text).

The walls of calls that ran concurrently measure contention as much as
the calls: for a concurrent group the scenario wall is the headline. The
chosen process is the fastest, ties to the lowest index. Every row
carries `meta.library.plan` (the plan digest), `meta.library.iteration`
(the chosen index) and `meta.library.identity`, the identity above as one
canonical JSON text (the plan digest, each call's context digest, the
dataset and generated identities, the features, and the seeds when a role
is seeded); a call row adds
`meta.library.context`, its context digest. The canonical plan and context
documents are stored once each, by digest, beside the rows. The scenario
row is written last, so it is the group's newest: what `brokkr results`
shows first, what the next row's `prev.*` names and what names the sidecar
session. `brokkr results <uuid>` shows every row of the group in full.

**Pairing.** The identity is also the rows' arm qualifier (`brokkr man
results`, `--compare`), so a library row pairs with another exactly when
everything a row of its command pairs on agrees and so does the identity:
same plan, same contexts, same dataset and generated identities, same
seeds, same features. The commit is never part of it. A row that does not pair is
followed by a line naming the other side's latest row of the same command,
mode, input, captured environment and build, and where its identity first
differs from it (a plan field, a call's context field, a role or the
features), or that the identities are equal and the invocations differ. That
line explains; it never says the named difference is the only one. A
library call is never compared with its CLI counterpart's timing here, and
`--compare` will not do it (see Isolation scenarios).

**Trajectories.** Each process's capture is stored in sidecar.db, under the
group's uuid with its iteration as the run, as soon as it is judged, pass or
fail, with the session's metadata (the default run, the run count, the
harness's XXH128, its ending) rewritten in the same transaction; brokkr
holds at most one process's trajectory in memory, and every process's is
stored. It holds the sealing and terminal records and each call's
boundaries and sink account from its document, so a call's last phase ends
at its return (`brokkr man measure`, Marker and counter rules). The rows
come after the last trajectory and the final metadata, and they publish the
measurement. When it fails, or the tree is dirty, the session is filed under
the `dirty` alias and its id printed.

## Profiling a scenario

`brokkr library SCENARIO --hotpath N` profiles function-level timing, and
`--alloc N` per-function allocation, of a scenario the describe document
declares `hotpath`; any other is refused once the describe document is
read, before anything runs, naming the scenarios that are. The hotpath
crate's state is process-wide, so a profile is the whole process's; only a
scenario of one call, with no other library work in its process, has a
profile that is its call's.

**The build.** The harness is built with the configured features and the
instrumentation feature added first: `hotpath` for `--hotpath`,
`hotpath-alloc` for `--alloc`. The harness member defines both, forwarding
them to the library's features of the same names. The CLI the counterparts
replay against stays release with default features, uninstrumented.

**The launch.** A profiled launch carries `HOTPATH_OUTPUT_PATH` (the run
directory's `hotpath.json`), `HOTPATH_OUTPUT_FORMAT=json` and
`HOTPATH_METRICS_SERVER_OFF=true`; every other harness launch, describe
mode, the counterparts and the digests have all three removed. The report
lies in the run directory, so the launch's coverage reserves it and the
release keeps it.

**Producer obligations**, which brokkr does not check: the harness profiles
only in `--test-harness` mode and only when `HOTPATH_OUTPUT_PATH` is set,
whatever it was built with; on a run that passes, it creates the hotpath
guard before any library work and drops it, writing the report, after the
call returned and its invocation sealed, and before it exits 0, with
nothing instrumented after it; built with `hotpath-alloc` it installs the
hotpath crate's counting allocator as its global allocator; it instruments
nothing of its own, and setup does no library work, so every instrumented
function in the report ran under the declared call. A failing run need not
write a report. The hotpath crate's error paths print to stderr, which fails
the run as any output does.

**The report**, read after the release and the layout check, before any
counterpart, without following a link and at most 16 MiB, and judged only
on exit 0 (any other exit fails the run already; a report it left is kept
with the evidence). It is JSON whose section for the mode,
`functions_timing` for `--hotpath` or `functions_alloc` for `--alloc`, holds
a `data` array in which every entry is an object with a nonempty `name` and
a `calls` count from 0 to 2^63-1, and at least one entry was called and has
a nonnegative `total` of the mode's dimension (a duration, or a byte size).
Anything else fails the process, naming what is wrong. The per-function
figures are the call's; the thread and process summaries are the harness
process's, its own work (the request, assertions, serialization) included.

**The measurement** is `--bench`'s (see Measuring a scenario): N processes,
each a full verification that agrees with the first, the report part of
each verdict, trajectories stored as each is judged, the same identity
(whose features name the instrumentation, so a profiled row never pairs
with a `--bench` one; the mode is in the pairing key besides). The rows are
the scenario row alone, mode `hotpath` or `alloc`, carrying the chosen
process's report: no call rows, since the profile is the process's.
`brokkr results <uuid>` shows the profile; a dirty tree's unstored row
prints it after its result line. `--alloc` prints that its walls are not
meaningful, and its chosen process is still the fastest.

## Isolation scenarios

A scenario declaring `isolation` is one call, the first library work in a
fresh process, with a CLI counterpart: the shape a library call against its
CLI counterpart's timing needs, since only then do the two start alike.
That the call is the process's first library work is the producer's
obligation; brokkr checks only the one call and the counterpart, and lists
the flag. brokkr shows no library against CLI timing yet: it needs the
pbfhogg CLI to mark its command's entry and return on the FIFO, which it
does not, and until it does brokkr compares no timing of the two.

## Certification

`brokkr library --all` certifies the harness against
`[pbfhogg.library.require]`, its applied requirement list.

**Coverage** is declared in the describe document, and nowhere else: an
assertion's `covers` lists the capabilities its result stands for, and a
call's `stream` lists those its `<call>/brokkr.stream` result stands for.
brokkr's other results (`brokkr.seal`, `brokkr.cli`) cover nothing. A
covering result is the one reported under the declared call (or at scenario
level for `call: null`) with the declared id; one reported elsewhere does
not count. A run reports results only once its artefact tree was read and
valid: a run that did not run, failed before its artefacts, or left any
invalid document reports none.

**The sweep.** brokkr prints the requirement list (each id with its meaning)
and its digest, resolves every scenario's dataset roles before the first run
(a variant a dataset lacks refuses the whole sweep, as for `SCENARIO`; this
is resolution up front, not a snapshot of the files), then runs each
scenario once, in describe order, as `SCENARIO` would, printing what
`SCENARIO` prints. It always goes on to the next: a scenario that failed, or
whose run did not come to a verdict (its error is that scenario's failure),
does not stop the sweep, since each run has its own directory and every
launch's reservation is checked against every other's.

**Classification**, for each required capability, in this order:

- **failed** when any covering result is `fail` or `unevaluated`, or a
  declared covering result, of an assertion or a stream, has no result (its
  scenario failed before reporting it);
- else **passed** when at least one covering result passed;
- else **unsupported**: nothing the harness declares covers it, as on a
  `--commit` baseline that predates the capability. It is shown apart and
  never called a regression, and it fails certification.

A covering result that passed counts as passed even when its scenario
failed for another reason; certification still fails on that scenario.
Capabilities the harness covers that the list does not name are shown on
one line, never judged.

**Certified** exactly when every scenario passed and every required
capability passed.

**The digest** of the requirement list is `require-v1:` and the XXH3-128
(seed 0, 32 lowercase hex digits) of the required ids in byte order, each
followed by a newline. It covers the ids only: a capability whose meaning
changes gets a new id, so two certifications with equal digests required
the same capabilities, though the wording of a meaning may differ; the
record keeps the wording.

**The record.** Every sweep that finishes writes one JSON document,
`<uuid>.json` under the `library-certifications/` directory of the current
tree's `.brokkr/`, certified or not. `brokkr clean` never removes it
(`brokkr man clean`). It is written atomically, never as a partial file; a
failure to write it fails the command, and the record may then exist or
not. It holds:

- `record` (`brokkr-library-certification`) and `version` (1), `uuid`,
  `unix_time_s`, `hostname`, `brokkr_version`;
- `commit`: `hash` (the full commit of the tree built, the `--commit`
  worktree's when given), `clean` (the tree was clean under the measurement
  policy) and `ref` (the `--commit` argument or `null`);
- `harness`: `package`, `binary` (the package's name when unset) and
  `features` as configured, the `profile`
  it was built under and its executable's `xxh128`; `cli`: the CLI
  executable's `xxh128`; `dataset` and `snapshot`;
- `require`: its `digest` and `capabilities`, id to meaning;
- `certified`;
- `scenarios`, in run order: `id`, `passed`, `failures` (every failure line
  as printed), `kept` (the directory a run that came to a verdict kept, or
  `null`; a run that did not come to a verdict has only its error, which
  names a directory it kept when it knows one), `bindings` (each
  dataset role's `role`, `variant`, `path` and the configured `xxhash` of
  its entry, `null` when none is configured, an unknown identity that never
  matches another), `coverage` (the assertions with their `call`, `id` and
  `covers`, and the calls with their stream `capabilities`, as declared) and
  `results` (each reported result's `call`, `id` and `status`; after an
  unsafe launch, the results reported, which lack the counterparts');
- `capabilities`, in id order: `id`, `meaning`, `class` (`passed`,
  `failed` or `unsupported`) and `covering`, each declared covering result's
  `scenario`, `call` (or `null`), `result` and `status` (`pass`, `fail`,
  `unevaluated`, or `missing`);
- `not_required`: the capabilities covered but not required.

**Output and exit.** After the sweep, one line per required capability: its
class, its meaning and, when it failed, each covering result that failed,
was unevaluated or is missing; then the certification line with the tallies,
the digest, whether the tree was dirty, and the record. Exit 0 when
certified, 1 otherwise, unsupported included. A stop seen before the
record is written interrupts the sweep, and no record is written; once that
last check passed, the record is written and reported whatever arrives.
