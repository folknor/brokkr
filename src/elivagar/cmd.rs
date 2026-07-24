use std::path::{Path, PathBuf};

use crate::cli::{CorpusArchiveArgs, PmtilesCorpusCommand};
use crate::config;
use crate::context::{HarnessContext, bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::lockfile::LockGuard;
use crate::measure::MeasureRequest;
use crate::project::{self, Project};
use crate::resolve::{
    resolve_default_pmtiles_path, resolve_pbf_with_size, resolve_pmtiles_by_commit,
    resolve_pmtiles_path,
};

pub(crate) fn bench_planetiler(req: &MeasureRequest) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench planetiler",
        req.force,        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_planetiler::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        req.runs(),
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        req.project_root,
    )
}

pub(crate) fn bench_tilemaker(req: &MeasureRequest) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench tilemaker",
        req.force,        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_tilemaker::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        req.runs(),
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        req.project_root,
    )
}

pub(crate) fn bench_all(req: &MeasureRequest) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench all",
        req.force,        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    // `bench all`'s self arm is a tilegen run and takes the same contract one
    // gets. It used to hardcode a bare PipelineOpts and lean on ocean
    // auto-detection, so what it measured depended on what was in data/.
    let tilegen = super::resolve_tilegen(req.dev_config, super::DEFAULT_TILEGEN)?;
    let (locations_on_ways, force_sorted) =
        super::input_assertions(req.dev_config, req.dataset, req.variant);
    let opts = super::PipelineOpts {
        tilegen,
        locations_on_ways,
        force_sorted,
    };
    super::bench_all::run(
        &ctx.harness,
        &ctx.paths,
        req.effective_build_root(),
        &pbf_path,
        file_mb,
        req.runs(),
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        &opts,
    )
}

pub(crate) fn compare_tiles(
    project: Project,
    build_root: &Path,
    file_a: &str,
    file_b: &str,
    sample: Option<usize>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "compare-tiles")?;
    let pi = bootstrap(None)?;
    super::compare_tiles::run(&pi.target_dir, build_root, file_a, file_b, sample)
}

pub(crate) fn download_ocean(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-ocean")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    super::download_ocean::run(&paths.data_dir)
}

pub(crate) fn download_natural_earth(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-natural-earth")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    super::download_natural_earth::run(&paths.data_dir)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn verify(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    tiles_variant: Option<&str>,
    features: &[String],
    geometry_stats: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "verify")?;
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = match tiles_variant {
        Some(v) => resolve_pmtiles_path(dataset, v, &paths, project_root)?,
        None => resolve_default_pmtiles_path(dataset, &paths, project_root)?,
    };
    let effective = build_root.unwrap_or(project_root);
    super::verify::run(&pmtiles_path, effective, features, geometry_stats)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn inspect(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    dataset: &str,
    variant: &str,
    commit: Option<&str>,
    file: Option<&str>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "pmtiles-inspect")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = resolve_pmtiles_by_commit(dataset, variant, commit, file, &paths, build_root)?;
    super::inspect::run(&pmtiles_path, build_root)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn diag(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    dataset: &str,
    variant: &str,
    commit: Option<&str>,
    file: Option<&str>,
    z: u8,
    x: u32,
    y: u32,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "diag")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = resolve_pmtiles_by_commit(dataset, variant, commit, file, &paths, build_root)?;
    super::diag::run(&pmtiles_path, build_root, z, x, y)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn svg(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    dataset: &str,
    variant: &str,
    commit: Option<&str>,
    file: Option<&str>,
    z: u8,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    layers: Option<&str>,
    output_path: Option<&Path>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "svg")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = resolve_pmtiles_by_commit(dataset, variant, commit, file, &paths, build_root)?;
    super::svg::run(
        &pmtiles_path,
        build_root,
        z,
        x,
        y,
        width,
        height,
        layers,
        output_path,
    )
}

/// `brokkr regress` - resolve two explicit tilegen archives (CURRENT via
/// --commit/--file, COMPARAND via --against-commit/--against) and exec
/// `elivagar regress <current> --against <comparand>` with the
/// tolerance/overlay/reporting flags passed through verbatim.
///
/// Both sides are explicit: there is no default baseline and no comparability
/// gate. regress is the tier-3 attribution instrument, whose legitimate uses
/// include deliberate cross-contract diffs; comparability is the caller's
/// responsibility (`brokkr pmtiles-inspect` reads the provenance blocks). A
/// missing comparand is refused by clap's required ArgGroup, not here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn regress(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    dataset: &str,
    variant: &str,
    commit: Option<&str>,
    file: Option<&str>,
    against_variant: &str,
    against_commit: Option<&str>,
    against: Option<&str>,
    tol: i32,
    max_moved: u64,
    max_examples: usize,
    overlay: Option<&Path>,
    overlay_max: Option<usize>,
    json: bool,
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "regress")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let current = resolve_pmtiles_by_commit(dataset, variant, commit, file, &paths, build_root)?;
    // The comparand is one of the two --against* flags (clap's required
    // ArgGroup guarantees exactly one is present). An explicit path is checked
    // for existence; a commit resolves through the same durable-output resolver
    // as the current side, but with its OWN variant: a cross-variant diff is a
    // legitimate regress use (it is the attribution instrument), so the
    // comparand's variant is addressed independently via --against-variant.
    let comparand = match against {
        Some(p) => {
            let path = std::path::PathBuf::from(p);
            if !path.exists() {
                return Err(DevError::Config(format!(
                    "comparand archive not found: {}",
                    path.display()
                )));
            }
            path
        }
        None => resolve_pmtiles_by_commit(
            dataset,
            against_variant,
            against_commit,
            None,
            &paths,
            build_root,
        )?,
    };
    super::regress::run(
        &current,
        &comparand,
        build_root,
        tol,
        max_moved,
        max_examples,
        overlay,
        overlay_max,
        json,
        lock,
    )
}

