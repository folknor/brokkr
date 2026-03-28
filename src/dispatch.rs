//! Unified dispatch layer for the `brokkr run <command>` CLI surface.
//!
//! This module replaces the scattered `cmd_bench` / `cmd_hotpath` / `cmd_profile`
//! dispatch for pbfhogg and elivagar with a single entry point per project that
//! handles all measurement modes (wall-clock, hotpath, alloc).

use std::collections::HashMap;
use std::path::Path;

use crate::config;
use crate::context::BenchContext;
use crate::db::KvPair;
use crate::elivagar;
use crate::elivagar::commands::ElivagarCommand;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{CommandContext, MeasureMode, MeasureRequest};
use crate::nidhogg;
use crate::oom;
use crate::output;
use crate::pbfhogg::commands::{InputKind, OutputKind, PbfhoggCommand};
use crate::project::{self, Project};
use crate::resolve::{
    self, resolve_bbox, resolve_default_osc_path, resolve_pbf_path, resolve_pbf_with_size,
};

// ---------------------------------------------------------------------------
// pbfhogg dispatch
// ---------------------------------------------------------------------------

/// Extract I/O mode flags from extra_params, run preflight checks, and return:
/// - extra cargo features to add to the build
/// - extra CLI args to append to the binary invocation
/// - variant suffix for the result DB
fn resolve_io_flags(
    command: &PbfhoggCommand,
    extra_params: &HashMap<String, String>,
) -> Result<(Vec<&'static str>, Vec<&'static str>, String), DevError> {
    let direct_io = extra_params.get("direct_io").is_some();
    let io_uring = extra_params.get("io_uring").is_some();

    if io_uring && !command.supports_io_uring() {
        return Err(DevError::Config(format!(
            "--io-uring is not supported by '{}' (only apply-changes, sort, cat-dedupe, diff-osc)",
            command.id(),
        )));
    }

    if io_uring {
        crate::preflight::run_preflight(&crate::preflight::uring_checks())?;
    }

    let mut features = Vec::new();
    let mut args = Vec::new();
    let mut suffix_parts = Vec::new();

    if direct_io {
        features.push("linux-direct-io");
        args.push("--direct-io");
        suffix_parts.push("direct-io");
    }
    if io_uring {
        features.push("linux-io-uring");
        args.push("--io-uring");
        suffix_parts.push("uring");
    }

    let suffix = if suffix_parts.is_empty() {
        String::new()
    } else {
        format!("+{}", suffix_parts.join("+"))
    };

    Ok((features, args, suffix))
}

/// Run a single pbfhogg command with the specified measurement mode.
///
/// Handles run, bench, hotpath, and alloc for any pbfhogg command.
/// `extra_params` carries command-specific parameters (e.g. `index_type`
/// for `add-locations-to-ways`, `bbox` for extract).
pub fn run_pbfhogg_command_with_params(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &HashMap<String, String>,
) -> Result<(), DevError> {
    project::require(req.project, Project::Pbfhogg, command.id())?;

    match req.mode {
        MeasureMode::Run => run_pbfhogg_run(req, command, osc_seq, extra_params),
        MeasureMode::Bench { .. } => run_pbfhogg_wallclock(req, command, osc_seq, extra_params),
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            run_pbfhogg_hotpath(req, command, osc_seq, extra_params)
        }
    }
}

/// Default run mode: build, run once, print timing. Acquires lockfile, no DB storage.
fn run_pbfhogg_run(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &HashMap<String, String>,
) -> Result<(), DevError> {
    let (io_features, io_args, _) = resolve_io_flags(command, extra_params)?;

    let mut feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    feat_refs.extend_from_slice(&io_features);

    // Run mode never stores results, so dirty tree is always fine.
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &feat_refs,
        true,
        &format!("run {}", command.id()),
        true,
        req.wait,
    )?;

    let cmd_ctx =
        build_pbfhogg_context(req, command, osc_seq, &ctx.binary, &ctx.paths, extra_params)?;
    let mut args = command.build_args(&cmd_ctx)?;
    for flag in &io_args {
        args.push((*flag).into());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let binary_str = ctx.binary.display().to_string();
    output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));

    let out = output::run_passthrough_timed(&binary_str, &arg_refs)?;

    cleanup_pbfhogg_output(command, &cmd_ctx);

    if out.code != 0 {
        return Err(DevError::ExitCode(out.code));
    }

    let ms = crate::duration_ms(out.elapsed);
    output::run_msg(&format!("elapsed={ms}ms"));

    Ok(())
}

