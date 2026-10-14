//! Unified dispatch layer for elivagar commands.

use crate::context::BenchContext;
use crate::db::KvPair;
use crate::elivagar;
use crate::elivagar::PipelineOpts;
use crate::elivagar::commands::{BuiltCommand, ElivagarCommand, ExampleCommand, ExternalTool};
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::output;
use crate::project::{self, Project};
use crate::resolve::resolve_pbf_entry;

/// Run an elivagar command with the specified measurement mode.
///
/// Handles run, bench, hotpath, and alloc for any elivagar command.
/// External commands (Planetiler, Tilemaker) delegate to old handlers for
/// bench mode but do not support run/hotpath/alloc.
pub(crate) fn run_command(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    project::require(req.project, Project::Elivagar, command.id())?;

    if req.dry_run {
        return run_elivagar_dry_run(req, command);
    }

    match command {
        ElivagarCommand::External(tool) => run_elivagar_external(req, tool),
        ElivagarCommand::Built(command) => match req.mode {
            MeasureMode::Run => run_elivagar_run(req, command),
            MeasureMode::Bench { .. } => run_elivagar_bench(req, command),
            MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
                run_elivagar_hotpath(req, command)
            }
        },
    }
}

/// Dry-run mode: validate config, path resolution, and argv construction
/// without building or running. Mirrors pbfhogg's `run_pbfhogg_dry_run`.
///
/// Does: project bootstrap (cargo metadata only, no compile), host-path
/// resolution, input-PBF resolution (requires the file to exist, same as
/// pbfhogg's dry-run which hash-verifies its input), and arg-vector
/// construction (catches `build_args` failures). Does NOT: cargo build, lock
/// acquisition, or process execution.
///
/// Note: for `tilegen`, `build_args` creates the scratch + `tilegen_tmp` dirs
/// as a side effect - they'd be created by any real run anyway, and a routine
/// `brokkr clean` reclaims `tilegen_tmp`.
fn run_elivagar_dry_run(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    // Resolve host paths without building (cargo metadata only, no compile).
    let pi = crate::context::bootstrap(req.build_root)?;
    let paths = crate::context::bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)?;

    // Resolve + report the input PBF for commands that need one.
    let pbf = if command.needs_pbf() {
        let pbf = resolve_pbf_entry(req.dataset, req.variant, &paths, req.project_root)?;
        let file_mb = crate::resolve::file_size_mb(&pbf.path)?;
        output::dry_run_msg(&format!("pbf: {} ({file_mb:.0} MB)", pbf.path.display()));
        Some(pbf)
    } else {
        None
    };
    let pbf_str = if let Some(pbf) = &pbf {
        pbf.path
            .to_str()
            .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?
            .to_owned()
    } else {
        String::new()
    };

    match command {
        // External tools construct argv in their own handlers, so there is
        // nothing to validate here beyond the resolved input.
        ElivagarCommand::External(_) => {
            output::dry_run_msg(&format!(
                "{} is an external tool (no Rust build)",
                command.id()
            ));
            output::dry_run_msg("ok");
            Ok(())
        }
        ElivagarCommand::Built(command) => {
            // Construct the arg vector (catches any build_args failures).
            let opts = pbf.as_ref().map(|p| PipelineOpts::from_pbf_entry(p.entry));
            let args =
                command.build_args(&pbf_str, &paths.scratch_dir, &paths.data_dir, opts.as_ref())?;
            output::dry_run_msg(&format!("{} args: {}", command.id(), args.join(" ")));
            output::dry_run_msg("ok");
            Ok(())
        }
    }
}

