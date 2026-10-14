//! Measured sync runs and best-of-N iteration handling.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::artefacts::{self, ArtefactDir};
use crate::build::CargoProfile;
use crate::config::DevConfig;
use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::context;
use crate::db::{KvPair, KvValue};
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;
use crate::project::Project;
use crate::ratatoskr::build::{self, HarnessBuild};
use crate::ratatoskr::discover;
use crate::ratatoskr::gate as gate_eval;
use crate::ratatoskr::gate_store::canonical_features;
use crate::ratatoskr::saehrimnir::{MockServer, endpoint_env_pairs, require_path, resolve_fixture};
use crate::ratatoskr::timing::{STDERR_TAIL_LINES, format_secs};
use crate::sidecar;

use super::gate::{self, GATE_ALL, GateHookCtx};
use super::runner::SYNC_ARTEFACT_PARENT;

/// Drive the measured `brokkr sync <SCRIPT> --bench N` end-to-end:
///
/// 1. Validate config (same as the unmeasured run shape).
/// 2. Resolve script + fixture from frontmatter.
/// 3. Bootstrap [`crate::config::ResolvedPaths`] + acquire the bench
///    lockfile via [`BenchHarness::new`]; build the harness binary.
/// 4. Allocate one top-level run dir
///    `.brokkr/ratatoskr/sync/<test>/run-N/` with `mock/` plus per-iter
///    `iter-K/harness/` subdirs. The whole bench shares one sæhrimnir
///    process - sæhrimnir is deterministic per fixture, so reusing it
///    across iterations keeps the iteration timing measuring the
///    sync code, not mock startup.
/// 5. Spawn sæhrimnir, get endpoints.
/// 6. For each iteration: spawn the harness binary with `BROKKR_MARKER_FIFO`,
///    `BROKKR_HARNESS_ARTEFACT_DIR=iter-K/harness`, `BROKKR_TEST_BIN_DIR`,
///    and the configured `RATATOSKR_TEST_*_ENDPOINT` family. Sidecar
///    captures `/proc` samples + phase markers; brokkr reads
///    `iter-K/harness/summary.json` after each iteration.
/// 7. Best-of-N is selected on the script's `SYNC_START` -> `SYNC_END`
///    marker span (falls back to wall-clock elapsed if the script
///    doesn't emit those markers). The best iteration's summary.json
///    metrics are stored as `meta.<key>` rows alongside the result;
///    sidecar data for the best iteration is what `brokkr sidecar
///    <uuid>` surfaces.
/// 8. Tear down sæhrimnir with the standard 1.5s budget.
#[allow(clippy::too_many_lines)] // entry point: validate + bootstrap + build + spawn + dispatch
pub fn run_sync_bench(req: &SyncBenchRequest<'_>) -> Result<(), DevError> {
    let cfg = req.ratatoskr;
    let harness_cfg = cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync: no [ratatoskr.harness] section in brokkr.toml. \
             Declare it with `package = \"<crate>\"` (and optional \
             `binary`, `features`, `debug`)."
                .into(),
        )
    })?;
    let mock_binary = require_path(
        &cfg.mock_server_binary,
        req.project_root,
        "mock_server_binary",
    )?;
    let fixtures_dir = require_path(&cfg.fixtures_dir, req.project_root, "fixtures_dir")?;
    if !mock_binary.exists() {
        return Err(DevError::Config(format!(
            "sync: sæhrimnir binary not found at {}. Build it first.",
            mock_binary.display()
        )));
    }
    if req.bench == 0 {
        return Err(DevError::Config("sync: --bench must be >= 1".into()));
    }

    // `all` is the reserved sweep value, so a gate of that name could
    // never be selected individually. Refuse rather than silently
    // shadow it.
    if cfg.gate.contains_key(GATE_ALL) {
        return Err(DevError::Config(format!(
            "sync: `[ratatoskr.gate.{GATE_ALL}]` collides with the reserved \
             `--gate {GATE_ALL}` sweep value - rename that gate"
        )));
    }

    // Validate `--gate <name>` references a configured gate before
    // building/running anything - errors here cost nothing.
    let gate_rules = if let Some(name) = req.gate {
        let gate = cfg.gate.get(name).ok_or_else(|| {
            DevError::Config(format!(
                "sync: gate `{name}` not found in [ratatoskr.gate.*] of brokkr.toml"
            ))
        })?;
        if gate.metrics.is_empty() && !req.as_baseline {
            return Err(DevError::Config(format!(
                "sync: gate `{name}` has no [ratatoskr.gate.{name}.metrics.*] rules. \
                 An empty rule set silently passes - add at least one rule, or use \
                 --as-baseline if you only want to record."
            )));
        }
        Some(gate_eval::GateRules::compile(name, gate)?)
    } else {
        None
    };

    let script_abs = Path::new(req.script)
        .canonicalize()
        .map_err(|e| DevError::Config(format!("sync: canonicalize script: {e}")))?;
    if !script_abs.is_file() {
        return Err(DevError::Config(format!(
            "sync: script not found or not a file: {}",
            req.script
        )));
    }
    let test_id = script_abs
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| DevError::Config(format!("sync: script has no stem: {}", req.script)))?
        .to_owned();

    let parsed = discover::parse_script(&script_abs, &test_id)
        .map_err(|e| DevError::Config(format!("sync: parse script: {e}")))?;
    let fixture_name = parsed.fixture.as_ref().ok_or_else(|| {
        DevError::Config(format!(
            "sync: script {test_id} has no `-- fixture: <NAME>` frontmatter line."
        ))
    })?;
    let fixture_path = resolve_fixture(&fixtures_dir, fixture_name)?;

    // Bootstrap paths and stand up the bench harness. The harness
    // acquires the lockfile internally, so concurrent brokkr
    // invocations block here.
    //
    // `code_root` is where the harness is built and where git state is read
    // from - the `--commit` worktree when one was requested. Handing it to
    // `BenchHarness` as the project root (with the real root demoted to
    // `db_root`) is the same convention `MeasurementSession` uses: the recorded
    // commit then describes the code that was actually built, while
    // results.db stays in the main tree. It also means a `--commit` run
    // reads as clean, which it is - a detached worktree has nothing
    // uncommitted in it.
    let code_root = req.build_root.unwrap_or(req.project_root);
    let db_root = req.build_root.map(|_| req.project_root);
    let pi = context::bootstrap(req.build_root)?;
    let paths = context::bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)?;
    let harness = BenchHarness::new(
        &paths,
        code_root,
        db_root,
        Project::Ratatoskr,
        "sync",
        req.force,
        None,
    )?
    .with_brokkr_args(req.brokkr_args.clone())
    .with_measure_mode(Some("bench"))
    // The same row provenance `MeasurementSession` attaches for every other
    // measured command: the configured features (joined as `MeasurementSession`
    // joins them; `None` for a default-features build) and the
    // `capture_env` snapshot. This path builds its harness by hand, and
    // used to record neither.
    .with_cargo_features((!harness_cfg.features.is_empty()).then(|| harness_cfg.features.join(",")))
    .with_env_kv(crate::config::captured_env_pairs(req.dev_config));

    // Cooperative SIGTERM for everything outside the sidecar's own window
    // (build, sæhrimnir spawn and readiness, the gaps between iterations,
    // mock teardown, recording). See `PhaseGuard`. Declared after
    // `harness` so it drops first, before the lock is released.
    let mut phase_guard = PhaseGuard::install();

    let debug = req
        .profile_override
        .unwrap_or_else(|| harness_cfg.debug.unwrap_or(false));
    let built = build::build_for_harness(
        code_root,
        harness_cfg,
        debug,
        Some(&|pid| harness.lock().set_child_pid(pid)),
        Some(&|| harness.lock().clear_child_pid()),
        // isolate_pg=false: the phase guard above makes the captured
        // runner forward `brokkr kill` to cargo; spawns stay in brokkr's PG
        // so terminal Ctrl-C also reaches them directly. --hard walks
        // brokkr's process tree, so rustc workers go down with it.
        false,
    )?;
    output::ratatoskr_msg(&format!(
        "harness build ok (features={}, binary={})",
        built.features_label,
        built.binary.display(),
    ));

    let artefact_parent = req.project_root.join(SYNC_ARTEFACT_PARENT);
    let artefacts = ArtefactDir::allocate(&artefact_parent, &test_id, req.keep_artefacts)?;
    let mock_dir = artefacts.path().join("mock");
    fs::create_dir_all(&mock_dir).map_err(DevError::Io)?;

    output::ratatoskr_msg(&format!(
        "running {test_id} (fixture: {fixture_name}, bench={})",
        req.bench
    ));

    let outcome = bench_loop(BenchLoopCtx {
        req,
        cfg,
        built: &built,
        mock_binary: &mock_binary,
        fixture_path: &fixture_path,
        script_abs: &script_abs,
        run_root: artefacts.path(),
        mock_dir: &mock_dir,
        scratch_dir: &paths.scratch_dir,
        harness: &harness,
        fixture_name,
        debug,
        gate_rules: gate_rules.as_ref(),
        // The frontmatter `ceiling:` (or discover's default) bounds each
        // measured iteration exactly as it bounds the unmeasured run: a
        // hung harness in a `--gate all` sweep otherwise held the global
        // lock indefinitely.
        ceiling: parsed.ceiling,
        phase_guard: &mut phase_guard,
    });

    match outcome {
        Ok(()) => {
            artefacts.finalize_success()?;
            // A request that landed after the last iteration (during
            // teardown, recording or the gate hook) had nothing left to
            // stop, but it still has to reach the caller: a `--gate all`
            // sweep must not start the next gate.
            if phase_guard.requested() {
                return Err(DevError::Interrupted);
            }
            Ok(())
        }
        Err(e) => {
            let path = artefacts.path().to_path_buf();
            artefacts.finalize_failure();
            output::ratatoskr_msg(&format!(
                "FAIL: {e} (artefacts preserved at {})",
                path.display()
            ));
            artefacts::emit_clean_hint(output::ratatoskr_msg);
            Err(e)
        }
    }
}

