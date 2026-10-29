# `brokkr library` and the library harness protocol

pbfhogg used as a library, not as its CLI. The harness is a bin of a pbfhogg
workspace member that links the library at the commit under test and runs
scripted sequences of library calls in one process; pbfhogg writes it to the
protocol below, and a pbfhogg commit without that member has nothing for this
command to build. `brokkr library` builds the harness per commit (so
`--commit` works), lists the scenarios it declares, and runs one as a
verification.

pbfhogg owns what a call does: the scenarios, the library calls they make
and the assertions about them that only the process itself can judge. brokkr
owns the build, the process and the checks a process boundary can make. It
knows no pbfhogg API semantics, only the identifiers the harness declares
and the shapes below.

This document is the protocol, binding on both sides. It describes protocol
version 1 as this brokkr implements it, and grows as brokkr does more with
it; a harness output it does not allow is refused, never guessed at. Version
1 is not yet frozen: a document carrying a field this brokkr does not
implement is refused as an unknown key, and compatibility between brokkr
releases is not promised until it is.

## Usage

```
brokkr library [--commit REF] [--debug | --release | --profile NAME]
brokkr library SCENARIO [--dataset NAME] [--snapshot KEY] [--commit REF] [...]
```

Bare, builds the harness and lists the scenarios it declares: per scenario
its id, call count, deadline and description, then one line per call (with
the capabilities its stream claims, and beneath it its CLI counterpart and
declared policy differences), role and assertion. The listing is data on
stdout; build narration goes where `brokkr man output-channels` says.

With `SCENARIO`, runs that scenario once as a verification (see Running a
scenario): quiet on pass, one result line; loud on fail, every failure and
the kept run directory, exiting non-zero. A scenario the describe document
does not declare is refused, naming the ones it does. A run also builds the
pbfhogg CLI of the same tree, release with default features whatever
profile the harness gets, the reference its counterparts replay against
(see CLI equality); both builds finish before either is used.

- `--commit REF` builds the harness from a git worktree at REF. Config,
  including `[pbfhogg.library]` and the host's datasets and scratch, still
  comes from the current tree. A commit that predates the harness member
  fails its build.
- `--debug`, `--release`, `--profile NAME` choose the cargo profile,
  overriding the config's `debug`/`profile`. The default is release: the
  library is measured in release, and one build serves every use.
- `--dataset NAME` (default the shared default dataset, as every pbfhogg
  command) and `--snapshot KEY` (default `base`, the dataset's primary data)
  choose the files the scenario's dataset roles bind. There is no
  `--variant`: each dataset role names its own variant, resolved in the
  chosen dataset and snapshot as `brokkr man datasets` describes, its
  `xxhash` checked when one is configured. Both need `SCENARIO`.

## `[pbfhogg.library]`

```toml
[pbfhogg.library.harness]
package = "pbfhogg-harness"   # the workspace member
binary = "pbfhogg-harness"    # its bin, when it is not named like the package
features = []                 # cargo features for the build
profile = "release"           # or debug = true
```

`[pbfhogg.library.harness]` is required inside `[pbfhogg.library]` and has
the shape of `[piners.harness]` (`brokkr man piners-config`). Nothing else is
accepted in either table.

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

brokkr runs it in the project root with `BROKKR_MARKER_FIFO` and
`BROKKR_HARNESS_ARTEFACT_DIR` removed from its environment, and kills it at
60 seconds. It must exit 0, write nothing to stderr, and print exactly one describe
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
        {"id": "outputs-equal", "call": null}
      ]
    }
  ]
}
```

- `scenarios`: at least one, ids unique.
- `description`: one line.
- `deadline_s`: 1 to 86400, the wall one process of the scenario gets.
- `calls`: at least one, ids unique within the scenario, none `setup`. Each
  has a one-line `label`, and may have `stream`: a list of capability ids,
  each at most once, that the call's stream verdict stands for. A nonempty
  list makes brokkr add `<call>/brokkr.stream` (see The call stream); absent
  and `[]` mean the same. brokkr gives the ids no other meaning. It may have
  `cli`, its CLI counterpart, which makes brokkr add `<call>/brokkr.cli`
  (see CLI equality).
- `roles`: every path the scenario uses, ids unique within the scenario;
  possibly none.
- `assertions`: the scenario's assertion inventory, ids unique within the
  scenario; possibly none. `call` is required: a call id of this scenario,
  or `null` for an assertion about the scenario as a whole.

### Roles

| `source` | `kind` | `owner` | `variant` |
|---|---|---|---|
| `dataset` | `pbf` | forbidden | required |
| `generated` | `pbf`, `file`, `directory` | forbidden | forbidden |
| `output` | `pbf`, `file`, `directory` | required | forbidden |
| `scratch` | `directory` | required | forbidden |

- A `dataset` role is a dataset file of the run in the named variant: read
  only, and shareable between calls. `variant` is a variant name
  (`[A-Za-z0-9_.-]`, 1 to 64 bytes). Dataset roles are PBFs only.
- A `generated` role is written by the harness's setup before any call and
  read by calls; setup owns it.
- An `output` role is written by its owner; a `scratch` role is its owner's
  run scratch. `owner` is a call id of this scenario, exactly one, so no two
  calls write one path.
- `kind: pbf` declares a PBF file; one a counterpart's output placeholder
  names must also be in `pbfhogg digest`'s domain (sorted, see CLI
  equality), or its comparison fails.

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
  roles start absent; scratch roles and generated directory roles start as
  empty directories.

Dataset roles bind the resolved dataset file. Every bound path is absolute
and canonical, the run directory's included.

**The request.**

```json
{"protocol": 1, "scenario": "repeat-cat", "iteration": 0,
 "bindings": {"input": "/abs/data/denmark.osm.pbf", "out-a": "/abs/.../roles/out-a"}}