/// Elivagar run mode: build, run once, print timing. No DB storage.
///
/// The `BuiltCommand` variant determines whether this builds the main binary
/// or a cargo example.
fn run_elivagar_run(req: &MeasureRequest, command: &BuiltCommand<'_>) -> Result<(), DevError> {
    match command {
        BuiltCommand::MainBinary { .. } => {
            let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
            let ctx = BenchContext::new_for_request(
                req,
                command.package(),
                &feat_refs,
                true,
                &format!("run {}", command.id()),
            )?;

            let pbf = resolve_pbf_entry(req.dataset, req.variant, &ctx.paths, req.project_root)?;
            let pbf_str = pbf
                .path
                .to_str()
                .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
            let opts = PipelineOpts::from_pbf_entry(pbf.entry);
            let args = command.build_args(
                pbf_str,
                &ctx.paths.scratch_dir,
                &ctx.paths.data_dir,
                Some(&opts),
            )?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let binary_str = ctx.binary.display().to_string();
            output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));

            let out =
                output::run_passthrough_timed(&binary_str, &arg_refs, Some(ctx.harness.lock()))?;

            if out.code != 0 {
                for path in command.output_files(&ctx.paths.scratch_dir) {
                    std::fs::remove_file(path).ok();
                }
                return Err(DevError::ExitCode(out.code));
            }

            rename_elivagar_output(
                command,
                &ctx.paths.scratch_dir,
                &ctx.paths.output_dir,
                &ctx.paths.data_dir,
                req.dataset,
                req.variant,
                req.effective_build_root(),
            )?;

            let ms = crate::duration_ms(out.elapsed);
            output::run_msg(&format!("elapsed={ms}ms"));
            Ok(())
        }
        BuiltCommand::Example(example) => {
            // No BenchContext on this path, so nothing else takes the lock:
            // take it here, before the build. It used to run unlocked, which
            // `cargo_build` now refuses - an unlocked build competes with a
            // concurrent measurement and fails outright on a guarded host.
            let lock = crate::context::acquire_cmd_lock(
                req.project,
                req.project_root,
                &format!("run {}", command.id()),
            )?;
            let build_root = req.build_root.unwrap_or(req.project_root);
            let binary = crate::build::cargo_build(
                &crate::build::BuildConfig {
                    package: None,
                    bin: None,
                    example: Some(example.cargo_example().into()),
                    features: vec![],
                    default_features: true,
                    all_features: false,
                    profile: "release",
                },
                build_root,
            )?;
            let binary_str = binary.display().to_string();

            let args =
                command.build_args("", std::path::Path::new(""), std::path::Path::new(""), None)?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));

            // Registered against the lock taken above, so `brokkr kill --hard`
            // reaches the example as well as brokkr.
            let out = output::run_passthrough_timed(&binary_str, &arg_refs, Some(&lock))?;

            if out.code != 0 {
                return Err(DevError::ExitCode(out.code));
            }

            let ms = crate::duration_ms(out.elapsed);
            output::run_msg(&format!("elapsed={ms}ms"));
            Ok(())
        }
    }
}

/// Elivagar bench mode: full harness with DB storage.
///
/// The `BuiltCommand` variant determines how to build. Tilegen uses
/// `run_external_ok` (brokkr's own external wall-clock, pbfhogg-style);
/// micro-benchmarks use `run_internal` (examples handle their own iteration).
fn run_elivagar_bench(req: &MeasureRequest, command: &BuiltCommand<'_>) -> Result<(), DevError> {
    match command {
        BuiltCommand::MainBinary { .. } => run_elivagar_wallclock(req, command),
        BuiltCommand::Example(example) => run_elivagar_internal(req, example),
    }
}

/// Elivagar wallclock benchmark for the main binary (Tilegen).
///
/// Builds the main binary and runs via `run_external_ok` - brokkr's own
/// best-of-N external wall-clock, identical to the pbfhogg path. tilegen emits
/// all metrics as FIFO counters (sidecar.db) as of the elivagar side's 54f9b07,
/// so brokkr no longer reads stderr: there is no `elapsed_ms=` contract, and
/// runs are distinguished purely by their recorded `cli_args` (the
/// `--locations-on-ways` flag lands in the subprocess argv when passed).
fn run_elivagar_wallclock(
    req: &MeasureRequest,
    command: &BuiltCommand<'_>,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new_for_request(
        req,
        command.package(),
        &feat_refs,
        true,
        &format!("bench {}", command.id()),
    )?;

    let pbf = resolve_pbf_entry(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let pbf_path = pbf.path;
    let file_mb = crate::resolve::file_size_mb(&pbf_path)?;
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let opts = PipelineOpts::from_pbf_entry(pbf.entry);
    let args = command.build_args(
        pbf_str,
        &ctx.paths.scratch_dir,
        &ctx.paths.data_dir,
        Some(&opts),
    )?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::bench_msg(&format!(
        "{} ({file_mb:.0} MB), {} run(s)",
        command.id(),
        req.runs(),
    ));

    let bench_config = BenchConfig {
        command: command.id().into(),
        mode: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &arg_refs,
        )),
        brokkr_args: None,
        metadata: command.metadata(),
    };

    // pbfhogg-style: brokkr's own best-of-N external wall-clock. tilegen emits
    // all metrics as FIFO counters (sidecar.db), so brokkr no longer reads its
    // stderr - the human prose that remains there is captured and discarded.
    // records internally + stores sidecar linked to the results uuid.
    ctx.harness
        .run_external_ok(&bench_config, &ctx.binary, &arg_refs, req.project_root, &[])?;

    rename_elivagar_output(
        command,
        &ctx.paths.scratch_dir,
        &ctx.paths.output_dir,
        &ctx.paths.data_dir,
        req.dataset,
        req.variant,
        req.effective_build_root(),
    )
}

