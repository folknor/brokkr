# brokkr CLI redesign

Synthesized from two independent design reviews (one internal, one external) that converged on the same core insight: measurement mode should be a flag on a command, not a separate command family.

## What's wrong today

Three pain points, all structural:

1. **Three entry points for the same work.** `brokkr bench`, `brokkr hotpath`, and `brokkr profile` are three ways to measure the same command. Adding build-geocode-index required touching 5-7 files across all three paths with near-identical argument wiring. The command identity is expressed three times: `bench_commands.rs`, `hotpath.rs`, and `cli.rs`/`main.rs`.

2. **`brokkr run` is a dumb passthrough.** Users type full file paths even though datasets are configured in `brokkr.toml`. The argument knowledge exists in `bench_commands.rs`'s `command_args()`, but it's locked inside the benchmark path.

3. **Mode-centric, not command-centric.** The user thinks "I want to measure add-locations-to-ways." The CLI thinks "are you benchmarking, hotpathing, or profiling?" The mental model is backwards.

## Proposed CLI surface

One verb, one entry point per measurable command:

```
brokkr run <command> [command options] [measurement options]
```

Measurement options (mutually exclusive, default is wall-clock):

- *(none)* — wall-clock benchmark, stored in results DB
- `--hotpath` — function-level timing via hotpath feature
- `--alloc` — per-function allocation tracking via hotpath-alloc feature
- `--profile` — two-pass: timing then alloc (pbfhogg), or `--profile=samply` for sampling profiler (elivagar)

Common flags stay top-level: `--runs N`, `--commit`, `--features`, `--force`, `--verbose`, `--no-mem-check`.

### Why `run`, not `bench`

`run` is more honest. You're running a command and choosing how to measure it. `bench` implies wall-clock benchmarking specifically, but hotpath/alloc/profile are different activities. `run` already exists as a top-level command — it becomes the unified entry point instead of a dumb passthrough.

### Before / after examples

pbfhogg CLI commands:

```
# before
brokkr bench commands add-locations-to-ways --dataset europe --index-type external
brokkr hotpath --test add-locations-to-ways --dataset europe
brokkr hotpath --alloc --test add-locations-to-ways --dataset europe

# after
brokkr run add-locations-to-ways --dataset europe --index-type external
brokkr run add-locations-to-ways --dataset europe --index-type external --hotpath
brokkr run add-locations-to-ways --dataset europe --index-type external --alloc
```

Dedicated benchmarks:

```
# before
brokkr bench build-geocode-index --dataset denmark
brokkr hotpath --test build-geocode-index --dataset denmark

# after
brokkr run build-geocode-index --dataset denmark
brokkr run build-geocode-index --dataset denmark --hotpath
```

Dataset-aware run (eliminates manual file paths):

```
# before
brokkr run -- add-locations-to-ways data/europe-20260301-seq4714-with-indexdata.osm.pbf -o data/scratch/altw.osm.pbf

# after
brokkr run add-locations-to-ways --dataset europe --index-type external
```

Elivagar:

```
# before
brokkr bench self --dataset denmark --no-ocean
brokkr hotpath --dataset denmark (in elivagar project)

# after
brokkr run tilegen --dataset denmark --no-ocean
brokkr run tilegen --dataset denmark --no-ocean --hotpath
```

Suites:

```
# before
brokkr bench all --dataset denmark

# after
brokkr run suite pbfhogg --dataset denmark
brokkr run suite pbfhogg --dataset denmark --hotpath
```

Passthrough escape hatch (for ad-hoc / unknown commands):

```
brokkr run -- some-new-command data/file.osm.pbf --whatever
```

### Synthetic names vs command + flags

Where a pbfhogg CLI command has natural flags, prefer those over synthetic brokkr command IDs:

```
# prefer this
brokkr run cat --dataset denmark --type relation
brokkr run inspect-tags --dataset denmark --type way

# over this
brokkr run cat-relation --dataset denmark
brokkr run inspect-tags-way --dataset denmark
```

Synthetic IDs are still acceptable where the flagful version would be awkward. This isn't a hard rule — optimize for what reads well at the terminal.

## Command categories

Known measurable commands fall into five categories:

### 1. Tool CLI commands (pbfhogg)

First-class `brokkr run <command>` subcommands. These are the 25+ pbfhogg CLI commands currently in `bench_commands.rs`, plus standalone commands like `build-geocode-index`.

Examples: `add-locations-to-ways`, `build-geocode-index`, `apply-changes`, `extract`, `cat`, `check-refs`, `inspect-tags`, `sort`, `tags-filter`, `merge-changes`, `diff`.

### 2. Tool pipelines (elivagar)

The elivagar full pipeline becomes `brokkr run tilegen`. It owns all pipeline-specific options (`--no-ocean`, `--force-sorted`, `--tile-format`, `--skip-to`, etc.) as command-specific flags — not promoted to top-level globals.

### 3. Microbench examples (elivagar)

Distinct measured commands with their own typed options:

