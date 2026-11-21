# Script-bench resources

This page covers what one script-bench iteration owns, how its children
borrow it, and what survives a crash. The protocol is
`src/reservations/iteration.rs`, an extension of the launch-lease store
(`src/reservations/`). The handles that carry a script's paths to a child
are `src/bench/resources.rs`. The script-bench runner (`src/bench/runner/`,
`brokkr man bench`) drives the protocol, one owner per iteration. Its tests
run it against real children spawned through the one spawn choke point.

## The owner and its borrows

An iteration holds an **owner lease**: a lease in the per-user store whose
record names every input the run reads (read) and one private **iteration
directory** (write). The directory is `.brokkr-iteration-TOKEN`, `TOKEN`
128 bits, in a location the caller names. It holds `out/`, `scratch/`,
`stdout/`, `stderr/` and `profile/`. brokkr alone holds the owner's
descriptor; it's never lent to a child.

Each child launches with a **borrow**: a lease of its own whose record names
its owner and the call's declared resources. A borrow never names a private
resource, since what it uses is the owner's to remove. The child inherits
the borrow's descriptor and nothing else of the store's, as a reserved
child inherits its launch lease. The spawn choke point makes the descriptor
inheritable for that one child, with the parent-death signal and the orphan
token.

A borrow's release gives up brokkr's own copy and tests that no process
holds it any more. That test is the call's completion, the point after
which its outputs and spool can be read as finished. A borrow still held
after the release window is the call's error, and stays a live
reservation.

The owner's release, when the run published or failed, removes the
iteration directory once nothing can use it. A held borrow **pins** its
owner: a scan keeps a free owner while any borrow naming it is held, and an
owner's own release keeps it, as its error, for the same reason. Two crash
cases follow:

- **Crash mid-borrow** (brokkr dies while a child runs): the owner's lock is
  free and the child's borrow is held. Every scan keeps the iteration until
  the child and everything that inherited its descriptor are gone; the scan
  after that reclaims both.
- **Crash after a child's exit**: neither lease is held, and the next scan
  reclaims both: the borrow's record and lock, and the owner's directory.

A scan classifies every lease before it reclaims any, holding each free
lease's exclusive flock meanwhile, so the order the store lists leases in
decides nothing. The exception is a free borrow-named lease (below), which
owns and pins nothing: the scan removes it as it meets it. A free lease
never becomes held again, so a borrow seen held that exits during the scan
only delays its owner's reclamation.

A lease whose record can't be read, other than a free borrow-named one, may
be a borrow of any owner. While one exists, the scan reclaims no free
owner: it keeps the owner, with that reason, and the owner blocks what
overlaps it. Untied leases are reclaimed once their last holder exits.

The record says which kind a lease is. Version 2 adds `owner` and
`borrows_from`, and a version 1 record reads as untied. A record is
unreadable, an unknown reservation, when it has both fields, is a borrow
holding a private resource, or has a `borrows_from` that is no lease id.

**A borrow isn't durable.** Every other lease's lock and record are synced
(file and directory) before anything they describe exists, so that after a
power loss no private directory is left without a record naming it. A
borrow holds no private resource, and pins its owner only while held, which
no lease is after a power loss. So no reader needs a borrow's record across
one: it matters only to the admissions and scans running beside it, which
see the page cache. brokkr therefore creates, records and removes a borrow
with no sync at all, which keeps each call's own bookkeeping, inside the
measured block, to plain file operations.

A borrow's files are named apart: `ID.borrow.lock` and `ID.borrow.record`,
made under the same pending name and flocked before the rename as every
lease. The name lets a scan clear what a power loss can leave:

- A free borrow-named lease is removed without its record being read:
  temporary, record, then the lock. A removal that fails leaves the lock
  for the next scan, with a warning, and blocks nothing.
- A borrow whose record couldn't be published is removed the same way.
- A borrow-named record or temporary is removed once its lock and pending
  name are certainly absent.
- A held borrow-named lease must hold a valid borrow record, else it's
  unknown.

A borrow under the ordinary names, as older brokkr wrote them, is read,
pins its owner and is reclaimed as an ordinary lease is.

## Admitting a call