/// Cooperative-SIGTERM coverage for the whole bench path: build, sæhrimnir
/// spawn and readiness, every measured iteration, the gaps between them,
/// mock teardown and recording. Without it, `brokkr kill` outside a guarded
/// window took the default terminate action: no `Drop` ran, and sæhrimnir
/// was left running with its ports bound.
///
/// One guard, held throughout. `SigtermGuard` nests, so the sidecar's own
/// guard around each iteration is an inner one: it neither clears a request
/// that is already pending nor uncovers the window when it drops. (Guards
/// used not to nest, and this held a guard per phase and stepped aside for
/// each sidecar window, which left the harness fork/exec and the step-aside
/// itself uncovered.)
struct PhaseGuard {
    _guard: crate::shutdown::SigtermGuard,
}

impl PhaseGuard {
    fn install() -> Self {
        Self {
            _guard: crate::shutdown::SigtermGuard::install(),
        }
    }

    /// Whether a shutdown request is pending under this guard.
    fn requested(&self) -> bool {
        crate::shutdown::is_shutdown_requested()
    }

    /// The last check before a measured iteration spawns: a request that is
    /// already pending surfaces as `Interrupted` instead of starting a run
    /// the sidecar would kill at its first tick. The guard stays in place.
    fn release_for_sidecar(&mut self) -> Result<(), DevError> {
        if self.requested() {
            return Err(DevError::Interrupted);
        }
        Ok(())
    }

