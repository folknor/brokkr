# Script-bench resources

What one script-bench iteration owns, how its children borrow it, and what
survives a crash. The protocol is `src/reservations/iteration.rs`, an
extension of the launch-lease store pbfhogg's launches use (`brokkr man
pbfhogg`, Launch reservations and the private directory); the handles a
script's paths reach a child through are `src/bench/resources.rs`. Nothing
drives it before the script-bench runner; its tests run it against real
children spawned through the one spawn choke point.

## The owner and its borrows

An iteration holds an **owner lease**: a lease in the per-user store whose
record names every input the run reads (read) and one private **iteration
directory** (write), `.brokkr-iteration-<128-bit token>` in a location the
caller names, holding `out/`, `scratch/` and `stdout/`. brokkr alone holds
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
decides nothing. A free lease never becomes held again, so a borrow seen
held that exits during the scan only delays its owner's reclamation. A lease
whose record cannot be read may be a borrow of any owner: while one exists,
no free owner is reclaimed (it is kept, with that reason, and blocks what
overlaps it). Leases with no tie, a pbfhogg launch's and a staged write's,
are reclaimed exactly as before ties existed.

The record says which a lease is: version 2 adds `owner` and
`borrows_from`, and a version 1 record reads as untied. A record with both,
a borrow holding a private resource, or a `borrows_from` that is no lease id
is unreadable, an unknown reservation.

## Admitting a call

A call declares resources, absolute paths each with the access the call
needs, plus, implicitly, its own stdout spool for writing. Every path is
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

brokkr makes the iteration's areas, scratch directories and spools through
directory descriptors it opened when it laid the iteration out, never
following a link, and before each creation checks that the iteration
directory and the area still name the directories it made (by identity,
without following a link); after it, that the advertised path names what it
made. A child that displaced `stdout/` or `scratch/`, by a link or by a
plain directory, makes the next creation there fail rather than land
somewhere the reservations do not name. A spool, and a scratch directory,
is admitted as a write through the same three steps before it is created,
so a sibling borrow still reserving the area refuses it. The read-only veto
resolves each input as the caller spelled it at admission, not its recorded
form, whose `..` were folded textually; an input that no longer resolves
refuses the admission.

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
| input | the resolved config's file entries and file-valued params | read | no |
| output | the iteration, a path under `out/`, not created | write | yes |
| scratch | the iteration, an empty directory under `scratch/` | write | yes |
| spool | a finished call's stdout, under `stdout/` | read | no |

A call's `reads` and `writes` lists override the default and also declare
handles the argv does not show (a path inside a file the script wrote, a
`--output=PATH` spelling). A handle in both lists is refused, and so is an
input or a spool in `writes`. Output and scratch names are one path
component, each given once per iteration.

## Stdout spools

Each call's stdout goes to its spool, a file brokkr creates under `stdout/`
and hands the child as its stdout descriptor: no pipe and no drain thread,
so nothing about it is bounded. The spool comes back from the call only when
its borrow's release succeeded; a preview reads its first bytes through the
spool's own descriptor, never its path, with a flag saying whether there was
more. Truncating the preview fails nothing. Feeding a result onward passes
the spool, a read-only handle.

## Placement and cleanup

The caller chooses where the iteration directory goes, and must place it
where every brokkr removal that can reach it goes through the reservation
gate: a directory another command wipes plainly (elivagar's scratch, say)
would remove a pinned iteration whatever its lease says. Inputs are
protected the same way only against gated mutations. The iteration directory
is removed by its lease alone, at the owner's release or by a later scan;
`brokkr clean` has no name for it.