A call declares resources: absolute paths, each with the access the call
needs. It also declares, implicitly, its own stdout and stderr spools for
writing and, for a call marked `hotpath`, its report (Hotpath reports,
below). Every path is spelled as it resolves at the call, never as an
earlier resolution recorded it. Admission runs inside the store's critical
section, in three steps:

1. **Read-only veto.** A declared write overlapping an input (as recorded,
   and as it resolves now) or an earlier call's spool is refused. This is
   "inputs are read-only for the run": an input's preflight digest stays
   current.
2. **Authority.** Every declared path, as it resolves now, must lie within
   an owner resource, and a write only within one the owner writes.
   Lexical containment alone authorizes nothing, so a path a child turned
   into a link out of the iteration is refused.
3. **Conflicts.** The ordinary rule (overlap where either side writes)
   applies against every reservation except the authorizing owner, sibling
   borrows included. A borrow a surviving helper of an earlier call still
   holds refuses a later call that writes what it uses. An unknown lease
   refuses every call, as it refuses every launch.

The owner lease itself is admitted like a launch: refused when its inputs or
its directory conflict with any reservation.

## brokkr's own creations

brokkr makes the iteration's areas, scratch directories, spools and reports
through directory descriptors it opened when it laid the iteration out,
never following a link. Before each creation it checks that the iteration
directory and the area still name the directories it made, by identity and
without following a link. After it, brokkr checks that the advertised path
names what it made. A child that displaced `stdout/`, `stderr/` or
`scratch/`, by a link or by a plain directory, makes the next creation
there fail, rather than write somewhere the reservations don't name.

A spool, and a scratch directory, is admitted as a write through the same
three steps before brokkr creates it, so a sibling borrow still reserving
the area refuses it. The read-only veto resolves each input as the caller
spelled it at admission, not its recorded form, whose `..` were folded
textually. An input that no longer resolves refuses the admission.

## brokkr's own file access

The script's file builtins and a call's stdin are brokkr reading and
writing through a handle (`docs/commands/bench.md`, Files). Each is
admitted as a call declaring that one path would be, read or write, inside
the store's critical section, and acts on the path as that admission
resolved it. The operations:

- **Opening for reading**: `read()`, `filesize()`, and a call's `stdin`.
  The stdin is opened against the call's own admission, inside the same
  critical section, and handed to the child as a fresh descriptor at offset
  zero. An entry of the iteration opens through its area's held
  descriptor, after the area is checked to be where its path says, with
  `O_NOFOLLOW`. Anything else, which authority leaves only an input, opens
  by walking its resolved path from `/` one component at a time, following
  no link. A link substituted after the admission resolved the path fails
  the open, and the file must be a regular one.
- **Opening a spool**: a spool opens only as the inode its call wrote. The
  iteration keeps a descriptor on every spool until it ends, so a file a
  child put in its place can't reuse the identity it checks.
- **Preparing the page cache**: a call's `cold` and `warm`, inputs only.
  After the call is admitted, brokkr opens each input as `read()` opens it,
  and does every write-back, advice, read and residency scan through that
  one descriptor. A prepared input that is also the call's stdin must be
  the same inode as the descriptor admission opened for it.
- **Prepared inodes and writes**: brokkr looks at each existing file the
  call declares written by its own path, and refuses it when it's a
  prepared inode. A declared directory isn't searched. A link a child makes
  afterwards is outside the guarantee, as any child's namespace work is.
- **A preflight's `page_cache()`** opens a bound input the same way,
  resolved as admission resolves it and walked without following a link,
  with no admission: the read hold already covers it.
- **Writing**: `write()`, an output only. It's admitted as a write, so the
  read-only veto refuses a name a child turned into a link onto an input.
  Then, through `out/`'s held descriptor, brokkr unlinks whatever entry the
  name holds (a directory is refused), creates the file with
  `O_CREAT | O_EXCL | O_NOFOLLOW`, and checks the path names what it made.
  Removing a name never writes the inode it named, so an output a child
  hard-linked to an input is cut from it, its contents untouched. A child
  recreating the name in between makes the exclusive create fail.

The boundary is the namespace's, as for creations. A child moving a held
directory or replacing a name while brokkr works is detected afterwards,
where a check can see it, and makes the operation fail. It isn't prevented,
and a check after the fact undoes no I/O. A spool's inode check proves the
file is the one its call wrote, not that nothing wrote it since; only
declared writes are refused.

