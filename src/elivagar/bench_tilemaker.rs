//! Benchmark: Tilemaker Shortbread tilegen for comparison.
//!
//! Replaces `bench-tilemaker.sh`. Auto-downloads/builds Tilemaker + shortbread
//! config via `tools::ensure_tilemaker`, fetches 4326 ocean shapefiles, symlinks
//! them into the shortbread config tree, then runs N times with external
//! wall-clock timing (best of N).

use std::path::Path;

use crate::clean::layout::ElivagarLayout;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness};
use crate::output;
use crate::tools;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    layout: &ElivagarLayout<'_>,
    project_root: &Path,
) -> Result<(), DevError> {
    let invocation = prepare(pbf_path, file_mb, runs, layout, false)?;
    harness.run_external_prepared(
        &invocation.config,
        &invocation.invocation(),
        project_root,
        &[],
        || invocation.prepare_sample(),
    )?;
    invocation.cleanup();
    Ok(())
}

pub(crate) fn prepare(
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    layout: &ElivagarLayout<'_>,
    dry_run: bool,
) -> Result<crate::measure::runner::ExternalInvocation, DevError> {
    let data_dir = layout.data_dir();
    let scratch_dir = layout.scratch();
    let tm = if dry_run {
        tools::TilemakerTools {
            tilemaker: data_dir.join("tilemaker/build/tilemaker"),
            config: data_dir.join("shortbread-tilemaker/config.json"),
            process: data_dir.join("shortbread-tilemaker/process.lua"),
        }
    } else {
        tools::ensure_tilemaker(data_dir)?
    };
    if !dry_run {
        super::download_ocean::ensure_ocean_4326(data_dir)?;

        // Symlink ocean directories into shortbread-tilemaker/data/ so Tilemaker
        // can find them at the relative paths its config expects.
        let sb_data = data_dir.join("shortbread-tilemaker/data");
        std::fs::create_dir_all(&sb_data)?;

        ensure_symlink(
            &data_dir.join(super::download_ocean::FULL_RES_4326_DIR),
            &sb_data.join(super::download_ocean::FULL_RES_4326_DIR),
        )?;
        ensure_symlink(
            &data_dir.join(super::download_ocean::SIMPLIFIED_4326_DIR),
            &sb_data.join(super::download_ocean::SIMPLIFIED_4326_DIR),
        )?;
    }

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    if !dry_run {
        std::fs::create_dir_all(scratch_dir)?;
    }
    let output_path = layout.tilemaker_output();
    let output_str = output_path.display().to_string();

    if !dry_run {
        output::bench_msg(&format!(
            "Tilemaker Shortbread: {basename} ({file_mb:.0} MB), {runs} run(s)"
        ));
    }

    let config_str = tm.config.display().to_string();
    let process_str = tm.process.display().to_string();

    let args_owned: Vec<String> = vec![
        "--input".into(),
        pbf_str.into(),
        "--output".into(),
        output_str,
        "--config".into(),
        config_str,
        "--process".into(),
        process_str,
        "--fast".into(),
    ];
    let args_refs: Vec<&str> = args_owned.iter().map(String::as_str).collect();

    let config = BenchConfig {
        command: "tilemaker".into(),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        external_build: Some(crate::build::BuildKind::CMake),
        runs,
        cli_args: Some(harness::format_cli_args(
            &tm.tilemaker.display().to_string(),
            &args_refs,
        )),
        metadata: vec![],
    };

    Ok(crate::measure::runner::ExternalInvocation {
        program: tm.tilemaker,
        args: args_owned,
        config,
        output_files: vec![output_path],
        scratch_dirs: vec![],
    })
}

/// Make `link` a symlink to `target`, repairing one that points elsewhere or
/// dangles. `Path::exists` follows the link, so a dangling symlink reads as
/// absent - and re-creating it over the existing link entry then fails with
/// `EEXIST`, which is how a moved data dir broke every later tilemaker run.
/// A real directory at `link` is the user's (a copied data tree, say) and is
/// used as-is, never replaced.
fn ensure_symlink(target: &Path, link: &Path) -> Result<(), DevError> {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(link)? == target && link.exists() {
                return Ok(());
            }
            std::fs::remove_file(link)?;
        }
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(DevError::Io(e)),
    }
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}
