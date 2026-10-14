//! Measured sync runs and best-of-N iteration handling.

use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::artefacts::{self, ArtefactDir};
use crate::build::CargoProfile;
use crate::clean::layout::ProjectState;
use crate::config::DevConfig;
use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::context;
use crate::db::{KvPair, KvValue};
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, WallSource};
use crate::output;
use crate::project::{BuildRoot, Project, ProjectRoot};
use crate::ratatoskr::build::{self, HarnessBuild};
use crate::ratatoskr::gate as gate_eval;
use crate::ratatoskr::gate_store::canonical_features;
use crate::ratatoskr::saehrimnir::{ManagedMock, MockArtifactScope};
use crate::ratatoskr::timing::{format_elapsed, format_elapsed_ms};
use crate::ratatoskr::{HARNESS_MODE_FLAG, harness_env};

use super::gate::{self, GateHookCtx};
use super::runner::{resolve_single, validate_sync_config};

/// Drive the measured `brokkr sync <SCRIPT> --bench N` end-to-end:
///
/// 1. Validate config (same as the unmeasured run shape).
/// 2. Resolve script + fixture from frontmatter.
/// 3. Bootstrap [`crate::config::ResolvedPaths`] + take the lock and the
///    bench harness via [`context::MeasurementSession::new`]; build the
///    harness binary.
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
///    marker span (missing markers fail the measurement). The best iteration's summary.json
///    metrics are stored as `meta.<key>` rows alongside the result;
///    sidecar data for the best iteration is what `brokkr sidecar
///    <uuid>` surfaces.
/// 8. Tear down sæhrimnir with the standard graceful-shutdown budget.
#[allow(clippy::too_many_lines)] // entry point: validate + bootstrap + build + spawn + dispatch
pub fn run_sync_bench(req: &SyncBenchRequest<'_>) -> Result<(), DevError> {
    let cfg = req.ratatoskr;
    let (cfg, harness_cfg, mock_binary, fixtures_dir) =
        validate_sync_config(req.project_root.as_path(), cfg)?;
    if req.bench == 0 {
        return Err(DevError::Usage("sync: --bench must be >= 1".into()));
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

    let resolved = resolve_single(req.script, &fixtures_dir)?;
    let script_abs = resolved.script_abs;
    let test_id = resolved.test_id;
    let fixture_name = resolved.fixture_name;
    let fixture_path = resolved.fixture_path;
    let ceiling = resolved.ceiling;

    // Lock, host paths and the bench harness, through the same session
    // constructor every other measured command uses: the lock is taken (or
    // re-entered) before `cargo metadata`, git state is read from the build
    // root - the `--commit` worktree when one was requested, so the recorded
    // commit describes the code that was actually built - and results.db
    // stays under the main tree's project root.
    let context::MeasurementSession { harness, .. } = context::MeasurementSession::new(
        req.dev_config,
        Project::Ratatoskr,
        req.project_root,
        req.build_root,
        req.force,
        None,
    )?;
    let code_root = req.build_root.as_path();
    let harness = harness
        .with_invocation(req.invocation.clone())
        .with_measure_mode(Some(crate::measure::RecordedMode::Bench))
        // The same row provenance `MeasurementSession` attaches for every other
        // measured command: the configured features (joined as `MeasurementSession`
        // joins them; `None` for a default-features build) and the
        // `capture_env` snapshot. This path builds its harness by hand, and
        // used to record neither.
        .with_cargo_features(
            (!harness_cfg.features.is_empty()).then(|| harness_cfg.features.join(",")),
        )
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
        crate::shutdown::ChildIsolation::ProcessGroup(phase_guard.signal_guard()),
    )?;
    // What was actually built: ratatoskr's config sets `debug = true`, so the
    // row's profile follows the build, not a release default.
    let harness = harness.with_build_identity(crate::harness::BuildIdentity {
        profile: CargoProfile::for_debug(debug),
        features: (!harness_cfg.features.is_empty())
            .then(|| canonical_features(&harness_cfg.features)),
    });
    output::ratatoskr_msg(&format!(
        "harness build ok (features={}, binary={})",
        built.features_label,
        built.binary.display(),
    ));

    let state = ProjectState::new(req.project_root.as_path());
    let artefact_parent = state.ratatoskr().sync_artifacts();
    let artefacts = ArtefactDir::allocate(&artefact_parent, &test_id, req.keep_artefacts)?;

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
        harness: &harness,
        fixture_name: &fixture_name,
        debug,
        gate_rules: gate_rules.as_ref(),
        // The frontmatter `ceiling:` (or discover's default) bounds each
        // measured iteration exactly as it bounds the unmeasured run: a
        // hung harness in a `--gate all` sweep otherwise held the global
        // lock indefinitely.
        ceiling,
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

    /// The live guard, borrowed as proof for process-group isolation.
    fn signal_guard(&self) -> &crate::shutdown::SigtermGuard {
        &self._guard
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

struct BenchLoopCtx<'a> {
    req: &'a SyncBenchRequest<'a>,
    cfg: &'a RatatoskrConfig,
    built: &'a HarnessBuild,
    mock_binary: &'a Path,
    fixture_path: &'a Path,
    script_abs: &'a Path,
    run_root: &'a Path,
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
        harness,
        fixture_name,
        debug,
        gate_rules,
        ceiling,
        phase_guard,
    } = ctx;
    // Allocate every fallible resource before the mock starts. Once a
    // managed mock exists, every error below still reaches the shared
    // graceful shutdown path.
    let script_str = script_abs.display().to_string();

    // PID published from inside spawn_observed - before readiness wait
    // - so a `brokkr kill --hard` arriving during startup finds it. The
    // phase guard covers the readiness wait: it polls the shutdown flag
    // and reaps sæhrimnir before returning `Interrupted`.
    let mock = ManagedMock::start(
        cfg,
        mock_binary,
        fixture_path,
        fixture_name,
        MockArtifactScope::Run(run_root),
        harness.lock(),
    )?;
    output::ratatoskr_msg(&format!(
        "mock ready in {}",
        format_elapsed(mock.ready_elapsed())
    ));
    let endpoint_envs = mock.env_pair_refs();

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
        // Cargo-built: the profile comes from the session's build identity
        // (`with_build_identity` above).
        external_build: None,
        runs: req.bench,
        cli_args: Some(format!("{HARNESS_MODE_FLAG} {}", script_abs.display())),
        metadata: Vec::new(),
    };

    // One mock serves every iteration; the shared measurement loop owns the
    // FIFO, progress, ranking on the `SYNC_START` -> `SYNC_END` span and
    // winner selection. Each iteration's `summary.json`, indexed by
    // iteration, is kept for the gate.
    let mut summaries: Vec<Option<serde_json::Map<String, serde_json::Value>>> =
        Vec::with_capacity(req.bench);
    let measured = harness.measure(
        &bench_config,
        Some(&built.binary),
        WallSource::MarkerSpan {
            start: "SYNC_START",
            end: "SYNC_END",
        },
        true,
        |i, fifo| {
            if phase_guard.requested() {
                return Err(DevError::Interrupted);
            }
            let fifo = fifo
                .ok_or_else(|| DevError::Internal("marker-span measurement owns a FIFO".into()))?;
            let iter_dir = run_root.join(format!("iter-{}", i + 1));
            let iter_harness_dir = iter_dir.join("harness");
            fs::create_dir_all(&iter_harness_dir).map_err(DevError::Io)?;
            let common_env = harness_env(&iter_harness_dir, &built.bin_dir);
            let mut env_pairs = common_env.as_pairs().to_vec();
            env_pairs.extend(endpoint_envs.iter().copied());

            // The sidecar's own guard nests inside the phase guard for the
            // measured window. Refuses with `Interrupted` if a request
            // already landed in the gap.
            phase_guard.release_for_sidecar()?;
            let captured = harness.capture(
                &built.binary,
                &[HARNESS_MODE_FLAG, &script_str],
                req.project_root.as_path(),
                &env_pairs,
                &[],
                fifo,
                i,
                // The frontmatter ceiling bounds each measured iteration.
                Some(ceiling),
            );
            phase_guard.resume();
            let mut iteration = captured?;

            // Persist each iter's stdout/stderr so a later FAIL can be
            // reproduced without re-running, then ingest summary.json from
            // a successful child. Observer failures keep the evidence.
            let observed = (|| -> Result<Option<_>, DevError> {
                fs::write(iter_dir.join("binary-stdout.log"), &iteration.stdout)
                    .map_err(DevError::Io)?;
                fs::write(iter_dir.join("binary-stderr.log"), &iteration.stderr)
                    .map_err(DevError::Io)?;
                if iteration.error.is_some() {
                    return Ok(None);
                }
                let summary = read_summary_json(&iter_harness_dir)?;
                if let Some(result) = &mut iteration.result {
                    result.kv.extend(summary_to_kv(&summary));
                }
                Ok(Some(summary))
            })();
            match observed {
                Ok(summary) => summaries.push(summary),
                Err(error) => {
                    summaries.push(None);
                    iteration.fail(error);
                }
            }
            Ok(iteration)
        },
    );

    let mock_shutdown = mock.shutdown();
    if let Ok(mock_outcome) = &mock_shutdown {
        output::ratatoskr_msg(&format!(
            "mock shutdown in {}",
            format_elapsed(mock_outcome.shutdown_elapsed)
        ));
    }
    let killed_after_budget = mock_shutdown
        .as_ref()
        .is_ok_and(|outcome| outcome.killed_after_budget);

    let mut measurement = measured?;
    let recorded = harness.record(&bench_config, &mut measurement, |result, _| {
        if killed_after_budget {
            result
                .kv
                .push(KvPair::text("meta.mock_killed_after_budget", "true"));
        }
        Ok(())
    });
    let result = match recorded {
        Ok(result) => result,
        Err(e) => {
            if let Err(shutdown_error) = &mock_shutdown {
                output::ratatoskr_msg(&format!(
                    "mock-server shutdown also failed: {shutdown_error}"
                ));
            }
            return Err(e);
        }
    };
    mock_shutdown?;

    let best_idx = measurement.winner.ok_or_else(|| {
        DevError::Internal("sync bench recorded without a winning iteration".into())
    })?;
    let elapsed_ms = result.elapsed_ms;
    let summary = summaries
        .get(best_idx)
        .and_then(Option::as_ref)
        .ok_or_else(|| DevError::Internal("sync bench winner has no summary".into()))?;
    let sidecar_data = measurement.iterations[best_idx]
        .sidecar
        .as_ref()
        .ok_or_else(|| DevError::Internal("sync bench winner has no sidecar data".into()))?;

    output::ratatoskr_msg(&format!(
        "best-of-{}: {} (iter {})",
        req.bench,
        format_elapsed_ms(elapsed_ms),
        best_idx + 1,
    ));

    // `run_sync_bench` compiles the rules whenever `req.gate` is set, so the
    // pair is either both present or both absent.
    if let (Some(gate_name), Some(rules)) = (req.gate, gate_rules) {
        gate::run_gate_hook(&GateHookCtx {
            req,
            cfg,
            gate_name,
            rules,
            project_root: req.project_root.as_path(),
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
            summary,
            sidecar_data,
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
        .map_err(|e| DevError::Verify(format!("sync: parse {}: {e}", path.display())))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        other => Err(DevError::Verify(format!(
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

// ---------------------------------------------------------------------------
// sync <SCRIPT> --bench N - measure one
// ---------------------------------------------------------------------------

/// CLI inputs for `brokkr sync <SCRIPT> --bench N`.
pub struct SyncBenchRequest<'a> {
    pub project_root: &'a ProjectRoot,
    /// The code tree to build and read git state from: the live checkout
    /// (cwd, which differs from `project_root` when `brokkr.toml` lives one
    /// level up), or the `--commit` worktree cut from it.
    ///
    /// Only the *harness build* moves. The script, the fixture, sæhrimnir,
    /// the artefact dir and both databases stay anchored to `project_root`:
    /// the same split-tree rule dellingr applies to its Lua workload, and
    /// for the same reason, that a baseline should vary the code under test
    /// and nothing else. The gate additionally depends on it, since it
    /// compares the baseline row's script path against the current run's.
    pub build_root: &'a BuildRoot,
    pub dev_config: &'a DevConfig,
    /// The `[ratatoskr]` section (required at load in a ratatoskr project).
    pub ratatoskr: &'a RatatoskrConfig,
    pub script: &'a str,
    /// Number of measured iterations. Best-of-N reported and stored.
    pub bench: usize,
    /// Allow a measurement on a dirty git tree. Results are not inserted into
    /// results.db; collected sidecar data uses its `dirty` latest pointer, and
    /// a completed gated run is recorded in gate.db with `dirty = true`.
    pub force: bool,
    pub keep_artefacts: bool,
    pub profile_override: Option<bool>,
    /// The brokkr invocation (exact argv and arm key) the results row
    /// records.
    pub invocation: crate::db::RunInvocation,
    /// Run the named gate after the bench completes. See
    /// `docs/commands/ratatoskr-gate.md`.
    pub gate: Option<&'a str>,
    /// Record this run as a baseline candidate for the named gate;
    /// suppress evaluation. Only meaningful when `gate` is set.
    pub as_baseline: bool,
}