    /// Nothing to re-take: the guard never stepped aside. Kept so the
    /// iteration loop reads the same either side of the sidecar window.
    fn resume(&mut self) {}
}

/// One iteration's measured outcome. `marker_span_ms` is the
/// `SYNC_START` -> `SYNC_END` span when those markers fired; otherwise
/// `None` and `wall_clock_ms` is used for best-of-N selection.
struct IterOutcome {
    run_idx: usize,
    marker_span_ms: Option<i64>,
    wall_clock_ms: i64,
    summary: serde_json::Map<String, serde_json::Value>,
}

impl IterOutcome {
    fn elapsed_ms(&self) -> i64 {
        self.marker_span_ms.unwrap_or(self.wall_clock_ms)
    }
}

struct BenchLoopCtx<'a> {
    req: &'a SyncBenchRequest<'a>,
    cfg: &'a RatatoskrConfig,
    built: &'a HarnessBuild,
    mock_binary: &'a Path,
    fixture_path: &'a Path,
    script_abs: &'a Path,
    run_root: &'a Path,
    mock_dir: &'a Path,
    scratch_dir: &'a Path,
    harness: &'a BenchHarness,
    fixture_name: &'a str,
    debug: bool,
    gate_rules: Option<&'a gate_eval::GateRules>,
    ceiling: Duration,
    phase_guard: &'a mut PhaseGuard,
}