## What a child does with a path

What a child does with a path after its admission is outside the guarantee.
Handles reserve what they name; they don't confine a child, and a literal
string a child treats as a path is undeclared. The guarantee rests on a
child, and every process it starts, keeping the borrow descriptor it
inherited while it can still use the iteration. A child that closes it can
make its call look complete and, with brokkr gone, its iteration
reclaimable.

## Handles

A handle is a path with an origin, and the origin gives its default access
in a call that passes it:

| origin | made by | default access | writable |
|---|---|---|---|
| input | a bound file-valued param | read | no |
| output | the iteration, a path under `out/`, not created | write | yes |
| scratch | the iteration, an empty directory under `scratch/` | write | yes |
| spool | a finished call's stdout or stderr | read | no |

Inputs are the first handles an iteration holds. A spool lives under
`stdout/` or `stderr/`, and a result exposes it as `stdout_file` or
`stderr_file`.

A call's `reads` and `writes` lists override the default. They also declare
handles the argv doesn't show: a path inside a file the script wrote, or a
`--output=PATH` spelling. A call's `stdin` is read, over its argv default.
brokkr refuses:

- a handle in both lists
- an input or a spool in `writes`
- the stdin handle in `writes`

Every refusal is judged on the lists as given, before they fold into one
access per path. Output and scratch names are one path component, each
given once per iteration.

The file builtins take a file's handle: `read()`, `filesize()` and `stdin`
take an input, an output or a spool, and `write()` takes an output. A
scratch handle names a directory, and none of them takes it. brokkr
recognises a handle by a protected metatable it made, never by a key, so no
data can pass for one.

Only a bound param makes a file an input. The resolved config `config()`
returns holds every other file entry as its path string. A file a script
picks from it is undeclared, like any literal, and is neither reserved,
digested nor paired; a script that needs one reserved binds it with a
param. So the file builtins that take handles (`read`, `write`, `filesize`)
reach a config file only through a bound param.

## The run's inputs

An iteration's owner lease reserves the inputs only while the iteration
lives, and their digests are taken before the build, so the run itself
holds them too. Once its params are bound, before hashing, the runner takes
a **read hold**: an untied lease reserving for reading every bound input of
every arm (`docs/brokkr.toml.bench.md`, Binding params), each path once.
The read hold is refused:

- like a launch, when any reservation writes an overlapping path
- when any lease can't be established
- outright without a lease store; it never degrades to an unreserved run

It's held until the last arm ends, its publication included, and lent to
no child. Each arm's iterations still list that arm's own inputs, in its
handles' order, a file two of its values share listed for each.

Between iterations and during the build, the read hold keeps another lease
from writing an input. A crash leaves it free, and a scan reclaims it.
Meanwhile any iteration a surviving child still uses keeps its own owner
lease, pinned by that child's borrow, and with it the same inputs.

