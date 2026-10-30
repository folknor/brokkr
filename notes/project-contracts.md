# Keeping brokkr and its projects in step across versions

Design discussion, 2026-10-30. Nothing here is decided. It records the
problem, what triggered it, and the ways it could be solved, so the choice
can be made deliberately.

## The problem

brokkr drives each project's binary through a contract it does not own: the
command line (commands, flags, values, defaults), the exit codes and what
they mean, the outputs a command writes (files, stdout, formats), side
channels (the marker FIFO, `BROKKR_*` variables) and, for some projects,
harness protocols (pbfhogg's library harness, piners' corpus harness).

The projects change that contract over time. brokkr has to keep working
against every version of it that a `--commit` baseline, an A/B or a verify
run can land on, not only against HEAD.

Today brokkr encodes each project's contract by hand, as of whenever someone
last edited the typed command layer (`src/pbfhogg/commands.rs` and its
counterparts). It is correct for roughly one point in history, and drift is
found by runs failing.

## What triggered it

One session's worth of pbfhogg 0.6.0 tickets, every one of them contract
drift:

- **bz 67**: pbfhogg 0.6.0 gave every short flag one meaning across all
  commands and refused the retired letters with exit 64. brokkr still sent
  `tags-filter -i` and `-t`. Fixed by spelling every flag long, except
  verify renumber's `diff -s`, whose long form was renamed (`--summary` to
  `--osmium-summary`) while `-s` kept its meaning, so `-s` is the only
  spelling every baseline with the option accepts.
- **bz 64**: pbfhogg added `check --index`; brokkr had no way to measure it.
- **bz 68**: text diff output outgrew the 16 MiB stdout capture at planet
  scale; brokkr now passes `--output`, which changes what older rows
  measured without anything in the row saying so.
- **bz 65**: pbfhogg 0.6.0 split its exit codes: 1 is a negative answer from
  a completed run, 2 a failure, 3 a host I/O feature unavailable, 64 a usage
  error. Before 0.6.0, exit 1 meant both "no" and "crashed". brokkr accepts
  exit 1 as a completed answer for `diff`, `diff-snapshots`, `check-refs`,
  `check-ids` and `check-index` (`ok_exit_codes`), for every build. A
  `--commit` baseline at v0.5.0 or earlier that crashed with exit 1 is
  therefore stored as a clean answer. The pbfhogg dev was about to run
  v0.5.0 baselines when this was raised.
- The `-j` help text claimed pbfhogg defaults that were never or no longer
  true (`inspect` "1", `apply-changes` "nproc-2"); pbfhogg 0.6.0 has one
  `-j` meaning everywhere, default `0` = sized automatically.

bz 65 is the one still open, and the one these approaches mostly decide.

## Approach 1: brokkr keeps the knowledge, gated by version

Keep the typed command layer and add version checks where the contract
changed: accept exit 1 only when the built pbfhogg is 0.6.0 or later, pick
the flag spelling by version, refuse a command the version predates.

- **For:** smallest change; no work on the project side; fixes bz 65 point 1
  this week.
- **Against:** every project change becomes a brokkr edit plus a gate, and
  the gates grow forever. Version numbers are the wrong key: `--commit` A/Bs
  live between releases (the two pbfhogg commits A/B'd on 2026-10-30,
  `d2d8836a` and `4157567d`, were both mid-0.6.0 development), and a
  `Cargo.toml` version cannot say which side of a change such a commit is
  on. The knowledge lives on the wrong side: the project author makes the
  change, brokkr finds out by a failed run.
- **Fits:** as a stopgap only.

## Approach 2: the binary describes its own contract

The project's binary answers a hidden command, for example
`pbfhogg contract --json`: a contract version, per command its flags,
outputs and which exits are answers, and the global exit table. brokkr asks
each built binary once, caching the answer under the binary's identity (as
the verify reference cache already keys reference tools), checks every argv
against it before launch, and takes exit handling from it. A binary that
does not answer gets one frozen "legacy" contract inside brokkr.

Precedents: `PbfhoggBinary` already probes the executable to decide whether
to pass `--overwrite`, and the library harness takes its argv from pbfhogg's
describe JSON, which brokkr treats as pbfhogg's data and does not rewrite.

- **For:** the project states its contract; brokkr refuses a baseline that
  lacks a needed flag up front ("baseline X has no `check --index`") instead
  of failing mid-suite with exit 64.
- **Against:** a static document describes one point. brokkr's typed
  commands still have to be expressible against every older point, and the
  document says nothing about how to get from one to another. Every binary
  built before the command existed, which is every baseline today, falls to
  the single legacy contract, and one legacy contract cannot describe the
  several shapes pbfhogg had before it. It also cannot say whether two
  rows from either side of a change are comparable.