```
brokkr run pmtiles-writer --tiles 500000
brokkr run node-store --nodes 50
```

`--hotpath` and `--alloc` remain valid because measurement mode is orthogonal.

### 4. Comparison baselines

External tools stay as first-class measurable commands, not hidden under a benchmark family:

```
brokkr run planetiler --dataset denmark
brokkr run tilemaker --dataset denmark
```

### 5. Suites

Explicit suite commands for running batteries of measurements:

```
brokkr run suite pbfhogg --dataset denmark
brokkr run suite elivagar --dataset denmark
```

These replace the current `bench all` and `bench commands all`. Suite membership is declared in the command registry.

## Command registry

A declarative table describes each command once. It drives CLI exposure, argument resolution, build requirements, measurement compatibility, and dispatch.

```rust
struct CommandSpec {
    /// CLI name: "add-locations-to-ways", "build-geocode-index", "tilegen"
    id: &'static str,
    project: Project,
    category: CommandCategory,  // ToolCli, Pipeline, Microbench, Baseline

    /// What inputs this command needs.
    input: InputKind,           // Pbf, PbfAndOsc, TilesOnly, None
    /// What this command produces (for scratch output and cleanup).
    output: OutputPolicy,       // ScratchPbf("name"), ScratchDir("name"), DevNull, None

    /// Which measurement modes are supported.
    supports_hotpath: bool,
    supports_profile: bool,

    /// Build target. None for external tools (planetiler, osmium).
    package: Option<&'static str>,

    /// Default dataset and variant.
    default_dataset: &'static str,
    default_variant: &'static str,
    default_runs: usize,

    /// Command-specific parameters (beyond dataset/variant/runs).
    /// These become clap args on the subcommand.
    params: &'static [ParamSpec],

    /// Builds the argument vector from resolved context.
    build_args: fn(&CommandContext) -> Result<Vec<String>, DevError>,

    /// DB labels for result storage.
    result_command: &'static str,
    result_variant: Option<&'static str>,

    /// Suite membership.
    suites: &'static [&'static str],  // ["pbfhogg"], ["elivagar"], []
}
```

Not every field needs to be a literal Rust struct field. The important split:

- **Declarative in the registry:** identity, options, defaults, required inputs, compatible modes, suite membership.
- **Imperative in executor code:** how to turn resolved inputs into argv and which harness method to call.

### What adding a new command looks like

Today (build-geocode-index): clap enum variant + dispatch arm + cmd handler + bench module + hotpath test entry + TEST_LABELS update. Six places.

After:

```rust
CommandSpec {
    id: "build-geocode-index",
    project: Project::Pbfhogg,
    category: CommandCategory::ToolCli,
    input: InputKind::Pbf,
    output: OutputPolicy::ScratchDir("geocode"),
    supports_hotpath: true,
    supports_profile: true,
    package: Some("pbfhogg-cli"),
    default_dataset: "denmark",
    default_variant: "indexed",
    default_runs: 3,
    params: &[],
    build_args: |ctx| Ok(vec![
        "build-geocode-index".into(),
        ctx.pbf_str()?.into(),
        "--output-dir".into(),
        ctx.scratch_dir.join(format!("geocode-{}", ctx.dataset)).display().to_string(),
        "--force".into(),
    ]),
    result_command: "run",
    result_variant: Some("build-geocode-index"),
    suites: &["pbfhogg"],
}
```

One place. Covers wall-clock, hotpath, alloc, profile, suite membership, and `brokkr run` argument resolution.

### Multi-variant commands

Commands like `extract`, `read`, and `write` that run the same tool command multiple times with varying parameters become commands with explicit variant flags:

```
brokkr run extract --dataset japan --strategy simple
brokkr run extract --dataset japan --strategy complete
brokkr run extract --dataset japan --strategy smart
brokkr run extract --dataset japan --strategy all     # fans out internally
```

This is more honest than hiding three variants behind a separate benchmark family.

## Dispatch layer

The current dispatch split (`cmd_bench` / `cmd_hotpath` / `cmd_profile` in `main.rs`) collapses into:

1. Parse a `MeasureRequest` — command ID, measurement mode, common build flags, command-specific params.
2. Look up `CommandSpec` in the registry.
3. Resolve inputs — dataset, variant, PBF path, OSC path, bbox, scratch outputs.
4. Build with appropriate features — release (wall-clock), hotpath, or hotpath-alloc.
5. Call the command's `build_args` to get the argument vector.
6. Select measurement runner based on mode — `run_external` (wall-clock), `run_hotpath_capture` (hotpath/alloc), or two-pass (profile).

```rust
enum MeasureMode {
    WallClock,
    Hotpath,
    Alloc,
    Profile(ProfileKind),  // TwoPass or Sampling(perf/samply)
}
```

This replaces `BenchRequest` / `HotpathRequest` / `ProfileRequest` with a single `MeasureRequest` carrying a `MeasureMode`.

## Profile semantics

Profile is deliberately asymmetric across projects, unified at the UI level:

- **pbfhogg:** `--profile` means two-pass (timing + alloc). This is what `profile.rs` does today.
- **elivagar:** `--profile` means sampling profiler. `--profile=samply` selects samply over perf.

The model is "profile this command" — not "all projects implement profile identically." Documentation should be clear about what profiling means per project.

## Elivagar specifics

Elivagar needs a broader command model than pbfhogg. Recommended command IDs:

| Current | Proposed |
|---|---|
| `bench self` | `run tilegen` |
| `bench pmtiles` | `run pmtiles-writer` |
| `bench node-store` | `run node-store` |
| `bench planetiler` | `run planetiler` |
| `bench tilemaker` | `run tilemaker` |

`tilegen` owns all pipeline options (`--no-ocean`, `--force-sorted`, `--tile-format`, `--compression-level`, etc.) as command-specific flags. These do not become top-level globals.

Microbench examples build cargo examples, not the main binary. They use `HarnessContext` (no standard build). They fit the registry model (they have an ID, they support hotpath, they produce results) but they don't benefit from dataset path resolution.

Do not force elivagar into pbfhogg's "tool subcommand + dataset + scratch output" shape. The registry accommodates different command shapes through different `build_args` implementations and different `InputKind`/`OutputPolicy` values.

## Migration path

### Phase 1: Registry behind current commands

- Build `CommandSpec` table for pbfhogg and elivagar measurable commands.
- Reuse current modules as executors — the registry calls `bench_commands::command_args()`, `bench_build_geocode_index::run()`, etc.
- Eliminate the separate hotpath test registry (`TEST_LABELS`, `build_test_suite()`) by generating hotpath-capable commands from the registry.
- Keep `bench`, `hotpath`, `profile` public and working.

### Phase 2: Canonical `brokkr run <command>` surface

- Add typed `run` subcommands for known measurable commands.
- Add `--hotpath`, `--alloc`, `--profile` measurement mode flags.
- Keep `brokkr run -- <raw args>` as escape hatch.
- Resolve datasets and scratch outputs for known commands.

### Phase 3: Migrate remaining projects

- Add nidhogg commands to the registry (`api`, `ingest`, `tiles`, plus `serve`/`stop`/`status` as non-measured commands).
- Add sluggrs and litehtml hotpath support to the registry.
- All projects must work through the new surface before proceeding.

### Phase 4: Deprecate mode-centric entry points

- `bench`, `hotpath`, `profile` become thin aliases with deprecation warnings.
- `hotpath --test ...` disappears because command IDs are the test selector.
- `bench commands ...` disappears because those commands are first-class.
- Only happens after Phase 3 — the binary must not ship with broken projects.

### Phase 5: Simplify internals

- Delete `HotpathTest`, `build_test_suite()`, `TEST_LABELS` from `hotpath.rs`.
- Collapse `BenchRequest` / `HotpathRequest` / `ProfileRequest` into `MeasureRequest`.
- Trim `cmd_bench` / `cmd_hotpath` / `cmd_profile` branching in `main.rs`.
- Delete standalone bench modules (`bench_build_geocode_index.rs`, etc.) whose logic now lives in registry closures.

Each phase is independently shippable. The registry can call old handlers during the transition. Phases 1-3 are additive — old commands keep working. Phase 4 (deprecation) requires all projects to be migrated first. No partial installs with broken projects.

## Trade-offs and rough edges

### What gets better

- Adding a command is one registry entry, not 5-7 files.
- No duplicated argument vectors between bench and hotpath.
- Users discover measurement modes via `--help` on any command.
- `brokkr run` becomes useful for daily development, not just benchmarking.
- Flat help surface: `brokkr run --help` shows all measurable commands.
- Command names become first-class, discoverable identifiers.

### What gets harder

- `run` becomes more ambitious — clap structure gets denser, help text quality matters more.
- Naming consistency matters more because command IDs are user-facing.
- Profile is semantically different across projects. The UI unifies it but docs must explain the difference.

### Rough edges

- **Multi-variant benchmarks** need explicit variant flags or an `--all` mode. `extract`, `read`, `write` are not single-invocation commands.
- **Synthetic command names** like `cat-way` and `inspect-tags-way` need decisions: decompose into command + flags where natural, keep synthetic IDs where the flagful version is awkward.
- **Microbench examples** (pmtiles-writer, node-store) fit the registry but don't benefit from dataset path resolution.
- **Suite commands** are special — they fan out over multiple registry entries. They're worth keeping as explicit `run suite` commands.
- **External baselines** (planetiler, osmium) don't build a cargo binary. They fit the registry with `package: None` but use different build/run paths.

### What this design does NOT change

- Database schema, result formatting, comparison queries.
- `BenchHarness`, `BenchConfig`, `run_external()`, `run_internal()`, `run_hotpath_capture()`.
- Project detection, `brokkr.toml` parsing, path resolution.
- `check`, `env`, `clean`, `results`, `history`, `pmtiles-stats` commands.
- Litehtml, nidhogg, sluggrs commands (out of scope).