/// Wall-clock benchmark: build release binary, run externally, record timing.
fn run_pbfhogg_wallclock(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &HashMap<String, String>,
) -> Result<(), DevError> {
    let (io_features, io_args, io_suffix) = resolve_io_flags(command, extra_params)?;

    let mut feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    feat_refs.extend_from_slice(&io_features);

    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &feat_refs,
        true,
        &format!("run {}", command.id()),
        req.force,
        req.wait,
    )?;

    let cmd_ctx =
        build_pbfhogg_context(req, command, osc_seq, &ctx.binary, &ctx.paths, extra_params)?;
    let mut args = command.build_args(&cmd_ctx)?;
    for flag in &io_args {
        args.push((*flag).into());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (_, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let basename = cmd_ctx.pbf_basename();

    // Append I/O mode suffix to variant (e.g. "add-locations-to-ways+direct-io").
    let variant = match command.result_variant() {
        Some(v) => Some(format!("{v}{io_suffix}")),
        None if !io_suffix.is_empty() => Some(format!("{}{io_suffix}", command.id())),
        None => None,
    };

    let config = BenchConfig {
        command: command.result_command().into(),
        variant,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &arg_refs,
        )),
        metadata: command.metadata(&cmd_ctx),
    };

    output::bench_msg(&format!(
        "{} ({file_mb:.0} MB), {} run(s)",
        command.id(),
        req.runs(),
    ));

    ctx.harness
        .run_external(&config, &ctx.binary, &arg_refs, req.project_root)?;

    // Clean up scratch output files.
    cleanup_pbfhogg_output(command, &cmd_ctx);

    Ok(())
}

/// Hotpath/alloc mode: build with hotpath feature, run via run_hotpath_capture.
fn run_pbfhogg_hotpath(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &HashMap<String, String>,
) -> Result<(), DevError> {
    if !command.supports_hotpath() {
        return Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        )));
    }

    let alloc = matches!(req.mode, MeasureMode::Alloc { .. });
    let feature = harness::hotpath_feature(alloc);
    let (io_features, io_args, io_suffix) = resolve_io_flags(command, extra_params)?;

    // Build features: hotpath + user features + I/O features.
    let mut all_features: Vec<&str> = vec![feature];
    all_features.extend(req.features.iter().map(String::as_str));
    all_features.extend_from_slice(&io_features);

    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &all_features,
        true,
        &format!(
            "run {} --{}",
            command.id(),
            if alloc { "alloc" } else { "hotpath" }
        ),
        req.force,
        req.wait,
    )?;

    // OOM check.
    let (_, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let risk = if alloc {
        oom::MemoryRisk::AllocTracking
    } else {
        oom::MemoryRisk::Normal
    };
    oom::check_memory(file_mb, &risk, req.no_mem_check)?;

    let cmd_ctx =
        build_pbfhogg_context(req, command, osc_seq, &ctx.binary, &ctx.paths, extra_params)?;
    let mut hotpath_args = command.build_hotpath_args(&cmd_ctx)?;
    for flag in &io_args {
        hotpath_args.push((*flag).into());
    }

    let label = feature;
    output::hotpath_msg(&format!("=== {} {label} ===", command.id()));

    if alloc {
        output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
    }

    let variant_suffix = harness::hotpath_variant_suffix(alloc);
    let variant = format!("{}{variant_suffix}{io_suffix}", command.id());

    let basename = cmd_ctx.pbf_basename();
    let subprocess_args: Vec<&str> = hotpath_args[1..].iter().map(String::as_str).collect();

    let config = BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &subprocess_args,
        )),
        metadata: vec![
            KvPair::text("meta.alloc", alloc.to_string()),
            KvPair::text("meta.test", command.id()),
        ],
    };

    let binary_str = ctx.binary.display().to_string();
    let scratch_dir = ctx.paths.scratch_dir.clone();
    let project_root = req.project_root.to_path_buf();

    ctx.harness.run_internal(&config, |_i| {
        output::hotpath_msg(command.id());
        let (result, _stderr, _sidecar) = harness::run_hotpath_capture(
            &binary_str,
            &subprocess_args,
            &scratch_dir,
            &project_root,
            &[],
        )?;
        Ok(result)
    })?;

    cleanup_pbfhogg_output(command, &cmd_ctx);

    Ok(())
}