#[allow(clippy::too_many_lines)] // bench loop: per-iter spawn + sidecar + summary ingest + post-loop record
fn bench_loop(ctx: BenchLoopCtx<'_>) -> Result<(), DevError> {
    let BenchLoopCtx {
        req,
        cfg,
        built,
        mock_binary,
        fixture_path,
        script_abs,
        run_root,
        mock_dir,
        scratch_dir,
        harness,
        fixture_name,
        debug,
        gate_rules,
        ceiling,
        phase_guard,
    } = ctx;
    // PID published from inside spawn_observed - before readiness wait
    // - so a `brokkr kill --hard` arriving during startup finds it. The
    // phase guard covers the readiness wait: it polls the shutdown flag
    // and reaps sæhrimnir before returning `Interrupted`.
    let mock = MockServer::spawn_observed(
        mock_binary,
        fixture_path,
        mock_dir,
        Some(&|pid| harness.lock().add_mock_pid(pid)),
        Some(&|pid| harness.lock().remove_mock_pid(pid)),
        // isolate_pg=false: same reason as the build call - the mock
        // stays in brokkr's PG so terminal Ctrl-C reaches it directly.
        false,
    )?;
    output::ratatoskr_msg(&format!(
        "mock ready in {}",
        format_secs(mock.ready_elapsed())
    ));
    let endpoint_envs = endpoint_env_pairs(cfg, mock.endpoints());

    let mut fifo = sidecar::SidecarFifo::create(scratch_dir)?;
    let fifo_path = fifo.path_str()?.to_owned();
    let bin_dir_str = built.bin_dir.display().to_string();
    let binary_str = built.binary.display().to_string();
    let script_str = script_abs.display().to_string();

    // Built before the loop so a failed run can still record sidecar
    // provenance against it.
    let bench_config = BenchConfig {
        // Rows file under the command name, and the command is now
        // `sync` - the measurement mode is what `bench` describes, and
        // it already has its own column. Migration v17->v18 rewrites
        // historical `sync-bench` rows so old and new runs still pair
        // under `brokkr results --compare`.
        command: "sync".into(),
        input_file: Some(fixture_name.to_owned()),
        input_mb: None,
        // The harness default (`with_cargo_features` in `run_sync_bench`).
        cargo_features: None,
        // What was actually built: ratatoskr's config sets `debug = true`,
        // and every row used to say `release` regardless.
        cargo_profile: CargoProfile::for_debug(debug),
        runs: req.bench,
        cli_args: Some(format!("--test-harness {}", script_abs.display())),
        metadata: Vec::new(),
    };

    let mut sidecar_runs: Vec<sidecar::SidecarData> = Vec::with_capacity(req.bench);
    let mut best: Option<IterOutcome> = None;
    // Each completed iteration's ranking value (marker span, else wall), in
    // execution order - the same list `run_external_*` stores.
    let mut walls: Vec<i64> = Vec::with_capacity(req.bench);
    let mut last_pid: u32 = 0;
    // The in-flight iteration's child status, for the dirty sidecar record:
    // set as soon as the child is reaped, cleared once the iteration
    // completes. A failure after a successful child (log write,
    // `summary.json`) therefore records that success, not a fabricated code.
    let mut failed_status: Option<std::process::ExitStatus> = None;
    // Stamped before the first iteration so `prev.gap_seconds` measures to
    // the start of this run, not to its recording.
    let start_epoch = harness.begin_measurement();

    let bench_outcome = (|| -> Result<(), DevError> {
        for i in 0..req.bench {
            if phase_guard.requested() {
                return Err(DevError::Interrupted);
            }
            crate::harness::announce_run(i, req.bench);
            harness.lock().set_progress(
                u32::try_from(i + 1).unwrap_or(u32::MAX),
                u32::try_from(req.bench).unwrap_or(u32::MAX),
            );
            if i > 0 {
                fifo.reopen()?;
            }

            let iter_dir = run_root.join(format!("iter-{}", i + 1));
            let iter_harness_dir = iter_dir.join("harness");
            fs::create_dir_all(&iter_harness_dir).map_err(DevError::Io)?;
            let iter_harness_str = iter_harness_dir.display().to_string();

            let mut env_pairs: Vec<(&str, &str)> = vec![
                ("BROKKR_MARKER_FIFO", fifo_path.as_str()),
                ("BROKKR_HARNESS_ARTEFACT_DIR", &iter_harness_str),
                ("BROKKR_TEST_BIN_DIR", &bin_dir_str),
            ];
            for (name, value) in &endpoint_envs {
                env_pairs.push((name.as_str(), value.as_str()));
            }

            // The sidecar's own guard nests inside the phase guard for the
            // measured window. Refuses with `Interrupted` if a request
            // already landed in the gap.
            phase_guard.release_for_sidecar()?;
            let start = Instant::now();
            let spawned = output::spawn_captured(
                &binary_str,
                &["--test-harness", &script_str],
                req.project_root,
                &env_pairs,
                // isolate_pg=true: sidecar installs its own
                // SigtermGuard around `run_sidecar` below, covering
                // the lifetime of this child.
                true,
            );
            let child = match spawned {
                Ok(c) => c,
                Err(e) => {
                    phase_guard.resume();
                    return Err(e);
                }
            };
            let pid = child.id();
            last_pid = pid;
            harness.lock().set_child_pid(pid);

            let result =
                sidecar::run_sidecar_with_deadline(child, &mut fifo, i, start, None, Some(ceiling));
            phase_guard.resume();
            // Iteration's child has reaped; clear so a stale PID can't
            // be SIGKILLed by `--hard` in the gap before the next iter.
            harness.lock().clear_child_pid();

            // The iteration's sidecar data joins the run's list the moment
            // the child is reaped, before anything below can fail. Every
            // later `?` - the log writes, the `summary.json` read - then
            // leaves it in `sidecar_runs` for the `dirty` record after the
            // loop, rather than dropping the trajectory of a run whose child
            // succeeded. `failed_status` is stamped alongside for the same
            // reason: a post-processing failure records the status the
            // child actually had, which may well be success. The index
            // stays `i`, so `best.run_idx` still addresses this entry.
            let marker_span_ms = sync_span_from_markers(&result.data.markers);
            failed_status = Some(result.exit_status);
            sidecar_runs.push(result.data);

            // Persist each iter's stdout/stderr so a later FAIL can
            // be reproduced without re-running. summary.json is
            // already in iter_harness_dir from the harness binary.
            fs::write(iter_dir.join("binary-stdout.log"), &result.stdout).map_err(DevError::Io)?;
            fs::write(iter_dir.join("binary-stderr.log"), &result.stderr).map_err(DevError::Io)?;

            if result.stopped_by_signal {
                return Err(DevError::Interrupted);
            }
            if result.stopped_by_deadline {
                return Err(DevError::Config(format!(
                    "harness binary exceeded ceiling {ceiling:?} on iter {}/{}",
                    i + 1,
                    req.bench
                )));
            }
            if !result.exit_status.success() {
                let stderr_tail = String::from_utf8_lossy(&result.stderr)
                    .lines()
                    .rev()
                    .take(STDERR_TAIL_LINES)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(DevError::Config(format!(
                    "harness binary exited with {:?} on iter {}/{}\n--- last 5 stderr lines ---\n{stderr_tail}",
                    result.exit_status,
                    i + 1,
                    req.bench
                )));
            }

            let summary = read_summary_json(&iter_harness_dir)?;
            let wall_clock_ms = i64::try_from(result.elapsed.as_millis()).unwrap_or(i64::MAX);
            let outcome = IterOutcome {
                run_idx: i,
                marker_span_ms,
                wall_clock_ms,
                summary,
            };

            output::bench_msg(&format!(
                "  iter {} -> {}ms{}",
                i + 1,
                outcome.elapsed_ms(),
                if marker_span_ms.is_none() {
                    " (wall-clock; no SYNC_START/SYNC_END markers)"
                } else {
                    ""
                },
            ));

            walls.push(outcome.elapsed_ms());
            if best
                .as_ref()
                .is_none_or(|b| outcome.elapsed_ms() < b.elapsed_ms())
            {
                best = Some(outcome);
            }
            // The iteration completed: no failure to attribute to it.
            failed_status = None;
        }
        Ok(())
    })();

    drop(fifo);
    let mock_outcome = mock.shutdown();
    harness.lock().clear_mock_pids();
    output::ratatoskr_msg(&format!(
        "mock shutdown in {}",
        format_secs(mock_outcome.shutdown_elapsed)
    ));

    let program = built.binary.as_path();
    if let Err(e) = bench_outcome {
        // Keep what was collected under the `dirty` alias, as
        // `run_external_*` does - the /proc trajectory of a crashed, hung or
        // interrupted iteration is the most useful data the run leaves.
        // Best-effort: the run's own error is the one to report.
        if !sidecar_runs.is_empty() {
            let info = harness.run_info_for(
                &bench_config,
                program,
                start_epoch,
                last_pid,
                failed_status.as_ref(),
            );
            harness
                .store_sidecar(None, &sidecar_runs, sidecar_runs.len() - 1, Some(&info))
                .ok();
        }
        return Err(e);
    }

    let best = best.ok_or_else(|| DevError::Config("sync: no successful iterations".into()))?;
    let elapsed_ms = best.elapsed_ms();

    let mut kv = summary_to_kv(&best.summary);
    if best.marker_span_ms.is_none() {
        kv.push(KvPair::text("meta.timing_source", "wall_clock_no_markers"));
    } else {
        kv.push(KvPair::text("meta.timing_source", "sync_markers"));
    }
    if mock_outcome.killed_after_budget {
        kv.push(KvPair::text("meta.mock_killed_after_budget", "true"));
    }

    let bench_result = BenchResult {
        elapsed_ms,
        elapsed_us: None,
        kv,
        // Best-of-N: every iteration's ranking value, in execution order.
        iterations: walls,
        distribution: None,
        hotpath: None,
    };

    let info = harness.run_info_for(&bench_config, program, start_epoch, last_pid, None);
    let uuid = match harness.record_result(&bench_config, &bench_result) {
        Ok(uuid) => uuid,
        Err(e) => {
            // The row is lost; the trajectory need not be.
            harness
                .store_sidecar(None, &sidecar_runs, best.run_idx, Some(&info))
                .ok();
            return Err(e);
        }
    };
    harness.store_sidecar(uuid.as_deref(), &sidecar_runs, best.run_idx, Some(&info))?;

    output::ratatoskr_msg(&format!(
        "best-of-{}: {}ms (iter {})",
        req.bench,
        elapsed_ms,
        best.run_idx + 1,
    ));

    // `run_sync_bench` compiles the rules whenever `req.gate` is set, so the
    // pair is either both present or both absent.
    if let (Some(gate_name), Some(rules)) = (req.gate, gate_rules) {
        gate::run_gate_hook(&GateHookCtx {
            req,
            cfg,
            gate_name,
            rules,
            project_root: req.project_root,
            script_abs,
            fixture_name,
            debug,
            // The same `[ratatoskr.harness] features` list the build above
            // used; `run_sync_bench` refused a config without the section.
            features: &cfg
                .harness
                .as_ref()
                .map(|h| canonical_features(&h.features))
                .unwrap_or_default(),
            elapsed_ms,
            summary: &best.summary,
            sidecar_data: &sidecar_runs[best.run_idx],
        })?;
    }

    Ok(())
}

