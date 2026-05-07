# `brokkr check` / `brokkr test` review (2026-05-07)

Two independent reviews of the check/test code paths. Findings merged below,
de-duplicated, severity-tagged. Files cited at exact line numbers.

Sources:
- `src/check_cmd.rs`, `src/test_cmd.rs`, `src/test_runner.rs`,
  `src/cargo_filter.rs`, `src/cargo_json.rs`, `src/profile.rs`,
  `src/scope.rs`, `src/gremlins.rs`, `src/config.rs`, `src/cli.rs`.

---

## Bugs

### B1 — HIGH — `brokkr check`'s trailing args never reach `cargo` (silent wrong-run)

`src/cli.rs:123` documents `args` as **"Raw arguments forwarded to `cargo
test`"**. `src/check_cmd.rs:923` actually appends every `extra_args` entry to
`libtest_args`, which all land **after `--`**, i.e. as libtest args, not cargo
args.

Repro:

```
brokkr check -- --test cargo_filter
```

builds:

```
cargo test --all-features -- --test-threads=1 --test cargo_filter
```

`--test cargo_filter` becomes two libtest positional name filters (`--test`
and `cargo_filter`). Cargo never sees `--test`, so all test binaries still
build. It exits 0. Silent wrong-run — the user thinks they scoped the run.

**Fix sketch:** split `extra_args` into "before `--`" (cargo-level) and "after
`--`" (libtest-level), or document the actual behaviour and reject `--test`
/ `--bin` / `--package` / `--features` in `extra_args`. The cleanest fix is
to forward them as cargo args and accept that libtest passthrough already has
its own escape hatch via the profile.

---

### B2 — HIGH — Per-test watchdog tracker corrupted by `--nocapture` interleaving

`src/test_runner.rs:215-273` watches partial libtest markers (`test NAME ... `,
no newline) under `--nocapture --test-threads=1`. When the marker is detected,
`pending_name = Some(NAME)` is set and the partial-line bytes are stripped from
the buffer. The tracker entry only clears when the **next** line through
`handle_stdout_line` matches `is_bare_status_line` (exactly `ok` / `FAILED` /
`ignored`, optionally + ` <X.Xs>`).

Two real triggers, same root cause:

**Trigger A — test calls `print!()` (no trailing newline).** Stream becomes
`test foo ... hellook\n`. Drain detects the partial marker, strips `test foo
... `, then accumulates `hellook` and forwards it on the `\n`. `pending_name`
never clears. Tracker still believes `foo` is running. The next test's marker
(`test bar ... `) is **not** detected because marker-detection is gated on
`pending_name.is_none()` (test_runner.rs:257). `bar`'s output then accumulates
into a long unparsed line. After 20s the watchdog fires, blames `foo`, kills
the process group — even though `foo` finished long ago and `bar` (or a later
test) is the actual running one.

**Trigger B — test calls `println!("ok")` (or "FAILED"/"ignored").** That
literal line satisfies `is_bare_status_line`, so the heuristic eats the test's
output, calls `tracker.observe_result(name)` early, and lets the actual
libtest `ok\n` through unstripped to the user. Watchdog stops timing the test
prematurely, so a real hang in the same test goes unnoticed.

Both modes are silent: A produces a false-positive kill; B produces a
false-negative hang.

**Fix sketch:** track an `intermediate_output_seen` flag set whenever a
non-blank, non-status-shaped line is forwarded after `pending_name` was set;
suppress the bare-status special case once it's true. Or: don't strip the
partial marker at all — leave the watchdog timer keyed on parsing of the
*final* `test NAME ... ok` form once the test ends, accepting that nocapture
output prefixes will be visually attached to the start marker. The timing-on-
partial-marker buys "watchdog can fire while the test is still running" —
worth keeping, but the clearing logic needs to be more conservative.

---

### B3 — MEDIUM — `brokkr test` silently drops profile `env`

`ResolvedSweep.env` (`src/profile.rs:39`) is populated by every profile's
`env = { ... }` table. `brokkr check` merges it into the subprocess env at
`src/check_cmd.rs:960` (`merged_env`). `brokkr test`'s `decide_sweeps`
(`src/test_cmd.rs:266`) projects the resolved sweep down to
`{label, feature_args, build_packages}` and discards `env`, `libtest_args`,
`cargo_test_filters`, and `name_filters`.

Dropping the libtest filters and name filters is intentional (the user's
`<NAME>` argument is the filter; documented at `src/test_cmd.rs:243-250`).
**Dropping `env` is not** — there's no docstring justifying it and no test
asserting it. A profile that gates platform tests behind
`BROKKR_TEST_PLATFORM=1` will work under `brokkr check --profile platform`
and silently misbehave under `brokkr test some_platform_test` (with the same
profile as `default_profile`).

**Fix:** carry `env` through. Only `libtest_args`/`cargo_test_filters`/
`name_filters` should be stripped.

---

### B4 — MEDIUM — Successful `brokkr check` text-mode doesn't notice zero-test runs

`src/check_cmd.rs:1001-1029` success path:

```rust
if !captured.status.success() { /* failure ... */ return Ok(false); }
if raw { /* print raw */ }
else  { let filtered = cargo_filter::filter_clippy(&stderr);
        if filtered != "cargo clippy: no issues" { output::warn(...); } }