/// Build the `CommandContext` for a pbfhogg command, resolving all input paths.
fn build_pbfhogg_context(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    binary: &Path,
    paths: &config::ResolvedPaths,
    extra_params: &HashMap<String, String>,
) -> Result<CommandContext, DevError> {
    let pbf_path = resolve_pbf_path(req.dataset, req.variant, paths, req.project_root)?;

    // Resolve OSC path if needed.
    let osc_path = if command.needs_osc() {
        let osc = match osc_seq {
            Some(seq) => resolve::resolve_osc_path(req.dataset, seq, paths, req.project_root)?,
            None => resolve_default_osc_path(req.dataset, paths, req.project_root)?,
        };
        Some(osc)
    } else {
        None
    };

    // Resolve merged PBF if needed (diff/diff-osc commands).
    let merged_pbf_path = if command.input_kind() == InputKind::PbfAndMerged {
        let osc = osc_path
            .as_ref()
            .ok_or_else(|| DevError::Config("merged PBF requires an OSC file".into()))?;
        Some(ensure_merged_pbf(
            binary,
            &pbf_path,
            osc,
            &paths.scratch_dir,
            req.project_root,
        )?)
    } else {
        None
    };

    // Resolve bbox if needed. Check extra_params for a CLI override first.
    let bbox = if command.needs_bbox() {
        let cli_bbox = extra_params.get("bbox").map(String::as_str);
        Some(resolve_bbox(cli_bbox, req.dataset, paths)?)
    } else {
        None
    };

    Ok(CommandContext {
        binary: binary.to_path_buf(),
        pbf_path,
        osc_path,
        merged_pbf_path,
        scratch_dir: paths.scratch_dir.clone(),
        dataset: req.dataset.to_owned(),
        bbox,
        params: extra_params.clone(),
    })
}

/// Clean up scratch output files after a benchmark run.
fn cleanup_pbfhogg_output(command: &PbfhoggCommand, ctx: &CommandContext) {
    match command.output_kind() {
        OutputKind::ScratchPbf(_) => {
            let name = command.id();
            let path = ctx.scratch_dir.join(format!("bench-{name}-output.osm.pbf"));
            std::fs::remove_file(path).ok();
        }
        OutputKind::ScratchOsc(_) => {
            let name = command.id();
            let path = ctx.scratch_dir.join(format!("bench-{name}-output.osc.gz"));
            std::fs::remove_file(path).ok();
        }
        OutputKind::ScratchDir(dir_name) => {
            let path = ctx.scratch_dir.join(format!("{dir_name}-{}", ctx.dataset));
            std::fs::remove_dir_all(path).ok();
        }
        OutputKind::None => {}
    }
}

/// Ensure a merged PBF exists in the scratch directory. Returns the path.
///
/// Ported from `bench_commands::ensure_merged_pbf`.
fn ensure_merged_pbf(
    binary: &Path,
    pbf_path: &Path,
    osc_path: &Path,
    scratch_dir: &Path,
    project_root: &Path,
) -> Result<std::path::PathBuf, DevError> {
    let stem = pbf_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("input");
    let merged_name = format!("{stem}-bench-merged.osm.pbf");
    let merged_path = scratch_dir.join(&merged_name);

    if merged_path.exists() {
        output::bench_msg(&format!("using cached merged PBF: {merged_name}"));
        return Ok(merged_path);
    }

    std::fs::create_dir_all(scratch_dir)
        .map_err(|e| DevError::Config(format!("failed to create scratch dir: {e}")))?;

    output::bench_msg(&format!("generating merged PBF: {merged_name}"));
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path not UTF-8".into()))?;
    let osc_str = osc_path
        .to_str()
        .ok_or_else(|| DevError::Config("OSC path not UTF-8".into()))?;
    let merged_str = merged_path
        .to_str()
        .ok_or_else(|| DevError::Config("merged path not UTF-8".into()))?;
    let binary_str = binary.display().to_string();

    let captured = output::run_captured(
        &binary_str,
        &["apply-changes", pbf_str, osc_str, "-o", merged_str],
        project_root,
    )?;

    captured.check_success(&binary_str)?;

    Ok(merged_path)
}