- **Fits:** as an ingredient (the "what does HEAD look like" half of
  approach 3), not on its own.

## Approach 3: a stepwise migration chain

Treat the contract like a database schema. Each change is one declarative
step, kept in its own file like an SQL migration, and recording the project
commit that introduced it (`introduced_in = <sha>`):

- `rename_flag { command: diff, from: --summary, to: --osmium-summary }`
- `retire_short { command: tags-filter, short: -i, long: --invert-match }`
- `exit_split { 1: answer, 2: failure, 64: usage }` at `d2d8836a`
- `add_flag { command: digest, flag: --form }`
- `add_command { check --index }`
- `change_default { command: diff, flag: --jobs, from: 1, to: 0 }`
- `output_change { ... }`, which can also mark rows before it as measuring
  something different

**A commit's level** is the set of steps whose `introduced_in` is an
ancestor of it (`git merge-base --is-ancestor`). No version numbers and no
probing; it works between releases, and recording the sha explicitly lets
history be backfilled with steps written today.

**The chain lives in the project's tree and brokkr reads it from the
project's HEAD.** An old binary cannot know about the changes after it, but
HEAD knows all of history. The project owns the chain because it makes the
changes; a snapshot test on its side (of the clap tree, for the Rust
projects) fails when the CLI or exit behaviour changes without a new step.

