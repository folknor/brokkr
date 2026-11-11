# Script-bench resources

What one script-bench iteration owns, how its children borrow it, and what
survives a crash. The protocol is `src/reservations/iteration.rs`, an
extension of the launch-lease store pbfhogg's launches use (`brokkr man
pbfhogg`, Launch reservations and the private directory); the handles a
script's paths reach a child through are `src/bench/resources.rs`. The
script-bench runner (`src/bench/runner/`, `brokkr man bench`) drives it, one
owner per iteration; its tests run it against real children spawned through
the one spawn choke point.

## The owner and its borrows

An iteration holds an **owner lease**: a lease in the per-user store whose
record names every input the run reads (read) and one private **iteration
directory** (write), `.brokkr-iteration-<128-bit token>` in a location the
caller names, holding `out/`, `scratch/`, `stdout/`, `stderr/` and `profile/`. brokkr alone holds
the owner's descriptor; it is never lent to a child.

Each child launches with a **borrow**: a lease of its own whose record names
its owner and the call's declared resources, never a private one, since what
it uses is the owner's to remove. The child inherits the borrow's descriptor
and nothing else of the store's, as a pbfhogg child inherits its launch lease
(made inheritable for that one child at the spawn choke point, with the
parent-death signal and the orphan token). A borrow's release gives up
brokkr's own copy and tests that no process holds it any more; that test is
the call's completion, the point after which its outputs and spool can be
read as finished. A borrow still held after the release window is the call's
error and stays a live reservation.

The owner's release, when the run published or failed, removes the
iteration directory once nothing can use it. A held borrow **pins** its
owner: a scan keeps a free owner while any borrow naming it is held, and an
owner's own release keeps it, as its error, for the same reason. So:

- **Crash mid-borrow** (brokkr dies while a child runs): the owner's lock is
  free, the borrow the child carries is held, and every scan keeps the
  iteration until the child and everything that inherited its descriptor are
  gone; the scan after that reclaims both.
- **Crash after a child's exit**: neither lease is held, and the next scan
  reclaims both, the borrow's record and lock and the owner's directory.

A scan classifies every lease before it reclaims any, holding each free
one's exclusive flock meanwhile, so the order the store lists leases in
decides nothing; a free borrow-named lease (below), which owns and pins
nothing, is the one removed as the scan meets it. A free lease never becomes
held again, so a borrow seen held that exits during the scan only delays its
owner's reclamation. A lease whose record cannot be read, other than a free
borrow-named one, may be a borrow of any owner: while one exists, no free
owner is reclaimed (it is kept, with that reason, and blocks what overlaps
it). Leases with no tie, a pbfhogg launch's and a staged write's,
are reclaimed exactly as before ties existed.

The record says which a lease is: version 2 adds `owner` and
`borrows_from`, and a version 1 record reads as untied. A record with both,
a borrow holding a private resource, or a `borrows_from` that is no lease id
is unreadable, an unknown reservation.

**A borrow is not durable.** Every other lease's lock and record are synced
(file and directory) before anything they describe exists, so that after a
power loss no private directory is left without a record naming it. A
borrow holds no private resource, and it pins its owner only while held,
which no lease is after a power loss, so no reader needs its record across
one: it matters only to the admissions and scans running beside it, which
see the page cache. A borrow is therefore created, recorded and removed with
no sync at all, which keeps each call's own bookkeeping, inside the measured
block, to plain file operations. Its files are named apart for it,
`<id>.borrow.lock` and `<id>.borrow.record` (made under the same pending
name and flocked before the rename as every lease), and the name is what
lets a scan clear what a power loss can leave: a free borrow-named lease is
removed without its record being read (temporary, record, then the lock, so
a removal that fails leaves the lock for the next scan, with a warning, and
blocks nothing), as is a borrow whose record could not be published, and a borrow-named
record or temporary is removed once its lock and pending name are certainly
absent. A held borrow-named lease must hold a valid borrow record, else it
is unknown. A borrow under the ordinary names, which brokkr wrote before
the split, is read, pins its owner and is reclaimed as an ordinary lease is.

## Admitting a call

A call declares resources, absolute paths each with the access the call
needs, plus, implicitly, its own stdout and stderr spools for writing and, for a call
marked `hotpath`, its report (Hotpath reports, below). Every path is
spelled as it resolves at the call, never as an earlier resolution recorded
it, and admission runs inside the store's critical section:

1. **Read-only veto.** A declared write overlapping an input (as recorded,
   and as it resolves now) or an earlier call's spool is refused. This is
   "inputs are read-only for the run": an input's preflight digest stays
   current.
2. **Authority.** Every declared path, as it resolves now, must lie within
   an owner resource; a write only within one the owner writes. Lexical
   containment alone authorizes nothing, so a path a child turned into a
   link out of the iteration is refused.
3. **Conflicts.** The ordinary rule (overlap where either side writes)
   against every reservation except the authorizing owner, sibling borrows
   included: a borrow a surviving helper of an earlier call still holds
   refuses a later call that writes what it uses. An unknown lease refuses
   every call, as it refuses every launch.

The owner lease itself is admitted like a launch: refused when its inputs or
its directory conflict with any reservation.

## brokkr's own creations

brokkr makes the iteration's areas, scratch directories, spools and reports through
directory descriptors it opened when it laid the iteration out, never
following a link, and before each creation checks that the iteration
directory and the area still name the directories it made (by identity,
without following a link); after it, that the advertised path names what it
made. A child that displaced `stdout/`, `stderr/` or `scratch/`, by a link or by a
plain directory, makes the next creation there fail rather than land
somewhere the reservations do not name. A spool, and a scratch directory,
is admitted as a write through the same three steps before it is created,
so a sibling borrow still reserving the area refuses it. The read-only veto
resolves each input as the caller spelled it at admission, not its recorded
form, whose `..` were folded textually; an input that no longer resolves
refuses the admission.

## brokkr's own file access

The script's file builtins and a call's stdin are brokkr reading and
writing through a handle (`docs/commands/bench.md`, Files), so each is
admitted as a call declaring that one path would be, read or write, inside
the store's critical section, and acts on the path as that admission
resolved it:

- **Opening for reading** (`read()`, `filesize()`, a call's `stdin`, the
  last opened against the call's own admission, inside the same critical
  section, and handed to the child as a fresh descriptor at offset zero):
  an entry of the iteration through its area's held descriptor, after the
  area is checked to be where its path says, with `O_NOFOLLOW`; anything
  else, which authority leaves only an input, by walking its resolved path
  from `/` one component at a time, following no link. A link substituted
  after the admission resolved the path fails the open; the file must be a
  regular one. A spool opens only as the inode its call wrote: the
  iteration keeps a descriptor on every spool until it ends, so the
  identity it checks cannot be reused by a file a child put in its place.
- **Preparing the page cache** (a call's `cold` and `warm`, inputs only):
  after the call is admitted, each input opened as `read()` opens it, and
  every write-back, advice, read and residency scan done through that one
  descriptor. A prepared input that is also the call's stdin must be the
  same inode as the descriptor admission opened for it. Each existing
  file the call declares written is looked at by its own path and refused
  when it is a prepared inode; a declared directory is not searched, and a
  link a child makes afterwards is outside the guarantee as any child's
  namespace work is. A preflight's `page_cache()` opens a bound input the
  same way, resolved as admission resolves it and walked without following
  a link, with no admission: the read hold already covers it.
- **Writing** (`write()`, an output only): admitted as a write, so the
  read-only veto refuses a name a child turned into a link onto an input;
  then through `out/`'s held descriptor, whatever entry the name holds is
  unlinked (a directory refused) and the file created with `O_CREAT |
  O_EXCL | O_NOFOLLOW`, and the path checked to name what was made.
  Removing a name never writes the inode it named, so an output a child
  hard-linked to an input is cut from it, its contents untouched; a child
  recreating the name in between makes the exclusive create fail.

The boundary is the namespace's, as for creations: a child moving a held
directory or replacing a name while brokkr works is detected afterwards
where a check can see it, and makes the operation fail; it is not
prevented, and a check after the fact undoes no I/O. A spool's inode check
proves the file is the one its call wrote, not that nothing wrote it since;
only declared writes are refused.

## What a child does with a path

What a child does with a path after its admission is outside the guarantee:
handles reserve what they name, they do not confine a child, and a literal
string a child treats as a path is undeclared. The guarantee rests on a
child, and every process it starts, keeping the borrow descriptor it
inherited while it can still use the iteration; one that closes it can make
its call look complete, and with brokkr gone, its iteration reclaimable.

## Handles

A handle is a path with an origin, which gives its default access in a call
that passes it:

| origin | made by | default access | writable |
|---|---|---|---|
| input | a bound file-valued param, the first handles an iteration holds | read | no |
| output | the iteration, a path under `out/`, not created | write | yes |
| scratch | the iteration, an empty directory under `scratch/` | write | yes |
| spool | a finished call's stdout or stderr, under `stdout/` or `stderr/` (a result's `stdout_file`, `stderr_file`) | read | no |

A call's `reads` and `writes` lists override the default and also declare
handles the argv does not show (a path inside a file the script wrote, a
`--output=PATH` spelling); its `stdin` is read, over its argv default. A
handle in both lists is refused, and so is an input or a spool in `writes`,
and the stdin handle in `writes`; every refusal is judged on the lists as
given, before they fold into one access per path. Output and scratch names
are one path component, each given once per iteration. The file builtins
take a file's handle: `read()`, `filesize()` and `stdin` an input, an
output or a spool, `write()` an output; a scratch handle names a
directory, and none of them takes it. A handle is recognised by a
protected metatable brokkr made, never a key, so no data can pass for one.

Only a bound param makes a file an input. The resolved config `config()`
returns holds every other file entry as its path string, so a file a script
picks from it is undeclared, like any literal, and is neither reserved,
digested nor paired; a script that needs one reserved binds it with a
param. The file builtins that take handles (`read`, `write`, `filesize`)
therefore reach a config file only through a bound param.

## The run's inputs

An iteration's owner lease reserves the inputs only while the iteration
lives, and their digests are taken before the build, so the run itself
holds them too: once its params are bound, before hashing, the runner takes
a **read hold**, an untied lease reserving every bound input of every arm
(`docs/brokkr.toml.bench.md`, Binding params) for reading, each path once,
refused like a launch when any reservation writes an overlapping path or
any lease cannot be established, and refused outright without a lease
store (it never degrades to an unreserved run). It is held until the last
arm ends, its publication included, and lent to no child. Each arm's
iterations still list that arm's own inputs, in its handles' order, a file
two of its values share listed for each. Between iterations and
during the build it is what keeps another lease from writing an input; a
crash leaves it free, and a scan reclaims it, while any iteration a
surviving child still uses keeps its own owner lease, pinned by that
child's borrow, and with it the same inputs. A tree digest (a plain
dataset's `path` naming a directory) records internal links by their
target text, so a file reached through a link out of the tree is neither
reserved nor digested by naming the tree.

The read hold is taken before a script's preflight runs
(`docs/commands/bench.md`, Preflight), so a preflight child that looks at
an input's path finds it held against writers. It does not stop the child
from writing it: a preflight holds no handle, so whatever it does with a
path is undeclared, as a literal path is in an iteration.

## Preflight children

A preflight runs before any build and outside any iteration, so its
children have no owner and no borrow. Each launches on a lease of its own
(`LaunchSpec` with nothing declared) that reserves no resource and is
armed for that one spawn, so the child inherits it, dies with brokkr and
carries the orphan token as any armed launch does. Its release after the
child is reaped is the call's completion, as a borrow's is: a descendant
still holding the lease past the release window fails the call. That is
cooperative, as a borrow is: a child that closes the descriptor escapes it.
A preflight child's output is captured through pipes, bounded as a result's
preview is, and nothing of it is stored.

## Spools

Each call's stdout and stderr go to its two spools, files brokkr creates
under `stdout/` and `stderr/`, both named by the call's index in the
iteration, and hands the child as those descriptors: no pipe and no drain
thread, so nothing about either is bounded, and the scratch drive holds
both whole until the iteration ends. Both are pinned and read-only from
their making, as every spool is. The stdout spool comes back from the call
only when its borrow's release succeeded; the stderr spool the runner keeps
apart from the call, so its end is snapshotted even when the release failed
(the stored capture then says the call was still held, so what it holds is
what was there, not the final contents). A preview reads a spool's first
bytes, and a tail its last as of one observation of its size, through the
spool's own descriptor, never its path; a file that shrank before the
range was read fails the read (the stored tail is then unreadable), never
passing a shorter range off as the end. Truncating a preview fails
nothing. Feeding a result onward passes a spool, a read-only handle, as an
argument or as the next call's stdin.

The runner previews the first MiB of each and, of stderr, the last MiB as
well; the script sees those and both spools' handles. The store keeps of
each child's stderr its observed size and its last 64 KiB as text, within a
run budget of 16 MiB of raw tail bytes (`reference/bench-store.md`, The run
document). A child's whole stderr is never stored: the spool goes with its
iteration.

## Hotpath reports

A call marked `hotpath` (`docs/commands/bench.md`, Hotpath capture) gets a
report file under `profile/`, named as its spools are, declared written by its
borrow beside the spools inside the same admission, and created empty with
`O_CREAT | O_EXCL | O_NOFOLLOW` through the area's held descriptor once
admitted; the child is told its path in `HOTPATH_OUTPUT_PATH` and writes it
by that path. No handle ever names it, so no script output or call can
claim it, and while the call's borrow is held any other call declaring it
conflicts. It is read once the borrow's release proved no holder is left,
admitted and opened as `read()` opens a file; a report read while something
the child started still holds the call is not read at all. Unlike a spool,
its inode is not pinned: what a child wrote there is evidence it supplied,
and a report a child replaced is read as what it is.

While a call's borrow release waits for its last holder, the caller may run
an observer between the window's polls; the runner drains the child's
marker FIFO there, so a descendant writing records after the child exited is
never left blocked on a full pipe until the window closes, and only once the
release proved no holder is the FIFO's capture closed. One FIFO per child, so
a record lands in the evidence of the child whose borrow wrote it.

## Placement and cleanup

The runner places every iteration directory in `<scratch>/bench-iterations/`,
`scratch` the host's resolved scratch directory
(`layout::bench_iterations_container`), so a script bench writes to the
drive the typed commands do. The iteration directory is removed by its
lease alone, at the owner's release or by a later scan; `brokkr clean` has
no name for it and reclaims free leases in every project, by the path each
lease recorded.

Before any attempt the runner refuses a container strictly inside the
project state root (`.brokkr/`) other than `<state root>/bench-iterations`,
the place a `scratch = ".brokkr"` gives: every other directory there is one
brokkr removes or prunes by fixed name outside the gate. The comparison is
lexical, `..` folded, so it is a diagnostic for that one misplacement, not
the guarantee.

The obligation the placement meets: every brokkr operation whose operand
names the container, or one of its ancestors, by path, as brokkr spells it
from its fixed layout or from configuration, goes through the reservation
gate or cannot name it.

- A configured path can name it (a host's `scratch = ".brokkr"`, or another
  project's absolute scratch pointing here), so every recursive removal of a
  configured directory goes through the gate, without the pbfhogg backup
  walk outside pbfhogg: the measured runner's scratch trees in every
  project; in `brokkr clean`, elivagar's scratch, `tilegen_tmp` and corpus
  calibrands and nidhogg's ingest output; elivagar's tilegen removal of its
  tmp dir. When that tmp dir is the scratch dir itself elivagar empties it
  in the child, so it is checked against the gate before the launch; a check
  suffices there because the global lock admits no new iteration between
  the check and the child, and a pin that exists is refused. The gate is
  per user, so another project's operand is refused as this project's is.
- brokkr's other recursive removals and directory renames (tool caches,
  staging directories, test-hung snapshots, the artifact store, litehtml
  caches) target a fixed name under a fixed or configured directory, which
  equals the container or an ancestor only if that fixed name is a
  checkout's ancestor's, or a configured scratch puts the container inside
  such a target (a scratch equal to or beneath a tool install or cache
  under the data dir, such as `planetiler-bench-classes`, whose pruning
  removes every entry but one); such a collision is outside the guarantee.
  A container beside such a target, the default `data/scratch` or a
  scratch of `data` included, is covered.
  Worktree removal, whose target is a checkout, is gated already.
- An alias, a symlink or a bind mount, that leads an ungated operation into
  the container is outside the guarantee. A gated operation resolves its
  operands and the reservations, so it stays protected through any symlink.

Inputs are protected only against gated mutations.