/// Read `summary.json` from the harness's per-iter artefact dir. Missing
/// file is fine (the script may not have written one) - we surface an
/// empty map rather than failing, so a script that doesn't bother with
/// metrics still benches cleanly.
pub(super) fn read_summary_json(
    harness_dir: &Path,
) -> Result<serde_json::Map<String, serde_json::Value>, DevError> {
    let path = harness_dir.join("summary.json");
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let text = fs::read_to_string(&path).map_err(DevError::Io)?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| DevError::Config(format!("sync: parse {}: {e}", path.display())))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        other => Err(DevError::Config(format!(
            "sync: {} root must be a JSON object, got {}",
            path.display(),
            crate::ratatoskr::gate::kind_of(&other)
        ))),
    }
}

/// Convert summary.json's flat top-level fields into KvPairs. Numeric
/// values become `Int` when the value is integer-valued (i64 fits) and
/// `Real` otherwise; strings become `Text`. Bools/null/array/nested
/// objects are skipped silently - first time a script needs nested
/// shape, lift it explicitly.
pub(super) fn summary_to_kv(map: &serde_json::Map<String, serde_json::Value>) -> Vec<KvPair> {
    let mut out = Vec::with_capacity(map.len());
    for (key, value) in map {
        let prefixed = format!("meta.{key}");
        match value {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    out.push(KvPair {
                        key: prefixed,
                        value: KvValue::Int(i),
                    });
                } else if let Some(f) = n.as_f64() {
                    out.push(KvPair {
                        key: prefixed,
                        value: KvValue::Real(f),
                    });
                }
            }
            serde_json::Value::String(s) => {
                out.push(KvPair {
                    key: prefixed,
                    value: KvValue::Text(s.clone()),
                });
            }
            _ => {}
        }
    }
    out
}