Ok(true)
```

Never invokes `parse_test_output`. Never checks `parsed.suites > 0` or
`parsed.passed + parsed.failed > 0`. A profile whose `only`/`skip` filters
exclude every test still exits 0 with `cargo test`'s `test result: ok. 0
passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`. `brokkr check`
prints `check passed` and the user is none the wiser.

The JSON path **does** emit a `TestSummary` event with the counts
(`emit_json_test_sweep` at line 1088), so consumers in JSON mode notice. Text
mode is the gap.

**Fix:** parse the test summary on success. If `parsed.suites == 0`, or
`parsed.passed + parsed.failed + parsed.ignored == 0`, surface a warning (and
likely fail) — that profile/filter combo collected nothing.

---

### B5 — LOW — `[both]` sweep tag misleading with 3+ sweeps

`src/check_cmd.rs:444-451`:

```rust
match sweeps.len() {
    0 => None,
    1 => Some(format!("[{}]", sweeps[0])),
    2 => Some("[both]".to_string()),
    _ => Some(format!("[{}]", sweeps.join("+"))),
}
```

The 2-sweep arm fires whenever a diagnostic appears in **any** two sweeps,
regardless of how many active sweeps there are. With 3+ active sweeps,
`[both]` hides which two produced the hit.

**Fix:** only render `[both]` when the active sweep set is exactly the same
two sweeps (i.e. `sweeps.len() == active_sweep_count == 2`). Otherwise show
the names joined.

---

### B6 — LOW — JSON-mode test stdout split misclassifies `{`-leading `println!`s

`src/check_cmd.rs:1037-1044` separates cargo-JSON lines from libtest text by
`line.starts_with('{')`. A test that does `println!("{...}")` (or panics with
a brace-prefixed message) gets routed to the JSON parser, where it fails to
deserialize and is dropped from `parse_test_output`.

Pre-existing, narrow. Worth either documenting or matching `{"reason":` more
specifically.

---

### B7 — LOW — `bin_dir_string` lifetime is brittle

`src/check_cmd.rs:791-799` and `src/test_cmd.rs:74-82` build
`project_env: Vec<(&str, &str)>` whose second elements borrow from a local
`String` (`bin_dir_string`). It works today because both live in the same
function scope, but moving the env-assembly into a helper that returns
`Vec<(&str, &str)>` would dangle. Convert to owned (`Vec<(String, String)>`)
to remove the trap — `merged_env` already does this conversion downstream.

---

### B8 — LOW — `CARGO_TARGET_TMPDIR=target/tmp` is a relative path

`src/check_cmd.rs:826` and `src/test_cmd.rs:79` set
`CARGO_TARGET_TMPDIR=target/tmp` literally. Today the cargo subprocess runs
with `current_dir(project_root)` so the relative path resolves correctly. Any
future code path that spawns cargo from a different cwd would silently write
to the wrong place. Use `project_root.join("target/tmp")` and stringify.

---

### B9 — LOW — `gremlins::scan_content` column drifts on CRLF

`src/gremlins.rs:185-205` increments `col` on every char and only resets at
`\n`. On CRLF files, `\r` is counted as a column character, so reported
columns are one greater than every editor will show. Cosmetic but trivial:
also reset on `\r`.

---

### B10 — LOW — Clippy JSON: per-sweep summary counts contradict deduped event stream

`src/check_cmd.rs:670-744`. A clippy hit appearing in two sweeps is emitted
**once** as a Diagnostic event with `sweeps: ["all","consumer"]`, but **each
sweep's** `DiagnosticSummary` counts it locally. Sum of summary errors > unique
diagnostic events. Defensible (one is "what's wrong", one is "per-sweep
status"), but undocumented and surprising.

---

## Doc drift

- **D1** — `src/test_cmd.rs:1-12` module docstring still references the
  `[check].consumer_features` legacy form (rejected at parse time by
  `parse_check`) and a `<FILE>` argument the CLI doesn't accept.
- **D2** — `CLAUDE.md` says profile `extends = "<other>"` walks "at most one
  parent profile". `src/profile.rs:160-184` walks the chain to its root with
  cycle detection; `resolve_extends_chain_three_levels` is a passing test.
- **D3** — `CLAUDE.md` lists `CheckEvent` as `(Diagnostic, TestFailure,
  DiagnosticSummary, TestSummary, Gremlin, GremlinSummary)`. The actual enum
  (`src/cargo_json.rs:12-20`) also has `TestHung`.

---

## Duplication

The shared shape between `brokkr check`'s test phase and `brokkr test` is
substantial, and B3 (env dropped) is a direct symptom of it diverging.

### A. Sweep selection logic

`decide_active_sweeps` (`src/check_cmd.rs:70-129`) and `decide_sweeps`
(`src/test_cmd.rs:258-292`) implement the same priority ladder: CLI override
→ profile → `[[check]]` entries → legacy `--all-features`. The check version
honours `--features` / `--profile`; the test version doesn't (by design).
test_cmd defines its own `Sweep` struct (`src/test_cmd.rs:26-35`) that's a
strict subset of `ResolvedSweep`.

**Consolidation:** delete `Sweep` and have `decide_sweeps` reuse
`decide_active_sweeps` with `features=&[], no_default_features=false,
profile_name=None`, then strip only the libtest filters (not env). About 35
lines of duplicated control flow gone, and B3 fixes itself.

### B. Pre-build helpers

`run_sweep_pre_build` (`src/check_cmd.rs:835-885`) and `run_pre_build`
(`src/test_cmd.rs:149-194`). Both: build a package with sweep features,
capture, filter the failure output. Differ in:

- release vs debug profile
- json/raw mode branching
- `Err(...)` vs `Ok(false)` return shape
- Different error-print formats

A single helper taking
`PreBuildOptions { release: bool, raw: bool, json: bool }` covers both.

### C. Project env stanza

`Project::Nidhogg => CARGO_TARGET_TMPDIR=target/tmp` plus the
`BROKKR_TEST_BIN_DIR` push is duplicated verbatim in
`src/check_cmd.rs:791-799,824-829` and `src/test_cmd.rs:73-82`. One
`fn build_test_env(project, project_root, profile_dir) -> Vec<(String,String)>`
covers both — and naturally fixes B7.

### D. `Sweep` ⊂ `ResolvedSweep`

`test_cmd::Sweep` is a strict subset of `profile::ResolvedSweep`. Just use
`ResolvedSweep` and ignore the unused fields.

### E. Two clippy parsers

`cargo_filter::parse_clippy` (text) is still alive in four sites
(`src/check_cmd.rs:879,1023`, `src/test_cmd.rs:184,346`) — all of them filter
`cargo build` / `cargo test` stderr where brokkr doesn't pass
`--message-format=json`. Adding `--message-format=json` to those four cargo
invocations and routing through `cargo_json::parse_cargo_diagnostics` +
`event_to_clippy` would let us delete `parse_clippy`,
`extract_rule_from_notes`, `extract_detail`, `parse_block`, and `parse_header`
— ~150 lines of legacy text-scraping.

### F. Watchdog `--test-threads=1` validation runs twice

`src/check_cmd.rs:930-940` calls `effective_test_threads` to decide whether
to default-add and again to enforce. Then `streaming_run_libtest` calls
`enforce_single_threaded` (`src/test_runner.rs:91`) on the same argv.
Pick one site — the streaming runner's check is the load-bearing one.

### G. `filter_clippy` reused as a generic noise stripper

`src/check_cmd.rs:1023-1027` runs `filter_clippy` on `cargo test` stderr in
the **success** path and rewrites `"cargo clippy:"` → `"cargo test:"`.
`cargo test` success-path stderr isn't clippy output; this is using the
filter for its noise-strip side effect. Either rename `filter_clippy` (it's
really "filter cargo's diagnostic stream") or extract the noise-strip helper.

---

## Suggested order of fixes

1. **B1** — fix raw-args forwarding. One-line user-visible bug, easy to
   wedge in independently.
2. **B2** — tighten the watchdog clearing logic. Highest blast radius;
   currently produces both false-positive kills and false-negative hangs.
3. **B4** — parse text-mode test summary on success and fail on zero-test
   runs.
4. **Consolidation A + D** (sweep struct + decision function). Eliminates
   ~35 lines and naturally...
5. ...fixes **B3** (profile env).
6. **Consolidation C** (env helper) and **B7** (lifetime brittleness)
   together — one refactor.
7. **B8** (`target/tmp` absolute), **B5** (`[both]` tag), **B9** (CRLF
   column), **B6** (JSON `{` heuristic) — small isolated cleanups.
8. **Consolidation E** (drop the text clippy parser) — biggest deletion,
   lowest urgency.
9. **D1**, **D2**, **D3** — doc drift, do alongside whatever code change
   touches the relevant area.
