# Design hunt: config-cli (config, CLI surface, dispatch, error model)

Scope read: `src/config.rs`, `src/config_parts/{parser,schema,compose,user}.rs`,
`src/project.rs`, `src/cli.rs` + `src/cli/{schema,validation,visibility}.rs`,
`src/main.rs` + `src/main_parts/{bootstrap,commands}.rs`, `src/man.rs`,
`src/error.rs`. Followed out to `src/context.rs`, `src/disk_gate.rs`,
`src/lockfile.rs` where a question led there.

Headline: the dispatch layer has no model of a *command*. Every property a
command has - which projects it applies to, whether it needs a `brokkr.toml`,
which root it anchors on, whether it takes the lock and under what label,
whether it opens a run log, which project section it requires - is decided
inline in a 1900-line `run()` match arm, and several of those properties are
also decided a second time elsewhere (the help table, the `man` topic table,
the handler's own `project::require`, help-text `[project]` tags). The biggest
payoff in this scope is a declarative command descriptor that owns all of
them, with `run()` reduced to "resolve the descriptor's needs, then call the
handler".

---

## 1. Axes that should be types

### 1.1 The two roots are both `&Path` / `PathBuf`, and are already swapped (bug)

`Detection` carries `project_root` (config dir: `.brokkr/`, data, results) and
`build_root` (cwd: cargo, git, `target/`). Both are `PathBuf`, and call sites in
`bootstrap.rs` rebind them under each other's names:

- `run`, `install`, `wc`, `deps`, `clippy`: `let (.., project_root) = ... d.build_root`
  - a local named `project_root` holding the build root.
- `check`: `project_root = d.build_root`, `state_root = d.project_root`.
- `test`: lock and run log on `project_root` (config dir), cargo on `build_root`.

The lock acquisition passes that path into `LockContext.project_root` (a
`&str`!), and `disk_gate::check` probes `<project_root>/target`
(`src/disk_gate.rs`, module header). So the free-space gate measures:

- `run`/`install`/`deps`/`clippy`: `<cwd>/target` - correct.
- `check` (`acquire_cmd_lock_opt(project, &state_root, "check")`) and `test`
  (`acquire_cmd_lock(project, &project_root, "test")`) and every
  post-detection command (`clean`, `download`, `serve`, ...): `<config dir>/target`.

Under the one-level-up layout (config in the parent, the layout `Detection`
exists for) the config dir has no `target/`, so the gate for the two heaviest
build commands probes the wrong directory - it fails open or measures the
parent's disk, never the disk cargo is about to fill. Nothing can catch this
because both roots are the same type and every function takes "a root".

Also `run_measured`, the `Verify` arm and the `Sync` arm each recompute the
build root from `std::env::current_dir()` and compare it to `project_root` to
derive `parent_build_root`, even though `detection.build_root` already holds
exactly that value - three re-derivations of a fact detection already
decided (see 2.6).

Fix: `struct ProjectRoot(PathBuf)` and `struct BuildRoot(PathBuf)` minted only
by `project::detect*` (and a `BuildRoot::cwd()` for the no-config case, which
also mints a `ProjectRoot` explicitly equal to it). `LockContext` takes a
`BuildRoot` for the disk gate and a separate display label; it should not be a
`&str` path at all. `with_worktree` takes `ProjectRoot` + `BuildRoot` instead of
`project_root` + `Option<&Path> parent_build_root`.

### 1.2 Dataset/variant are strings with sentinel values that already disagree

`run_measured(.., dataset: &str, variant: &str, ..)` receives:

- `"denmark"`, `"raw"` hard-coded for `PmtilesWriter`/`NodeStore` (inputs they
  may not even read), `"raw"` for `RunApi`/`RunTiles`.
- `""`, `""` for dellingr, mogwai and measured piners corpus ("so nothing
  invented lands in the DB").
- `"n/a"`, `"n/a"` for sluggrs `hotpath`.

So "this measured command has no dataset" is spelled two ways, and both land in
`results.db`'s `dataset` column where `brokkr results --dataset` and
`--compare` pairing read them. The comment on the dellingr arm states a policy
(empty, not invented) that the hotpath arm violates. Fix: `Option<DatasetRef>`
where `DatasetRef { dataset: DatasetName, variant: VariantName }`; `None` is
the only way to say "no dataset", and the DB layer owns the one rendering of
`None`. The `"denmark"`/`"raw"` defaults belong in the command definition (or
should be `None` too, if those benches do not read the dataset).

`PbfArgs.variant` / `VerifyPbfArgs.variant` are also free strings whose doc
says "(raw, indexed, locations)"; the variant set is a closed enum in practice
and in `PbfEntry` keys.

### 1.3 `--bench/--hotpath/--alloc` are three `Option<usize>` checked at runtime

`ModeArgs` has three independent `Option<usize>`s; `resolve_mode`
(`commands.rs`) counts them to enforce mutual exclusion and rejects `runs: 0`
after the fact, and `ModeArgs::is_measured` is a second reading of the same
three fields. clap can make these an `ArgGroup` (exclusive) with a
`NonZeroUsize` value parser, so `resolve_mode` becomes infallible and
`MeasureMode` is the only reading. Also `Hotpath` arm mutates
`mode.hotpath = Some(1)` to express "default mode for this command" - a
per-command default mode belongs on the command definition, not as a
field poke before `run_measured`.

### 1.4 `profile_override(debug, release) -> Option<bool>`

`Some(true)` = dev, `Some(false)` = release. Used by `test`, `service`,
`sync`, `corpus`, `lint-corpus`. A bool whose `true` means "debug" is
swappable with any other `Option<bool>` the handlers take. `run`/`install`
take `debug: bool, release: bool` raw instead and resolve them a third way
inside `runnables`. One `enum BuildProfile { Dev, Release }` plus a clap
`Args` struct (`ProfileArgs { --debug | --release }`) flattened into every
command that has the pair, resolved once.

### 1.5 `Project::Other(&'static str)` leaked, and `Other("")` as "unknown"

`config::load` leaks the TOML string to get `&'static str`. `man` dispatch
uses `Project::Other("")` to mean "no project detected", which is not a
project. Visibility rules then compare by discriminant precisely so the
payload never matters. Suggest `Project::Other(Box<str>)` (or an interned
`ProjectName`), drop `Copy`, and make "no project" `Option<Project>` at every
site (`acquire_cmd_lock_opt` already takes `Option<Project>`; `man` should
too). `is_builtin` is `#[allow(dead_code)]` - dead.

### 1.6 Project name <-> enum mapping written twice

`parser.rs::load` has `"litehtml-rs" => Project::Litehtml, ...` and
`Project::name` has the inverse. Two hand-maintained tables that must be exact
inverses; nothing tests the round trip. Put `FromStr` next to `name()` on
`Project` (or a single `const BUILTINS: &[(&str, Project)]` both derive from).

### 1.7 Config fields that are domain values held as strings

- `PbfEntry/OscEntry/PmtilesEntry.xxhash: Option<String>`, dellingr/mogwai
  pins, datasets `xxh128`: a digest type (`Xxh128([u8;16])`, parsed and
  length-checked at load) would make a sha256 pasted in impossible - AGENTS.md
  explicitly warns that delivery manifests carry sha256.
- `TilegenConfig.sort_budget/way_budget/assemble_budget: Option<String>`,
  `compress_sort_chunks`, `tile_format`, `tile_compression`: parsed later
  somewhere downstream; should be typed at load (byte sizes, enums).
- `VersionAlign.granularity: String`, `LitehtmlConfig.mode: String`,
  `LitehtmlFixture.mode/expected: String`: closed sets.
- `RatatoskrConfig.test_endpoint_env_{jmap,imap,smtp,graph,gmail,caldav,carddav,people,gcal}`:
  nine sibling `Option<String>` fields keyed by protocol. Should be
  `BTreeMap<Protocol, EnvName>` with `Protocol` an enum the sync code also
  uses; today adding a protocol means a new field, a new parser line and a new
  consumer branch.
- Lint names (`LintsConfig.allow: Vec<String>`, `SitedAllow.lint: String`) are
  classified by string prefix in three free functions
  (`reaches_test_build`, `is_cargo_lint`, plus the prefix tests inside
  `test_phase_allow_flags`). A `LintName { tool: Rustc|Clippy|Rustdoc|Cargo, name }`
  parsed at load makes "which phase can this allow reach" a method on the type.
  `cmd_check` and `cmd_clippy` both receive `allow` and `allow_exact` as two
  separate slices unbundled from `LintsConfig` at the call site in
  `bootstrap.rs` (`clippy_cfg.as_ref().map_or(&[][..], |c| &c.allow)` twice
  each) - pass `&LintsConfig`.
- `hosts: HashMap<String, HostConfig>` keyed by hostname, looked up by
  `config::hostname()` at each consumer (`worktree_keep(&config::hostname()?)`
  appears four times in `bootstrap.rs`). Resolve "this host's config" once at
  detection into `Detection.host: Option<&HostConfig>` (or a resolved
  `HostView`), so no handler can look up the wrong host or forget the
  fallback.

### 1.8 `--strategy all` as a string sentinel

`Extract`/`MultiExtract` arms compare `strategy == "all"` and otherwise
`ExtractStrategy::parse` at runtime inside the closure. Should be a clap
`ValueEnum` `StrategySel { All, One(ExtractStrategy) }`, parsed before
anything is built. Same for `Suite.name` (`"pbfhogg" | "elivagar" | "nidhogg"`
string match with a hand-written "expected:" list) and `Sync --gate` compared
to `GATE_ALL`.

### 1.9 The error model: categories that are not outcomes

`DevError` is mostly `Variant(String)`: `Config`, `Build`, `Lock`,
`Database`, `Verify`, `Refused`, `Reported`, `CargoEnv`. Observations:

- `Config` is used for everything that is not obviously something else:
  "cannot determine current directory", "--dry-run is not yet supported",
  "'visual' is only available for ...", "internal error: a pre-detection
  command reached project dispatch", "run count must be >= 1", clap-level
  argument conflicts. A caller cannot tell a malformed `brokkr.toml` from a
  wrong-project invocation from a dispatch-ordering bug. At minimum split:
  `Config` (file), `Usage` (argv combination clap could not express),
  `WrongProject { command, expected: Vec<Project>, current }`,
  `Internal` (contract break).
- `WrongProject` as a typed variant would let `project::require`, the four
  hand-written "only available for litehtml/sluggrs projects" messages and
  `cmd_pmtiles_stats`'s own refusal produce one rendering (see 2.1).
- `From<serde_json::Error> for DevError` maps to `Build("json: ...")`: any JSON
  parse anywhere (results output, sidecar, harness NDJSON) becomes a "build"
  error. That is a wrong-category conversion hidden behind `?`.
- Exit status is decided by `main` from the variant (`ExitCode(n)`,
  `Interrupted` -> 130, else 1), but 124 (watchdog) and regress/corpus's
  `INCOMPLETE_EXIT` are threaded as `ExitCode(n)` built elsewhere
  (`elivagar::cmd::operational`). "Which exit status does this outcome map to"
  is therefore answered in `main`, in check's watchdog path, and in
  elivagar's `operational` wrapper. A typed `Outcome`/`ExitStatus` enum
  (`Ok`, `Failed`, `Incomplete`, `TimedOut`, `Interrupted`, `Child(i32)`) owned
  by `error.rs` would make the map one function.
- `already_printed()` keys off two variants (`Reported`, `CargoEnv`);
  `Refused` renders bare but is "first print". These are really a
  `render: AlreadyPrinted | Bare | Prefixed(category)` property, now encoded
  three ways (variant choice, `Display` arm, `already_printed`). Make the
  rendering policy one method returning an enum.
- `Subprocess { code: Option<i32>, stderr }` where `stderr` sometimes holds
  "killed by signal 9" and `Display` sniffs `starts_with("killed by signal")`.
  The signal is a domain fact smuggled through a prose field and recovered by
  string match. Carry `ended: ExitStatus { Code(i32) | Signal(i32) }`.

---

## 2. Decisions made in more than one place

### 2.1 "Does command X apply to project P?" - five answerers

1. `cli/visibility.rs::TABLE` (help visibility; its header says it "must be
   kept in agreement" with the handlers - the drift is acknowledged).
2. Handler-side `project::require(...)` calls, scattered (`corpus`, `sync`,
   `service`, `hotpath`, `lint-*`, `verify` subcommands, `suite <name>`,
   and inside pbfhogg/elivagar/nidhogg dispatch).
3. Hand-written `match project { Litehtml => .., Sluggrs => .., other => Err("'visual' is only available ...") }`
   in `Visual`, `List`, `Report`, `VisualStatus`, `cmd_approve` - five copies,
   each with its own message text.
4. `cmd_pmtiles_stats`'s own `matches!(project, Elivagar | Nidhogg)` check.
5. The `/// [pbfhogg]`, `/// [litehtml/sluggrs]` prefixes on 85 doc comments
   in `cli/schema.rs`, which are what `--help` prints.

Plus 6: `man::TOPICS` visibility, which for `results`/`visual`/`hotpath`
explicitly "mirrors the subcommand's own visibility" by hand.

The only cross-check is `require_labels_are_real_commands`, which validates
that a label string names a command - not that the project matches the table.
Disagreements to look for today: `hotpath` is `Only(Sluggrs)` in TABLE and
required in the handler, but `generic-hotpath` is `Any` and its handler has no
gate; `Prepare`/`HtmlExtract`/`Outline` have no handler-side gate in
`bootstrap.rs` at all (they reach `litehtml::cmd::*` with whatever project and
`Prepare` only checks for a `[litehtml]` section) - whether they refuse in a
non-litehtml tree depends on code outside this scope.

Single owner: a per-command descriptor (`CommandSpec { name, applies: Visibility, ... }`)
attached to the `Command` enum (a `fn spec(&self) -> &'static CommandSpec`
exhaustive match, so a new variant is a compile error, not a fail-open table
miss). The gate runs once in dispatch before the handler; the help filter, the
`[project]` help prefix and `man` topic visibility (topic -> command link)
all read the spec. Handlers stop gating.

### 2.2 "Which config section does this command need, and is it present?"

Project enum and project config section are independent axes:
`DevConfig` holds `litehtml`, `sluggrs`, `ratatoskr`, `piners`, `dellingr`,
`mogwai` as six `Option`s parsed for every project. So a handler must check
both "is the project litehtml" and "is `[litehtml]` present", and does so
inline with repeated `.ok_or_else(|| DevError::Config("no [litehtml] section in brokkr.toml"))`
(seven copies across `Visual`, `List`, `VisualStatus`, `Prepare`,
`cmd_approve`, `MockServe` with a different wording). Meanwhile a pbfhogg
tree can carry a `[litehtml]` section that is parsed, validated and ignored.

Fix: a `ProjectConfig` enum - `Litehtml(LitehtmlConfig)`, `Sluggrs(SluggrsConfig)`,
`Ratatoskr(RatatoskrConfig)`, ... `Generic` - built by `load` from the
`project` key. A section for a different project is a load error; a missing
required section is a load error once, not a per-command one. Detection then
yields one value from which `Litehtml`-ness and its config cannot disagree.
The visual-command arms collapse to `match detection.project_config { Litehtml(cfg) => .., Sluggrs(cfg) => .. }`.

### 2.3 "Which root does a command anchor on / is a config needed?"

Each pre-detection arm in `run()` independently decides
`detect_optional()?` vs no detection vs `detect()`, and which root to use, with
a comment explaining its choice. As shown in 1.1, they disagree (lock path
for `check`/`test` vs `run`/`clippy`). This is a property of the command, so it
belongs on the descriptor: `config: Required | Optional | None`,
`anchors: { lock/disk gate: BuildRoot, state: ProjectRoot }` - the latter
should not vary per command at all.

### 2.4 "Does this command take the global lock, with which label?"

Each arm decides, with a free-text label (`"run"` for `Passthrough`,
`"verify pmtiles"`, `"regress"`). `Stop`, `Status`, `Query`, `Geocode`,
`Results`, `Sidecar`, `Invalidate`, `CompareTiles`, `PmtilesStats`, `List`,
`Report`, `Approve`, `Prepare` take none; `Approve` (which rebuilds?) and
`Prepare` (runs Node) are the kind of thing the `Visual` arm's comment says was
"the one cargo-driving command reaching its handler unlocked" last time.
`context::invoked_command()` exists as a third way to name the lock holder.
Lock policy (`None | Blocking | NonBlocking`) and label (derived from the
command name) belong on the descriptor; dispatch acquires it, so an unlocked
cargo-driving handler is a descriptor field someone chose, visible in one
table.

### 2.5 "How does a named config entry shadow another with the same name?"

Two implementations of the same layering rule:

- `config_parts/compose.rs::merge_entries` - on raw TOML, include-graph
  layers, later layer's named `[[array]]` entry replaces earlier one, unnamed
  entries always survive.
- `config_parts/user.rs::merge_named` - on parsed `Vec<TextlintRule>` /
  `Vec<ScriptCheck>`, user layer first, project entry replaces user entry of
  the same name.

They agree today. But the user layer is just "the lowest-precedence include,
restricted to three keys", and it is applied *after* parsing, which has two
consequences: (a) the user file's `[textlint_preset.*]` are resolved only
against the user's own rules and the project's presets only against the
project's - neither layer can use the other's presets, unlike includes, which
are merged before preset resolution; (b) `check`'s no-config branch has to
call `config::load_user()` itself and hand-assemble a 15-tuple, a third site
applying the user layer. Fix: make the user file the bottom layer of
`compose()` (with its key allow-list enforced in compose), delete
`merge_named` and `apply_user_layer`, and let the no-config path compose a
config from the user layer alone, yielding a normal `DevConfig`.

### 2.6 "Is the config parsed exactly once?" - documented yes, actually up to 4x

`config::load` claims to be "the single code path" and `project::detect` says
`brokkr.toml` "is read and parsed exactly once". In practice one invocation
runs: `parse_cli` -> `detect_optional()` (for help filtering); `run()` ->
`detect_optional()?` (to arm `disable_toolchain`); then the per-command
`detect_optional()?` or `detect()`; plus `Fmt` and `Man` each call it again
in their own branches, and the `Interrupted` path in `main` calls `detect()`
once more. Each call also re-reads the user config and calls
`cargo_env::install` again. Besides cost, it means three different
fallibility policies on one file: `parse_cli` swallows errors, `Man`
swallows errors, everything else propagates. And the toolchain arming reads a
*different* parse than the command later uses (harmless today, because the
file does not change mid-run, but the structure invites it). Fix: detect once
in `main`, as `Result<Option<Detection>, DevError>`, hand the result to help
filtering, toolchain arming, and dispatch; each command's descriptor says
whether an `Err` is fatal for it.

### 2.7 "Which features does a build get?"

`resolve_features` (host features + CLI features) is called in `run_measured`,
`Passthrough`, `Verify`, `Serve`, `Ingest`, `Update`; other build paths
(`run`, `install`, `check`, `clippy`, `test`, `bench`) do not consult host
features at all. Whether `[<host>] features` applies to a command is thus
decided by whether its arm remembered to call the helper. Either it is a
property of build-through-`BenchContext` (and should be applied there), or of
the command (descriptor). Today it is neither.

### 2.8 Exit-status mapping in `main` is written twice in a row

`main` computes `exit_code` from `result` for history, then matches `result`
again to decide `process::exit(...)`. Same three arms, held in step by
adjacency. One `fn exit_status(&Result<..>) -> i32` (or the `Outcome` type
from 1.9) used for both.

---

## 3. Structure

### 3.1 `main_parts/` is `include!`d, and `commands.rs` is a grab bag

`main.rs` `include!`s `bootstrap.rs` and `commands.rs`, so they are not
modules: no privacy boundary, no `use` hygiene, no module docs (`//!`
impossible), and everything in them is visible to everything else in the
crate root. `cli.rs` does the same with three files for the same reason
(`visibility.rs` notes it must use `//` comments). Make them real modules.

`commands.rs` (1600 lines) contains, unrelated to each other and to
dispatch: the `clean` implementation including per-project artefact rules
(`if project == Project::Elivagar`, `Nidhogg`, `Ratatoskr`, `Dellingr`,
`Piners`) and a `/proc` owner-liveness check (`join_dir_owner_alive`,
`proc_start_epoch`); `lock`/`kill` including pidfd signalling and
preflight of kill targets; `passthrough`'s run/timing stats; `env`;
`pmtiles-stats`; `verify` routing; generic hotpath. Each should live with its
domain:

- `kill`/`lock` status -> `lockfile`/`lock_service` (they already own
  identity verification and pidfds; the `/proc` starttime work in
  `commands.rs` is a second copy of that competence).
- `clean` -> `src/clean.rs`, with each project contributing its own clean
  rules (a `fn clean_targets(&ResolvedPaths) -> Vec<CleanTarget>` per project
  module) rather than `clean` branching on every project.
- `env` -> `src/env.rs` (which exists).

### 3.2 `run()` is a 1750-line function in two halves with an ordering contract

The first half is a chain of `if let Command::X = cli.command { return ...; }`
for "pre-detection" commands, then `detect()`, then a `match` whose first two
arms exist only to error if the chain was reordered ("internal error: a
pre-detection command reached project dispatch"). That contract is a type
that has not been written: split `Command` into the needs it has, e.g. a
descriptor `needs: NoProject | OptionalProject | Project`, and dispatch on the
need first. Or split `Command` handling into one `fn handle(self, ctx: Ctx<'_>)`
per command family module (`pbfhogg::cli`, `elivagar::cli`, `ratatoskr::cli`,
...) where `Ctx` is built once with exactly the detection/lock/run-log the
descriptor asks for. Then the "internal error" arms and the
`as_pbfhogg() -> None` safety arm disappear.

The project-specific argument plumbing (`Sync`'s mode resolution and
`validate_gate_selection`, `Corpus`'s `--force` refusal, `Service`'s
bare-is-an-index branching, `Suite`'s per-name require) is command logic
living in the dispatcher. It belongs in each project's module; the
dispatcher's job is resolving the context.

### 3.3 `cli/schema.rs` (3250 lines) is the whole surface in one enum

One flat `Command` enum with ~85 variants, project tags in doc strings, and
per-project arg structs (`PbfArgs`, `VerifyPbfArgs`, `ModeArgs`). Project
commands should be defined next to their project (`src/pbfhogg/cli.rs`
defining `PbfhoggCommand` args, flattened into the top-level enum via
`#[command(flatten)]` or `#[clap(subcommand)]`-free `Subcommand` derive on
per-project enums merged at the top). That moves the definition of
`brokkr tilegen` next to `elivagar::dispatch`, and lets each project module
own its `CommandSpec`s. `as_pbfhogg()` (in `cli/`, mapping CLI variants to
`PbfhoggCommand`) is a pbfhogg concern living in the CLI module - the
dependency edge points the wrong way (cli -> pbfhogg internals).

### 3.4 `config_parts/schema.rs` holds check-phase behaviour

`schema.rs` defines not only config types but `test_phase_allow_flags`,
`reaches_test_build`, `is_cargo_lint`, `rustflags_target_key`,
`shape_target_key`, `EffectiveUnification::cargo_args/describe/is_per_package`,
`SweepProfile::target_subdir/cargo_args`, `ParallelBinaries::resolved_budget`.
Those are decisions of `check_cmd` (what flags a build gets, which target dir
a shape uses) living in the config module, so `config` depends on knowledge of
cargo invocation shape and `check_cmd` must reach into `config` for its own
rules. Keep `schema.rs` as data + validation; move build-shape derivation
into `check_cmd` (e.g. `check_cmd/shape.rs`).

Likewise `parser.rs` holds `captured_env_pairs`/`matches_capture` (measurement
DB concern), `resolve_paths`/`host_features`/`all_dataset_keys` (path
resolution, which `src/resolve.rs` exists for), and `hostname()` (an OS
query). The parser is also where cross-section validation of `[[check]]` vs
`[test]` vs `[[quarantine]]` (profiles, complete-universe rules, doctest
ledger - ~600 lines) lives; that is `check`'s policy and arguably belongs in
`profile.rs`/`check_cmd`, invoked from load, so the config module stops being
the place that must change whenever check's certification rules change.

Per-project sections (`LitehtmlConfig`, `RatatoskrConfig`, `PinersConfig`,
`DellingrConfig`, `MogwaiConfig`, `TilegenConfig`) are defined in the shared
schema rather than in their project modules. With `ProjectConfig` (2.2), each
project module could own its section type and parser, and `config` would
only dispatch on `project`.

### 3.5 Unknown top-level keys in `brokkr.toml` are silently ignored

`load` pulls known keys out of the composed table with `parse_*` functions;
nothing rejects an unrecognised top-level key. Section structs use
`deny_unknown_fields`, but a typo'd *table name* (`[lint]`, `[[checks]]`,
`[gremlin]`, `[[script_checks]]`) parses clean and switches the feature off.
The user-wide layer, by contrast, rejects every key outside its allow-list,
with a comment explaining that silently ignoring a typo "would mean a typo
quietly switches off every rule the user wrote" - the same argument applies
with more force to the project file. Hostname tables (`[<host>]`) are what
makes this non-trivial: any unknown key is currently a candidate hostname.
Fix: a known-key set plus the rule that a host table must be a table whose
keys are host-section keys, or make hosts explicit (`[host.<name>]`).
Worth checking whether `parse_hosts` treats every unrecognised table as a
host; if so, `[lint]` becomes a host named "lint" with no error.

### 3.6 `man` topic table and AGENTS.md list are parallel

`man.rs`'s header says the AGENTS.md doc list is "prose and nothing verifies
it" and `TOPICS` is the executable version. AGENTS.md also says "When
editing this list, update `TOPICS` to match". The two are hand-synced; a test
that every `docs/**.md` file is either in `TOPICS` or explicitly excluded
would at least catch a new doc missing from `man`.

---

## Lateral findings

- Probable bug (1.1): free-space gate for `check`, `test` and every
  post-detection locked command probes `<config dir>/target`, not the build
  root's `target`, under the one-level-up layout.
- `Project::is_builtin` is dead (`#[allow(dead_code)]`).
- `profile_override`'s doc comment in `commands.rs` is preceded by an orphaned
  doc comment for `resolve_features` ("Merge CLI `--features` with
  host-configured default features..."), so rustdoc attaches both paragraphs
  to `profile_override` and `resolve_features` has none.
- `From<serde_json::Error>` -> `DevError::Build("json: ..")` mislabels
  non-build JSON failures.
- `sync` computes `parent_build_root` from a fresh `current_dir()` call rather
  than `detection.build_root`; if cwd were unreadable it errors as
  `DevError::Config`, a third spelling of the same failure that
  `project::detect` already handled.
- `cmd_approve` and four other arms re-derive "no [litehtml] section" with
  slightly different texts; `MockServe` has a long bespoke one. One typed
  `MissingSection(&'static str)` would unify them (moot with 2.2).
- The `Passthrough` arm takes the lock labelled `"run"`, so `brokkr lock`
  reports a `passthrough` invocation as `run`.