// ---------------------------------------------------------------------------
// elivagar dispatch
// ---------------------------------------------------------------------------

/// Run an elivagar command with the specified measurement mode.
///
/// Handles run, bench, hotpath, and alloc for any elivagar command.
/// External commands (Planetiler, Tilemaker) delegate to old handlers for
/// bench mode but do not support run/hotpath/alloc.
pub fn run_elivagar_command(
    req: &MeasureRequest,
    command: &ElivagarCommand,
) -> Result<(), DevError> {
    project::require(req.project, Project::Elivagar, command.id())?;

    // External tools (Planetiler, Tilemaker) keep their old dispatch path.
    if command.is_external() {
        return run_elivagar_external(req, command);
    }

    match req.mode {
        MeasureMode::Run => run_elivagar_run(req, command),
        MeasureMode::Bench { .. } => run_elivagar_bench(req, command),
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            run_elivagar_hotpath(req, command)
        }
    }
}

/// Elivagar run mode: build, run once, print timing. No DB storage.
///
/// Uses `ElivagarCommand::build_config()` to determine build type and
/// `ElivagarCommand::build_args()` for argument construction.
fn run_elivagar_run(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    use crate::elivagar::commands::BuildKind;

    match command.build_config() {
        BuildKind::MainBinary => {
            let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
            // Run mode never stores results, so dirty tree is always fine.
            let ctx = BenchContext::new(
                req.dev_config,
                req.project,
                req.project_root,
                req.build_root,
                command.package(),
                &feat_refs,
                true,
                &format!("run {}", command.id()),
                true,
                req.wait,
            )?;

            let pbf_str = if command.needs_pbf() {
                let (p, _) =
                    resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
                p.to_str()
                    .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?
                    .to_owned()
            } else {
                String::new()
            };

            let args = command.build_args(&pbf_str, &ctx.paths.scratch_dir, &ctx.paths.data_dir)?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let binary_str = ctx.binary.display().to_string();
            output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));

            let out = output::run_passthrough_timed(&binary_str, &arg_refs)?;

            if out.code != 0 {
                for path in command.output_files(&ctx.paths.scratch_dir) {
                    std::fs::remove_file(path).ok();
                }
                return Err(DevError::ExitCode(out.code));
            }

            rename_elivagar_output(command, &ctx.paths.scratch_dir, req.dataset, req.project_root);

            let ms = crate::duration_ms(out.elapsed);
            output::run_msg(&format!("elapsed={ms}ms"));
            Ok(())
        }
        BuildKind::Example(example) => {
            let build_root = req.build_root.unwrap_or(req.project_root);
            let binary = crate::build::cargo_build(
                &crate::build::BuildConfig {
                    package: None,
                    bin: None,
                    example: Some(example.into()),
                    features: vec![],
                    default_features: true,
                    profile: "release",
                },
                build_root,
            )?;
            let binary_str = binary.display().to_string();

            let args =
                command.build_args("", std::path::Path::new(""), std::path::Path::new(""))?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));

            let out = output::run_passthrough_timed(&binary_str, &arg_refs)?;

            if out.code != 0 {
                return Err(DevError::ExitCode(out.code));
            }

            let ms = crate::duration_ms(out.elapsed);
            output::run_msg(&format!("elapsed={ms}ms"));
            Ok(())
        }
        BuildKind::NoBuild => Err(DevError::Config(format!(
            "command '{}' does not support run mode",
            command.id(),
        ))),
    }
}

/// Elivagar bench mode: full harness with DB storage.
///
/// Uses `ElivagarCommand::build_config()` to determine how to build and
/// `ElivagarCommand::build_args()` to get the argument vector. Tilegen uses
/// `run_external_with_kv` (parses kv from stderr); micro-benchmarks use
/// `run_internal` (examples handle their own iteration).
fn run_elivagar_bench(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    use crate::elivagar::commands::BuildKind;

    match command.build_config() {
        BuildKind::MainBinary => run_elivagar_wallclock(req, command),
        BuildKind::Example(_) => run_elivagar_internal(req, command),
        BuildKind::NoBuild => Err(DevError::Config(format!(
            "command '{}' does not support bench mode via this path",
            command.id(),
        ))),
    }
}

