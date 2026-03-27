# Brokkr Post-Refactor Code Review

Comprehensive architectural review — 2026-03-27.

## 1. Three Request Types That Should Be One

**Files:** `src/request.rs`, `src/measure.rs`, `src/dispatch.rs`, `src/main.rs`

Three request structs that are 90% identical:

- `MeasureRequest` (the new unified one in `measure.rs`)
- `BenchRequest` (legacy, in `request.rs`)
- `HotpathRequest` (legacy, in `request.rs`)

The refactor to `MeasureRequest` was started but never finished. The legacy types are still used in all of these call sites:
- `pbfhogg::cmd::bench_read`, `bench_write`, `bench_merge`
- `elivagar::cmd::bench_self`, `bench_pmtiles`, `bench_node_store`, `bench_planetiler`, `bench_tilemaker`, `bench_all`
- `nidhogg::cmd::bench_api`, `bench_ingest`, `bench_tiles`
- The entire `suite` command in `main.rs` (3 BenchRequest constructions)
- All nidhogg hotpath via `dispatch::run_nidhogg_command`

The dispatch layer *converts* between them — `dispatch.rs:517-527` constructs a `BenchRequest` from a `MeasureRequest`, and `dispatch.rs:576-588` constructs a `HotpathRequest` from a `MeasureRequest`. Pure overhead.

**Fix:** Kill `BenchRequest` and `HotpathRequest`. Refactor all bench/hotpath module entry points to accept `&MeasureRequest` directly. Eliminates `request.rs` entirely and removes conversion boilerplate in `dispatch.rs`.

## 2. `runs` Is Stored In Two Places

**File:** `src/measure.rs`

`MeasureMode` has `runs` inside each variant (`Bench { runs }`, `Hotpath { runs }`, `Alloc { runs }`), AND `MeasureRequest` has a top-level `runs: usize` field. These are always set to the same value via `runs: mm.runs()` in the macros (`main.rs:68`). The `MeasureMode::runs()` method exists solely to extract the duplicated value back out.

**Fix:** Remove `runs` from `MeasureRequest` and use `req.mode.runs()`. Or the opposite — remove `runs` from each `MeasureMode` variant and keep it on the request only. Either way, one source of truth.

## 3. The Macro Situation in `main.rs`

**File:** `src/main.rs:43-129`

Three macros (`pbfhogg_cmd!`, `elivagar_cmd!`, `nidhogg_cmd!`) that each do the same thing: resolve_mode, resolve_features, set_quiet, with_worktree, construct MeasureRequest, call dispatch. ~90 lines of macro that could be a single function:

```rust
fn run_measured<F>(mode: &ModeArgs, dev_config: &DevConfig, project_root: &Path,
                   dataset: &str, variant: &str, f: F) -> Result<(), DevError>
where F: FnOnce(&MeasureRequest) -> Result<(), DevError>
```