/// Compute the millisecond span between the script-emitted `SYNC_START`
/// and `SYNC_END` markers, if both fired. Returns `None` when either
/// is missing - caller falls back to wall-clock. Multiple `SYNC_START`s
/// are tolerated (last one wins) so a script that warms up under the
/// same marker name doesn't break; first `SYNC_END` after the last
/// `SYNC_START` wins.
pub(super) fn sync_span_from_markers(markers: &[sidecar::Marker]) -> Option<i64> {
    let last_start = markers.iter().rfind(|m| m.name == "SYNC_START")?;
    let end = markers
        .iter()
        .find(|m| m.name == "SYNC_END" && m.timestamp_us > last_start.timestamp_us)?;
    let span_us = end.timestamp_us.checked_sub(last_start.timestamp_us)?;
    Some(span_us / 1000)
}

// ---------------------------------------------------------------------------
// sync <SCRIPT> --bench N - measure one
// ---------------------------------------------------------------------------

/// CLI inputs for `brokkr sync <SCRIPT> --bench N`.
pub struct SyncBenchRequest<'a> {
    pub project_root: &'a Path,
    /// The code tree to build and read git state from, when it differs from
    /// `project_root`: the `--commit` worktree, or cwd when `brokkr.toml`
    /// lives one level up. `None` in the common case.
    ///
    /// Only the *harness build* moves. The script, the fixture, sæhrimnir,
    /// the artefact dir and both databases stay anchored to `project_root`:
    /// the same split-tree rule dellingr applies to its Lua workload, and
    /// for the same reason, that a baseline should vary the code under test
    /// and nothing else. The gate additionally depends on it, since it
    /// compares the baseline row's script path against the current run's.
    pub build_root: Option<&'a Path>,
    pub dev_config: &'a DevConfig,
    /// The `[ratatoskr]` section (required at load in a ratatoskr project).
    pub ratatoskr: &'a RatatoskrConfig,
    pub script: &'a str,
    /// Number of measured iterations. Best-of-N reported and stored.
    pub bench: usize,
    /// Allow recording on a dirty git tree (results land under the `dirty`
    /// alias instead of being skipped). Mirrors the existing bench-flag
    /// semantics across pbfhogg/elivagar.
    pub force: bool,
    pub keep_artefacts: bool,
    pub profile_override: Option<bool>,
    /// Literal `brokkr <...>` invocation, threaded through for the
    /// `brokkr_args` column in results.db.
    pub brokkr_args: String,
    /// Run the named gate after the bench completes. See
    /// `docs/commands/ratatoskr-gate.md`.
    pub gate: Option<&'a str>,
    /// Record this run as a baseline candidate for the named gate;
    /// suppress evaluation. Only meaningful when `gate` is set.
    pub as_baseline: bool,
}