/// Elivagar wallclock benchmark for the main binary (Tilegen).
///
/// Builds the main binary, runs via `run_external_with_kv`, parses kv metrics
/// from stderr, detects `locations_on_ways`, and stores results.
fn run_elivagar_wallclock(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        command.package(),
        &feat_refs,
        true,
        &format!("bench {}", command.id()),
        req.force,
        req.wait,
    )?;

    let (pbf_path, file_mb) = if command.needs_pbf() {
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?
    } else {
        (std::path::PathBuf::new(), 0.0)
    };
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let args = command.build_args(pbf_str, &ctx.paths.scratch_dir, &ctx.paths.data_dir)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::bench_msg(&format!(
        "{} ({file_mb:.0} MB), {} run(s)",
        command.id(),
        req.runs(),
    ));

    let mut bench_config = BenchConfig {
        command: command.result_command().into(),
        variant: command.result_variant(),
        input_file: if command.needs_pbf() {
            Some(basename)
        } else {
            None
        },
        input_mb: if command.needs_pbf() {
            Some(file_mb)
        } else {
            None
        },
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &arg_refs,
        )),
        metadata: command.metadata(),
    };

    // Use kv parsing: elivagar emits timing/metrics to stderr.
    // Sidecar monitoring runs automatically via run_external_with_kv_raw.
    let (result, stderr) = ctx.harness.run_external_with_kv_raw(
        &bench_config,
        &ctx.binary,
        &arg_refs,
        req.project_root,
    )?;
    let detected = elivagar::detect_locations_on_ways_stderr(&stderr);
    bench_config.metadata.push(KvPair::text(
        "meta.locations_on_ways_detected",
        detected.to_string(),
    ));
    ctx.harness.record_result(&bench_config, &result)?;

    rename_elivagar_output(command, &ctx.paths.scratch_dir, req.dataset, req.project_root);

    Ok(())
}

/// Elivagar internal benchmark for cargo examples (PmtilesWriter, NodeStore).
///
/// Builds the example binary, runs via `run_internal` (the example handles
/// its own iteration), and stores results. The harness does 1 external run
/// while the example does N internal runs.
fn run_elivagar_internal(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    use crate::context::HarnessContext;

    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        &format!("bench {}", command.id()),
        req.force,
        req.wait,
    )?;

    let example = command.example().ok_or_else(|| {
        DevError::Config(format!("command '{}' has no cargo example", command.id()))
    })?;

    let build_root = req.build_root.unwrap_or(req.project_root);
    let binary = crate::build::cargo_build(
        &crate::build::BuildConfig {
            package: None,
            bin: None,
            example: Some(example.into()),
            features: vec![],
            default_features: true,
            profile: "release",
        },
        build_root,
    )?;
    let binary_str = binary.display().to_string();

    // build_args returns ["--tiles", "500000", "--runs", "1"] or similar.
    // We pass an empty pbf_str and dummy paths since examples don't need them.
    let args = command.build_args("", std::path::Path::new(""), std::path::Path::new(""))?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::bench_msg(&format!("{}, {} run(s)", command.id(), req.runs()));

    let config = BenchConfig {
        command: command.result_command().into(),
        variant: command.result_variant(),
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: 1, // example handles its own iterations
        cli_args: None,
        metadata: command.metadata(),
    };

    ctx.harness.run_internal(&config, |_i| {
        let captured = output::run_captured(&binary_str, &arg_refs, build_root)?;
        captured.check_success(&binary_str)?;
        let ms = harness::elapsed_to_ms(&captured.elapsed);
        Ok(crate::harness::BenchResult {
            elapsed_ms: ms,
            kv: vec![],
            distribution: None,
            hotpath: None,
        })
    })?;

    Ok(())
}

