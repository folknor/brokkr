# Per-test 20s watchdog for `brokkr check` / `brokkr test`

Status: **planning, not started.** Captures the design ahead of
implementation so the next pass doesn't re-derive the structure.

## Background

`brokkr check`'s test phase calls `output::run_captured_with_env(
"cargo", ...)` which buffers stdout/stderr and blocks until cargo
exits. Between the `[run] cargo test --all-features` line and the
final `[result] check passed` (or the failure dump), there is no
output whatsoever. When a single test deadlocks, the watcher (human
or agent) sees a silent process for 5, 10, sometimes 30 minutes and
assumes the build is being slow. **In practice this has never been
a slow build - it has always been a deadlocked test.** The test
phase needs a hard ceiling that converts hangs from "indistinguishable
from slow build" into "loud, specific failure naming the test."

## Goal

A hard 20-second per-test ceiling. If any individual test runs more
than 20s, brokkr kills the cargo subprocess (and its test child),
captures `/proc/<test-pid>/{wchan,status,syscall,stack}`, and emits
a clear failure block naming the test, the elapsed time, and where
the snapshot was written. No escape hatch; no env-var override;
20s for debug and release builds alike. A project with hundreds of
tests cannot afford a single test that needs more than 20s of
budget - if one shows up, that is itself a smell.

The default-quiet output of `brokkr check` is preserved exactly:
five lines for a healthy run, no per-test boundary chatter. The
watchdog only breaks the silence when it fires.

## Scope

- `brokkr check` test phase (`src/check_cmd.rs:run_one_test_sweep`).
- `brokkr test` (`src/test_cmd.rs:run_one`).
- **Not** `brokkr service-test`. Service-test has its own backstop
  policy via the wait combinator inside ratatoskr's harness module
  (per `notes/ratatoskr-service-harness.md`); layering a 20s libtest
  ceiling on top of a Lua-script-as-test does not make sense.

## Existing structure (what's there to build on)

Two paths in tree today, with different infrastructure:

### Path A: `brokkr check` test phase

`src/check_cmd.rs:run_one_test_sweep` (lines 893-992). Pure batch:
`output::run_captured_with_env` blocks until cargo exits, returns
`CapturedOutput { stdout, stderr, status, elapsed }` with everything
buffered. Post-mortem parsing via `cargo_filter::filter_test(&stdout,
&stderr)`.

**No streaming infrastructure here.** The watchdog cannot attach to
a `run_captured` because by the time it returns, the hang has
already happened. This path needs a refactor to a streaming model.

### Path B: `brokkr test`

`src/test_cmd.rs:run_one` (lines 299-392). Already streams. Uses
`output::spawn_captured` to get the `Child`, takes the `stdout` /
`stderr` pipes off it, and runs two background drain threads
(`drain_stdout` lines 394-417, `drain_stderr` lines 419-451). Each
thread reads line-by-line, decides whether to forward the line live
via `keep_stdout_line` / `keep_stderr_compile_line` filters, and
*also* pushes every line into a `Mutex<Vec<String>>` buffer for
end-of-run parsing.

The crucial detail: `keep_stdout_line` (line 455) suppresses
`test foo::bar ... ok|FAILED|ignored` from the live output (so the
user sees only test panics and their own `println!`s during the
run), but the lines are still pushed into the buffer. The watchdog
needs exactly those `test foo::bar ... ` lines as start markers
and the `... ok|FAILED|ignored` suffixes as end markers - they are
already being read in real time, just being thrown away from
display, not from the read loop.

### Shared parse + format

Both paths end at `cargo_filter::parse_test_output(&[&str])` →
`ParsedTestResults`:

```rust
pub struct ParsedTestResults {
    pub failures: Vec<ParsedTestFailure>,  // {name, location, message}
    pub passed: usize, pub failed: usize, pub ignored: usize,
    pub filtered_out: usize, pub suites: usize, pub duration: Option<f64>,
}
```

The renderer `cargo_filter::filter_test(stdout, stderr)` (line 348)
has three modes: build-failed (no `test result:` + compile errors →
fall back to `filter_clippy(stderr)`, relabel `cargo clippy:` to
`cargo test:`); all-passed (`format_test_summary` → one-line
summary); failures present (`format_test_failures` → header +
per-failure one-liners). Both check and test share this pipeline.

### Output convention

`output::error(&str)` (`src/output.rs:102`) prefixes every line of
its input with `[error]   `. Multi-line failure blocks become
multi-line `[error]` blocks. `output::warn` does the same with
`[warn]    `. `brokkr test`'s per-run summary uses raw
`println!("[test]    ...")` because each line is already
self-contained and the prefix is part of the format.

## Architecture

A new shared module, tentatively `src/test_runner.rs`, owns the
streaming loop and the watchdog. Both `brokkr check`'s test phase
and `brokkr test` consume the same primitive.

### `streaming_run_libtest(...)`

```rust
struct LibtestRun {
    captured: CapturedOutput,         // for end-of-run parse
    outcome: LibtestOutcome,
}