/// `brokkr pmtiles-corpus <sub>` - resolve the archive (and, where the
/// subcommand uses one, the corpus dir) from the shared selector, assemble the
/// trailing flags verbatim, and exec `elivagar corpus <sub>`. Every value set
/// (`--mode`, `--op`) is elivagar's to own, so brokkr carries strings; exit
/// codes (0/1/2) pass through untouched.
#[allow(clippy::too_many_lines)]
pub(crate) fn corpus(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    cmd: &PmtilesCorpusCommand,
    lock: Option<&LockGuard>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "pmtiles-corpus")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    // Resolve an archive through the same commit/file resolver as
    // pmtiles-inspect/diag/svg.
    let resolve = |a: &CorpusArchiveArgs| -> Result<PathBuf, DevError> {
        resolve_pmtiles_by_commit(
            &a.dataset,
            &a.variant,
            a.commit.as_deref(),
            a.file.as_deref(),
            &paths,
            build_root,
        )
    };
    // Corpus dir default: corpus/<dataset> under the repo root (build_root),
    // where the git-committed corpus lives - NOT project_root (config/data dir).
    // Overridable with --corpus.
    let corpus_dir = |a: &CorpusArchiveArgs, over: &Option<PathBuf>| -> String {
        over.clone()
            .unwrap_or_else(|| build_root.join("corpus").join(&a.dataset))
            .display()
            .to_string()
    };

    match cmd {
        PmtilesCorpusCommand::Check { archive, corpus } => {
            let path = resolve(archive)?;
            let trailing = vec!["--corpus".to_owned(), corpus_dir(archive, corpus)];
            super::corpus::run(build_root, "check", &path, &trailing, lock)
        }
        PmtilesCorpusCommand::Bless {
            archive,
            corpus,
            rotate,
            mode,
        } => {
            let path = resolve(archive)?;
            let mut trailing = vec!["--corpus".to_owned(), corpus_dir(archive, corpus)];
            if *rotate {
                trailing.push("--rotate".to_owned());
            }
            if let Some(m) = mode {
                trailing.push("--mode".to_owned());
                trailing.push(m.clone());
            }
            super::corpus::run(build_root, "bless", &path, &trailing, lock)
        }
        PmtilesCorpusCommand::RenderManifest {
            archive,
            corpus,
            style,
        } => {
            let path = resolve(archive)?;
            let mut trailing = vec!["--corpus".to_owned(), corpus_dir(archive, corpus)];
            if let Some(s) = style {
                trailing.push("--style".to_owned());
                trailing.push(s.display().to_string());
            }
            super::corpus::run(build_root, "render-manifest", &path, &trailing, lock)
        }
        PmtilesCorpusCommand::Render {
            archive,
            z,
            x,
            y,
            layers,
            style,
            output,
        } => {
            let path = resolve(archive)?;
            let mut trailing = vec![
                "-z".to_owned(),
                z.to_string(),
                "-x".to_owned(),
                x.to_string(),
                "-y".to_owned(),
                y.to_string(),
            ];
            if let Some(l) = layers {
                trailing.push("--layers".to_owned());
                trailing.push(l.clone());
            }
            if let Some(s) = style {
                trailing.push("--style".to_owned());
                trailing.push(s.display().to_string());
            }
            if let Some(o) = output {
                trailing.push("-o".to_owned());
                trailing.push(o.display().to_string());
            }
            super::corpus::run(build_root, "render", &path, &trailing, lock)
        }
        PmtilesCorpusCommand::Rings { archive, output } => {
            let path = resolve(archive)?;
            let trailing = vec!["-o".to_owned(), output.display().to_string()];
            super::corpus::run(build_root, "rings", &path, &trailing, lock)
        }
        PmtilesCorpusCommand::Mutate {
            archive,
            output,
            op,
            tile,
        } => {
            let path = resolve(archive)?;
            // Default `-o` to a calibrand under data/corpus-calibrands/ (cleared
            // by a routine `brokkr clean`); an explicit `-o` is the user's file.
            let out_path = match output {
                Some(o) => o.clone(),
                None => {
                    let dir = paths.data_dir.join(crate::CORPUS_CALIBRAND_DIR);
                    std::fs::create_dir_all(&dir).ok();
                    dir.join(format!("{}-{}-{op}.pmtiles", archive.dataset, archive.variant))
                }
            };
            let mut trailing = vec![
                "-o".to_owned(),
                out_path.display().to_string(),
                "--op".to_owned(),
                op.clone(),
            ];
            if let Some(t) = tile {
                trailing.push("--tile".to_owned());
                trailing.push(t.clone());
            }
            super::corpus::run(build_root, "mutate", &path, &trailing, lock)
        }
    }
}
