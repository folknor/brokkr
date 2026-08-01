use std::path::Path;

// `PathBuf` and `CorpusArchiveArgs` are used only by the corpus/regress bodies,
// commented out while the `elivagar` path dep is disabled (see Cargo.toml).
use crate::cli::PmtilesCorpusCommand;
use crate::config;
use crate::context::{HarnessContext, bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::lockfile::LockGuard;
use crate::measure::MeasureRequest;
use crate::project::{self, Project};
use crate::resolve::{resolve_pbf_with_size, resolve_pmtiles_by_commit};

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

/// `brokkr compare-tiles` - the lenient per-layer census. Native since the
/// corpus redesign, so it needs no build and no lock.
///
/// TEMPORARILY DISABLED with the `elivagar` path dep (see Cargo.toml).
pub(crate) fn compare_tiles(
    project: Project,
    _file_a: &str,
    _file_b: &str,
    _sample: Option<usize>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "compare-tiles")?;
    Err(disabled("compare-tiles"))
}

/// The error every elivagar-linking command returns while the `elivagar` path
/// dependency is disabled in Cargo.toml.
fn disabled(what: &str) -> DevError {
    DevError::Config(format!(
        "{what} is disabled: it links the elivagar crate in-process, and the \
         elivagar path dependency is currently commented out in brokkr's Cargo.toml"
    ))
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
    build_root: &Path,
    dataset: &str,
    variant: &str,
    commit: Option<&str>,
    file: Option<&str>,
    features: &[String],
    geometry_stats: bool,
    unique_payloads: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "verify")?;
    let pi = bootstrap(Some(build_root))?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = resolve_pmtiles_by_commit(dataset, variant, commit, file, &paths, build_root)?;
    super::verify::run(
        &pmtiles_path,
        build_root,
        features,
        geometry_stats,
        unique_payloads,
    )
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
/// --commit/--file, COMPARAND via --against-commit/--against) and run the
/// native two-archive semantic diff over them.
///
/// Both sides are explicit: there is no default baseline and no comparability
/// gate. regress is the tier-3 attribution instrument, whose legitimate uses
/// include deliberate cross-contract diffs; comparability is the caller's
/// responsibility (`brokkr pmtiles-inspect` reads the provenance blocks). A
/// missing comparand is refused by clap's required ArgGroup, not here.
///
/// TEMPORARILY DISABLED with the `elivagar` path dep (see Cargo.toml).
#[allow(clippy::too_many_arguments, unused_variables)]
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
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "regress")?;
    Err(disabled("regress"))

    // Body retained, commented out, until the elivagar path dep is restored:
    // return Err(disabled("regress"));
    // #[allow(unreachable_code)]
    // let pi = bootstrap(None)?;
    // let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    // let current = resolve_pmtiles_by_commit(dataset, variant, commit, file, &paths, build_root)?;
    // // The comparand is one of the two --against* flags (clap's required
    // // ArgGroup guarantees exactly one is present). An explicit path is checked
    // // for existence; a commit resolves through the same durable-output resolver
    // // as the current side, but with its OWN variant: a cross-variant diff is a
    // // legitimate regress use (it is the attribution instrument), so the
    // // comparand's variant is addressed independently via --against-variant.
    // let comparand = match against {
    // Some(p) => {
    // let path = std::path::PathBuf::from(p);
    // if !path.exists() {
    // return Err(DevError::Config(format!(
    // "comparand archive not found: {}",
    // path.display()
    // )));
    // }
    // path
    // }
    // None => resolve_pmtiles_by_commit(
    // dataset,
    // against_variant,
    // against_commit,
    // None,
    // &paths,
    // build_root,
    // )?,
    // };
    // super::regress::run(
    // &current,
    // &comparand,
    // tol,
    // max_moved,
    // max_examples,
    // overlay,
    // overlay_max,
    // json,
    // )
}