/// Elivagar internal benchmark for cargo examples (PmtilesWriter, NodeStore).
///
/// Delegates to the same `bench_pmtiles::run` / `bench_node_store::run` that
/// `bench all` uses, so one command name files one row shape (`tiles` /
/// `nodes_millions` plus `internal_runs`) whichever entry point produced it.
/// The harness does 1 external run while the example does `req.runs()`
/// internal runs - this path used to hard-code `--runs 1` while printing
/// "N run(s)", and recorded no values at all.
fn run_elivagar_internal(req: &MeasureRequest, command: &ExampleCommand) -> Result<(), DevError> {
    use crate::context::HarnessContext;

    let ctx = HarnessContext::new_for_request(req, &format!("bench {}", command.id()))?;

    let build_root = req.build_root.unwrap_or(req.project_root);
    match command {
        ExampleCommand::PmtilesWriter { tiles } => {
            elivagar::bench_pmtiles::run(&ctx.harness, build_root, *tiles, req.runs())
        }
        ExampleCommand::NodeStore { nodes } => {
            elivagar::bench_node_store::run(&ctx.harness, build_root, *nodes, req.runs())
        }
    }
}

/// Elivagar hotpath/alloc: build with hotpath feature, run with instrumentation.
///
/// The `BuiltCommand` variant determines build type:
/// - MainBinary (Tilegen): `BenchContext::new` with hotpath features, `build_args()`,
///   OOM check, locations_on_ways detection, output cleanup.
/// - Example (PmtilesWriter, NodeStore): `cargo_build` with example + hotpath feature,
///   `build_args()`, `run_hotpath_capture`.
///
/// Both paths go through `run_hotpath_capture` for JSON report collection.
#[allow(clippy::too_many_lines)]
fn run_elivagar_hotpath(req: &MeasureRequest, command: &BuiltCommand<'_>) -> Result<(), DevError> {
    let alloc = req.is_alloc();
    let feature = harness::hotpath_feature(alloc);

    output::detail(&format!("{} {feature}", command.id()));
    harness::hotpath_alloc_note(alloc);

    match command {
        BuiltCommand::MainBinary { .. } => {
            // Tilegen: build main binary with hotpath features, resolve PBF, OOM check.
            let hotpath_features = req.hotpath_features();
            let ctx = BenchContext::new_for_request(
                req,
                command.package(),
                &hotpath_features,
                true,
                &format!("hotpath {}", command.id()),
            )?;

            let pbf = resolve_pbf_entry(req.dataset, req.variant, &ctx.paths, req.project_root)?;
            let pbf_path = pbf.path;
            let file_mb = crate::resolve::file_size_mb(&pbf_path)?;

            let pbf_str = pbf_path
                .to_str()
                .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

            let basename = pbf_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();

            let opts = PipelineOpts::from_pbf_entry(pbf.entry);
            let args = command.build_args(
                pbf_str,
                &ctx.paths.scratch_dir,
                &ctx.paths.data_dir,
                Some(&opts),
            )?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let binary_str = ctx.binary.display().to_string();

            let metadata = command.metadata();

            let config = BenchConfig {
                command: command.id().into(),
                mode: None,
                input_file: Some(basename),
                input_mb: Some(file_mb),
                cargo_features: None,
                cargo_profile: crate::build::CargoProfile::Release,
                runs: req.runs(),
                cli_args: Some(harness::format_cli_args(&binary_str, &arg_refs)),
                brokkr_args: None,
                metadata,
            };

            ctx.harness.run_hotpath(&config, &ctx.binary, |_i| {
                let (mut result, stderr, sidecar) = harness::run_hotpath_capture(
                    &binary_str,
                    &arg_refs,
                    &ctx.paths.scratch_dir,
                    req.project_root,
                    &[("ELIVAGAR_NODE_STATS", "1")],
                    &[],
                    req.stop_marker,
                    Some(ctx.harness.lock()),
                )?;
                result.kv.push(KvPair::text(
                    "meta.locations_on_ways_detected",
                    elivagar::detect_locations_on_ways_stderr(&stderr).to_string(),
                ));
                Ok((result, sidecar))
            })?;

            // Clean up output files.
            for path in command.output_files(&ctx.paths.scratch_dir) {
                std::fs::remove_file(path).ok();
            }
            // Also clean hotpath-specific output.
            let suffix = if alloc { "alloc-" } else { "" };
            let hp_output = ctx
                .paths
                .scratch_dir
                .join(format!("hotpath-{suffix}output.pmtiles"));
            std::fs::remove_file(hp_output).ok();

            Ok(())
        }
        BuiltCommand::Example(example) => {
            // Micro-benchmarks: build example with hotpath feature.
            let ctx = crate::context::HarnessContext::new_for_request(
                req,
                &format!("hotpath {}", command.id()),
            )?;

            let binary = crate::build::cargo_build(
                &crate::build::BuildConfig {
                    package: None,
                    bin: None,
                    example: Some(example.cargo_example().into()),
                    features: vec![feature.into()],
                    default_features: true,
                    all_features: false,
                    profile: "release",
                },
                req.effective_build_root(),
            )?;
            let binary_str = binary.display().to_string();

            let args =
                command.build_args("", std::path::Path::new(""), std::path::Path::new(""), None)?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let metadata = command.metadata();

            let config = BenchConfig {
                command: command.id().into(),
                mode: None,
                input_file: None,
                input_mb: None,
                cargo_features: Some(feature.into()),
                cargo_profile: crate::build::CargoProfile::Release,
                runs: 1,
                cli_args: None,
                brokkr_args: None,
                metadata,
            };

            ctx.harness.run_hotpath(&config, &binary, |_i| {
                let (result, _stderr, sidecar) = harness::run_hotpath_capture(
                    &binary_str,
                    &arg_refs,
                    &ctx.paths.scratch_dir,
                    req.project_root,
                    &[],
                    &[],
                    req.stop_marker,
                    Some(ctx.harness.lock()),
                )?;
                Ok((result, sidecar))
            })?;

            Ok(())
        }
    }
}

