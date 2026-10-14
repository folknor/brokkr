# Design hunt: other-projects scope

Scope: `src/piners/`, `src/ratatoskr/`, `src/ratatoskr_sync/`, `src/litehtml/`,
`src/sluggrs/`, `src/dellingr/`, `src/mogwai/`, `src/artefacts.rs`,
`scripts/litehtml-prepare/`. Edges into the core are named where the question
leads there.

Headline: this scope has the same few domain questions answered over and over
by sibling commands that grew separately: what a run's verdict is, which build
profile applies, whether two runs are comparable, where a project's on-disk
stores live. Each copy carries its own small bug fixes and its own strings. The
biggest wins come from giving those answers typed homes and from collapsing two
sibling families (litehtml/sluggrs visual; piners corpus/lint-corpus) into one
engine with per-project adapters.

---

## 1. Axes that should be types

### 1.1 piners dispositions are string lists held in step by tests

- `report::PARITY_TIERS` (4), `report::NON_PARITY_OUTCOMES` (5),
  `report::HARNESS_ABORT`, `registry::DISPOSITION_LABELS` (9, documented as
  "must equal PARITY_TIERS followed by NON_PARITY_OUTCOMES; a test holds them
  together", `report.rs` asserts the lengths and the concatenation), and
  `gate::BREAK_DISPOSITIONS` (3).
- Consumers match the raw strings: `report.rs` counts by `match` on labels,
  `report.rs`'s render filter and `integrity.rs`'s `harness_aborts` compare
  `p.outcome == HARNESS_ABORT` (the raw outcome, not the validated
  disposition, so a malformed `harness_abort` line with a tier is still treated
  as an abort in those two places but not in the gate), `corpus_query::shown_by_default`
  compares a DB string, `Pin::expected` is `Option<String>`.
- This is the textbook pairwise-agreement finding: the copies are still there.

Shape it wants: one `enum Disposition { Parity(Tier), CompileFail, RuntimeFail,
NoTvData, NoOverlap, HarnessAbort }` with `enum Tier { ByteExact, Accepted,
ActionableDrift, CountDivergent }`, `Disposition::parse(outcome, tier)` as the
single validator (today's `valid_label`), `is_break()` replacing
`BREAK_DISPOSITIONS`, `is_crash()` replacing `HARNESS_ABORT`, and
`Pin::expected: Option<Disposition>` deserialized directly so `registry.lint()`'s
`bad_expected` check disappears into serde. The DB keeps the `as_str` spelling.

### 1.2 lint-corpus dispositions: same pattern, and the copies already disagree

- `lint::DISPOSITION_LABELS` = agree_clean, agree_flagged, divergent,
  piners_error, lint_error. `lint/cmd.rs::TOOL_ERROR_DISPOSITIONS` = the last two.
  `lint::diff::classify` returns `("divergent", ...)` string literals.
- **Disagreement (bug):** `lint/registry.rs`'s `lint()` accepts any
  `is_disposition` label as a pin's `expected`, so `expected = "lint_error"`
  loads cleanly. But `lint_corpus` fails every run that has a tool error
  "whatever the pin says", and bless refuses to stamp one. So the registry
  admits a pin that can never be satisfied. Nobody owns the question "is this a
  pinnable disposition?".
- Shape: `enum LintDisposition { AgreeClean, AgreeFlagged, Divergent(Signature),
  ToolError(Tool) }`. `ToolError` can't be deserialized as an `expected` value
  because the pin type only takes the pinnable subset (`enum PinnableLint`).
  The `Option<Signature>` "Some only when disposition == divergent" invariant
  (`lint::diff::LintOutcome`, `lint::ProbeResult`) goes away as well.

### 1.3 Run result is `"pass"`/`"fail"`/`"error"`/`"interrupted"`

`RunRecord::result: &str` (corpus) and `RunMeta::result: &str` (lint) are
documented as `"pass"` or `"fail"`, but `cmd.rs` also passes `"error"` and
`"interrupted"` to `record_unfinished`. With `fail_reason: Option<String>`
beside it, nothing stops pass+reason or fail+no reason. A
`enum RunResult { Pass, Fail(Reason), Error(Reason), Interrupted }` would also
make `fail_reason`'s precedence (harness before gate, tool error before gate) a
constructor rather than an if-chain repeated in each command (see 2.2).

### 1.4 Build profile as `Option<bool>` / `bool` / `"debug"` string / `CargoProfile`

- `profile_override: Option<bool>` on `CorpusArgs`, `LintArgs`, every ratatoskr
  request struct, and the `service_test`/`service_suite` signatures.
- `HarnessConfig::debug: Option<bool>`.
- `bench_gate.rs` stores `profile: "debug" | "release"` in `gate.db`.
- `dellingr`/`mogwai` set `BuildConfig::profile: "release"` (a `&str`) and,
  separately, `BenchConfig::cargo_profile: CargoProfile::Release`. These are two
  fields for one fact with no link between them.
- `crate::build::CargoProfile` already exists and `piners/measured.rs` uses it
  (`CargoProfile::for_debug(debug)`).

Use `CargoProfile` everywhere. Pass a `ProfileChoice { Explicit(CargoProfile),
Default }`, resolved once against a per-surface default (see 2.1). `BuildConfig`
should hold the `CargoProfile` and the `BenchConfig` should read it from there,
not restate it.

### 1.5 Run-context comparability parsed back out of a JSON string

`corpus_db::compare::context_of(selector)` reads `debug` and `harness_args` from
the stored `selector` JSON with `unwrap_or(Value::Null)` and
`.unwrap_or(true)` for debug. A malformed or legacy selector silently reads as
"debug, no args", so two incomparable runs compare as comparable. The `true`
default is also a third copy of the parity default profile (see 2.1). The
profile and the forwarded args should be typed columns on the run row, written
from the same `RunContext` struct that produced them, never re-derived from
display JSON.

### 1.6 Visual status counts as positional arrays

`litehtml::cmd::test` uses `counts = [0u32; 5] // pass, fail, expected_fail,
error, unexpected pass` and `sluggrs::cmd::test` uses `[0u32; 4] // pass, fail,
error, no-baseline`. Each pairs its array with its own `labels` array and its
own index-based exit rule (`counts[1] > 0 || counts[3] > 0` vs
`counts[1] > 0 || counts[2] > 0`). The same index means different statuses in
the two files. A `StatusTally` keyed by `compare::Status` (plus the
`unexpected_pass` refinement) with `fn fails_run(&self)` owned by `Status` is
the fix (see 2.4 for the decision side).

### 1.7 Visual result status stored and compared as strings

`compare::Status::as_str()` goes into the DB, and the DB rows come back as
`String`. Then `sluggrs approve` compares `r.status == Status::Error.as_str()`,
and both `format_status_columns` match on `"PASS"` / `"NO_BASELINE"` literals.
`Status` has no `FromStr`, so the round trip is one-way. Add `Status::parse` and
make `ResultRow::status: Status`.

### 1.8 ratatoskr frontmatter: typed enums with string escapes and silent fallbacks

- `list_smoke::select_cohort` filters `s.expected.as_str() != "ignored"` on a
  value that is already the `Expected` enum. `service_suite` compares the enum.
- `discover::parse_frontmatter` drops unparseable values without a word: a typo
  like `expected: ignore` runs the reproducer as `Pass`, `ceiling: 5min` falls
  back to the 60s `DEFAULT_CEILING`, and `preserve_data_dir: on-success-too` is
  ignored. The ceiling fallback is the worst one, because the script gets a
  different wall limit than it asked for. Parsing should return
  `Result<ScriptInfo, FrontmatterError>` and fail discovery, or at minimum
  warn.
- `ScriptInfo` is one struct for two script kinds: `fixture`/`protocol` are
  "sync-only, service scripts leave None". `ScriptInfo { common, kind:
  Service | Sync { fixture, protocol } }` would stop a sync script with no
  fixture getting all the way to `resolve_info` before it is refused.
- The `Expected` doc says the runner "can flip a Fail->Pass outcome to expected
  failure". No code does that. `Ignored` only excludes the script from
  selection. Either the doc or the variant name is wrong (it is really
  `Skip`).

### 1.9 ratatoskr gate rules: seven `Option`s kept in step with a counter

`MetricRule` has `max`, `min`, `max_relative`, `min_relative`, `max_delta`,
`equal`, `equal_to_baseline` as independent options. `gate::predicate_count`
re-lists all seven to decide "does this rule check anything", and its doc says
it must equal "the count of outcomes `evaluate_rule` will produce". That is a
second list held in step with `evaluate_rule` only by care. `warn_if_rebasing`
is a third site that picks out "strict" predicates
(`max_delta == Some(0.0) || equal_to_baseline == Some(true)`).

Shape: deserialize into `Vec<Predicate>` where `enum Predicate { Max(f64),
Min(f64), MaxRelative(f64), MinRelative(f64), MaxDelta(f64), Equal(Scalar),
EqualToBaseline }`. An empty rule is then a deserialization error,
`predicate_count` disappears, and `Predicate::admits_no_drift()` owns the
strictness question. `equal`'s value-type check (`toml_to_scalar` returning
`None`, currently a run-time error after the bench ran) also moves to load time.
Bare-metric names (`elapsed_ms`/`exit_code`/`success`) and the `sidecar.`/`meta.`
namespaces could likewise be a parsed `MetricKey` validated before the build.

### 1.10 Gate breach reaches the caller as `DevError::Config`

`evaluate_against_baseline` returns `DevError::Config("gate ... FAILED")` for a
breached rule. A regression and a misconfigured gate therefore share an error
variant and an exit code. A machine caller (or `--gate all`'s cohort loop)
can't tell "the code got slower" from "brokkr.toml is wrong". The same goes for
the identity refusals (script/fixture/profile/features mismatch), which really
are "incomparable", a third category. Give it a typed
`GateVerdict { Passed, Breached(Vec<RuleOutcome>), Incomparable(Mismatch) }`
and a distinct exit code for `Breached`.

### 1.11 Other primitives worth naming

- `gate::format_report` returns `(String, bool)`. The bool is the verdict and
  should be computed from the outcomes by the caller-facing type in 1.10, not
  come back as a side output of formatting.
- `GateRow { exit_code: 0, success: true }` is hard-coded in `run_gate_hook`.
  Only successful runs reach the gate, so the bare metrics `exit_code` and
  `success` are constants and any rule on them is vacuous. Either drop them
  from the "v1 set" or record real values.
- `LitehtmlFixture::expected` is a `String` compared against `"fail"`
  (`litehtml/cmd.rs`). It wants a small enum at config load.
- `litehtml::run_pipeline` decides `mode == "ahem"` from a string field.
- `generate_run_id` is a hand-rolled UUIDv4 hex in both `litehtml/mod.rs` and
  `sluggrs/mod.rs`. The two copies are harmless, but the run id is a
  `String` sliced `[..8]` for display in four places. A `RunId` newtype with
  `short()` would do.

---

## 2. Decisions made in more than one place

### 2.1 "Which cargo profile does this harness build use?"

Sites:
- `piners/cmd.rs`: `profile_override` else `[piners.harness] debug` else **debug**.
- `piners/lint/cmd.rs`: override else `[piners.lint] debug` else **debug**.
- `piners/measured.rs`: override else **release**, deliberately ignoring config.
- `piners/corpus_db/compare.rs::context_of`: a missing value reads as **debug**
  (a read-back copy of the parity default).
- `ratatoskr/service_test.rs`, `service_suite.rs`, `ratatoskr_sync/list_smoke.rs`
  (twice), `bench_gate.rs`: override else `[ratatoskr.harness] debug` else
  **release**.
- `ratatoskr/build.rs`'s doc says "the default is release for parity with
  `brokkr test`". But it serves piners, whose default is debug.
- `dellingr`/`mogwai`/`sluggrs hotpath`: always release.

Nine sites, three defaults. They agree today only because each one was reasoned
out separately. The owner should be one
`fn resolve_profile(surface: HarnessSurface, cli: ProfileChoice, cfg:
Option<bool>) -> CargoProfile`, where `HarnessSurface` is
Parity/Lint/Measured/Service/Sync and states its default and whether config is
consulted. The stored run row then records the resolved `CargoProfile`, so
`context_of` never has to guess.

### 2.2 "Does this corpus-style run pass, and why not?"

`piners/cmd.rs::corpus` and `piners/lint/cmd.rs::lint_corpus` each compute:
- `gate_blocks = !args.bless && !args.no_gate && <deviations>`
- `gated = !args.no_gate && !args.bless` (for the DB row), separately from
  `gate_blocks`, in both files: four sites for one "is this a gated run" fact.
- `run_pass = <tool/harness ok> && !gate_blocks`
- the `fail_reason` precedence chain
- the `"pass"`/`"fail"` string
- the bless short-circuit and its "tool/harness failed: nothing blessed" exit
- final `PASS`/`FAIL` line and `ExitCode(1)`.

The run mode is spread across `reseed`, `reanchor`, `bless`, `verify_only` and
`no_gate` booleans. Exclusivity is enforced by clap `conflicts_with` plus
"dispatch-rejected" comments (`measured.rs`), and `profile_override` lives
beside them. Shape: `enum CorpusMode { Verify, Run { gate: GatePolicy }, Bless,
Reseed, Reanchor }` with `GatePolicy { Enforce, ReportOnly }`, and one
`RunVerdict::decide(integrity_failures, gate_deviations, mode)` shared by both
commands. Given 2.3, this is one function with one test table, not two.

### 2.3 "Is this harness run acceptable?" over `RunAssessment`

Three policies:
- parity: `assessment.failure_reasons(exit1_explained)` (`integrity.rs`), with
  `exit1_explained` decided by `gate::breaks_all_pinned` /
  `gate::report_has_break`;
- measured: `measured.rs::check_iteration` (exit 0 && `completed()` && no
  aborts);
- isolation: `cmd.rs::classify_attempt` (run_error stops it, `completed()`
  passes, violations/cut means protocol-invalid, abnormal end means abort,
  otherwise incomplete).

The policies differ on purpose (harness_abort is a pinnable break for parity,
a disqualifier for measurement, and "attributed, so completed" for isolation).
That difference is the domain fact, and right now it is scattered across three
files with three message builders. `integrity.rs` claims to be "the one run
assessment every consumer reads". It should also own the verdicts:
`RunAssessment::verdict(Policy::{Parity{exit1_explained}, Measured,
IsolationAttempt}) -> Verdict`. `classify_attempt` in particular sits in
`cmd.rs` but belongs with `isolate.rs`'s `Failure`/`StopReason` types.

### 2.4 "Which visual status fails the run?" and the status annotation

- Exit rule: `litehtml::print_run_summary` fails on indices 1 and 3, `sluggrs`
  on 1 and 2 (same meaning, different indices, see 1.6). Both bucket
  "everything else" with `_ => counts[1] += 1` (failed). Litehtml's wildcard
  also catches `NoBaseline`. Litehtml never produces that status today, but a
  future litehtml path that did would fail the run, which `Status::NoBaseline`'s
  own doc says must never happen.
- **Already drifted (bug):** `sluggrs::format_status_columns` carries the fix
  "only ever an annotation, never a repeat... used to render
  `FAIL_THRESHOLD (FAIL_THRESHOLD)`". `litehtml::format_status_columns` still
  has `s => format!(" ({s})")`, so `brokkr visual-status` in litehtml prints
  `FAIL_THRESHOLD (FAIL_THRESHOLD)`, `ERROR (ERROR)`, and so on.
- "improved" threshold `0.5` is hard-coded in both status renderers. It is the
  same number as `compare::REGRESSION_TOLERANCE` but not that constant. The
  "no visible delta" `0.05` is hard-coded in both too.
- "approval record exists but the baseline artefact is gone means ERROR, not
  NO_BASELINE / not recapture" is decided in `litehtml::run_fixture` (Chrome
  reference) and `sluggrs::run_snapshot` (`approved.png`) separately.

Owner: `compare::Status` gains `fails_run()`, `annotation(current, approval)`
and the tolerance constants. One status renderer serves both projects.

### 2.5 "Are these two runs comparable?"

- ratatoskr gate: `evaluate_against_baseline` checks gate name, script, fixture,
  profile and features field by field. Each one is its own `if` with its own
  message, and features has a legacy `None` escape.
- piners: `corpus_db::compare::context_of` returns `(debug, harness_args)`.
- core `brokkr results --compare` has its own pairing key (sibling scope).

Each one is a "run identity" decision. Within this scope, a
`GateIdentity { gate, script, fixture, profile: CargoProfile, features:
CanonicalFeatures }` with one `fn mismatch(&self, other) -> Option<Mismatch>`
would replace the five ifs and feed 1.10's `Incomparable`. The piners
equivalent is the typed run context from 1.5.

### 2.6 "Where does project X keep its stores and artefacts?"

Paths are string constants scattered across files:
`service_test::ARTEFACT_PARENT` (`.brokkr/ratatoskr/service`),
`SERVICE_MOCK_PARENT`, `saehrimnir::MOCK_DIR`, `list_smoke::SYNC_ARTEFACT_PARENT`,
`bench_gate.rs` builds `.brokkr/ratatoskr/gate.db` inline, `piners/cmd.rs::ARTEFACT_PARENT`
(`.brokkr/piners`, so artefacts share `.brokkr/piners/corpus/` with `runs.db`),
`resolve::corpus_runs_db_path` / `lint_runs_db_path`, and litehtml's
`.brokkr/capture.js`. `main_parts/commands.rs` (`brokkr clean`) then
re-encodes the same layout separately. It knows to sweep directories under
`.brokkr/ratatoskr` but keep files (because `gate.db` lives there), and to sweep
`.brokkr/piners/corpus` run dirs but not `runs.db`. The "constructed-name
rule" in `clean.md` relies on these staying in step. Owner: each project module
exports a `Layout` (artefact roots, measurement stores marked as permanent),
and `clean` iterates the layouts instead of hard-coding them.

### 2.7 "Does this mode instrument the build?" and mode-to-runner dispatch

`uses_hotpath(req)` is defined three times (`dellingr/cmd.rs`, `mogwai/cmd.rs`,
`sluggrs/hotpath.rs`). `piners/measured.rs` re-derives `alloc` from a match on
`req.mode`. Each file then chooses features (`req.hotpath_features()` vs
`req.features`), then `run_hotpath` + `run_hotpath_capture` vs `run_external`,
the `harness::hotpath_feature(...)` label, `hotpath_alloc_note`, and a scratch
dir. That is the same ~40-line decision in four places. `piners/measured.rs`
differs on two points: it ignores `req.features` (CLI `-F`), pushing only the
hotpath feature onto `[piners.harness] features`, and it uses
`hotpath_feature(alloc)` alone rather than `hotpath_features()`. Whether that is
intended is not stated anywhere. The mode question belongs on `MeasureMode`
(`fn instruments(&self)`), and the runner choice belongs in a core
`run_measured_target(ctx, config, argv, env)` that the four adapters call. That
is a sibling scope's file, but this scope is where the copies are.

### 2.8 Lock-before-read rule

`corpus`, `lint_corpus` and `measured::run` each acquire the lock "before
anything is read", with the same paragraph-long justification. `measured.rs`
then relies on re-entrancy because `BenchContext` acquires it again. This is
one rule ("a registry with writers is read only under the lock"), and
`Registry::load` / `LintRegistry::load` could take a `&LockGuard` to make it
structural rather than commented.

---

## 3. Structure

### 3.1 `include!` splicing hides real module boundaries

- `src/ratatoskr/cmd.rs` is two lines: `include!("service_test.rs");
  include!("service_suite.rs")`.
- `src/ratatoskr/sync.rs` is two lines: `include!("../ratatoskr_sync/list_smoke.rs");
  include!("../ratatoskr_sync/bench_gate.rs")`.
- `src/ratatoskr_sync/` is a sibling top-level directory that is not a module.
  It only exists to be textually spliced into `ratatoskr::sync`.

This looks like a way to get under the `wc` 800-line threshold without deciding
the boundaries. The cost: the spliced files share one namespace (private items
leak across what look like file boundaries), `gate.rs`'s header points readers
at `src/ratatoskr/sync.rs` for plumbing that is not there, and the
directory tree misdescribes the module tree. Make them real modules:
`ratatoskr::service::{single, suite, list}`,
`ratatoskr::sync::{list, smoke, all, bench, gate_hook}`. Move `ratatoskr_sync/`
under `ratatoskr/sync/`. `bench_gate.rs` alone (1643 lines) holds cohort
selection, the bench loop, the gate hook, baseline lookup, identity checks and
sidecar-to-JSON projection. Those are at least three modules.

### 3.2 Dependency edges into `ratatoskr` from the core and other projects

- `crate::ratatoskr::process::send_signal_pgrp` is called by `output.rs` (3
  sites) and `sidecar.rs` (3 sites). `send_signal` is called by
  `nidhogg/bench_tiles.rs`. `test_runner.rs` imports
  `ratatoskr::process::snapshot_proc`.
- `crate::ratatoskr::build::build_for_harness` is the build entry point for
  **piners** (`cmd.rs`, `lint/cmd.rs`). Its doc and log text are written for
  `[ratatoskr.harness]`, and it states a release default that piners does not
  use.
- `config::HarnessConfig` is shared by ratatoskr and piners. `lint/cmd.rs`
  synthesizes one from `LintConfig` fields.

These are core facilities living in a project directory. `process.rs`
(raw-pid `kill`, pgrp signal, `/proc` snapshot) belongs next to `lockfile`'s
pidfd path, in a `crate::proc` module. Its raw-pid `send_signal` is the
unverified sibling of the identity-checked pidfd path the core went to some
effort to build. `build_for_harness`/`HarnessBuild` belong in `crate::build`.
Edges should point project to core, never core to project.

### 3.3 litehtml and sluggrs are one visual engine written twice

Both have `cmd.rs` with `test`/`list`/`approve`/`report`/`status`,
`open_db`, `format_pct`, `print_table_header`, `resolve_X`/`resolve_Xs`,
`print_run_summary`, `print_X_status`, `format_status_columns`, a `db.rs` with
the same three tables and the same `map_*` row mappers, and a
`generate_run_id`. `sluggrs` already imports `crate::litehtml::compare`, so
the shared core is filed under one of its two clients. The shared `visual`
CLI command has to refuse litehtml-only flags in sluggrs by hand.

Shape: `src/visual/{engine, compare, db, status}` owning the run loop, the
status tally (1.6), the approval/ratchet policy (2.4), and the DB. The
project-specific part is a trait: `Subject { fn render(&self, item) ->
Rendered; fn reference(&self, item) -> Reference; fn scores(...) }`.
Litehtml renders through the pipeline and scores against a Chrome capture on
pixel and element metrics. Sluggrs renders a snapshot and scores against
`approved.png` on pixels only. Per-project flags (`--suite`, `--recapture`)
become part of the subject's argument type, so the refusal is structural.
2.4's drift (the `FAIL_THRESHOLD (FAIL_THRESHOLD)` bug) is the evidence that
two copies don't stay in step.

### 3.4 litehtml's Node tooling has two distribution mechanisms

- `CAPTURE_JS` is a 90-line Puppeteer script embedded as a Rust string in
  `litehtml/cmd.rs`, written to `.brokkr/capture.js` per run, run with
  `NODE_PATH = $(npm root -g)`. That makes it depend on a globally installed
  puppeteer.
- `prepare.js` / `smoke.js` live in `scripts/litehtml-prepare/` with a
  `package.json` and a pnpm lockfile. They are found at **run time** via
  `env!("CARGO_MANIFEST_DIR")`, and `ensure_prepare_script` runs `pnpm install`
  into that source directory.

Consequences: an installed `brokkr` binary works only while the source checkout
it was built from still exists at the same path. `brokkr` writes
`node_modules` into its own source tree. The two scripts resolve puppeteer and
cheerio through two different package managers and scopes, so the Chrome
version behind the reference captures isn't pinned by any lockfile. Put
`capture.js` beside `prepare.js` under one `package.json` (puppeteer pinned
there), `include_str!` all of them, and materialize them plus their
`node_modules` into a brokkr-owned cache dir (XDG data, keyed by the lockfile
hash) rather than the source tree.

### 3.5 piners: two command pipelines, one shape

`piners/cmd.rs` (1295 lines) and `piners/lint/cmd.rs` each do lock, registry
load and lint, select, verify, build the harness, run, classify, render, record
to a SQLite run store, bless/reseed writers, and a `*-results` query surface,
with parallel modules (`registry`/`lint::registry`, `pins_write`/`lints_write`,
`select`/`lint::select`, `reseed`/`lint::reseed`, `corpus_db`/`lint::db`,
`corpus_query`/`lint::query`). Some of this is legitimately different (the
corpus has harness integrity and isolation, lint runs two validators). But the
run-mode, verdict (2.2), bless gating, record-before-mutate ordering and
registry-writer locking are the same protocol. A `corpus_engine` with a
`Subject` trait (select/verify/execute/classify) would give both one
implementation of the protocol. `lint/` already shares `registry_io`,
`time::now_rfc3339` and the corpus root, so the seam is half there. Inside
`cmd.rs`: `classify_attempt`, `store_attempt_streams`, `AttemptEnv` and
`diagnose` are isolation machinery and belong in `isolate.rs`. `reserve_run_id`
and `record_unfinished` are run-store concerns that belong in `corpus_db`.
`cmd.rs` should read as orchestration only.

### 3.6 `registry.rs` (1885 lines) and `report.rs` (1412 lines)

`registry.rs` holds the schema types, loading, keyword index, lint, feed
groups, harness-file pins, xxh128 verification (`verify_probe`,
`verify_harness_files`), and tests. Verification is a separate job with a
different failure mode, and `cmd.rs::verify_selected_feeds` is a third piece
of it living outside the registry. Split into `registry::{schema, load, lint,
verify}`. `report.rs` mixes NDJSON parsing, the disposition vocabulary (which
should become 1.1's type module), aggregation (`Summary`, root-cause groups)
and terminal rendering. Rendering wants its own file.

### 3.7 `artefacts.rs`

- `ArtefactDir::armed` is dead state. It is set by the finalizers and never
  read, and `Drop` does nothing. The doc's "Drop preserves when no finalizer
  ran" is just the absence of removal. Remove the field, or make `Drop` do
  something observable (log that the dir was kept unfinalized).
- The retention decision is split. `keep_on_success` is fixed at allocation,
  ratatoskr's `PreserveDataDir::OnSuccessToo` frontmatter is folded into that
  bool by each caller, and piners decides "keep despite success because
  diagnosis ran" by calling `finalize_failure` on a run that didn't fail. A
  `Retention { OnFailure, Always }` plus `finalize(Outcome::{Success, Failure,
  KeepEvidence})` would state the intent instead of overloading "failure".
- The module header repeats its "generic parent path" paragraph twice.

### 3.8 `bench_gate.rs` round-trips its own data through JSON strings

`run_gate_hook` serializes the sidecar and meta blobs to strings
(`unwrap_or_else(|_| "{}")`). `evaluate_against_baseline` then parses them back
with `unwrap_or_default()` to build `GateRun::from_parts`, even though
`from_parts` exists to skip that round trip. A serialization failure becomes
an empty map, and the run then fails with "missing `sidecar.x`". That message
blames the data rather than the encoder. Keep the `Map`s in the in-memory row
and serialize only at the DB edge.

---

## Lateral findings and bugs (summary)

1. **litehtml `visual-status` prints the status twice** for every non-PASS row
   (`FAIL_THRESHOLD (FAIL_THRESHOLD)`). Sluggrs fixed this in its copy and
   litehtml never got the fix (`litehtml/cmd.rs::format_status_columns`).
2. **lint registry accepts unpinnable `expected` values** (`lint_error`,
   `piners_error`) that every run then fails (1.2).
3. **ratatoskr frontmatter typos are silent**: a bad `ceiling` reverts to 60s,
   and a bad `expected` runs an ignored reproducer as a normal script (1.8).
4. **piners comparability defaults a malformed selector to "debug, no args"**
   and compares on it (1.5).
5. **Gate breach and gate misconfiguration share `DevError::Config`** and an
   exit code (1.10).
6. **Gate bare metrics `exit_code`/`success` are constants** in every recorded
   row (1.11).
7. **Installed brokkr depends on its source checkout** for litehtml `prepare`
   and writes `node_modules` into it (3.4).
8. `piners/measured.rs` drops CLI `-F` features without saying so (2.7). This
   may be intended, but it is undocumented.
9. `ratatoskr/build.rs`'s doc claims a release default that one of its two
   client projects (piners) overrides to debug (2.1, 3.2).
10. `harness_abort` detection uses the raw `outcome` field in `report.rs`
    rendering and `integrity::harness_aborts`, but the validated disposition
    in the gate. A tier-carrying `harness_abort` line is treated differently
    by the two (1.1).