enum LibtestOutcome {
    Completed,                        // child exited normally (any exit code)
    HungTest {
        test: String,
        elapsed: Duration,
        snapshot_dir: PathBuf,
        cargo_pid: u32,
        test_pids: Vec<u32>,
    },
}

fn streaming_run_libtest(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    forward_stdout_line: impl Fn(&str) + Send + 'static,
    forward_stderr_line: impl Fn(&str) + Send + 'static,
) -> Result<LibtestRun, DevError>;
```

The 20s timeout is hardcoded inside the module - not a parameter.
That stays out of the call sites and out of the user surface.

The two `forward_*_line` callbacks let each call site pick its
display policy:

- `brokkr check` passes no-op closures (silent operation; only
  the watchdog can break the silence).
- `brokkr test` passes the existing `keep_stdout_line` /
  `keep_stderr_compile_line` filtering that lets test panics +
  user `println!`s through.

### Internals

One Child via `output::spawn_captured`. Two drain threads on
stdout/stderr, similar to `brokkr test`'s pattern, sharing a
`Mutex<Vec<String>>` per stream for end-of-run parsing.

A per-test tracker shared between the stdout drain thread and a
small watchdog thread:

```rust
struct TestTracker {
    current: Option<(String, Instant)>,  // (test name, start time)
}
```

The stdout drain thread updates `current`:

- Line matching `^test (\S+) \.\.\. $` (libtest's start marker
  with trailing space, no newline before the result) →
  `current = Some((name, Instant::now()))`.
- Line matching `^test \S+ \.\.\. (ok|FAILED|ignored)( \(\d+(\.\d+)?s\))?$`
  (or `... ok` etc. on the same line) → `current = None`.

(Worth noting: libtest's exact line shape depends on
`--report-time` / `--test-threads`. Brokkr's sweeps already pass
`--test-threads=1`, so tests are sequential and the start/end
markers come in pairs without interleaving.)

The watchdog thread polls `current` every ~250ms. When
`Some((name, started))` and `started.elapsed() >= 20s`, it triggers
the kill-and-snapshot path and exits.

### Kill-and-snapshot

`cargo test` spawns one test binary per integration test file
(plus one for unit tests). The test binary is a child of cargo;
the hung Rust code runs in *that* process, not cargo itself. So
the snapshot target is the test binary's pid, found via
`/proc/<cargo_pid>/task/<cargo_pid>/children` (or by walking
`/proc` for processes whose ppid matches cargo's pid).

The kill needs a process group, not just `kill(cargo_pid, SIGKILL)`,
because cargo does not always forward signals to the test binary
under it. Spawn cargo with a fresh process group
(`std::os::unix::process::CommandExt::process_group(0)`) so the
whole subtree can be SIGKILL'd by `kill(-pgid, SIGKILL)`. The
`src/ratatoskr/process.rs::send_signal` helper takes a pid and a
signum; a tiny addition for negative pids (process groups) is the
clean path.

For the snapshot, reuse `src/ratatoskr/process.rs::snapshot_proc`.
That helper writes `proc-{wchan,status,syscall,stack}.txt` into a
target directory and is tolerant of read failures (CAP_SYS_PTRACE
on `stack`, process already gone, etc.). The snapshot dir lives at
`<project_root>/.brokkr/test-hung/<YYYYMMDD-HHMMSS>-<test_name>/`
where `<test_name>` is sanitized (`::` → `_`, etc.) for path safety.

### Typed error

A new variant on `DevError`:

```rust
TestHung {
    test: String,
    elapsed_secs: f64,
    snapshot_dir: PathBuf,
},
```

`Display` formats it as a one-liner; the call sites (which know the
formatting convention) are responsible for rendering the multi-line
failure block via `output::error`. Add the variant to `error.rs`
alongside the existing `Build`, `Subprocess`, etc.

### Output shape on hang

```
[test]    test binaries built in 130.5s; running tests
[error]   test foo::bar did not finish within 20s after libtest started it
[error]     per-test timeout: cargo build time excluded
[error]     killed cargo process group (pgid 12345) and test child (pid 12389)
[error]     /proc/12389/wchan: futex_wait_queue_me
[error]     /proc/12389/stack: [first frame, e.g. __futex_abstimed_wait_common]
[error]     full snapshot: .brokkr/test-hung/20260506-143022-foo__bar/
```

The first line is always emitted (text mode only) when cargo's
`Finished` line appears on stderr - it pins the compile/run boundary
so readers can't mistake the per-test 20s ceiling for a cargo
wallclock timeout. The remaining lines are the failure block, with
the same `[error]` prefix as every other failure. `/proc/wchan` and
`/proc/stack`'s first frame are inlined for the common case; the full
files live under `.brokkr/test-hung/...` for deeper inspection.

### Healthy-run output

`brokkr check` adds one `[test]    test binaries built in X.Xs; running
tests` line per sweep, then stays silent until the post-mortem summary
prints (or the watchdog fires). `brokkr test` adds the same line per
sweep, then prints `PASS|FAIL|BUILD FAILED|SKIP` when each run ends -
plus whatever the test itself wrote to stdout/stderr live. JSON mode
suppresses the build-finished line so the output stream stays
machine-parseable.

## Suggested implementation order

1. **Extract the streaming primitive** into `src/test_runner.rs`.
   At first this just re-implements `brokkr test`'s existing drain
   pattern as a reusable function with the two-callback signature.
   No watchdog yet. Refactor `brokkr test` to use it; verify
   identical behaviour. **This step alone has no user-visible
   change** but unblocks everything that follows.

2. **Refactor `brokkr check` test phase** to call the same
   primitive with no-op forward callbacks. End-of-run parsing
   continues to use the buffered output. Verify `brokkr check`
   output is byte-identical to today.

3. **Add the per-test tracker** (stdout drain thread updates
   `current` on start/end markers). No watchdog yet; just
   instrumentation. Add a unit test for the marker parser.

4. **Add the watchdog thread + kill-and-snapshot path.** Include
   process-group spawn for cargo. Extend `snapshot_proc` only if
   needed (it already handles missing-file gracefully).

5. **Wire `DevError::TestHung` through.** `cargo_filter::filter_test`
   does not need to know about this variant - the call sites
   already handle Result<(), DevError> and the failure block goes
   to `output::error` directly.

6. **Add a self-test** that spawns a child process which sleeps
   forever and confirms the watchdog fires within 20-22s and the
   snapshot dir lands on disk. Easy to write; gates regressions.

Each step is independently mergeable. Step 1 is the largest
(extraction); 2-5 are small.

## Open questions / known wrinkles

- **First test in a sweep starts after compile + link.** Cargo's
  output goes `Compiling foo`, `Finished`, `Running unittests/...`,
  *then* `running N tests`, *then* `test foo ... `. The watchdog
  must only count time from the `test foo::bar ... ` start marker,
  not from cargo's launch. The tracker design (`current = None`
  until a start line lands) handles this correctly - linker time
  is not counted against the per-test budget.
- **Multiple test binaries per cargo invocation.** Each integration
  test file is its own binary, run sequentially. Between
  binaries, cargo emits another `Running tests/...` line. The
  tracker's `current = None` between tests is correct - only when
  a test is actively running is the timer ticking.
- **`cargo test --no-run` builds without running.** The watchdog
  is harmless (no `test ...` lines fire, `current` never gets set,
  watchdog never triggers).
- **Doctest output is different.** Doctests print `test
  src/lib.rs - foo (line 42) ... ok`. The marker parser should
  match those too; same shape with extra path detail in the name.
- **What if the watchdog fires while end-of-run parsing is
  inflight?** It can't - the drain threads are still running when
  the watchdog fires (kill happens, drain threads see EOF on
  pipes, threads exit). The hang outcome short-circuits the
  parse path entirely.
- **The `[error] full snapshot: <path>` line uses a relative
  path.** Should it be absolute? Lean relative because it's
  shorter and the user is almost always in the project root when
  reading the failure. The directory itself uses absolute paths
  internally so it's reproducible from anywhere.
- **`.brokkr/test-hung/` should be gitignored.** Add to
  `.brokkr/`'s gitignore stanza if not already covered (today
  `.brokkr/` is wholesale ignored per the existing pattern, so
  this is a no-op).
- **Concurrency with `--test-threads=N`.** Brokkr's sweeps pass
  `--test-threads=1`, so this design is sequential-safe. If a
  user passes `--test-threads=4` via `extra_args`, multiple tests
  start before any finishes and the simple `current = Option<...>`
  tracker breaks. For v1, document this as a known limitation:
  the watchdog assumes single-threaded test execution, which
  brokkr's sweeps already enforce. Multi-threaded support could
  layer on later by tracking a `HashMap<String, Instant>` of
  in-flight tests, with the watchdog firing on whichever exceeds
  20s first.

## Decisions already settled

- **Hardcoded 20s, no escape hatch.** A test that legitimately
  needs more than 20s in a hundreds-of-tests project does not
  exist; if one appears, the test is the bug.
- **Same threshold for `--debug` and `--release`.** Debug overhead
  shouldn't push a healthy test past 20s; if it does, that's
  still a smell worth surfacing.
- **Silent during normal operation.** The whole point is making
  hangs visible *while preserving* `brokkr check`'s five-line
  default output. The watchdog only emits on fire.
- **Applies to `brokkr check` + `brokkr test`, not
  `brokkr service-test`.** The latter has its own backstop
  policy in ratatoskr's harness module per
  `notes/ratatoskr-service-harness.md`.

## Out of scope

- Per-test reporting in the live output. The streaming-boundaries
  idea was rejected: `brokkr check`'s current five-line default
  is a feature, not an oversight.
- Configurable timeout. Same reason - a knob would just enable
  bad tests to hide.
- Resource-budget enforcement (per-test RSS / file descriptors /
  etc.). Different problem, different mechanism.
- Wrapping non-libtest test runners (cargo-nextest, criterion,
  bespoke harnesses). Brokkr only knows about `cargo test` today;
  if a project introduces another runner the watchdog stays out
  of its way.
- Reporting hung tests to the history DB. The exit code is
  already non-zero so the run is recorded as failed; the
  artefact dir under `.brokkr/test-hung/` is the diagnostic
  surface, not a database row.