/// Number of most-recent `<dataset>-<commit>.pmtiles` archives kept per
/// dataset in the durable output dir. Older ones are pruned after each run so
/// the store does not grow without bound (denmark alone is ~350 MB/commit).
const OUTPUT_RETENTION: usize = 5;

/// After a successful tilegen run, move the raw output PMTiles from the
/// scratch dir to the durable output store as `<output>/<dataset>-<commit>.pmtiles`
/// and print the path. The output dir is deliberately NOT the scratch/tmp dir:
/// elivagar wipes its `--tmp-dir` at every run start, so an archive written
/// into scratch would be destroyed by the next run (which is exactly how a
/// probe pair was already lost). Retention prunes the store to the last
/// [`OUTPUT_RETENTION`] archives per dataset.
///
/// For non-Tilegen commands, cleans up output files as before.
///
/// Every way of ending without a durable archive is an `Err`, not a logged
/// line: a tilegen run whose output never reaches the store has not produced
/// what `pmtiles-inspect`/`regress`/`pmtiles-corpus` will go looking for, and
/// returning success there let a caller (or a script) move on to a resolver
/// that then reports "no build" with nothing tying it to this run. The scratch
/// output is left in place on every failure path so it can be moved by hand.
fn rename_elivagar_output(
    command: &BuiltCommand<'_>,
    scratch_dir: &std::path::Path,
    output_dir: &std::path::Path,
    data_dir: &std::path::Path,
    dataset: &str,
    variant: &str,
    git_root: &std::path::Path,
) -> Result<(), DevError> {
    let output_files = command.output_files(scratch_dir);
    if output_files.is_empty() {
        return Ok(());
    }

    // Only Tilegen produces output to rename.
    if !matches!(command, BuiltCommand::MainBinary { .. }) {
        for path in &output_files {
            std::fs::remove_file(path).ok();
        }
        return Ok(());
    }

    // Refuse to use a wiped directory as the durable store: elivagar wipes its
    // `--tmp-dir` (`<data>/tilegen_tmp`) at every run start, and on some hosts
    // `scratch` is configured to that same dir. With the default output dir
    // (`data/tilegen`) neither branch fires; this guards a host that
    // mis-points `output` at scratch or the tmp dir.
    let tmp_dir = data_dir.join("tilegen_tmp");
    if output_dir == scratch_dir || output_dir == tmp_dir {
        return Err(DevError::Config(format!(
            "output dir {} coincides with elivagar's scratch/tmp dir; \
             set a distinct [<host>].output in brokkr.toml. Output left in {}",
            output_dir.display(),
            scratch_dir.display(),
        )));
    }

    // Provenance must name the commit whose code produced these tiles. For a
    // `--commit <hash>` run that is the worktree's HEAD (`git_root`), NOT the
    // main tree's HEAD - collecting from the main tree would stamp an
    // unrelated commit onto an old build's output (and feed the same
    // misattribution as a stale binary). Callers pass `effective_build_root()`.
    //
    // A git failure is fatal rather than a `-unknown` suffix: no resolver
    // constructs that name, so the archive would sit in the store unreachable
    // by every consumer while the run reported success.
    let commit = crate::git::collect(git_root)
        .map_err(|e| {
            DevError::Io(std::io::Error::other(format!(
                "cannot name the tilegen archive: git commit of {} unavailable ({e}); \
                 output left in {}",
                git_root.display(),
                scratch_dir.display(),
            )))
        })?
        .commit;

    std::fs::create_dir_all(output_dir).map_err(|e| {
        DevError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "failed to create output dir {}: {e}; output left in {}",
                output_dir.display(),
                scratch_dir.display(),
            ),
        ))
    })?;

    for path in &output_files {
        if !path.exists() {
            return Err(DevError::Verify(format!(
                "tilegen exited 0 but wrote no output at {}",
                path.display()
            )));
        }
        let dest = crate::resolve::pmtiles_archive_name(output_dir, dataset, variant, &commit);
        std::fs::rename(path, &dest).map_err(|e| {
            DevError::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "failed to move {} to {}: {e}; output left in place",
                    path.display(),
                    dest.display(),
                ),
            ))
        })?;
        output::run_msg(&format!("output: {}", dest.display()));
        prune_output_dir(output_dir, dataset, variant);
    }
    Ok(())
}