/// Elivagar hotpath/alloc: build with hotpath feature, run with instrumentation.
///
/// Uses `ElivagarCommand::build_config()` to determine build type:
/// - MainBinary (Tilegen): `BenchContext::new` with hotpath features, `build_args()`,
///   OOM check, locations_on_ways detection, output cleanup.
/// - Example (PmtilesWriter, NodeStore): `cargo_build` with example + hotpath feature,
///   `build_args()`, `run_hotpath_capture`.
///
/// Both paths go through `run_hotpath_capture` for JSON report collection.
#[allow(clippy::too_many_lines)]
fn run_elivagar_hotpath(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    use crate::elivagar::commands::BuildKind;

    if !command.supports_hotpath() {
        return Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        )));
    }

    let alloc = req.is_alloc();
    let feature = harness::hotpath_feature(alloc);
    let variant_suffix = harness::hotpath_variant_suffix(alloc);
    let variant = format!("{}{variant_suffix}", command.id());

    output::hotpath_msg(&format!("=== {} {feature} ===", command.id()));
    if alloc {
        output::hotpath_msg("NOTE: alloc profiling — wall-clock times are not meaningful");
    }

    match command.build_config() {
        BuildKind::MainBinary => {
            // Tilegen: build main binary with hotpath features, resolve PBF, OOM check.
            let hotpath_features = req.hotpath_features();
            let ctx = BenchContext::new(
                req.dev_config,
                req.project,
                req.project_root,
                req.build_root,
                command.package(),
                &hotpath_features,
                true,
                &format!("hotpath {}", command.id()),
                req.force,
                req.wait,
            )?;

            let (pbf_path, file_mb) =
                resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
            let risk = if alloc {
                oom::MemoryRisk::AllocTracking
            } else {
                oom::MemoryRisk::Normal
            };
            oom::check_memory(file_mb, &risk, req.no_mem_check)?;

            let pbf_str = pbf_path
                .to_str()
                .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

            let basename = pbf_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();

            let args = command.build_args(pbf_str, &ctx.paths.scratch_dir, &ctx.paths.data_dir)?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let binary_str = ctx.binary.display().to_string();

            let mut metadata = command.metadata();
            metadata.push(KvPair::text("meta.alloc", alloc.to_string()));

            let config = BenchConfig {
                command: "hotpath".into(),
                variant: Some(variant),
                input_file: Some(basename),
                input_mb: Some(file_mb),
                cargo_features: None,
                cargo_profile: "release".into(),
                runs: req.runs(),
                cli_args: Some(harness::format_cli_args(&binary_str, &arg_refs)),
                metadata,
            };

            ctx.harness.run_internal(&config, |_i| {
                let (mut result, stderr, _sidecar) = harness::run_hotpath_capture(
                    &binary_str,
                    &arg_refs,
                    &ctx.paths.scratch_dir,
                    req.project_root,
                    &[("ELIVAGAR_NODE_STATS", "1")],
                )?;
                result.kv.push(KvPair::text(
                    "meta.locations_on_ways_detected",
                    elivagar::detect_locations_on_ways_stderr(&stderr).to_string(),
                ));
                Ok(result)
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
        BuildKind::Example(example) => {
            // Micro-benchmarks: build example with hotpath feature.
            let ctx = crate::context::HarnessContext::new(
                req.dev_config,
                req.project,
                req.project_root,
                req.build_root,
                &format!("hotpath {}", command.id()),
                req.force,
                req.wait,
            )?;

            let binary = crate::build::cargo_build(
                &crate::build::BuildConfig {
                    package: None,
                    bin: None,
                    example: Some(example.into()),
                    features: vec![feature.into()],
                    default_features: true,
                    profile: "release",
                },
                req.effective_build_root(),
            )?;
            let binary_str = binary.display().to_string();

            let args =
                command.build_args("", std::path::Path::new(""), std::path::Path::new(""))?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let mut metadata = command.metadata();
            metadata.push(KvPair::text("meta.alloc", alloc.to_string()));

            let config = BenchConfig {
                command: "hotpath".into(),
                variant: Some(variant),
                input_file: None,
                input_mb: None,
                cargo_features: Some(feature.into()),
                cargo_profile: "release".into(),
                runs: 1,
                cli_args: None,
                metadata,
            };

            ctx.harness.run_internal(&config, |_i| {
                let (result, _stderr, _sidecar) = harness::run_hotpath_capture(
                    &binary_str,
                    &arg_refs,
                    &ctx.paths.scratch_dir,
                    req.project_root,
                    &[],
                )?;
                Ok(result)
            })?;

            Ok(())
        }
        BuildKind::NoBuild => Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        ))),
    }
}

