# Design hunt: check-plan

Scope: what `check`/`test` plan and select before anything runs -
`src/profile.rs`, `src/check_cmd/{selection,features,prepare,coverage,accounting,continuation,install_shape}.rs`,
plus the config-load rules in `src/config_parts/parser.rs` and the invocation
resolution in `src/check_cmd/phase.rs` that feed them.

The headline: `selection.rs` is the model the rest of the scope should look
like (one decision, private constructors, disposition enum). Around it, the
sweep itself (`ResolvedSweep`) is a 25-field bag of primitives and argv
fragments that every downstream module re-interprets, and most of the
duplicated decisions below are re-interpretations of that bag.

---

## 1. Axes that should be types

### 1.1 `ResolvedSweep` is two types glued together by a mutable field
`ResolvedSweep` is built by `profile.rs` with `effective_unification:
EffectiveUnification::Ambient` as a placeholder (`build_resolved_sweep`,
`sweep_from_check_entry`), and later overwritten in place by
`phase.rs::resolve_sweep_unification`. Everything keyed on it -
`build_shape_key`, `needs_isolated_target_dir`, `Resolutions::for_selection`,
`shape_id` - is silently wrong if read before that mutation; nothing in the
type says which state a sweep is in. The struct also derives `Default`, so
tests (and potentially production) mint sweeps with the placeholder.

Proposal: split into `SweepDecl` (pure config result of profile resolution:
what the user wrote, with provenance) and `PlannedSweep` (invocation-resolved:
effective unification, isolated target dir, lane kind, the selection
`Attempt`s). Only `PlannedSweep` has `build_shape_key`/`shape_id`. The
resolution function (`resolve_unification` + the promotion eligibility now in
`phase.rs`) becomes the only constructor of `PlannedSweep`.

### 1.2 Lane kind is three loose fields with a priority order
`harness: Harness`, `doc_only: bool`, `parallel_budget: Option<u32>`,
`process_isolation: bool`, and `test_threads: Option<u32>` jointly encode one
fact - how the lane executes - and `prepare::lane_kind` recovers it by a
priority ladder (nextest > doc_only > parallel > isolated > serial). Illegal
combinations are refused in four different places (see 2.1/2.4) or not at
all: `lane_kind` silently picks `Parallel` when both `parallel_budget` and
`process_isolation` are set, so correctness depends on
`phase.rs::reject_conflicting_lanes` having run first. Note also that
`profile.rs`'s doc comment on `parallel_budget` says the exclusion is
"enforced at resolve time" while `reject_conflicting_lanes` says it is
"refused here rather than at resolve time" - the docs already disagree.

Proposal: `enum LaneExec { Serial { threads: SerialThreads }, Parallel {
budget }, Isolated, Nextest { threads }, DocOnly }` built once, at
resolution, with the conflicts as construction errors. `LaneKind` in
`accounting.rs` then becomes a projection of it rather than a re-derivation.

### 1.3 `test_threads: Option<u32>` is a three-state mode in disguise
`None`/`Some(1)` = serial under watchdog, `Some(0)` = libtest default,
`Some(n>=2)` = n threads. The predicate `matches!(test_threads, Some(n) if n != 1)`
("cargo-threaded") is spelled independently at: `prepare.rs` (serial
unattributable), `phase.rs` (doc-only refusal), `output.rs` (three sites),
`config_parts/parser.rs` (two sites, profile-def level). A
`enum Threads { Watchdogged, CargoDefault, Cargo(NonZeroU32>1) }` with
`is_cargo_mediated()` deletes all of them.

### 1.4 The filter set has five parallel encodings
A sweep's selection filters live as: `libtest_args` (flattened argv, also
carrying `--include-ignored`), `name_filters`, `cargo_test_filters` (argv
`--test X` pairs), `qualified_skips`, and `declared_filters` (the provenance
copy, added because the argv forms are "flattened past recovery"). They are
built in parallel in three constructors (`shaping_from`,
`build_resolved_sweep`, `sweep_from_check_entry`), and each consumer
re-parses the argv back into meaning:
- "does this lane lift `#[ignore]`" - see 2.2;
- "does this lane name a target" - `output.rs::has_target_selector` scanning
  `cargo_test_filters` by `starts_with`, vs `phase.rs`'s doc-carrier check
  using `cargo_test_filters.is_empty()`;