/// Keep only the [`OUTPUT_RETENTION`] most-recent
/// `<dataset>-<variant>-*.pmtiles` archives (by mtime) in `output_dir`,
/// deleting older ones. Scoped to the `(dataset, variant)` pair: building one
/// variant must never evict another's archives (the clobber that made a raw
/// build wipe the locations baseline at the same commit). Best-effort: any IO
/// error just leaves the file in place.
fn prune_output_dir(output_dir: &std::path::Path, dataset: &str, variant: &str) {
    let mut archives: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(output_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_match = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| crate::resolve::pmtiles_archive_matches(n, dataset, variant));
        if !is_match {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        archives.push((mtime, path));
    }
    if archives.len() <= OUTPUT_RETENTION {
        return;
    }
    // Newest first; delete everything past the retention window.
    archives.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    for (_, path) in archives.into_iter().skip(OUTPUT_RETENTION) {
        std::fs::remove_file(&path).ok();
    }
}

/// External tool dispatch (Planetiler, Tilemaker). Request-aware context
/// construction applies the mode's dirty-tree policy for both run and bench.
fn run_elivagar_external(req: &MeasureRequest, tool: &ExternalTool) -> Result<(), DevError> {
    match req.mode {
        MeasureMode::Run | MeasureMode::Bench { .. } => match tool {
            ExternalTool::Planetiler => elivagar::cmd::bench_planetiler(req),
            ExternalTool::Tilemaker => elivagar::cmd::bench_tilemaker(req),
        },
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            tool.id(),
        ))),
    }
}