/// After a successful tilegen run, rename the output PMTiles to
/// `<scratch>/<dataset>-<commit>.pmtiles` and print the path.
///
/// For non-Tilegen commands, cleans up output files as before.
fn rename_elivagar_output(
    command: &ElivagarCommand,
    scratch_dir: &std::path::Path,
    dataset: &str,
    project_root: &std::path::Path,
) {
    let output_files = command.output_files(scratch_dir);
    if output_files.is_empty() {
        return;
    }

    // Only Tilegen produces output to rename.
    if !matches!(command, ElivagarCommand::Tilegen { .. }) {
        for path in &output_files {
            std::fs::remove_file(path).ok();
        }
        return;
    }

    let commit = crate::git::collect(project_root)
        .map(|g| g.commit)
        .unwrap_or_else(|_| "unknown".into());

    for path in &output_files {
        if path.exists() {
            let dest = scratch_dir.join(format!("{dataset}-{commit}.pmtiles"));
            match std::fs::rename(path, &dest) {
                Ok(()) => {
                    output::run_msg(&format!("output: {}", dest.display()));
                }
                Err(e) => {
                    output::error(&format!("failed to rename output: {e}"));
                    // Leave the original in place.
                }
            }
        }
    }
}

/// External tool dispatch (Planetiler, Tilemaker). Only bench mode is supported.
fn run_elivagar_external(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    match req.mode {
        MeasureMode::Run | MeasureMode::Bench { .. } => match command {
            ElivagarCommand::Planetiler => elivagar::cmd::bench_planetiler(req),
            ElivagarCommand::Tilemaker => elivagar::cmd::bench_tilemaker(req),
            _ => unreachable!(),
        },
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        ))),
    }
}

// ---------------------------------------------------------------------------
// nidhogg dispatch
// ---------------------------------------------------------------------------

/// Run a nidhogg command with the specified measurement mode.
///
/// Ingest follows the standard build+run_external pattern (like pbfhogg).
/// Api and Tiles have custom lifecycles and delegate to per-module functions.
pub fn run_nidhogg_command(
    req: &MeasureRequest,
    command: &crate::nidhogg::commands::NidhoggCommand,
) -> Result<(), DevError> {
    use crate::nidhogg::commands::NidhoggCommand;

    project::require(req.project, Project::Nidhogg, command.id())?;

    match req.mode {
        MeasureMode::Run => match command {
            NidhoggCommand::Ingest => run_nidhogg_ingest_run(req),
            // Api/Tiles have no lightweight run mode — fall through to bench.
            NidhoggCommand::Api { query } => nidhogg::cmd::bench_api(req, query.as_deref()),
            NidhoggCommand::Tiles { tiles_variant, uring } => {
                nidhogg::cmd::bench_tiles(req, tiles_variant.as_deref(), *uring)
            }
        },
        MeasureMode::Bench { .. } => match command {
            NidhoggCommand::Ingest => run_nidhogg_ingest_bench(req, command),
            NidhoggCommand::Api { query } => nidhogg::cmd::bench_api(req, query.as_deref()),
            NidhoggCommand::Tiles { tiles_variant, uring } => {
                nidhogg::cmd::bench_tiles(req, tiles_variant.as_deref(), *uring)
            }
        },
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            if !command.supports_hotpath() {
                return Err(DevError::Config(format!(
                    "command '{}' does not support hotpath/alloc profiling",
                    command.id(),
                )));
            }
            run_nidhogg_hotpath(req, command)
        }
    }
}

/// Nidhogg ingest: lightweight run mode (build, run once, print timing, no DB).
fn run_nidhogg_ingest_run(req: &MeasureRequest) -> Result<(), DevError> {
    let feat_refs = req.feat_refs();
    // Run mode never stores results, so dirty tree is always fine.
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("nidhogg"),
        &feat_refs,
        true,
        &format!("run {}", "nid-ingest"),
        true,
        req.wait,
    )?;

    let (pbf_path, _) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let output_dir = ctx.paths.scratch_dir.join("run-ingest-output");
    std::fs::create_dir_all(&output_dir)?;
    let output_str = output_dir.display().to_string();

    let binary_str = ctx.binary.display().to_string();
    let args = ["ingest", pbf_str, &output_str];
    output::run_msg(&format!("{binary_str} {}", args.join(" ")));

    let out = output::run_passthrough_timed(&binary_str, &args)?;

    // Clean up ingest output.
    std::fs::remove_dir_all(&output_dir).ok();

    if out.code != 0 {
        return Err(DevError::ExitCode(out.code));
    }

    let ms = crate::duration_ms(out.elapsed);
    output::run_msg(&format!("elapsed={ms}ms"));
    Ok(())
}

