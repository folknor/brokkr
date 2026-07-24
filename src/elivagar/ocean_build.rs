//! `brokkr ocean-build` - wrap `elivagar ocean-build` (one shot per shapefile
//! release).
//!
//! The invocation is derived entirely from `[<host>.tilegen.default].ocean` -
//! the same block `tilegen` reads its ocean inputs from. The shapefile entries
//! become the `--ocean` specs (banded, exactly as tilegen passes them); the
//! `.pmtiles` entry becomes the output path. There are no override flags: to
//! build a different artifact, edit the block, same philosophy as tilegen.
//! This makes the artifact builder and the artifact consumer read the same
//! statement, so they cannot drift on spelling, and the artifact key elivagar
//! records is derived from the same shapefiles every run re-hashes.
//!
//! Rotating the artifact is an output-changing event: the next `pmtiles-corpus
//! check` refuses on the artifact key until the corpus is re-blessed. That
//! refusal is elivagar's job; brokkr just runs the commands.

use std::path::Path;

use crate::build;
use crate::config::{self, OceanSpec};
use crate::context::{bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::lockfile::LockGuard;
use crate::output;
use crate::project::{self, Project};

use super::{DEFAULT_TILEGEN, resolve_tilegen};

pub fn run(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    dry_run: bool,
    lock: Option<&LockGuard>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "ocean-build")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let tg = resolve_tilegen(dev_config, DEFAULT_TILEGEN)?;
    let specs = tg
        .ocean_specs()
        .map_err(|e| DevError::Config(format!("tilegen ocean: {e}")))?;

    // Partition the ocean block by role: shapefiles are the inputs to build
    // from, the single .pmtiles artifact is where the result goes. The two
    // refusals are the spec's: no artifact means nowhere to write, no
    // shapefiles means nothing to build from.
    let mut shapefiles: Vec<&OceanSpec> = Vec::new();
    let mut artifact: Option<&OceanSpec> = None;
    for spec in &specs {
        match spec {
            OceanSpec::Artifact(_) => {
                if artifact.is_some() {
                    return Err(DevError::Config(
                        "[<host>.tilegen.default].ocean names more than one .pmtiles \
                         artifact; ocean-build writes exactly one output"
                            .to_owned(),
                    ));
                }
                artifact = Some(spec);
            }
            _ => shapefiles.push(spec),
        }
    }

    let artifact = artifact.ok_or_else(|| {
        DevError::Config(
            "[<host>.tilegen.default].ocean has no .pmtiles artifact entry, so \
             ocean-build has nowhere to write its output; add the artifact path \
             to the block"
                .to_owned(),
        )
    })?;
    if shapefiles.is_empty() {
        return Err(DevError::Config(
            "[<host>.tilegen.default].ocean has no shapefile entries, so \
             ocean-build has nothing to build from; add the zoom-banded \
             shapefile specs to the block"
                .to_owned(),
        ));
    }

    // Resolve shapefile inputs against the data dir and render each as its
    // banded --ocean spec (identical to how tilegen passes them). Existence is
    // checked here so a missing input fails with a clear brokkr message rather
    // than deep inside elivagar.
    let mut args: Vec<String> = vec!["ocean-build".to_owned()];
    for spec in &shapefiles {
        let path = paths.data_dir.join(spec.file());
        if !path.exists() {
            return Err(DevError::Config(format!(
                "ocean shapefile not found: {} (named by brokkr.toml as '{}')",
                path.display(),
                spec.file()
            )));
        }
        args.push("--ocean".to_owned());
        args.push(spec.render(&path.display().to_string()));
    }

    let output_path = paths.data_dir.join(artifact.file());
    args.push("-o".to_owned());
    args.push(output_path.display().to_string());

    if dry_run {
        output::run_msg(&format!("[dry-run] elivagar {}", args.join(" ")));
        output::run_msg(&format!("[dry-run] output: {}", output_path.display()));
        output::run_msg("[dry-run] ok");
        return Ok(());
    }

    let binary = build::cargo_build(&build::BuildConfig::release(None), build_root)?;
    let binary_str = binary.display().to_string();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));
    let out = output::run_passthrough_timed(&binary_str, &arg_refs, lock)?;
    if out.code != 0 {
        return Err(DevError::ExitCode(out.code));
    }
    Ok(())
}
