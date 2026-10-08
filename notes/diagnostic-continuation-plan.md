# Diagnostic continuation after a kill (item 3)

Transient plan, agreed with codex. Builds on item 2 (commit 8000e01, see
notes/coverage-accounting-plan.md). Delete when landed; the durable description
goes into docs/commands/check.md.

## The request

After a watchdog kill (or any termination), name the interrupted and
unobserved tests and print the exact command to rerun them. No automatic rerun;
a failed run never turns green. Motivating case: a downstream merged its
integration tests into one binary per crate, so one hang on its in-process
libtest detector lane - a serial lane under a PARTIAL profile, the bare
`brokkr check` - hides a whole crate's results.

## Design

### 1. Execution inventory for every enumerable lane, not only complete profiles

Two separate preparation products:

- **Execution inventory**: the selected executable tests, binary attribution,
  launch recipe, and journaled observations. Prepared for EVERY enumerable lane
  of every `check` run (serial, parallel, isolated, nextest) and for
  `brokkr test`, before its first test executes. The whole selected invocation
  is prepared up front, so a timeout can name tests in later binaries and later
  lanes.
- **Policy universe**: every test of each required shape, exclusions, filter
  liveness. Only under a complete profile, as today.

Separate the concepts in code: today `prepare.rs` bundles hashing and
attribution requirements into "complete", and `phase.rs` derives
complete-ness from `prepared.is_some()`. Preparing a lane must not switch on
certification restrictions. A partial profile keeps the execution modes a
complete one refuses (cargo-mediated parallelism, serial with no shim); for
those lanes the inventory is reported as unavailable, with whatever
interrupted names the stream can defensibly attribute, and says so. Doctests
stay outside the enumerable inventory.

The journal is written for every run that has an inventory, not only complete
ones (today `accounting.rs` discards records when no journal is open).

### 2. Persist a replay recipe

The persisted plan stores inventory and artifact identity but not how to
reproduce a launch. Add a versioned replay recipe per lane and resolution:

- resolved cargo build args and support-build recipes (`build_packages`);
- per-binary cwd and the recorded environment additions (the `DirectRuntime`
  envelope plus the sweep env, `BROKKR_TEST_BIN_DIR` etc.);
- runtime and support fingerprints;
- harness args, effective thread policy, execution model (serial shared
  process, parallel, isolated, nextest), include-ignored mode.

Never resolve the shape from today's `brokkr.toml` at replay time.

Environment boundary (decided): replay applies the recorded envelope over the
current ambient environment. It is NOT a full environment snapshot, and the
report says so. A fresh hold capability and orphan-reap token are always minted
for the new invocation.

### 3. `brokkr test --from-run <run-id>`

- Conflicts with every ordinary selection, feature, profile, filter, `-p`,
  `--sweep`, `-N` and `--timeout` flag (clap-level).
- Selection: exactly the original executions whose reconciled outcome is
  `interrupted` or `unobserved`. Failed, timed-out, passed and ignored stand as
  earlier results and are excluded. No dedup by name or pair: the same test
  selected by two lanes is two executions. Group launches by lane, resolution,
  binary.
- Artifacts: rebuild with the recorded shape and support-build recipe, verify
  every executable, runtime and support fingerprint against the original
  record, and refuse on drift before executing anything in that resolution.
  Changed source is a new experiment for an ordinary invocation.
- Execution model preserved: a serial shared-process lane's unresolved subset
  runs together in ONE harness process per binary with the recorded thread
  policy - never auto-isolated, which would hide the interaction being chased.
  Parallel, isolated and nextest lanes replay their recorded policy.
- libtest selection: full positive names with `--exact`; drop every original
  selection predicate including all `--skip` after resolving the selection to
  names; keep execution options (ignored mode, threads, json format). Never
  append `--exact` to the original argv. Zero selected names never launches (an
  empty positive filter runs the whole harness). If the argv would exceed the
  system limit, refuse with a clear message rather than splitting a shared
  process (splitting changes semantics).
- `--from-run <id> --list` prints the continuation report from the persisted
  evidence and executes nothing - the recovery path when a hard exit prevented
  the original run's final printing.
- Each continuation writes its own immutable record (`source_run_id`, the
  selected original `ExecutionId`s, its own observations). It never appends to
  or edits the source journal, never changes the source verdict, never
  certifies. A continuation whose selected executions all pass exits zero and
  reports `diagnostic_completed`, not success of the original.
- Retention: today only the newest ten accounting runs are kept. A continuation
  must not prune its own source before loading it, and a pruned source fails
  with a message saying the run is gone. Raise or rethink the bound so a printed
  command stays usable for a reasonable while (document it).

### 4. Output

After the failure verdict, before the `--json` trailer:

```
diagnostic continuation: 3 unresolved executions

default / crate-a / test:integration
  interrupted  detector::alpha       phase deadline
  unobserved   detector::beta        phase deadline
  unobserved   detector::gamma       phase deadline

rerun these recorded executions:
  brokkr test --from-run 123456-2
```

Every unresolved name, no cap, grouped lane -> resolution -> binary, with
outcome and detail; later lanes skipped by fail-fast or deadline included and
distinguished from the binary actually killed. Reconciliation stays where it
is; the continuation report rides the run report and is rendered on the final
reporting path.

`--json` schema 2 gains a `diagnostic_continuation` object: source run id;
inventory availability and completeness; every candidate's full execution
identity, outcome and detail; replay availability and concrete refusal
reasons; the command as structured argv and cwd plus a shell-quoted display
string; an explicit statement that it is diagnostic and certifies nothing.
`policy_coverage` stays null for non-certifying runs; if `execution_accounting`
is populated outside complete profiles its inventory scope must be explicit.

### 5. What it must not claim (say these in the report and docs)

- timed out: exceeded its attributed budget; not proven the cause of a hang;
- interrupted: a start observed without an acceptable terminal;
- unobserved: no start or terminal recorded - it may have run if records were
  lost; missing evidence is not proof of non-execution;
- continuation passed: the selected executions passed in a NEW process against
  CURRENT external state; the original run stays failed. Replay restores
  neither process history (globals earlier tests initialised) nor external state
  the killed tests left (files, services, locks).

### Validation cases (each needs a test)

Partial-profile serial lane killed mid-run names its unobserved tests and
prints a command; a lane with no attribution reports inventory unavailable
instead of inventing names; the same test in two lanes is two candidates;
`--from-run` refuses on artifact drift; `--from-run` builds argv with full
`--exact` names and no original `--skip`; an empty selection never launches;
`--list` executes nothing; the source journal and verdict are untouched by a
continuation; a pruned source run gives a clear error; flags conflicting with
`--from-run` are refused by clap.