- coverage's `filter_liveness` re-reads `declared_filters` and
  `qualified_skips` and trusts a comment that `libtest_args` never contains
  `--exact`.

Proposal: one `LaneFilters { skips: Vec<Filter>, only: Vec<Filter>,
targets: Vec<TargetName>, include_ignored: bool }` where each `Filter`
carries its origin. Argv is rendered from it at the cargo boundary only.
`declared_filters` stops being a second copy and becomes the only copy.

### 1.5 `BuildShapeKey` is a bare 7-tuple, hashed through `Debug`
`profile.rs::BuildShapeKey` is `(Vec<String>, Vec<String>, Vec<String>,
Vec<(String,String)>, Vec<String>, Option<SweepProfile>, EffectiveUnification)`
- three `Vec<String>` siblings that can be swapped without a type error.
`accounting::shape_id` then hashes `format!("{:?}", key)` and persists the
result in the plan on disk: identity depends on `Debug` output of every
component type, which any derive change or field rename silently alters.
Make it a named struct with an explicit, stable encoding (or a dedicated
`ShapeId` newtype minted only from it).

### 1.6 Identity primitives in the plan
In `accounting.rs`: `shape: String` (a hex digest), `resolution:
Option<String>` (a package name, or "combined"), `lane: usize` (sweep index),
`hash: String` (xxh3 hex), `executable: String`, `BinaryUnit.kind: String`.
`artifact_drift` takes `&[(Option<String>, String, String)]` - resolution,
path, hash as an anonymous triple. `ShapeId`, `Resolution` (`enum {
Combined, Package(PkgName) }` - matches `Resolutions` in selection.rs
already), `LaneIdx`, `ContentHash` newtypes would make the universes map
key `(String, Option<String>)` in `prepare_profile` self-describing and
un-swappable with the many other `(String, Option<String>)` pairs.

### 1.7 `LaneRecord`'s inventory state is three overlapping fields
`prepared: bool`, `unavailable: Option<String>`, `skipped: Option<String>`
plus `unlisted` non-empty. Meanings collide: a skipped lane is recorded as
`prepared = true` (prepare.rs, "not attempted" branch), so `prepared` means
"its expectation is known", not "it was prepared". `continuation::lane_inventory`
reconstructs a disposition from them with an if-ladder in a specific order
(superseded > skipped > unavailable > doc-only > unlisted > complete) and
emits it as `&'static str`; `prepare_profile` reads `prepared ||
kind == DocOnly` for `plan.complete`; `phase.rs` reads `unavailable.is_none()`
separately. Proposal: `enum Inventory { Prepared { executions, ..,
unlisted }, Skipped(reason), Unavailable(UnavailableReason), DocTests }`,
with `UnavailableReason` typed (`CargoThreaded`, `NoShim(reason)`,
`PrepareFailed(msg)`, `Stopped`) - today those are prose strings, and a
test asserts on `.contains("one stream")`.

### 1.8 Continuation availability is stringly typed
`LaneInventoryOut.availability` and `InventoryOut.availability` are
`&'static str` compared with `matches!(l.availability, "complete" | "partial" | ...)`;
`is_fail_fast_casualty` compares `c.outcome == Outcome::Unobserved.as_str()`
after the enum was already flattened to string in `CandidateOut`. The JSON
DTOs flatten too early; keep enums in the model and serialize with
`rename_all`. Same for `SuspectOut.cause`.

### 1.9 Phase names as strings
`CheckSelections::build` takes `skip: &dyn Fn(&str) -> bool` and calls
`skip("clippy")`, `skip("rustdoc")`, `skip("install_feature")` while
`SelectionPhase` already exists. A typo here silently means "never skipped".
A `Phase` enum covering every phase (incl. install_feature, prepare,
coverage) should be what `skip_phases` parses into.

### 1.10 Claim strength as `Option<Certifies>` compared everywhere
`certifies == Some(Certifies::Complete)` is evaluated at ~6 runtime sites
(`phase.rs` x5, `install_shape::install_feature_applies`) and ~6 parse sites;
`LaneInputs.certifying` and `AccountingPlan.certifying` are bools derived
from it. Each site encodes one obligation of the claim ("refuse -p", "refuse
trailing args", "enumerate universe", "run install-feature in gate mode",
"refuse unattributable lanes"). A `Claim` type with named obligation methods
(`refuses_scoping()`, `enumerates_universe()`, `requires_attribution()`,
`runs_install_feature(mode)`) puts the permission table in one place - the
`reject_scoped_complete` doc already calls it "the certifies permission
table", but there is no table, only scattered comparisons.

