# Coverage accounting: policy vs execution (item 2)

Transient implementation plan. Agreed with codex (sessions 01a11cd1, two
rounds). Delete when landed; the durable description goes into
`docs/commands/check.md` ("`coverage` phase") and code comments.

## The request

Split two claims `check`'s coverage audit mixes today:

- **Policy coverage**: every required (shape, test) pair was SELECTED somewhere,
  or legitimately excluded (ignored, quarantined, curated).
- **Execution accounting**: each selected test execution is passed, failed,
  timed_out, interrupted, ignored, or unobserved, taken from the libtest JSON
  events (nextest engine events on nextest lanes).

A timeout is an accounted failure, never coverage passed. Interrupted and
unobserved stay unresolved. Complete accounting after a timeout certifies only
that the failed run's record is complete, never the gate. Item 3 (later) builds
on this: after a watchdog kill, name the interrupted and unobserved tests and
print the exact command to rerun them.

## What is wrong today

- `src/check_cmd/coverage.rs` builds each lane's "ran" set from `--list` under
  the lane's filter for lanes the test phase *reached* (`executed` mask set in
  `phase.rs` before `run_test_lane`, so it means reached, not executed), and it
  enumerates AFTER the test phase.
- `phase.rs::audit_coverage` keys on the `"tests failed"` error string; any
  other error (a timeout) leaves `stats: None`. After a watchdog fires the
  shutdown flag refuses every new spawn, so post-test enumeration cannot run at
  all.
- Pair identity is conditional (`binary_unit`, `nextest_keyed`): package name
  for libtest-only shapes, nextest binary id when a nextest lane shares the
  shape. Two binaries of one package with the same test path merge into one
  pair.
- `TestTracker` keeps only in-flight and untyped completed names; failures come
  back by parsing reconstructed text. The serial lane's harness shim gives
  per-harness trackers but records no binary identity
  (`harness_shim.rs::accept_one`, `Session::totals` flattens names).
- Every lane drops its observations on the timeout and interrupt paths
  (`streaming_run_libtest` and `run_libtest_parallel` return `Interrupted` before
  extracting the tracker; `run_parallel_sweep` discards joined runs under
  shutdown; isolate and serial convert timeouts to `Verify`; the nextest lane
  keeps only aggregate `RunFinished` stats and `try_execute(...)?` loses events).

## Agreed design

### Identities

- `BinaryUnit` = (package id, normalized target kind, target name). Kind
  normalization as `selector_kind_of` (`lib` for every library flavour). The
  executable path is launch metadata, not identity.
- `PairId` = (build shape key, resolution `Option<package>`, `BinaryUnit`, test
  name). Policy works on pairs. No conditional package keying anywhere; nextest
  binary ids are mapped onto `BinaryUnit` through the artifact index.
- `ExecutionId` = (lane invocation, harness invocation, attempt, `PairId`).
  Accounting works on expected executions. The same pair selected by two sweeps
  is two expected executions.

### Plan before execution