**brokkr's typed commands target the head of the chain and are rewritten
downward** for an older commit: a renamed flag gets its old spelling; an
exit split that has not happened yet makes exit 1 a failure (bz 65 point 1
is then one step's consequence, not a special case); an added flag or
command cannot be undone, so brokkr refuses up front. Steps that mark a
comparability break make `brokkr results --compare` warn when a pair
crosses them.

### Made universal

pbfhogg is one of several projects, so the mechanism belongs to brokkr and
each project brings its own chain:

- **brokkr owns:** the list of contract surfaces (argv, exit codes, outputs,
  side channels, harness protocols), a fixed vocabulary of step kinds over
  them, the ancestry-based level, the downward rewrite and up-front refusal,
  and the comparability warning.
- **Each project owns:** its chain, found through a new optional
  `brokkr.toml` key (for example `[contract] path = "contract/"`), so a
  project without one behaves as today and no existing config changes; its
  typed command layer in brokkr, written against its chain's head; and the
  test that keeps the chain honest.
- **Surfaces brokkr owns** (the marker FIFO format, the `BROKKR_*`
  variables) get their own chain inside brokkr, and a project's chain
  records which version it adopted and from which commit. This is about
  projects' history, not about older brokkr binaries, so it does not run
  into the rule that those are never a design input.
- **Opt-in:** it pays where brokkr drives a binary across commits
  (`--commit`, A/B, verify): clearly pbfhogg, probably elivagar, nidhogg and
  piners. litehtml-rs and sluggrs (visual and criterion) may never need one.

- **For:** solves drift for every past and future version with one engine;
  backfillable; comparability becomes data.
- **Against:** brokkr still carries a typed command layer per project, so a
  new project command is still a brokkr change; the step vocabulary has to
  be right across projects, so it should come from a survey of all of them,
  not from pbfhogg alone; two places (the project's chain and brokkr's
  typed layer) still have to agree.

## Approach 4: projects declare their own benchmarks (the ratatoskr route)

Turn it around. brokkr stops knowing each project's commands. Each project
declares its benchmarks in its own tree, and brokkr is a generic runner.
ratatoskr already works this way: its gates and sync scripts are declared in
its own `brokkr.toml` and tree, and brokkr runs, measures and stores them.

**The deciding property: definitions are versioned with the code.** For
`--commit X`, brokkr reads the definitions from X's tree
(`git show X:<file>`), not HEAD's. An old commit's definition already says
how to invoke that commit's binary: its flag spellings, which exits are
answers, where it writes output. The author changes a flag and its
benchmark definition in the same commit, and their tests keep the two in
step. No migration chain is needed for anything that has definitions.

**brokkr keeps the generic machinery:** building at a commit, the lock,
dataset / variant / snapshot resolution, scratch and overwrite handling,
measurement (`--bench`, `--hotpath`, `--alloc`, the sidecar, markers), the
stdout cap, results.db, pairing and compare, `clean`.

**The project declares, per benchmark:**

- a name, the stable identity rows pair on;
- an argv template over what brokkr provides (`{pbf}`, `{osc}`, `{output}`,
  `{scratch}`, and parameters such as `{jobs}`, a flag appearing only when
  its parameter is set);
- the output kind (file, directory, none), so brokkr reserves and cleans it;
- accepted exit codes (`ok_exit = [1]`);
- axes and presets (what `brokkr suite pbfhogg` runs today).

**Comparability comes for free:** rows pair on name plus a hash of the
definition, and `--compare` says when the definition differs between the two
sides. bz 68's change from stdout to a file would have been flagged by the
data instead of by prose.

### Hard parts

1. **Commits before the scheme have no definitions**, which is every
   baseline today, v0.5.0 included. HEAD needs a one-time backfill: "for
   ancestors of the adoption commit, use these definitions". That is the
   migration problem again, but bounded, written once and frozen. It could
   be a small approach-3 chain for the pre-adoption era only.
2. **Setup chains.** Some benchmarks consume another run's output: `diff`
   needs the merged PBF from `apply-changes`, and `repack`, `degrade` and
   `cat --as-snapshot` promote outputs into new snapshots. Definitions need
   "input = artifact produced by benchmark Y", with caching. This is the
   biggest design piece.
3. **Things that are not plain benchmarks:** `verify` is cross-tool
   (pbfhogg against osmium, osmosis and osmconvert) and full of
   pbfhogg-specific comparison logic, so it would stay typed in brokkr at
   first. Elivagar's tilegen, the library harness and piners' corpus are
   protocols more than argv lists; they stay as they are or become a second
   kind of declaration later.
4. **Where definitions live:** in their own file referenced from
   `brokkr.toml` (for example `benches = "brokkr/benches.toml"`), not
   inline. `brokkr.toml` carries host sections and include layering, and
   definitions read at old commits should not be entangled with host
   configuration. A new key, so the config contract is untouched.
5. **The CLI name:** `brokkr bench` is criterion. pbfhogg's commands are
   top-level brokkr subcommands today with project-gated help; declared
   benchmarks could keep that (`brokkr <name>`, resolved dynamically) or
   live under one verb. Decide early: it decides whether muscle memory
   survives.

- **For:** the knowledge lives with the people who change it; adding a
  benchmark is a project change, not a brokkr release; old commits describe
  themselves; one runner serves every project.
- **Against:** the largest change by far; setup chains and the backfill are
  real design work; brokkr loses typed knowledge some features lean on
  (verify, snapshot promotion), which stays typed for a while, so there is a
  long period with two ways of defining a pbfhogg command.

### Migration path

1. Build the generic engine; pbfhogg declares its read-only commands first
   (`inspect`, `check-*`, `digest`, `sort`, `cat`), which have no setup
   chains, proving the declaration format on the easy cases.
2. Port the rest, designing the artifact / setup piece against `diff` and
   `apply-changes`.
3. Retire the typed commands in `src/pbfhogg/commands.rs`; results rows keep
   pairing because benchmark names match today's command IDs.
4. Elivagar and nidhogg next, where they fit.

## Separate from all four: a negative answer that is itself the bug

bz 65 point 2. `check-refs` on an honest dataset should exit 0; a regression
that makes it report false violations exits 1 and is stored as a completed
run with a verdict. The same for a diff of two inputs expected to be
identical. Whether "no" is wrong depends on the input, not the binary, so no
contract mechanism catches it. It needs brokkr to know expected verdicts
(a primary or repack snapshot checks clean, a `degrade --corrupt-*` snapshot
has violations, two different snapshot keys differ), store a run when the
exit matches and fail it when it does not, an exit 0 where violations were
expected included. Some of that is not recorded yet, such as which
`degrade` flags produced a snapshot. Its own ticket.

Landed for bz 71 as declarations, not inference: an input declares its
answer in brokkr.toml and only that answer completes a run (pbfhogg.md,
Declared answers). Three of the proposed defaults did not survive checking:
a primary is not known clean without a key saying so (assuming it would
change what existing configs mean), two snapshot keys do not imply differ
(a repack of `base` is the same), and brokkr's `degrade` has no
`--corrupt-*` flag, while `--unsort` violates ID order only on inputs large
enough. Only `repack --as-snapshot` derives a declaration, `same` against
its source.

## Exit 3

Agreed with the pbfhogg dev on bz 65: exit 3 (the host refused
`--direct-io` or `--io-uring`) stays a failure but gets a named message,
"feature unavailable on this host", so it is not read as a code bug. No row
is stored. Independent of which approach is chosen.

## Where this leaves bz 65 point 1

Under every approach, commits before the exit split must treat exit 1 as a
failure. Until a mechanism exists, the v0.5.0 baselines need a stopgap (an
approach-1 gate keyed on whether the commit contains the split, by ancestry
of `d2d8836a` rather than by version number) or a rule not to trust their
exit-1 rows.

## Open questions

- Which projects actually need any of this: a survey of what each project's
  binary contract with brokkr is, and whether it is driven across commits.
- Approach 3 or 4 as the long-run direction; 4 subsumes 3 for every commit
  after adoption and needs 3 only for the backfill.
- For approach 4: the setup-chain model, the CLI naming, and how much of
  verify ever moves.
- Whether the pbfhogg side agrees to own a chain or definitions file and the
  test that guards it.