### 1.11 Failures that reach callers only as prose
`PhaseSelection.refusal: Option<String>` surfaces as `DevError::Config(msg)`;
`plan.incomplete: Vec<String>`; `Anomaly.detail: String`;
`DeadFilterRecord { sweeps: String /* comma-joined */, origin: String,
label: String }`. `--json` consumers get the same prose. `DeclaredFilter.origin`
is admittedly "rendered text because the two sources have no common address
type" - make the address type (`FilterOrigin::Profile(name) |
FilterOrigin::Entry(name)`); the ledger keys on it (coverage.rs `FilterLedger`
keys on `origin` + rendered `label` string equality today).

---

## 2. Decisions made in more than one place

### 2.1 "Is this lane cargo-threaded / unattributable?"
- `config_parts/parser.rs::validate_complete_test_threads` (load time):
  re-resolves `test_threads` via its own `effective_test_threads` extends-walk,
  and re-classifies lanes as serial via `e.parallel.is_none() && e.harness !=
  Nextest && !e.doc_only`.
- `prepare.rs::serial_unattributable` (run time): `!doc_only &&
  matches!(test_threads, Some(n) if n != 1)`.
- `phase.rs` doc-only check (`process_isolation || threads != 1`).
- `output.rs` x3.
The parser's copy is a second implementation of both profile `extends`
resolution (`profile::resolve_profile_chain`) and lane classification
(`prepare::lane_kind`). They agree today. Owner: resolve profiles once at
load (the parser can call `profile::resolve` for every complete claim) and
ask the resolved `LaneExec` (1.2).

Related gap, suspected: `parser.rs`'s `isolation = "process"` +
`test_threads != 1` refusal checks each profile *def* alone, not the merged
chain. A child setting `test_threads = 4` that `extends` a parent with
`isolation = "process"` passes load validation and resolves to an isolated
lane carrying threads 4. Probably inert (the isolated lane ignores it), but
it is exactly the class of drift a per-def copy invites.

### 2.2 "Does this lane run `#[ignore]`d tests?"
- `prepare.rs::lane_runs_ignored` / `args_run_ignored`: libtest args +
  forwarded args, recognises `--include-ignored` and `--ignored`, skips
  `--skip` values.
- `prepare_profile` first sets `record.include_ignored` from
  `args_run_ignored(sweep.libtest_args)` (no forwarded args), then overwrites
  it with `prepared.include_ignored` for prepared lanes only - so unprepared
  lanes carry the first answer, prepared ones the second.
- `coverage.rs::filter_liveness`: `libtest_args.iter().any(|a| a ==
  "--include-ignored")` - no `--ignored`, no forwarded args.
- `output.rs:266` same string test; `nextest_lane.rs` maps the flag itself.
These agree only because a complete claim (the only one that runs coverage)
refuses forwarded args, and profiles can't emit `--ignored`. Owner: a field on
the typed filters (1.4), computed once with the forwarded args folded in.

### 2.3 "Does this lane carry doctests?"
- `accounting::lane_runs_doctests`: DocOnly, or Serial with `doctests &&
  !has_target_selector(cargo_test_filters)`.
- `phase.rs` complete-profile carrier check (~line 1103): `doc_only ||
  (harness == Libtest && parallel_budget.is_none() && !process_isolation &&
  cargo_test_filters.is_empty() && doctests)`.
Two predicates, written differently (`is_empty` vs `has_target_selector`,
explicit field ladder vs `lane_kind`), deciding the same question. The
accounting doc says it is "the one definition the plan's carrier flag and
the strict shim's rustdoc admission share" - the carrier refusal in
`phase.rs` is a third reader that does not share it. Owner:
`lane_runs_doctests`; the phase check should call it.

### 2.4 "Is the bare selection the whole workspace?" / workspace membership
- `build::project_info(..).bare_selection_is_whole_workspace` (phase.rs,
  promotion eligibility and the doc-carrier check).