/// `brokkr pmtiles-corpus <sub>` - the corpus gate, now native brokkr code over
/// the linked elivagar crate (no shelling). Resolves the archive and corpus dir
/// from the shared selector, runs the gate in-process, prints the report, and
/// maps the verdict to the process exit code (0 pass / 1 content mismatch / 2
/// archive refusal / 3 baseline trouble) via `DevError::ExitCode`.
///
/// `lock` is currently unused: the gate is read-only on the archive (check /
/// render) or writes only into the committed corpus dir (bless / render-manifest
/// / mutate output), neither of which touches tilegen scratch or the global lock.
///
/// TEMPORARILY DISABLED with the `elivagar` path dep (see Cargo.toml).
#[allow(clippy::too_many_lines, unused_variables)]
pub(crate) fn corpus(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    cmd: &PmtilesCorpusCommand,
    _lock: Option<&LockGuard>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "pmtiles-corpus")?;
    Err(disabled("pmtiles-corpus"))

    // Body + helpers retained, commented out, until the elivagar path dep is restored:
    // use super::corpus::{self as gate, MutationOp};
    //
    // project::require(project, Project::Elivagar, "pmtiles-corpus")?;
    // let pi = bootstrap(None)?;
    // let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    //
    // // Resolve an archive through the same commit/file resolver as
    // // pmtiles-inspect/diag/svg.
    // let resolve = |a: &CorpusArchiveArgs| -> Result<PathBuf, DevError> {
    // resolve_pmtiles_by_commit(
    // &a.dataset,
    // &a.variant,
    // a.commit.as_deref(),
    // a.file.as_deref(),
    // &paths,
    // build_root,
    // )
    // };
    // // Corpus dir default: corpus/<dataset> under the repo root (build_root),
    // // where the git-committed corpus lives - NOT project_root (config/data dir).
    // // Overridable with --corpus.
    // let corpus_dir = |a: &CorpusArchiveArgs, over: &Option<PathBuf>| -> PathBuf {
    // over.clone()
    // .unwrap_or_else(|| build_root.join("corpus").join(&a.dataset))
    // };
    //
    // match cmd {
    // PmtilesCorpusCommand::Check { archive, corpus } => {
    // let path = resolve(archive)?;
    // let (outcome, report) = gate::check(&path, &corpus_dir(archive, corpus))
    // .map_err(DevError::Io)?;
    // emit_corpus(outcome, &report)
    // }
    // PmtilesCorpusCommand::Bless {
    // archive,
    // corpus,
    // rotate,
    // mode,
    // } => {
    // let path = resolve(archive)?;
    // let mode = parse_mode(mode.as_deref())?;
    // let (outcome, report) = gate::bless(&path, &corpus_dir(archive, corpus), mode, *rotate)
    // .map_err(DevError::Io)?;
    // emit_corpus(outcome, &report)
    // }
    // PmtilesCorpusCommand::RenderManifest {
    // archive,
    // corpus,
    // style,
    // } => {
    // let path = resolve(archive)?;
    // let dir = corpus_dir(archive, corpus);
    // let style_path = style
    // .clone()
    // .unwrap_or_else(|| build_root.join("corpus").join("style.toml"));
    // let (outcome, report) = gate::render_manifest(&path, &dir, &style_path)
    // .map_err(DevError::Io)?;
    // emit_corpus(outcome, &report)
    // }
    // PmtilesCorpusCommand::Render {
    // archive,
    // z,
    // x,
    // y,
    // layers,
    // style,
    // output,
    // } => {
    // let path = resolve(archive)?;
    // let style_path = style
    // .clone()
    // .unwrap_or_else(|| build_root.join("corpus").join("style.toml"));
    // let layer_list: Option<Vec<String>> = layers
    // .as_ref()
    // .map(|l| l.split(',').map(str::to_owned).collect());
    // gate::render_tile(
    // &path,
    // *z,
    // *x,
    // *y,
    // &style_path,
    // layer_list.as_deref(),
    // output.as_deref(),
    // )
    // .map_err(DevError::Io)
    // }
    // PmtilesCorpusCommand::Rings { archive, output } => {
    // let path = resolve(archive)?;
    // gate::rings(&path, output).map_err(DevError::Io)
    // }
    // PmtilesCorpusCommand::Mutate {
    // archive,
    // output,
    // op,
    // tile,
    // } => {
    // let path = resolve(archive)?;
    // let mop = MutationOp::parse(op)
    // .ok_or_else(|| DevError::Config(format!("unknown mutate op: {op}")))?;
    // // Default `-o` to a calibrand under data/corpus-calibrands/ (cleared
    // // by a routine `brokkr clean`); an explicit `-o` is the user's file.
    // let out_path = match output {
    // Some(o) => o.clone(),
    // None => {
    // let dir = paths.data_dir.join(crate::CORPUS_CALIBRAND_DIR);
    // std::fs::create_dir_all(&dir).ok();
    // dir.join(format!("{}-{}-{op}.pmtiles", archive.dataset, archive.variant))
    // }
    // };
    // let target = tile.as_deref().map(parse_tile).transpose()?;
    // gate::mutate::mutate(&path, &out_path, target, mop).map_err(DevError::Io)?;
    // crate::output::result_msg(&format!("mutated -> {}", out_path.display()));
    // Ok(())
    // }
    // }
    // }
    //
    // /// Print a corpus report and translate the verdict into the process exit code.
    // fn emit_corpus(outcome: super::corpus::Outcome, report: &super::corpus::CheckReport) -> Result<(), DevError> {
    // use super::corpus::Outcome;
    // for w in &report.warnings {
    // crate::output::run_msg(&format!("warning: {w}"));
    // }
    // for d in &report.contract_diffs {
    // crate::output::run_msg(&format!("contract {d}"));
    // }
    // if !report.message.is_empty() {
    // for line in report.message.lines() {
    // crate::output::run_msg(line);
    // }
    // }
    // if report.changed > 0 {
    // crate::output::run_msg(&format!("{} changed run(s)", report.changed));
    // }
    // match outcome {
    // Outcome::Pass => {
    // crate::output::result_msg("corpus: pass");
    // Ok(())
    // }
    // other => Err(DevError::ExitCode(other.exit_code())),
    // }
    // }
    //
    // fn parse_mode(mode: Option<&str>) -> Result<super::corpus::DigestMode, DevError> {
    // use super::corpus::DigestMode;
    // match mode {
    // None | Some("leaves") => Ok(DigestMode::Leaves),
    // Some("buckets") => Ok(DigestMode::Buckets),
    // Some(other) => Err(DevError::Config(format!("unknown digest mode: {other}"))),
    // }
    // }
}

/// Parse a `z/x/y` mutate/render target. Only the corpus body calls this, so it
/// is dead while the `elivagar` path dep is disabled (see Cargo.toml).
#[allow(dead_code)]
fn parse_tile(s: &str) -> Result<(u8, u32, u32), DevError> {
    let mut p = s.split('/');
    let bad = || DevError::Config(format!("invalid tile spec (want z/x/y): {s}"));
    let z: u8 = p.next().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
    let x: u32 = p.next().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
    let y: u32 = p.next().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
    if p.next().is_some() {
        return Err(bad());
    }
    Ok((z, x, y))
}