- Before the test phase, prepare the WHOLE profile: for every active sweep and
  resolution, prebuild (the same `cargo test --no-run` the lane uses) and list
  every binary, producing:
  - per shape+resolution: universe, ignored subset;
  - per lane: its selected pairs and the expected executions they imply
    (ignored excluded unless the lane includes ignored; filtered-out and
    benchmarks are outside the claim);
  - exclusion reasons, filter-liveness findings (today's `FilterLedger`);
  - launch metadata per binary (executable path, content hash, envelope);
  - explicit markers for anything that could not be prepared.
- Listings run under the SAME envelope execution will use
  (`DirectRuntime::envelope`), not `binary_list`'s smaller loader-path-only env.
- Preparation is its own phase with its own ceiling (new phase name
  `prepare`; pick a ceiling equal to the test phase's 15 minutes, document it),
  inside the whole-run ceiling. A watchdog or failure during preparation yields
  an explicitly incomplete plan (`plan_complete = false`).
- Artifact stability: target dirs stay as today. Before a lane executes, its
  shape is prebuilt again (a cached no-op normally, which also re-uplifts
  support bins) and the resulting test executables are checked against the plan
  by path AND content hash (xxh3 of the file). Any difference is a hard error for
  that lane ("artifacts changed since the plan"), never a silent re-plan.
- The executor CONSUMES the prepared selection; it does not re-enumerate. For
  the nextest lane, keep and execute the engine `TestList` built during
  preparation rather than building a second one.
- The plan is persisted to disk (under the state dir, `.brokkr/accounting/`,
  one run id per invocation) before the first test executes; observations are
  appended to a journal file incrementally (one JSON line per observation, flush
  each). A truncated journal (no closing record) is detectable and reported as
  such. This survives `watchdog::backstop_exit`.

### Policy

- `policy = classify(universe, CONFIGURED selections, exemptions)` - from the
  plan, not from the reached mask. A lane skipped by fail-fast shows as
  unobserved executions, not orphaned pairs. This deliberately changes failed-run
  policy classification; document it.
- Incomplete plan: policy status `incomplete`, known findings kept, never a
  profile-wide pass.
- Orphan/stale/dead-filter rules otherwise unchanged.

### Observations and outcomes

Typed observations from the runner, per attributed harness stream: started,
terminal (ok / failed / ignored), with anomalies. Outcomes per expected
execution:

| Outcome | Evidence |
|---|---|
| passed | terminal ok for that execution |
| failed | terminal failed, or attributable execution failure (spawn error, crash of a one-test process) |
| timed_out | this execution exceeded its own deadline, attributed (one-test process, or the per-test cap naming a watched in-flight test) |
| interrupted | started, no terminal, killed by a termination it did not cause (phase deadline, sibling timeout, fail-fast, interrupt) |
| ignored | explicit ignored result / engine ignored disposition |
| unobserved | expected, no start and no terminal |

Each carries detail: `assertion`, `signal`, `spawn_error`, `per_test_deadline`,
`phase_deadline`, `sibling_timeout`, `fail_fast`, `stream_truncated`,
`read_error`, `interrupt`.

Termination rules:
- phase deadline: run failure; active executions interrupted, queued unobserved;
- per-test timeout: the attributed execution timed_out, siblings interrupted;
- idle / no-completion / wall with no defensible identity: run-level timeout,
  affected executions unresolved; never invent a timed-out test from a suspect;
- termination is a structured recorded event (scope + cause), not a string;
  parallel sibling cancellation must be recorded as such, not inferred from a
  signal status.

Protocol transitions within one suite (each an anomaly on the execution, all
fail the green invariant): duplicate start; start after terminal; repeated
terminal (even two identical `ok`); terminal for a name absent from the plan;
overlapping suite start; missing suite closure; suite totals inconsistent with
the individual outcomes. A terminal without a start still records its outcome,
with an anomaly. Never silently deduplicate. `test/timeout` events (recognised
but unhandled today in `observe_event`) must be handled or flagged, not dropped.
Synthesized output (`harness_shim::close_unfinished_suite`) is display only and
never evidence.

Pipe EOF, read error and forced drain cancellation must be distinguishable
(`read_unless_cancelled` returns `None` for all three today).

### Green invariant (accounting status `complete` on a passing run)

Every expected execution has an acceptable terminal outcome (passed), no
protocol or attribution anomaly, plan complete. `interrupted` fails it; `ignored`
under include-ignored fails it (the lane promised to run it). A green test phase
that violates it is a hard failure of the check. Exit codes unchanged: watchdog
124, interrupt 130, ordinary failure nonzero.

### Attribution per lane

- Serial libtest via cargo: harness shim mandatory under a complete profile. The
  runner argv carries the target executable (cargo runs `RUNNER <exe> <args>`),
  so the shim sends that path in its handshake BEFORE exec; the parent resolves
  it against THIS lane+resolution's artifact index (a path from another lane
  does not satisfy it). Roles: `harness` (must resolve to a planned binary) and
  `rustdoc` (must match the planned doctest carrier) - unknown paths are an
  attribution error only for the harness role. Under a complete profile a
  handshake failure, unknown binary or missing accepted stream is an attribution
  error recorded in the parent; the shim fails closed instead of exec'ing
  unisolated. Non-complete runs keep today's permissive fallback.
- Where the shim cannot run (`serial_shim_fallback`: configured runner, cross
  target, custom rustdoc) a complete profile refuses certification during
  preparation, with the reason.
- Cargo-mediated parallel on the serial lane (`test_threads` 0 or > 1, which
  takes the parallel runner and disables the shim): a complete profile refuses
  it during preparation, pointing at `parallel = { budget = N }` (the direct
  lane), which is attributable.
- Parallel and isolated lanes: attribution is the executable they launch.
- Nextest lane: capture typed engine events (starts, finishes, retries, skips,
  cancellation) before calling the reporter; keep the report even when the
  reporter or callback fails; distinguish cancellation-induced deaths from
  spontaneous failures; any engine timeout prevents certification even where the
  engine config could mark it passing.

### Doctests

Own block: observed outcomes, carrier completion, inventory unavailable,
accounting unknown. Neither satisfies nor fails binary accounting, but a failing
or interrupted carrier still fails the test verdict. `execution_accounting` is
explicitly scoped to enumerable binary tests. Doc-only structural carrier rule
unchanged.

### Reports

Every lane returns a `LaneReport { observations, termination, verdict }` on
every return path (failure, timeout, interrupt included). `audit_coverage` stops
branching on an error string; it reconciles plan + journal purely, never
spawning, never building, never rearming a deadline.

### Summary schema 2

`CheckSummary` bumps `schema` to 2. Separate objects:

```json
{
  "schema": 2,
  "verdict": "failed",
  "termination": { "kind": "per_test_timeout", "scope": "..." },
  "policy_coverage": {
    "status": "passed | failed | incomplete",
    "plan_complete": true,
    "pairs": 0, "selected": 0, "ignored": 0, "quarantined": 0,
    "curated": 0, "orphaned": 0, "dead_filters": 0
  },
  "execution_accounting": {
    "scope": "binary_tests",
    "status": "complete | incomplete | violated",
    "expected_executions": 0,
    "passed": 0, "failed": 0, "timed_out": 0, "interrupted": 0,
    "ignored": 0, "unobserved": 0, "anomalies": 0
  },
  "doctests": { "inventory": "unavailable", "accounting": "unknown", "observed": {} }
}
```

Counts partition expected executions. Unknown inventory is reported as
unknown, never as zero.

### Module ownership

- `src/check_cmd/accounting.rs` (new): identities, plan, journal, outcome and
  anomaly types, reconciliation, schema-2 serialisation types.
- `src/test_runner.rs`, `src/test_runner/harness_shim.rs`: a small neutral
  observation contract (they must not know `ResolvedSweep` or policy), typed
  events, distinguishable stream ends, handshake with role + executable,
  fail-closed mode, structured termination.
- Lanes: `output.rs` (serial), `parallel.rs`, `isolate.rs`, `nextest_lane.rs`,
  `nextest.rs`: consume prepared selections, return `LaneReport`s.
- `coverage.rs`: enumeration moves into preparation; classification reads the
  plan; `binary_unit`/`nextest_keyed` go.
- `phase.rs`: preparation phase before tests, plan persistence, journal wiring,
  `audit_coverage` as pure reconciliation, schema 2 in `CheckSummary`.
- `watchdog.rs`: phase name `prepare` and its ceiling.
- Docs: `docs/commands/check.md` (coverage phase, time ceilings, summary
  trailer), AGENTS.md lines that describe these modules.

### Build order

1. Identities, expected executions, transitions, completeness semantics.
2. Whole-profile plan with launch envelopes and artifact hashes, persisted.
3. Typed runner observations, attributable handshakes, structured termination,
   distinguishable stream ends.
4. Every lane executes its prepared selection and returns its report.
5. Pure reconciliation; schema 2; docs.

### Validation cases (each needs a test)

Cross-shape artifact replacement detected; two same-named tests in different
binaries of one package are two pairs; one pair selected by two lanes is two
expected executions; cargo-mediated parallel refused under complete; handshake
failure under complete is an attribution error; terminal-without-start anomaly;
cancellation during buffered-output drain marks stream_truncated; nextest
immediate fail-fast leaves interrupted/unobserved, not passed; watchdog during
preparation yields plan_complete=false; watchdog during execution keeps the
journal and reports interrupted/unobserved with a failed verdict.