- `features::WorkspaceFeatures::default_members` vs `member_names`
  (selection's `Selection::concrete` for `Bare`).
- `DocFacts` (another metadata read) in prepare.
- `test_cmd::focused::eligibility` re-reads manifests for `harness = false`.
At least three `cargo metadata` snapshots per invocation, each answering a
slice of "what is this workspace". They could disagree only if the
workspace changes mid-run, but the *derivations* (e.g. older cargo with no
`workspace_default_members` handled in features.rs, and however project_info
handles it) are independent. Owner: one lazily-fetched `WorkspaceSnapshot`
per invocation (generalise `FeatureOracle`) that owns members, default
members, targets/doctest facts, harness flags.

### 2.5 "Are two sweeps the same build?" - two keys
`ResolvedSweep::build_shape_key` (configured: coverage grouping, accounting
`shape_id`, universes) vs `selection::EffectiveKey` (effective: clippy /
rustdoc dedupe, `ExecProjection` for `brokkr test`). `build_shape_key` keeps
`build_packages` (its own doc: "KNOWN GAP ... phase-contract conflation"),
`EffectiveKey` keeps support requests. Universes are keyed by configured
shape, so two lanes of one configured shape with *different effective
selections* would share a universe; that is safe only because a complete
claim refuses `-p`. Owner: selection.rs should mint both keys - a
`DeclaredShape` (for policy grouping) and an `EffectiveBuild` (for dedupe and
artifact identity) - and `profile.rs` should not know about build identity
at all.

### 2.6 Qualified-skip applicability
"Where can a package-qualified skip be applied?" is decided in
`profile::resolve_single` (isolated or nextest), in `profile::run_shaping`
(isolated only - by design, ad-hoc lanes are libtest), and implicitly by
which lanes read `qualified_skips` (prepare `list_binary`, coverage
`filter_liveness`, nextest filterset). The rule is "lanes with per-test
attribution", i.e. a property of `LaneExec`. Make it
`LaneExec::applies_qualified_skips()` and have both resolvers ask it.

### 2.7 `-p` admission for install vs every other phase
`selection::admit` produces `Admission { kept, notes }` with typed rules;
`InstallSelection::build` does its own `cfg.install.iter().filter(|p|
cli.contains(p))` with no notes. Same question ("which invocation packages
does this phase admit"), two answers with different output shape. Fold into
`admit` with an `AdmissionRule::NotInstallPackage`.

### 2.8 Unification resolution sits outside the selection module
`selection.rs` claims "One decision, made once" for which runs exist, but
the input that decides `Resolutions::Combined` vs `PerPackage` -
`effective_unification` - is decided in `phase.rs::resolve_sweep_unification`
(with its own promotion-eligibility and metadata fetch) and written back onto
the sweep. Two modules jointly own "what cargo resolutions does this lane
run". Move promotion and `resolve_unification` into selection (it already
holds the oracle and the CLI scope).

### 2.9 Feature argv for a sweep with no projection
`FeatureConfig::configured_key` and `argv_with` are both the identity
rendering; `Attempt.features` ("described features") is a third rendering
filtered by "carried in at least one run". Fine as data, but the
"described" form is what announcements print while `cargo_args` is what
runs - check that no reporting path prints `configured_key` where a run
dropped tokens (output.rs is outside scope; flagging for the sibling).

---

## 3. Structure

### 3.1 `profile.rs` is in the wrong place and does three jobs
It is top-level (`src/profile.rs`) but has one client family (`check_cmd`,
`test_cmd`). It holds (a) the `extends`-chain merge, (b) the
`ResolvedSweep` god-struct and its build identity, (c) `FeatureConfig`
argv rendering, (d) `resolve_unification`. Meanwhile the *validation* of the
same profiles lives in `config_parts/parser.rs`, which reimplements (a).
Proposed home: `src/check_cmd/plan/` with `profile.rs` (merge only, used by
both the parser's validation and runtime), `sweep.rs` (`SweepDecl`),
`lane.rs` (`LaneExec`, `LaneFilters`), and selection/features moved under it.
Validation that needs merged profiles (threads, qualified skips, doc carrier)
runs on resolved sweeps at load time instead of on raw defs.

### 3.2 `accounting.rs` (2.7k lines) is four modules
Plan types + wire format; the journal (lock-free fd write, global state,
`JOURNAL_TEST_LOCK`, retention/pruning); the live `LaneTap` sink; the pure
reconciliation + outcome/status/green; the `--json` blocks. The plan types
are written by `prepare` and read by everyone; the journal is execution
infrastructure (sibling scope); reconciliation is pure. Split into
`plan.rs` (types, `LaneRecordWire`), `journal.rs`, `reconcile.rs`,
`report_json.rs`. `lane_runs_doctests` and `artifact_drift` are plan-domain
predicates living in the accounting file because that file happened to hold
`LaneKind`.

### 3.3 `lane_kind` lives in prepare, `LaneKind` in accounting
`LaneKind` is defined in `accounting.rs`, derived in `prepare.rs`, consumed
by `phase.rs`, `continuation.rs`, and the doctest predicate. Dependency
points accounting <- prepare for the type and prepare <- accounting for the
derivation. With 1.2, it belongs on the planned sweep.

### 3.4 `prepare.rs` (2.6k lines) mixes building, listing, policy
`prepare_profile` is a ~300-line loop that interleaves lane preparation,
stop handling, universe enumeration (policy), filter liveness (policy -
coverage's function called from prepare), pair minting, and shape-record
assembly. The module header says inventory and policy universe "are kept
apart in code because they are different claims", but they share this one
loop and its mutable `plan`/`ledger`/`universes`. Split: `inventory` (per
lane, always) and `policy` (per shape, certifying only) as two passes over
the prepared lanes; filter liveness and the ledger move wholly into
`coverage.rs`, which today owns `FilterLedger` but has it driven from
prepare.

### 3.5 `coverage.rs` owns enumeration argv builders
`shape_selection_args` / `shape_enumeration_args` assemble cargo argv
(profile, unification, run args, allows) - a second argv assembly point
beside the lanes'. Comments justify each piece by "the same as the lane".
The argv for "build this lane's shape" should be one function on the
`ResolutionRun` (+ planned sweep), used by both the lane prebuild and the
universe enumeration.

### 3.6 Selection's index-alignment invariant
"The sweep index is the lane identity everywhere" - `PhaseSelection.entries`
and `LaneRecord.lane` and `ReachLedger`'s `Vec<bool>`s are all aligned to the
`sweeps` slice by position, and `nothing_reached`/`reached_labels`/
`feature_drop_lines` zip with `sweeps` again. A `LaneIdx` newtype and a
`Lanes<T>` index-aligned container (constructed only from the sweeps slice)
would make a misaligned zip a type error rather than a silent truncation.

### 3.7 Tests mirror construction accidents
Many tests construct `ResolvedSweep { .., ..Default::default() }` directly
and then mutate `effective_unification` (`selection.rs` test helper
`package_mode`), building states production never reaches. With 1.1 the
tests would go through the one constructor.

---

## Lateral findings

- **Stale/contradictory docs:** `profile.rs` `parallel_budget` doc says the
  isolation/parallel exclusion is "enforced at resolve time";
  `phase.rs::reject_conflicting_lanes` says it is refused at run time
  "rather than at resolve time". One of them is wrong (the code says the
  latter).
- **Stale comment:** `phase.rs::reject_extra_args_complete` says "the plan
  takes each lane's selection from the sweep's own filters alone, without
  [forwarded args]", but `prepare::lane_filter_args` does fold forwarded
  libtest args into the listing. The refusal is still right (cargo-side
  forwarded args are not folded), but the stated reason is partly stale.
- **Unknown-member projection:** `features::project` returns the identity
  when any selected package is not a workspace member - correct for cargo's
  error, but it also means a typo in `-p` under a sweep with features gets no
  drop announcement at all, so the user sees cargo's feature error rather
  than the unknown-package one first, depending on cargo's check order.
- **`continuation::suspects`** treats only `unavailable` lanes (and
  superseded) as blind; a `partial` lane (unlisted binaries) can also only
  name suspects on an unattributed binary's stream, which then has no
  binary - check whether those terminations are dropped.
- **`shape_id` via `Debug`** is persisted in plan files; retention keeps
  only the newest few and nothing reads a plan after its run, so the
  instability is harmless today but becomes a bug the moment any cross-run
  reader appears.
- **`prepare_profile` complete flag:** `plan.complete` re-checks
  `l.prepared || l.kind == DocOnly` though DocOnly lanes already get
  `prepared = lane_ok = true`; the special case is dead or the invariant is
  not what it appears.