/// Nidhogg ingest: bench mode via BenchContext + run_external.
///
/// Uses `run_internal` with per-run scratch cleanup (ingest produces a
/// data directory that must be cleaned between runs for accurate timing).
fn run_nidhogg_ingest_bench(
    req: &MeasureRequest,
    command: &crate::nidhogg::commands::NidhoggCommand,
) -> Result<(), DevError> {
    let feat_refs = req.feat_refs();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("nidhogg"),
        &feat_refs,
        true,
        &format!("bench {}", command.id()),
        req.force,
        req.wait,
    )?;

    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let output_dir = ctx.paths.scratch_dir.join("bench-ingest-output");
    let output_str = output_dir.display().to_string();

    output::bench_msg(&format!(
        "nidhogg ingest: {basename} ({file_mb:.0} MB), {} run(s)",
        req.runs(),
    ));

    let args: Vec<&str> = vec!["ingest", pbf_str, &output_str];

    let config = BenchConfig {
        command: command.result_command().into(),
        variant: command.result_variant(),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &args,
        )),
        metadata: command.metadata(),
    };

    // Clean scratch before first run.
    clean_nidhogg_scratch(&output_dir)?;

    // Per-run cleanup: ingest produces a data directory that must be
    // cleaned between runs for accurate timing.
    ctx.harness.run_internal(&config, |_i| {
        clean_nidhogg_scratch(&output_dir)?;

        let captured =
            output::run_captured(&ctx.binary.display().to_string(), &args, req.project_root)?;
        let ms = harness::elapsed_to_ms(&captured.elapsed);
        captured.check_success(&ctx.binary.display().to_string())?;

        Ok(harness::BenchResult {
            elapsed_ms: ms,
            kv: vec![],
            distribution: None,
            hotpath: None,
        })
    })?;

    std::fs::remove_dir_all(&output_dir).ok();
    Ok(())
}

/// Nidhogg hotpath/alloc via the dispatch layer.
///
/// Builds the nidhogg binary with hotpath features, resolves PBF,
/// runs via `run_hotpath_capture`, same pattern as pbfhogg and elivagar.
fn run_nidhogg_hotpath(
    req: &MeasureRequest,
    command: &crate::nidhogg::commands::NidhoggCommand,
) -> Result<(), DevError> {
    let alloc = req.is_alloc();
    let feature = harness::hotpath_feature(alloc);
    let variant_suffix = harness::hotpath_variant_suffix(alloc);
    let variant = format!("{}{variant_suffix}", command.id());

    output::hotpath_msg(&format!("=== nidhogg {} {feature} ===", command.id()));
    if alloc {
        output::hotpath_msg("NOTE: alloc profiling — wall-clock times are not meaningful");
    }

    let hotpath_features = req.hotpath_features();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("nidhogg"),
        &hotpath_features,
        true,
        &format!("hotpath {}", command.id()),
        req.force,
        req.wait,
    )?;

    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let risk = if alloc {
        oom::MemoryRisk::AllocTracking
    } else {
        oom::MemoryRisk::Normal
    };
    oom::check_memory(file_mb, &risk, req.no_mem_check)?;

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let output_dir = ctx.paths.scratch_dir.join("hotpath-ingest-output");
    let output_str = output_dir.display().to_string();
    std::fs::create_dir_all(&output_dir)?;

    let args: Vec<&str> = vec!["ingest", pbf_str, &output_str];

    let config = BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &args,
        )),
        metadata: vec![KvPair::text("meta.alloc", alloc.to_string())],
    };

    let binary_str = ctx.binary.display().to_string();

    ctx.harness.run_internal(&config, |_i| {
        let (result, _stderr, _sidecar) =
            harness::run_hotpath_capture(&binary_str, &args, &ctx.paths.scratch_dir, req.project_root, &[])?;
        Ok(result)
    })?;

    std::fs::remove_dir_all(&output_dir).ok();
    Ok(())
}

/// Remove and recreate a scratch directory (used by nidhogg ingest between runs).
fn clean_nidhogg_scratch(dir: &std::path::Path) -> Result<(), DevError> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    Ok(())
}
