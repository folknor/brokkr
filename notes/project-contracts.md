# Keeping brokkr and its projects in step across versions

Design discussion, 2026-10-30. It records the problem, what triggered it,
and the ways it could be solved. As of 2026-11-01 the leaning direction is
approach 4; its design (the pbfhogg dev's proposal and its gaps, below) is
open.

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

bz 65 is the one these approaches mostly decide.

A later one, the same shape:

- **bz 73**: pbfhogg 587bfc12 made the text diff one streaming engine that
  writes no scratch and refuses `--tmp-dir` with exit 64. brokkr passed
  `--tmp-dir` to every text diff, so verify diff, derive-changes, renumber
  and the diff bench failed at startup. Fixed against HEAD only: brokkr no
  longer declares scratch for a text diff, so a baseline from before
  587bfc12 now drops its text-diff scratch wherever pbfhogg defaults it,
  outside the per-launch private directory. A migration step
  (approach 3) or a definition read from the baseline's tree (approach 4)
  would have kept both sides right.

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
  piners. sluggrs and dellingr are driven by `--commit` too; litehtml-rs
  is not driven across commits today.

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
ratatoskr declares its gates and sync scripts in its own `brokkr.toml` and
tree, but brokkr reads them from the working tree, never at the commit
under test, so it is a precedent for project-owned declarations, not for
the deciding property below (see the survey).

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
failure. The stopgap landed as an approach-1 gate, keyed neither on version
nor on ancestry but on behaviour: `PbfhoggBinary::splits_answer_from_failure`
asks the built executable how it exits on a missing input (1 before the
split, 2 after) and caches the answer under the executable digest. The same
probe-the-binary pattern decides `--overwrite`, `--staging-dir` and
`--tmp-dir`. It cannot help where the flag still parses but a later check
refuses it, which is how bz 73 slipped through.

## Approach 4: the pbfhogg dev's design (2026-11-01)

Drawn from every project's brokkr.toml, not pbfhogg's alone.

- **Two halves, two versioning rules.** Definitions (argv templates, input
  roles, outputs, exit meanings) live in the project tree and are read at
  the commit under test (`git show X:brokkr/benches.toml`). Artifacts (files,
  digests, which datasets a host has) stay in the host sections of
  brokkr.toml, always read from the working tree.
- **Kinds are the seam** (`pbf`, `osc`, `pmtiles`, ...): few, boring, never
  renamed, since an old definition names them.
- **Artifacts generalize `datasets.<name>.<kind>.<key>`:** a file, a digest
  and free attributes (`seq`) brokkr never interprets. Snapshots stay
  alternative artifact sets; dataset params (`bbox`, `getid_ids`,
  `data_dir`) are plain values; host resources (elivagar's ocean list,
  nidhogg's port, ratatoskr's mock server) are artifacts or values outside a
  dataset.
- **Definitions bind roles, not positions.** An input is a selector (kind,
  key, snapshot, attribute constraints such as `after = "base.seq"`) with a
  cardinality (`count = "1.."`, spread as `{changes...}`). Today's
  `--variant`, `--snapshot`, `--osc-seq` and `--osc-range` become one
  `--input role=selector`, with per-bench aliases.
- **A dumb argv template:** a token array, never a shell string; a nested
  array is an optional group dropped when any placeholder in it is unbound;
  no conditionals, no expressions.
- **Typed outputs**, so brokkr reserves, cleans and hashes them, and
  promotion (`--as-snapshot`) becomes generic: any output can be registered
  as an artifact of its kind.
- **Derived inputs** (`from = "<bench>"`), cached by the producer's
  definition hash plus its resolved input hashes. Default a pinned producer
  (one build makes it, both A/B arms consume it), `producer = "same"` when
  the setup step is itself under test.
- **Answers:** `answers` maps exit codes to verdict names; host config
  declares `expect.<bench> = "<verdict>"` on an artifact.
- **Row identity:** bench name, definition hash, input content hashes, bound
  params. `--compare` across differing definition hashes refuses unless
  forced.
- **Out of v1:** verify (an oracle comparison, stays typed), the protocol
  harnesses (library, ratatoskr gates, piners corpus), suites.
- **CLI:** `brokkr <bench>` resolved dynamically from HEAD's definitions.
- **Settle first:** relational selectors limited to equality and ordering
  against other roles; the pinned-producer default; the derived-input model,
  designed against `diff` and `merge-changes`.

## Approach 4: gaps in the design so far

- **The derived-input cache key needs the producer's executable digest.**
  Definition hash plus input hashes is the same for two builds of one
  definition, so HEAD's merge and a baseline's would share a cache entry.
  The pinned producer also needs a name: which build, and whose definition
  (HEAD's tree or the baseline's) makes it.
- **Pinned producer reverses today's rule for declared diffs.** Today a
  declared `diff` rebuilds its merged side with the build under test,
  because the expectation is "this file against itself with that OSC
  applied, by this build". Under a pinned producer the expectation says
  something about the producer instead. Probably right for A/B, but it
  changes what `expect.diff` means, and existing configs carry it.
- **Expectations over several inputs.** `expect.<bench>` on one artifact
  fits the checks. `diff` today is keyed by OSC seq on the base and
  `diff-snapshots` by an unordered digest pair, so a multi-input
  expectation has to be keyed by the other roles' content.
- **The definitions format upgrades as a migration chain (decided).** Any
  old commit can be a baseline, so brokkr reads every `benches.toml`
  format ever committed. It works like SQL migrations built into an
  application: the file carries `format = N`; brokkr parses it with the
  frozen reader for N and runs it through the upgrade steps N to N+1 to
  ... to the current format, and only the current shape reaches the
  runner. Consequences:
  - A format's reader and its upgrade step are frozen once released; a
    fix is a new step, never an edit.
  - Every step is total: a field a later format adds gets the value that
    reproduces the old behaviour, so an upgraded definition invokes the
    old binary exactly as its own commit meant.
  - The definition hash is taken after upgrading, so a format bump alone
    never splits results rows.
  - Each format keeps a fixture file, upgraded to the head format and
    compared against a snapshot in brokkr's tests.
  - Absent `format` is the first format, so it must be decided before the
    first commit adopts the file.
  - This is the contract-chain idea of approach 3, applied to brokkr's own
    definition format rather than the project's CLI.
- **Hash the resolved definition, not the text.** A comment, reordering or a
  new optional group left unbound should not split rows; the resolved argv
  with paths replaced by role names, plus the answers table, is the
  identity that matters.
- **Scratch and staging belong in the definition.** bz 73 was a scratch
  declaration that went stale; a `{scratch}` placeholder (and the staging
  directory) declared per bench retires the binary probes for every
  post-adoption commit.
- **Pre-adoption commits.** The design does not say. A commit whose tree has
  no definitions file could be served from today's typed layer, frozen at
  adoption with its binary probes. That layer is already what every
  baseline runs against, so freezing loses nothing, and "has the file"
  replaces ancestry. With the format chain this could become format 0: a
  built-in definitions file shipped inside brokkr for commits without one,
  upgraded like any other, so the runner has one path. The binary probes
  would then be a format-0 feature the head format no longer needs.
- **Dynamic subcommands collide with brokkr's own.** A bench named
  `check`, `clean`, `run`, `build` or `results` must be refused at load,
  built-ins winning. A bench that exists only at an older commit cannot be
  reached by name.
- **Measurement modes are an instrument axis, not bench entries.** A run is
  benchmark x instrument x inputs; `--hotpath` and `--alloc` change the
  build and the environment, never the argv, so a twin bench per mode would
  duplicate every argv. The project declares instruments in the same
  versioned file (`[instrument.hotpath] features = ["hotpath"]`); the
  default is none, and `--bench` needs no declaration. Today brokkr
  hard-codes the feature per mode (`ProfiledMode::feature`) and reads the
  report from `HOTPATH_OUTPUT_PATH` as JSON, with `HOTPATH_OUTPUT_FORMAT`
  and `HOTPATH_METRICS_SERVER_OFF` set, so the report already goes to a file
  and never reaches stdout under brokkr; the stdout banner in pbfhogg's
  brokkr.toml comment is what an instrumented build prints without those
  variables, in the test sweeps. The env names and report format are the
  hotpath crate's contract, versioned by the commit's `Cargo.lock`, not
  pbfhogg's; an instrument names the report kind (`hotpath`) and brokkr
  owns the parser per hotpath version. A feature may need a package
  (piners builds hotpath on its harness crate).
- **Some params carry build features too.** `--direct-io` and `--io-uring`
  change the argv and also add a cargo feature (`resolve_io_features`), and
  `--io-uring` runs a host preflight. A param needs an optional feature
  list in the definition; the preflight is host-side and stays brokkr's.
- **Results history keeps reading.** Rows stored before adoption have no
  definition hash; they need a pairing rule with rows after it, or a
  boundary `--compare` names.
- **Ordering constraints need a type.** `after = "base.seq"` compares
  numbers, so an attribute used in an ordering is an integer, and brokkr
  interprets it that far.
- **Exit 64 is a definition mismatch.** The runner can say "this commit's
  definition of X does not match its binary" instead of a plain failure.
- **The project-side test has to run, not only parse.** Checking each
  expanded argv against the clap tree catches bz 67, not bz 73. Running each
  definition once on a tiny fixture catches both.

## Approach 4: the definitions syntax

Undecided between a TOML data file and a script that generates the same
data. A project changing what a bench means is the project's business, not
an argument against scripts: brokkr only needs every run to say what it
ran.

- **The contract is the versioned data structure either way.** The
  migration chain runs over the definitions structure (`format = N`), not
  over the syntax that produced it. A script would be a generator whose
  result is that structure; brokkr never calls into it during a run.
  That makes the choice reversible: start with one front end, add the
  other later, both emitting the same structure.
- **Identity is taken on the evaluated result**, so a changed meaning still
  splits rows and makes `--compare` refuse, whichever front end wrote it.
- **For TOML:** no interpreter dependency, nothing to keep callable for old
  commits beyond the format readers, and the pbfhogg dev's sketch already
  fits it. Made comfortable with `deny_unknown_fields`, a `brokkr benches`
  command that prints each bench's expanded argv for given inputs, and a
  generated JSON Schema for editor completion (taplo reads one for TOML).
- **For a script:** repetition goes away (benches sharing argv patterns,
  axes, instruments), and a project can compute its definitions instead of
  copying them.
- **What a script would cost:**
  - Evaluation must be hermetic and deterministic (no filesystem, network,
    environment, clock or randomness, nothing from the built binary), or
    one commit evaluates differently on two hosts, its identity drifts, and
    help and dry runs at HEAD need a build.
  - The builtins a script calls become a second frozen surface, callable
    for every old commit, like the format readers. Kept to constructors of
    the data structure, with no run-time callbacks, it stays small.
  - If chosen, Starlark over Lua: hermetic and deterministic by design
    (no I/O, insertion-ordered dicts), with a maintained Rust
    implementation (`starlark-rust`, used by Buck2). Lua needs sandboxing by
    hand, and its `pairs` order is unspecified, which breaks a hash taken
    on the result.
- **Leaning:** TOML first, since the choice is reversible and nothing yet
  shows the repetition a script would remove; a Starlark front end later if
  real definitions show it.
- **Considered and set aside:** the binary emitting its own definitions
  (`pbfhogg benches --json`, as the library harness's describe does). It
  ties the definition to the binary exactly, but needs a build before help
  or a dry run can resolve a name.

## Approach 4: answers (2026-11-01)

The pbfhogg dev's positions on the gaps above and the open questions,
agreed unless marked. Where an answer here and a gap above disagree, this
section wins.

### Decided

- **`format` is required.** No file exists yet, so a missing key is a load
  error, not version 1. Format 1 may ship with known gaps; format 2 is an
  ordinary upgrade.
- **`expect.diff` keeps its meaning.** It reads "this base, diffed against
  itself with this OSC applied, gives this answer", a property of the data,
  under a pinned producer and under `producer = "same"` alike. Today's
  behaviour, the merged side rebuilt by the build under test, is exactly
  `producer = "same"`; pinning changes which build makes the merged side,
  not what the declaration asserts. A contradicted declaration can now be
  either the diff or the producer at fault, so its failure names both
  builds.

### Proposed, to write up as the design

- **Before adoption:**
  - Benchmarks take today's command IDs as names, so existing rows pair
    without migration. brokkr owns a reserved-name list and refuses a
    definitions file declaring one when HEAD's file loads. A rename carries
    `was = ["old-name"]`, so pairing survives it; a `was` naming a live
    bench is refused.
  - Pre-adoption commits are format 0, a file brokkr embeds, generated once
    from today's typed layer and upgraded like any other. Format 0 alone may
    carry variant blocks keyed by ancestry (`until = "<sha>"`) for the shapes
    pbfhogg had (before and after the exit split, short flags, `--tmp-dir`),
    and today's binary probes may stay as one way it picks a variant.
  - The project owns fixtures, brokkr owns the runner: the definitions file
    declares a fixture artifact set and a command that builds it in scratch,
    `brokkr benches test` resolves and runs every definition against it with
    the production resolver, and the project wires that in as a
    `[[script_check]]`.
- **Derived inputs:**
  - The producer defaults to HEAD's build with HEAD's definition, so both
    arms get byte-identical input. `producer = "same"` (the arm's own build)
    is the opt-out when the setup step is under test. An output an old
    baseline cannot read fails that arm; the remedy is the opt-out, never a
    silent fallback.
  - Cache key: the producer's executable digest, its resolved definition
    hash, its input content hashes and bound params. The output's content
    hash is recorded after production, and consumer rows pair on that, so
    two builds emitting identical bytes pair as one input.
  - Expectations over several inputs are `[[expect]]` entries: a bench, a
    role-to-input map and an answer, with `symmetric = true` on a bench
    whose input order does not matter. A derived role is keyed by its recipe
    (producer and its inputs, which is what the OSC seq in `expect.diff`
    is), never its output hash.
  - Producers in another repo are not derived inputs. elivagar promotes its
    output into a shared artifact with provenance attributes (producer
    project, commit, executable digest), and nidhogg consumes it as a plain
    artifact.
- **Settled in principle:**
  - Selectors: equality between roles' attributes, ordering on integer
    attributes only.
  - Instruments: a feature may name a crate, defaulting to the bench's
    package. The hotpath parser is keyed on the report's own version, not
    inferred from the crate version.
  - A param may declare `features = [...]`, applied when it is bound; it
    joins the build variant as an instrument's do. `--direct-io` and
    `--io-uring` become a param plus a feature, no special case.
  - Rows stored before adoption get an identity by resolving format 0
    against their stored argv where that resolves; the rest are marked
    legacy identity, and `--compare` on them warns.
  - Scratch is declared per bench (`scratch = true` binds `{scratch}`); bz
    73 would have been a one-line definition change at 587bfc12.
- **Scope:**
  - A definition takes optional `package` and `bin`, defaulting to the
    project's `[bin]`.
  - Server plus client is out of v1. Later: services, long-running
    processes with a readiness probe, started before the bench and stopped
    after (nidhogg `serve`, ratatoskr's mock server).
  - Suites become presets in the same file in a later format; the typed
    layer retires only after that.
  - Tilegen is both: the ocean shapefiles are host artifacts, which ones a
    run uses is an input role in the definition, and resolved inputs in row
    identity cover what the host-only design was for.
  - Definitions declare the brokkr-owned surfaces they rely on
    (`requires = { markers = 1, env = 1 }`), so a FIFO protocol change
    becomes a refusal instead of silently lost markers.
  - verify stays typed, probably for good: it is oracle logic, not a
    project contract.

### Not yet resolved

- **Where promotion writes.** The proposal: never into a hand-written file,
  only into one brokkr-generated include (`brokkr.artifacts.toml`) that
  nothing else edits. Today registration edits the file that defines the
  dataset, and a dataset table split across two files is refused (`brokkr
  man config include`), so a generated file adding a snapshot to a
  hand-defined dataset is refused as things stand. Either the generated
  file owns whole datasets, or the split rule relaxes for it, which is a
  config-contract change. It also has to be included by every project
  sharing the datasets.
- **"HEAD's build" when neither arm is HEAD.** An A/B of two `--commit`
  values needs a rule for which build makes a pinned input: the working
  tree, or one arm named explicitly.
- **The hotpath report version.** hotpath 0.28.7 and later put a top-level
  `version` in the JSON report; 0.21.5 and 0.22.0 do not. Keying the parser
  on it needs a shape-sniffing fallback for unversioned reports, which is
  what brokkr's parser does today for all of them. Four hotpath versions
  are in use across the projects at once, and one `Cargo.lock` can hold
  three, so the lock cannot settle it.
- **Config contract approvals still needed** before building: datasets
  entries into the generic artifact shape, and moving each project's
  default variant into the definitions.

## Approach 4: the survey (2026-11-01)

Every project brokkr drives was read from both sides: pbfhogg, elivagar,
nidhogg, ratatoskr, piners, dellingr, litehtml-rs, mogwai, sluggrs and
broadarrow. Read-only; nothing was run. Bugs it found are listed apart at
the end, since they are fixable today whatever is decided here.

### Three kinds of data, not two

The design splits definitions (read at the commit) from host data (read
from the working tree). Four projects show a third kind.

1. **Surface facts that move with the code:** argv shape, package, bin or
   example, cargo profile and features, env protocol, exit meanings, the
   input and output protocol versions the binary accepts. These belong in
   the versioned file. Today every project reads them from the working
   tree, and that is where most cross-commit breakage comes from (below).
2. **Pinned in-tree inputs and their registries:** dellingr workloads,
   ratatoskr sync scripts and fixtures, piners' corpus and `pins.toml`,
   litehtml fixtures. Not host data, but read from the working tree on
   purpose, so both arms see the same input; dellingr calls it the
   split-tree rule. They need a registry of their own, with content
   digests always in row identity (dellingr pins `xxh128`; ratatoskr's gate
   checks a script's path, not its content).
3. **Judgments:** ratatoskr gate rules and host baselines, piners'
   expected, label and performance pins, litehtml approvals, pbfhogg's
   declared answers. All working tree, all evaluated by brokkr, and much
   richer than an exit-code `answers` table: absolute, baseline-relative,
   equal across arms (dellingr's cost fingerprints), host-keyed.

### Surfaces and benches: the mogwai question

mogwai's registry was named argv in `brokkr.toml`, read from HEAD and
applied to every commit, so a name had to promise comparability; it lived
about a day (mogwai 3853ad6 to 93c4a9d). Read at the commit and hashed,
a name cannot lie that way: `was` records lineage, the hash decides
comparability. mogwai's workload space is still genuinely open, and it
pays the drift cost knowingly (`gen --warmup` renamed `--burn-in` at
d3dabc8 with no alias; its performance reference accepts back-fill only
"to any commit whose CLI still parses the invocation").

So the definitions file has two levels:

- **`[surface.NAME]`:** what to build and run (bin or example, package,
  build features, which instruments apply, exit table and usage exit,
  scratch, timing source, which tree the child runs in, `requires`). The
  argv comes from the call site and is captured verbatim. Identity:
  surface name, resolved surface hash, captured argv, instrument, bound
  input hashes. mogwai, dellingr, sluggrs, ratatoskr, piners and litehtml
  each have one or a few surfaces and no argv templates.
- **`[bench.NAME]`:** a surface plus an argv template, input roles, typed
  outputs, derived inputs and expectations. Identity as in the dev's
  design. pbfhogg is shaped like this, elivagar's tilegen partly.

Every project's named registry (workloads, gates, probes, fixtures,
targets) names inputs or judgments, never argv, so it sits beside this,
not inside it. mogwai's doc is also wrong about pbfhogg "registering zero
workloads": brokkr's typed layer is pbfhogg's registry, about 25 named
argv templates, and pbfhogg rows pair on the brokkr argv, a name plus
params.

### Vocabulary the survey adds

- **Build features apart from instrument features.** Some mogwai examples
  carry `required-features = ["hotpath"]` and cannot build bare, so their
  `--bench` walls are instrumented today; sluggrs builds `--bench` bare.
- **Which instruments a surface allows** (mogwai's allocator-wrapping
  probes, `tape_lateness`), so `--alloc` on them is refused instead of
  built.
- **Instrument-dependent inputs:** dellingr picks `hotpath_file` over
  `file` under instrumentation.
- **A declared timing source:** wall, self-reported `elapsed_ms` (sluggrs,
  pbfhogg's `bench-*`), or a marker span (ratatoskr). Today only a marker
  span is stamped in the row.
- **One process, many rows:** pbfhogg's osmpbf and planetiler baselines
  print `---`-separated blocks; elivagar's examples repeat R times
  internally and are timed as one sample.
- **Output protocols with a version and frozen readers,** like the
  definitions format: piners' NDJSON tightened version 1 without a bump,
  so brokkr now rejects some older piners commits' output; also
  summary.json, stderr kv, the FIFO. "Typed outputs" needs protocol and
  version, not only file, dir or none.
- **brokkr-generated input documents** with a version the surface accepts
  (piners' manifest v3 and v4, pbfhogg multi-extract's region config).
- **Dependent roles:** a ratatoskr script's frontmatter selects its
  fixture.
- **Services:** readiness by a stderr line (nidhogg tiles), an HTTP health
  poll (`brokkr serve`) or a readiness file (sæhrimnir); endpoints mapped
  to env; a scope (per bench, reused across iterations, per cohort); the
  service binary's identity in the row. nidhogg is mostly this, so it gets
  little from v1.
- **Compatibility between a pinned input and the subject:** HEAD's
  ratatoskr scripts call a `harness.*` API an old binary may lack;
  dellingr's brokkr.toml carries "never `--bench --commit` older than
  2bcc71e" as a comment.
- **Host-bound params:** elivagar's budgets and thread counts are tuned per
  host but spelled per commit, so the definition declares the param and the
  host binds it, with a rule for a host value whose param the commit lacks.
- **Flags switched by an attribute** (`locations_on_ways` gives
  `--locations-on-ways`), which "drop the group when unbound" cannot say.
- **Env as params** (elivagar's `ELIVAGAR_*` knobs), in identity.
- **Conditional scratch:** merge-changes writes scratch only with several
  inputs and no `--simplify`, and pbfhogg refuses `--tmp-dir` otherwise.
- **Params with a built-in default and a host override,** qualifying
  identity only when they differ (`getid_ids`, `bbox` today, through
  `arm_qualifier`); format 0 has to keep that pairing.
- **Spell defaults in the argv.** pbfhogg 16154b0d changed `digest`'s
  default form, so defaulted rows on both sides pair but measured different
  things; dellingr's harness runs 20 iterations no argv names. A
  definition hash cannot see a binary's defaults.
- **The usage exit is per project:** clap exits 2 (mogwai, dellingr,
  broadarrow), pbfhogg 64. Exit 3 is a verdict in broadarrow's `backtest`
  and "host feature unavailable" in pbfhogg. Exit tables are per surface.
- **Sibling path dependencies are part of the subject.** `--commit` links
  a sibling path dependency to the live checkout (`src/worktree.rs`), so a
  baseline of nidhogg (pbfhogg, elivagar), ratatoskr (bifrost) or
  broadarrow (mogwai, piners) is the old commit against today's siblings,
  possibly dirty, and the row records one commit. Rows need the siblings'
  commits and dirty state, or a refusal.
- **External tools in identity:** planetiler, tilemaker and osmium
  versions are absent from rows, so a bump silently changes what a
  baseline row measured.
- **Stderr kv is an implicit surface for every child:** every measured
  child's stderr is scraped for `key=value` lines, not only the
  self-reporting ones. `requires` should name it.

### Drift the format-0 file has to express

Each driven project has several incompatible shapes in its history.

- **pbfhogg:** the CLI crate split (ed5f0365, 2026-02-25); `bench-*`
  subcommands added (3a2306ee); 22 commands consolidated into 14
  (8f1aad9c, 2026-03-06: `merge` to `apply-changes`, `check-refs` to
  `check --refs`, `merge-pbf` to `cat --dedupe`, and more), which is why a
  bench name must not be the subcommand; `-j` added per command through
  April; `--summary` renamed (b8ac693b); `--index-type dense` removed
  (b70dd8ca); the exit split (854f9465); one flag vocabulary (6e87c0ef);
  OSC diff exit 1 and name-framed OSC output (50a9fc06, 46b48840);
  `--staging-dir` (568b6e3f); usage exit 64 (d2d8836a); `digest --form`
  and its default flip (16154b0d); merge-changes scratch (c97b9985); text
  diff refuses `--tmp-dir` (587bfc12).
- **elivagar:** subcommands added (d3f360c); explicit `--ocean SPEC`
  replacing autodetection and five flags (38250b9, whose message says
  brokkr tilegen fails until it follows); env knobs removed (ab935ff);
  allocator features coming and going; `--dedup-cap` added and
  `--seam-reconcile-layers` removed (0ae55a6, 099611b).
- **nidhogg:** clap replacing hand parsing (9e467ff); the optional disk
  store (fd4b671).
- **dellingr:** the harness took a target name against
  `CARGO_MANIFEST_DIR` before e8799c8 (2026-07-25), so nothing before it
  can be benched; a missing `_bench` exited 0 then and 1 now; FIFO errors
  became fatal.
- **sluggrs:** hotpath features from f619a3e; mandatory `elapsed_ms` from
  29431ea, so every `--bench --commit` before it fails.
- **ratatoskr:** package `app` to `runner` (729eabe4); profile `debug` to
  a custom `harness` profile (5201476f, 2026-10-15).
- **piners:** package `piners-runner` to `piners-harness` (3902a46a1);
  profile `corpus`.
- **mogwai:** `gen --warmup` to `--burn-in` (d3dabc8).

### Where verify, the harnesses and services land

- **verify** stays typed, but it drives pbfhogg across commits heavily and
  keeps exactly the drift exposure approach 4 removes elsewhere (the dense
  index bug below). A middle path: the oracle logic stays typed, and the
  pbfhogg steps it judges come from the commit's definitions by name.
- **piners' parity corpus** stays typed (a generated input, a lifecycle
  protocol, an evaluator with writers, its own store); only its surface,
  argv and accepted manifest versions, is definable.
- **ratatoskr sync** is a bench plus a service plus a marker-span timing
  source; `brokkr service` is a test runner and stays outside.
- **litehtml visual**: the invocation is a bench; the Chrome oracle and
  the comparison stay typed.
- **pbfhogg's library harness** already runs a self-describing role model
  in production (sources, owners, kinds, a versioned protocol): a
  precedent for the role vocabulary and for the binary-emitted form the
  syntax section set aside.
- **broadarrow** benchmarks nothing. If it starts, `ba backtest` is the
  first candidate: answers 0 and 3, a versioned `--report` output, an
  in-tree input and a host data cache.

### Bugs found, fixable today

Checked in code where marked; the rest are the survey's reading.

- **`[cargo_env]` breaks `--commit` to older commits (checked).** The
  command comes from HEAD's config and runs in the old commit's checkout
  (`brokkr man config`, Where it runs). mogwai's
  `scripts/source_identity.py` arrived 2026-10-16, so every earlier
  `--commit` build fails; broadarrow has the same exposure.
- **The build spec is read from the working tree under `--commit`:**
  ratatoskr's `harness` profile and piners' `piners-harness` package
  likely fail at older commits.
- **mogwai's child runs in the live tree under `--commit` (checked):**
  `src/mogwai/cmd.rs` passes `project_root` where sluggrs passes the build
  root, so old mogwai code reads HEAD's relative inputs.
- **`brokkr corpus --alloc` cannot build (checked):** piners-harness has
  `hotpath` but no `hotpath-alloc` feature. `corpus --hotpath` likely
  cannot read its report: the `corpus` binary installs no hotpath guard.
  `BROKKR_HARNESS_ARTEFACT_DIR` and `BROKKR_TEST_BIN_DIR` are set for
  piners but never read, and `piners.md` presents them as contract.
- **litehtml's viewport reaches Chrome only (checked):** the pipeline is
  never passed a width (`src/litehtml/cmd.rs`), so any viewport other than
  800 compares mismatched renders. Latent: config uses 800.
- **verify add-locations-to-ways still runs `--index-type dense`
  (checked)**, which pbfhogg removed in April; the error is read as
  "allocation failure, skipped", so that variant has been skipped since.
- **`ELIVAGAR_NODE_STATS=1` is still set (checked)**; elivagar removed its
  reader (ab935ff).
- **sluggrs' `gpu` target cannot be reached (checked):** the example is
  named `gpu-bench`; `--target gpu` builds `gpu_bench`.
- **pbfhogg flags offered where pbfhogg refuses them:** `--direct-io` and
  `--compression` on every measured command (merge-changes, check,
  inspect, digest and diff refuse some), `--io-uring` on diff-osc,
  tags-filter's `-j` with `-R` or in OSC mode and `-R` with
  `--remove-tags`; each exits 64.
- **merge-changes writes scratch** (since c97b9985) but brokkr declares
  none for it, so its scratch lands outside the per-launch directory.
- **Defaulted `brokkr digest` rows pair across 16154b0d** but measured
  different forms.
- **elivagar tilegen pairing ignores the tilegen block,** so rows from
  different ocean sets or budgets pair; the archive store key
  `(dataset, variant, commit)` has the same gap.
- **`--seam-reconcile-layers` is still emitted when configured** and shown
  in `brokkr.toml.elivagar.md`; elivagar removed it.
- **Results pairing does not stop at `--`:** a child argv token like `-v`
  or `--bench N` after it is stripped from the pairing key
  (`src/run_invocation.rs`). Latent.
- **mogwai's self-reported `elapsed_ms` is dropped** silently;
  mogwai-side comments still describe it as used.
- **ratatoskr's marker timestamps are UNIX-epoch microseconds,** while
  `brokkr man measure` says microseconds since process start; span
  lengths are unaffected.
- **The nidhogg tiles dry-run prints a different argv** from the one that
  runs.
- **The planetiler baseline compiles its Java bench from the project root**
  while the osmpbf baseline follows `--commit`.
- **Stale prose:** `ratatoskr.md` and `service.md` (the `app` crate,
  `test-helpers`, `parent_death_helper` as a bin); `mogwai.md` on pbfhogg
  registering nothing; pbfhogg.md's I/O and scratch command lists and
  `--force-altw` composing with `--inject-prepass`; the `--index-type`
  help; `hotpath.md`'s `--commit v0.3.0` example; sluggrs example
  comments naming `brokkr sluggrs hotpath`.
