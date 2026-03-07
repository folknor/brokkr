//! Preview pipeline: end-to-end visual inspection of the pbfhogg -> elivagar -> nidhogg pipeline.
//!
//! Orchestrates building each project from its own source tree, running the data
//! pipeline steps, starting nidhogg serve, and opening a map viewer in the browser.

use std::path::{Path, PathBuf};

use crate::build;
use crate::cli::PreviewStep;
use crate::config::{self, PreviewConfig, ResolvedPaths};
use crate::error::DevError;
use crate::output;
use crate::resolve;

// ---------------------------------------------------------------------------
// Preview output directory
// ---------------------------------------------------------------------------

/// All preview artifacts live under `.brokkr/preview/` in the current project root.
fn preview_dir(project_root: &Path) -> PathBuf {
    project_root.join(".brokkr").join("preview")
}

fn enriched_pbf_path(project_root: &Path, dataset: &str, variant: &str) -> PathBuf {
    preview_dir(project_root).join(format!("{dataset}-{variant}-enriched.osm.pbf"))
}

fn pmtiles_path(project_root: &Path, dataset: &str, variant: &str) -> PathBuf {
    preview_dir(project_root).join(format!("{dataset}-{variant}.pmtiles"))
}

fn data_dir_path(project_root: &Path, dataset: &str, variant: &str) -> PathBuf {
    preview_dir(project_root).join(format!("{dataset}-{variant}-data"))
}

fn tilegen_tmp_path(project_root: &Path) -> PathBuf {
    preview_dir(project_root).join("tilegen_tmp")
}

// ---------------------------------------------------------------------------
// Config resolution
// ---------------------------------------------------------------------------

/// Resolve the preview config from the current host section.
fn resolve_preview_config(
    paths: &ResolvedPaths,
) -> Result<&PreviewConfig, DevError> {
    paths.preview.as_ref().ok_or_else(|| {
        DevError::Config(format!(
            "no [{}.preview] section in brokkr.toml (required for preview command)",
            paths.hostname,
        ))
    })
}

/// Resolve a preview project path (relative to project_root or absolute).
fn resolve_project_root(
    project_root: &Path,
    configured: &str,
) -> Result<PathBuf, DevError> {
    let p = Path::new(configured);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        project_root.join(p)
    };
    if !resolved.join("Cargo.toml").exists() {
        return Err(DevError::Config(format!(
            "preview project root '{}' does not contain Cargo.toml (resolved from '{configured}')",
            resolved.display(),
        )));
    }
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Artifact validation
// ---------------------------------------------------------------------------