```

`bindings` names every role of the scenario and nothing else. `iteration`
counts the processes of one run from 0; a verification makes one. The
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
scratch.

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
  into the call documents.
- **Producer obligations**, which brokkr does not check: the harness reads
  only the request, the bound input roles and the private directory, and
  writes only its artefact directory, the roles its calls or setup own and
  the private directory; setup is deterministic and runs before any call;
  the call schedule honours ownership; it sets its own soft `RLIMIT_NOFILE`
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

**`scenario.json`**: `protocol`, `scenario` (the scenario's id), `plan` and
`assertions` (the scenario-level results).

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
  `{"ordinal", "decision", "installed"}`: ordinals 1, 2, ... without gaps;
  `decision` `authorized`, `refused_cancelled` or `refused_error`;
  `installed` a boolean, never true for a refused one.
- `cancel`: `null`, or what `Invocation::cancel()` returned:
  `{"result": "before_decision", "ordinal": K}` (K a `refused_cancelled`
  publication), `{"result": "after_decision", "ordinal": K}` (K an
  `authorized` one), or `{"result": "no_publication_pending"}`. After a
  cancel's K, every publication is refused. brokkr checks this consistency
  and nothing else of order; order judgments are the harness's assertions.
- `seal`: `{"state": "sealed", "outstanding_blocks": 0}`,
  `{"state": "outstanding", "outstanding_blocks": N}` (N at least 1), or
  `{"state": "refused", "reason": ONE_LINE}` (1 to 1000 bytes).
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
- `diagnostics`: the call's captured `host::Diagnostics` events, each
  `{"kind", "key", "message", "truncated"}`. `kind` is an identifier, the
  event kind's name in lower case (any kind, so a new one needs no brokkr
  change); `key` is 1 to 200 bytes; `message` is at most 4000 bytes, and a
  longer one is cut by the harness at a character boundary with `truncated`
  true. brokkr prints them and never judges them.
- `assertions`: this call's results.

**Results**: `{"id", "status", "detail"}`, `status` `pass`, `fail` or
`unevaluated`, `detail` any text of at most 4000 bytes. Across the
documents the results are exactly the inventory: one per declared
assertion, in `scenario.json` for `call: null`, else in its call's
document. A missing, duplicate, undeclared or misplaced result fails the
run, naming it.

**Plan and context** are the harness's to define; brokkr interprets
neither. Each stays in a plain value domain: objects with unique keys,
arrays, strings without NUL, booleans, `null` and integers in -2^63 to
2^64-1 (no other number, and not `-0`). `plan` holds `calls`, an object
whose keys are exactly the declared call ids, each a nonempty object. No
string in either, object keys included, may contain the run directory's
absolute spelling or a bound dataset file's (as resolved, or canonical).
That is a narrow check on the value domain, not proof that the plan is free
of run-specific paths: relative spellings and split strings pass it.

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
own:

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
  call owns, for which brokkr passes a fresh path of its own and compares
  the two files. A placeholder is the path alone: the argv spells any flag
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
- `policy_differences` (default none): one-line texts, each how the CLI's
  process policy differs from the call's. brokkr prints them with the
  listing and the result and never judges them.

**The replay.** After the harness's documents are read, whatever its other
results, and only when the run directory passed its layout check, brokkr
replays each call's counterpart, in describe order, with the CLI of the
same tree:

1. Each library output an output placeholder names must be a regular file
   at `roles/<role>`; one that is absent or of another type fails the result
   without a replay.
2. A generated input must be a regular file, and a `files_in` directory a
   directory of regular files only, at least one, every name UTF-8; else the
   result is `unevaluated`, nothing replayed.
3. brokkr makes `replay-<call>/` in the run directory holding `cwd/` and
   `outputs/`, both empty, and runs the argv in `cwd/` with each output
   placeholder bound to `outputs/<role>`, through the same flag negotiation
   and reservation as every pbfhogg launch (`brokkr man pbfhogg`, Output
   overwrite, and Launch reservations and the private directory): the
   private directory goes in `outputs/`, and the lease covers the whole run
   directory. `BROKKR_MARKER_FIFO` and `BROKKR_HARNESS_ARTEFACT_DIR` are
   removed from its environment, and it is killed at the scenario's
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
file); each library output is still a regular file. A breach fails that
result, and every later counterpart is `unevaluated`, nothing more
launched. (The release itself, which runs only once no process holds the
lease, removes the launch's private directory by the identity the lease
recorded, even through a link a child put in an ancestor's place.) A
launch whose admission or release cannot be shown safe (a lease
still held after the child exited, a private directory that could not be
made, an overlapping reservation) stops the run as a held harness lease
does: the failure is reported and nothing in the run directory is read or
written again. A stop request interrupts the run, its directory kept, also
when it arrived while a release waited. The
capability probes the negotiation makes are not run launches: they never
touch the run directory, and are bounded as every probe is.

Producer obligation, which brokkr does not check: setup leaves every
generated role as the counterparts read it, and no process the
counterpart started touches the run directory after it exits (pbfhogg's
CLI starts no helper process).

`<call>/brokkr.cli` is `pass` when every output is equal (its detail lists
each with its counts), `fail` on the first defect above (every defect found
stays in its detail), and `unevaluated` when nothing could be replayed: the
layout check failed, an input was missing, or an earlier counterpart left a
breach. Declared policy differences close its detail. The run fails on
anything but `pass`.

## The verdict

After the launch is released, brokkr checks the run directory against its
layout: exactly `request.json` (bytes unchanged), `cwd/` (the same
directory, still empty), `artefacts/` and `roles/`, nothing else (the
private directory went with the lease); in `roles/` only bound role names,
each absent or of its kind (a regular file for `file` and `pbf`, a
directory for `directory`), whatever its source, since a call that is
cancelled or fails before publishing leaves its output absent. An entry
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
of an undeclared call, an invalid or
incomplete artefact tree, an exit inconsistent with the harness's own
results (0 with a result that is not `pass`, 1 with none), or any result
that is not `pass`. `unevaluated` fails exactly as `fail` does; the report
keeps them apart.

brokkr adds one result per call, `<call>/brokkr.seal`: `pass` only for a
sealed call, since the snapshot exists only then; `<call>/brokkr.stream`
for a call that claims its stream (see The call stream); and
`<call>/brokkr.cli` for a call with a counterpart (see CLI equality). They
are reported once the call documents were read, and a result brokkr adds
never makes the harness's exit inconsistent.

On pass, brokkr prints each call's stream line, each diagnostic, each
counterpart's detail (its counts and declared policy differences) and one
result line, and removes the run directory through the reservation gate (a
refused removal is a warning,
and the directory stays). On fail, it prints every failure, results as
`<call>/<id>` or, for a scenario-level one, `<id>`, with any detail
indented beneath; harness-supplied text is printed with control characters
escaped. It writes `evidence/` into the run directory (`stdout`, `stderr`,
`stream.txt` with every FIFO record kept, in arrival order and in its wire
spelling, after a comment line per cause of capture loss, `failures.txt`),
and keeps the directory, naming it. Routine `brokkr clean` removes kept run
directories through the gate (`brokkr man clean`); the cleanup after an
interrupted command never does. An interrupted run, including one stopped
after the harness exited, keeps its directory and exits as interrupted.