Each match arm becomes one line. The macro approach makes the code harder to navigate (can't jump-to-definition on a macro invocation) and the error messages are worse.

## 4. nidhogg Dispatch Is Fundamentally Different From pbfhogg/elivagar

**File:** `src/dispatch.rs:673-721`

`run_nidhogg_command` takes two closures (`bench_fn` and `hotpath_fn`) instead of a command enum. Nidhogg doesn't benefit from the unified dispatch at all — it's just boilerplate around constructing the legacy request types and calling closures. Compare with pbfhogg which has a proper `PbfhoggCommand` enum with `build_args()`, `supports_hotpath()`, etc.

**Fix:** Give nidhogg a proper `NidhoggCommand` enum like pbfhogg, with `id()`, `supports_hotpath()`, `build_args()`, etc. Nidhogg will evolve soon and needs the same solid foundation.

## 5. `main.rs` run() — 300 Lines of Identical pbfhogg Match Arms

**File:** `src/main.rs:164-1044`

The `run()` function is a single monolithic `match cli.command` with ~50 arms. The pbfhogg commands alone are lines 201-500 — 300 lines of near-identical patterns:

```rust
Command::Inspect { mode, pbf } => {
    pbfhogg_cmd!(mode, pbf, dev_config, project, project_root,
        pbfhogg::commands::PbfhoggCommand::Inspect)
}
Command::InspectNodes { mode, pbf } => {
    pbfhogg_cmd!(mode, pbf, dev_config, project, project_root,
        pbfhogg::commands::PbfhoggCommand::InspectNodes)
}
// ... 24 more identical patterns
```

**Important constraint:** collapsing the 26 `Command` variants in `cli.rs` into a single variant with `external_subcommand` would lose clap help text, tab completion, and typo correction. The variants must stay for UX.

**Fix:** Keep the clap variants but add `Command::as_pbfhogg()` that extracts the common parts as a one-liner mapping:

```rust
impl Command {
    fn as_pbfhogg(&self) -> Option<(&ModeArgs, &PbfArgs, PbfhoggCommand, Option<&str>, HashMap<String, String>)> {
        match self {
            Self::Inspect { mode, pbf } => Some((mode, pbf, PbfhoggCommand::Inspect, None, HashMap::new())),
            Self::InspectNodes { mode, pbf } => Some((mode, pbf, PbfhoggCommand::InspectNodes, None, HashMap::new())),
            Self::ApplyChanges { mode, pbf, osc_seq } => Some((mode, pbf, PbfhoggCommand::ApplyChanges, osc_seq.as_deref(), HashMap::new())),
            Self::AddLocationsToWays { mode, pbf, index_type } => {
                let mut p = HashMap::new();
                if let Some(v) = index_type { p.insert("index_type".into(), v.clone()); }
                Some((mode, pbf, PbfhoggCommand::AddLocationsToWays, None, p))
            }
            // ... remaining one-liners
            _ => None,
        }
    }
}
```

Then `main.rs` run() collapses the entire pbfhogg block to:

```rust
if let Some((mode, pbf, cmd, osc, params)) = cli.command.as_pbfhogg() {
    return run_measured(mode, &dev_config, project, &project_root,
        &pbf.dataset, &pbf.variant, |req| {
            dispatch::run_pbfhogg_command_with_params(req, &cmd, osc, &params)
        });
}
```

300 lines of match arms become 1 dispatch call + 26 one-liner mappings in `as_pbfhogg()`. The three macros (`pbfhogg_cmd!`, `elivagar_cmd!`, `nidhogg_cmd!`) are replaced by a single `run_measured()` function. Zero user-facing regressions — help, completion, and typo correction all preserved.

The `cli.rs` variant definitions stay verbose — that's the price of clap derive. But that's the part that's fine to be repetitive, because each line is a user-facing contract. It's the dispatch side that was the real problem.

## 6. elivagar Bench Path Inconsistency

**File:** `src/dispatch.rs:511-558`

`run_elivagar_bench` delegates to the *old* bench modules via a `BenchRequest`, bypassing the unified dispatch. But `run_elivagar_run` and `run_elivagar_hotpath` use the unified path. This means bench mode goes through a completely different code path than run mode for the same command. If you fix a bug in the run path for tilegen, the bench path won't get it (and vice versa). The comment on line 516 even acknowledges this: "Delegate to existing bench modules which handle their own harness setup."

## 7. `elivagar::cmd` Redundant Bootstrap

**File:** `src/elivagar/cmd.rs:11-57`

`bench_node_store` and `bench_pmtiles` manually call `bootstrap()`, `bootstrap_config()`, and construct `BenchHarness::new()` — exactly what `HarnessContext::new()` does (4 lines vs 8 lines). Should use `HarnessContext` like the other bench functions already do (`bench_planetiler`, `bench_tilemaker`, `bench_all`).

## 8. `MeasureMode::Run` Comment Says "No lockfile" But It Does Acquire One

**File:** `src/measure.rs:23`

Comment says `Run` mode has "No lockfile, no DB." But `run_pbfhogg_run` at `dispatch.rs:64-74` creates a `BenchContext` which acquires the lockfile (`context.rs:109`). Doc/behavior mismatch.

## 9. `output::run_captured` vs `run_captured_with_env` Duplication

**File:** `src/output.rs:129-192`

These two functions are identical except `run_captured_with_env` adds a loop to set env vars. The only caller is `run_tests` for nidhogg. Should just be `run_captured` with an optional `env` parameter (or always accept `&[(&str, &str)]` with an empty slice as default).

## 10. `KvPair` Clone Verbosity in `build_row`

**File:** `src/harness.rs:304-338`

Manual clone of each `KvPair` with a 3-arm match on `KvValue` — but both types already `#[derive(Clone)]`. The whole block can be:

```rust
let mut kv = config.metadata.clone();
for pair in &result.kv {
    if pair.key == "peak_rss_kb" { ... continue; }
    kv.push(pair.clone());
}
```

## 11. `Project::Other` Memory Leak

**File:** `src/config.rs:221`

```rust
Project::Other(Box::leak(other.to_owned().into_boxed_str()))
```

Leaks memory for every `Other` project. Since `load()` is called once at startup this is technically fine, but if someone ever calls `load()` in a loop (tests?), it silently leaks. A `Cow<'static, str>` or just a `String` in the `Other` variant would be cleaner. The `Copy` derive on `Project` is the reason for the leak — dropping `Copy` is fine since `Project` is only passed by value a handful of times and is cheap to clone.

## 12. `validate_since` Tautology + Recursive Call

**File:** `src/cli.rs:1157-1160`

```rust
let datetime_ok = s.len() == 19
    && !date_ok
    && s[..10].len() == 10  // <-- always true if s.len() == 19
    && validate_since(&s[..10]).is_ok()
```

Recursive self-call works but is unnecessarily clever. The `s[..10].len() == 10` check is dead code.

## 13. `config::hostname()` Called Multiple Times Per Run

**Files:** `src/config.rs:342`, called from `resolve_paths`, `host_features`, `record_history`, `nidhogg/cmd.rs`

`hostname()` calls `libc::gethostname()` via FFI every time. It's cheap, but would be cleaner to call once during config loading and store on `DevConfig` or `ResolvedPaths`.

## 14. Bench-Only Commands Use a Separate Mode Type

**Files:** `src/cli.rs:830-852`, `src/main.rs:542-579`

`Read`, `Write`, and `MergeBench` use `BenchOnlyModeArgs` instead of `ModeArgs`, and their dispatch doesn't go through the unified dispatch layer — they construct `BenchRequest` directly in `main.rs`. If you ever want to add hotpath support to `read` or `write`, you'd need to redo the plumbing.

**Fix:** Use `ModeArgs` and error at runtime if someone passes `--hotpath` (which `resolve_mode` + the dispatch layer already handles).

## 15. Dead `--runs` Flags in CLI

Several commands have both `ModeArgs` (which contains `--bench N`) and a separate `--runs N` flag:
- `Tilegen` (line 359), `PmtilesWriter`, `NodeStore`, `ElivPlanetiler`, `ElivTilemaker`
- `SluggrsHotpath`, `GenericHotpath`, `Suite`

The `--runs` field is vestigial from before the `ModeArgs` refactor. Looking at the tilegen dispatch (`main.rs:619`): the standalone `runs` field is matched as `runs: _` (unused). It's dead code that still shows up in `--help`.

---

## Priority Order

1. **Kill `BenchRequest`/`HotpathRequest`** — biggest bang-for-buck. Eliminates `request.rs`, removes conversion boilerplate, simplifies ~15 module entry points.
2. **`Command::as_pbfhogg()` + `run_measured()` function** — collapses 300 lines of match arms to 1 dispatch call + 26 one-liner mappings. Kills the three macros. Zero user-facing regressions. (Items 3 and 5 from the review.)
3. **Unify elivagar bench path** — `run_elivagar_bench` should use the same code path as `run_elivagar_run`.
4. **Bring bench-only commands into the unified dispatch** — use `ModeArgs` for read/write/merge.
5. **Remove dead `--runs` flags** from Tilegen, PmtilesWriter, NodeStore, etc.
6. **Clean up the small stuff** — KvPair clone verbosity, run_captured_with_env consolidation, hostname caching, `validate_since` tautology, Project::Other leak, `MeasureMode::Run` comment fix, elivagar::cmd redundant bootstrap.