fn check_artifact(path: &Path, name: &str, produced_by: &str) -> Result<(), DevError> {
    if !path.exists() {
        return Err(DevError::Config(format!(
            "{name} not found at {}\n\
             This artifact is produced by the '{produced_by}' step.\n\
             Run without --from or with --from {produced_by} first.",
            path.display(),
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pipeline steps
// ---------------------------------------------------------------------------

fn step_enrich(
    pbfhogg_root: &Path,
    source_pbf: &Path,
    output_pbf: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    output::preview_msg(&format!(
        "enriching {} -> {}",
        source_pbf.display(),
        output_pbf.display(),
    ));

    let binary = build::cargo_build(
        &build::BuildConfig::release(Some("pbfhogg-cli")),
        pbfhogg_root,
    )?;

    let binary_str = binary.display().to_string();
    let source_str = source_pbf.display().to_string();
    let output_str = output_pbf.display().to_string();

    let captured = output::run_captured(
        &binary_str,
        &["add-locations-to-ways", &source_str, "-o", &output_str],
        project_root,
    )?;

    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    captured.check_success(&binary_str)?;

    let mb = resolve::file_size_mb(output_pbf)?;
    output::preview_msg(&format!("enriched PBF: {mb:.1} MB"));

    Ok(())
}

fn step_tilegen(
    elivagar_root: &Path,
    input_pbf: &Path,
    output_pmtiles: &Path,
    tmp_dir: &Path,
    data_dir: &Path,
    project_root: &Path,
    pipeline_opts: &crate::elivagar::PipelineOpts<'_>,
) -> Result<(), DevError> {
    output::preview_msg(&format!(
        "generating tiles {} -> {}",
        input_pbf.display(),
        output_pmtiles.display(),
    ));

    let binary = build::cargo_build(
        &build::BuildConfig::release(None),
        elivagar_root,
    )?;

    let binary_str = binary.display().to_string();

    std::fs::create_dir_all(tmp_dir)?;

    let mut args: Vec<String> = vec![
        "run".into(), input_pbf.display().to_string(),
        "-o".into(), output_pmtiles.display().to_string(),
        "--tmp-dir".into(), tmp_dir.display().to_string(),
    ];

    pipeline_opts.push_args(&mut args, data_dir);

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured(
        &binary_str,
        &arg_refs,
        project_root,
    )?;

    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    captured.check_success(&binary_str)?;

    let mb = resolve::file_size_mb(output_pmtiles)?;
    output::preview_msg(&format!("PMTiles: {mb:.1} MB"));

    Ok(())
}

fn step_ingest(
    nidhogg_root: &Path,
    source_pbf: &Path,
    data_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    output::preview_msg(&format!(
        "ingesting {} -> {}",
        source_pbf.display(),
        data_dir.display(),
    ));

    let binary = build::cargo_build(
        &build::BuildConfig::release(Some("nidhogg")),
        nidhogg_root,
    )?;

    std::fs::create_dir_all(data_dir)?;

    crate::nidhogg::ingest::run(&binary, source_pbf, data_dir, project_root)
}

fn step_serve(
    nidhogg_root: &Path,
    data_dir: &Path,
    pmtiles_path: &Path,
    port: u16,
    project_root: &Path,
) -> Result<(), DevError> {
    output::preview_msg("starting nidhogg server");

    let binary = build::cargo_build(
        &build::BuildConfig::release(Some("nidhogg")),
        nidhogg_root,
    )?;

    let data_str = data_dir.display().to_string();
    let tiles_str = pmtiles_path.display().to_string();

    // Stop any existing server first, then start fresh.
    crate::nidhogg::server::stop(project_root)?;
    crate::nidhogg::server::serve(&binary, &data_str, Some(&tiles_str), port, project_root)?;

    output::preview_msg(&format!("server running on port {port}"));
    Ok(())
}

fn open_browser(port: u16) {
    let url = format!("http://localhost:{port}/map");
    output::preview_msg(&format!("opening {url}"));

    let result = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    if let Err(e) = result {
        output::preview_msg(&format!("could not open browser: {e}"));
        output::preview_msg(&format!("open manually: {url}"));
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(
    dev_config: &config::DevConfig,
    project_root: &Path,
    from: Option<PreviewStep>,
    dataset: &str,
    variant: &str,
    no_open: bool,
    pmtiles_override: Option<&str>,
    pipeline_opts: &crate::elivagar::PipelineOpts<'_>,
) -> Result<(), DevError> {
    // Resolve host config.
    let pi = build::project_info(None)?;
    let hostname = config::hostname()?;
    let paths = config::resolve_paths(dev_config, &hostname, project_root, &pi.target_dir);

    // Validate dataset and PBF exist.
    let source_pbf = resolve::resolve_pbf_path(dataset, variant, &paths, project_root)?;

    // Resolve preview project roots.
    let preview = resolve_preview_config(&paths)?;
    let pbfhogg_root = resolve_project_root(project_root, &preview.pbfhogg)?;
    let elivagar_root = resolve_project_root(project_root, &preview.elivagar)?;
    let nidhogg_root = resolve_project_root(project_root, &preview.nidhogg)?;

    output::preview_msg(&format!("dataset={dataset} variant={variant}"));
    if let Some(p) = pmtiles_override {
        output::preview_msg(&format!("pmtiles  = {p}"));
    }
    output::preview_msg(&format!("pbfhogg  = {}", pbfhogg_root.display()));
    output::preview_msg(&format!("elivagar = {}", elivagar_root.display()));
    output::preview_msg(&format!("nidhogg  = {}", nidhogg_root.display()));

    // Ensure preview output directory exists.
    let preview_out = preview_dir(project_root);
    std::fs::create_dir_all(&preview_out)?;

    // Determine artifact paths.
    let enriched = enriched_pbf_path(project_root, dataset, variant);
    let pmtiles = if let Some(p) = pmtiles_override {
        let path = PathBuf::from(p);
        if !path.exists() {
            return Err(DevError::Config(format!(
                "PMTiles file not found: {}",
                path.display(),
            )));
        }
        path
    } else {
        pmtiles_path(project_root, dataset, variant)
    };
    let data_dir = data_dir_path(project_root, dataset, variant);
    let tmp_dir = tilegen_tmp_path(project_root);

    // Resolve port.
    let port = resolve_port(dev_config);

    // --pmtiles implies --from serve when no explicit --from is given.
    let start_step = match (from, pmtiles_override) {
        (Some(step), _) => step,
        (None, Some(_)) => PreviewStep::Serve,
        (None, None) => PreviewStep::Enrich,
    };

    match start_step {
        PreviewStep::Enrich => {}
        PreviewStep::Tilegen => {
            check_artifact(&enriched, "enriched PBF", "enrich")?;
        }
        PreviewStep::Ingest => {
            check_artifact(&enriched, "enriched PBF", "enrich")?;
            check_artifact(&pmtiles, "PMTiles", "tilegen")?;
        }
        PreviewStep::Serve => {
            check_artifact(&pmtiles, "PMTiles", "tilegen")?;
            if pmtiles_override.is_some() {
                // Tile-only QA: create an empty data dir so nidhogg can start
                // without requiring a prior ingest run.
                std::fs::create_dir_all(&data_dir)?;
            } else {
                check_artifact(&data_dir, "nidhogg data dir", "ingest")?;
            }
        }
    }

    // Run pipeline from the requested step onward.
    if matches!(start_step, PreviewStep::Enrich) {
        step_enrich(&pbfhogg_root, &source_pbf, &enriched, project_root)?;
    }

    if matches!(start_step, PreviewStep::Enrich | PreviewStep::Tilegen) {
        step_tilegen(
            &elivagar_root,
            &enriched,
            &pmtiles,
            &tmp_dir,
            &paths.data_dir,
            project_root,
            pipeline_opts,
        )?;
    }

    if matches!(start_step, PreviewStep::Enrich | PreviewStep::Tilegen | PreviewStep::Ingest) {
        step_ingest(&nidhogg_root, &enriched, &data_dir, project_root)?;
    }

    step_serve(&nidhogg_root, &data_dir, &pmtiles, port, project_root)?;

    if !no_open {
        open_browser(port);
    }

    let url = format!("http://localhost:{port}/map");
    output::preview_msg(&format!("ready: {url}"));

    Ok(())
}

fn resolve_port(dev_config: &config::DevConfig) -> u16 {
    if let Ok(port_str) = std::env::var("PORT")
        && let Ok(port) = port_str.parse::<u16>()
    {
        return port;
    }
    if let Ok(hostname) = config::hostname()
        && let Some(host) = dev_config.hosts.get(&hostname)
        && let Some(port) = host.port
    {
        return port;
    }
    crate::nidhogg::server::DEFAULT_PORT
}