A tree digest (a plain dataset's `path` naming a directory) records
internal links by their target text. So naming the tree neither reserves
nor digests a file reached through a link out of the tree.

The runner takes the read hold before a script's preflight runs
(`docs/commands/bench.md`, Preflight), so a preflight child that looks at
an input's path finds it held against writers. The hold doesn't stop the
child from writing it: a preflight holds no handle, so whatever it does
with a path is undeclared, as a literal path is in an iteration.

## Preflight children

A preflight runs before any build and outside any iteration, so its
children have no owner and no borrow. Each launches on a lease of its own
(`LaunchSpec` with nothing declared) that reserves no resource and is armed
for that one spawn. So the child inherits it, dies with brokkr, and has the
orphan token, as any armed launch does.

The lease's release after the child is reaped is the call's completion, as
a borrow's is: a descendant still holding the lease past the release window
fails the call. That's cooperative, as a borrow is: a child that closes the
descriptor escapes it. brokkr captures a preflight child's output through
pipes, bounded as a result's preview is, and stores none of it.

## Spools

Each call's stdout and stderr go to its two spools. brokkr creates them
under `stdout/` and `stderr/`, both named by the call's index in the
iteration, and hands them to the child as those descriptors. There's no
pipe and no drain thread, so nothing about either is bounded, and the
scratch drive holds both whole until the iteration ends. Both are pinned
and read-only from their making, as every spool is.

The call returns the stdout spool only when its borrow's release succeeded.
The runner keeps the stderr spool apart from the call, so it snapshots its
end even when the release failed. The stored capture then says the call
was still held, so what it holds is what was there, not the final
contents.

A preview reads a spool's first bytes, and a tail its last as of one
observation of its size, through the spool's own descriptor, never its
path. A file that shrank before the range was read fails the read, and the
stored tail is then unreadable; brokkr never passes a shorter range off as
the end. Truncating a preview fails nothing. Feeding a result onward passes
a spool, a read-only handle, as an argument or as the next call's stdin.

The runner previews the first MiB of each spool and, of stderr, the last
MiB as well; the script sees those and both spools' handles. The store
keeps of each child's stderr its observed size and its last 64 KiB as text,
within a run budget of 16 MiB of raw tail bytes (`reference/bench-store.md`,
The run document). A child's whole stderr is never stored: the spool is
removed with its iteration.

## Hotpath reports

A call marked `hotpath` (`docs/commands/bench.md`, Hotpath capture) gets a
report file under `profile/`, named as its spools are. Its borrow declares
the report written, beside the spools, inside the same admission. Once the
call is admitted, brokkr creates it empty with
`O_CREAT | O_EXCL | O_NOFOLLOW` through the area's held descriptor. The
child is told its path in `HOTPATH_OUTPUT_PATH` and writes it by that path.

No handle ever names the report, so no script output or call can claim it,
and while the call's borrow is held any other call declaring it conflicts.
brokkr reads it once the borrow's release proved no holder is left,
admitted and opened as `read()` opens a file. A report read while something
the child started still holds the call isn't read at all. Unlike a spool,
its inode isn't pinned: what a child wrote there is evidence it supplied,
and a report a child replaced is read as what it is.

While a call's borrow release waits for its last holder, the caller may run
an observer between the window's polls. The runner drains the child's
marker FIFO there, so a descendant writing records after the child exited
is never left blocked on a full pipe until the window closes. brokkr closes
the FIFO's capture only once the release proved no holder. There's one FIFO
per child, so a record goes into the evidence of the child whose borrow
wrote it.

## Placement and cleanup

The runner places every iteration directory in `SCRATCH/bench-iterations/`,
`SCRATCH` the host's resolved scratch directory
(`layout::bench_iterations_container`), so a script bench writes to the
configured scratch drive. Only its lease removes the iteration directory,
at the owner's release or in a later scan. `brokkr clean` has no name for
it, and reclaims free leases in every project by the path each lease
recorded.

Before any attempt, the runner refuses a container strictly inside the
project state root (`.brokkr/`) other than `STATE_ROOT/bench-iterations`,
the place a `scratch = ".brokkr"` gives. Every other directory there is one
brokkr removes or prunes by fixed name outside the gate. The comparison is
lexical, `..` folded, so it's a diagnostic for that one misplacement, not
the guarantee.

The placement meets one obligation: every brokkr operation whose operand
names the container, or one of its ancestors, by path, goes through the
reservation gate or can't name it. The path here is as brokkr spells it
from its fixed layout or from configuration. How each operation meets it:

- **Configured paths** can name it: a host's `scratch = ".brokkr"`, or
  another project's absolute scratch pointing here. So every recursive
  removal of a configured directory goes through the overlap gate; in
  `brokkr clean`, that's the ingest outputs nidhogg's retired commands
  left. The gate is per user, so another project's operand is refused as
  this project's is.
- **Fixed names**: brokkr's other recursive removals and directory renames
  (staging directories, test-hung snapshots, the artifact store, litehtml
  caches) target a fixed name under a fixed or configured directory. That
  equals the container or an ancestor only if the fixed name is a
  checkout's ancestor's, or a configured scratch puts the container inside
  such a target; such a collision is outside the guarantee. A container
  beside such a target, the default `data/scratch` or a scratch of `data`
  included, is covered.
- **Worktree removal**, whose target is a checkout, is gated already.
- **Aliases**: a symlink or a bind mount that leads an ungated operation
  into the container is outside the guarantee. A gated operation resolves
  its operands and the reservations, so it stays protected through any
  symlink.

Inputs are protected only against gated mutations.
