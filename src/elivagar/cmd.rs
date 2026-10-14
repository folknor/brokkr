use std::path::{Path, PathBuf};

use super::archive_store::ArchiveStore;
use crate::clean::layout::ElivagarLayout;
use crate::cli::{CorpusArchiveArgs, PmtilesCorpusCommand};
use crate::config;
use crate::context::{MeasurementSession, bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::lockfile::LockGuard;
use crate::measure::MeasureRequest;
use crate::project::{self, Project};

pub(crate) fn bench_all(req: &MeasureRequest) -> Result<(), DevError> {
    let ctx = MeasurementSession::new_for_request(req)?;
    // The input assertions come from the same resolved entry that supplied
    // the path, not from a second lookup through the raw host config.
    let pbf = crate::resolve::resolve_pbf_entry(
        req.dataset,
        req.variant,
        &ctx.paths,
        req.project_root.as_path(),
    )?;
    let file_mb = crate::resolve::file_size_mb(&pbf.path)?;
    // `bench all`'s self arm is a tilegen run and takes the same contract one
    // gets. It used to hardcode a bare PipelineOpts and lean on ocean
    // auto-detection, so what it measured depended on what was in data/.
    let tilegen = super::resolve_tilegen(req.dev_config, super::DEFAULT_TILEGEN)?;
    let opts = super::PipelineOpts::from_pbf_entry(pbf.entry);
    let pbf_path = pbf.path.clone();
    super::bench_all::run(
        &ctx.harness,
        &ctx.paths,
        req.features,
        req.build_root.as_path(),
        &pbf_path,
        file_mb,
        req.runs(),
        tilegen,
        &opts,
    )
}

/// `brokkr compare-tiles` - the lenient per-layer census. Native since the
/// corpus redesign, so it needs no build and no lock.
pub(crate) fn compare_tiles(
    project: Project,
    file_a: &str,
    file_b: &str,
    sample: Option<usize>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "compare-tiles")?;
    super::compare_tiles::run(file_a, file_b, sample)
}

pub(crate) fn download_ocean(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-ocean")?;
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    super::download_ocean::run(&paths.data_dir)
}

pub(crate) fn download_natural_earth(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-natural-earth")?;
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    super::download_natural_earth::run(&paths.data_dir)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn verify(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
    dataset: &str,
    variant: &str,
    commit: Option<&str>,
    file: Option<&str>,
    features: &[String],
    geometry_stats: bool,
    unique_payloads: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "verify")?;
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let build_root = build_root.as_path();
    let pmtiles_path = ArchiveStore::new(paths.output_dir.as_path())
        .resolve(dataset, variant, commit, file, build_root)?
        .path;
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
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
    dataset: &str,
    variant: &str,
    commit: Option<&str>,
    file: Option<&str>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "pmtiles-inspect")?;
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let build_root = build_root.as_path();
    let pmtiles_path = ArchiveStore::new(paths.output_dir.as_path())
        .resolve(dataset, variant, commit, file, build_root)?
        .path;
    super::inspect::run(&pmtiles_path, build_root, &paths.features)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn diag(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
    dataset: &str,
    variant: &str,
    commit: Option<&str>,
    file: Option<&str>,
    z: u8,
    x: u32,
    y: u32,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "diag")?;
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let build_root = build_root.as_path();
    let pmtiles_path = ArchiveStore::new(paths.output_dir.as_path())
        .resolve(dataset, variant, commit, file, build_root)?
        .path;
    super::diag::run(&pmtiles_path, build_root, &paths.features, z, x, y)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn svg(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
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
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let build_root = build_root.as_path();
    let pmtiles_path = ArchiveStore::new(paths.output_dir.as_path())
        .resolve(dataset, variant, commit, file, build_root)?
        .path;
    super::svg::run(
        &pmtiles_path,
        build_root,
        &paths.features,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn regress(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
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
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let build_root = build_root.as_path();
    let current = ArchiveStore::new(paths.output_dir.as_path())
        .resolve(dataset, variant, commit, file, build_root)?
        .path;
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
        None => {
            ArchiveStore::new(paths.output_dir.as_path())
                .resolve(dataset, against_variant, against_commit, None, build_root)?
                .path
        }
    };
    super::regress::run(
        &current,
        &comparand,
        tol,
        max_moved,
        max_examples,
        overlay,
        overlay_max,
        json,
    )
}

/// Run a verdict-bearing command (`regress`, `pmtiles-corpus`) so that a failure
/// to reach any verdict exits `code` rather than the generic 1.
///
/// Both commands give exit 1 a meaning - "the archive regressed" / "content
/// mismatch" - and `main` maps every unmapped `Err` to 1 as well, so a lock
/// conflict, a `cargo metadata` failure, an unresolvable archive or a real IO
/// error used to read, to an automated caller, as a regression. The error is
/// printed here, since `main` prints nothing for `ExitCode`. Verdicts
/// (`ExitCode`) and cooperative shutdown (`Interrupted`) pass through untouched.
pub(crate) fn operational<F>(code: i32, f: F) -> Result<(), DevError>
where
    F: FnOnce() -> Result<(), DevError>,
{
    match f() {
        Err(e @ (DevError::ExitCode(_) | DevError::Interrupted)) => Err(e),
        Err(e) => {
            crate::output::error(&e.to_string());
            Err(DevError::ExitCode(code))
        }
        Ok(()) => Ok(()),
    }
}

/// `brokkr pmtiles-corpus <sub>` - the corpus gate, now native brokkr code over
/// the linked elivagar crate (no shelling). Resolves the archive and corpus dir
/// from the shared selector, runs the gate in-process, prints the report, and
/// maps the verdict to the process exit code (0 pass / 1 content mismatch / 2
/// archive refusal / 3 baseline trouble) via `DevError::ExitCode`. The dispatch
/// wraps this in [`operational`] with `corpus::INCOMPLETE_EXIT` (4), so any
/// error that is not a verdict exits 4.
///
/// `lock` is currently unused: the gate is read-only on the archive (check /
/// render) or writes only into the committed corpus dir (bless / render-manifest
/// / mutate output), neither of which touches tilegen scratch or the global lock.
#[allow(clippy::too_many_lines)]
pub(crate) fn corpus(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
    cmd: &PmtilesCorpusCommand,
    _lock: Option<&LockGuard>,
) -> Result<(), DevError> {
    use super::corpus as gate;

    project::require(project, Project::Elivagar, "pmtiles-corpus")?;
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let build_root = build_root.as_path();

    // Resolve an archive through the same commit/file resolver as
    // pmtiles-inspect/diag/svg. `variant` is `None` only alongside `--file`
    // (clap's `required_unless_present`), where the resolver ignores it.
    let resolve = |a: &CorpusArchiveArgs| -> Result<PathBuf, DevError> {
        Ok(ArchiveStore::new(paths.output_dir.as_path())
            .resolve(
                &a.dataset,
                a.variant.as_deref().unwrap_or_default(),
                a.commit.as_deref(),
                a.file.as_deref(),
                build_root,
            )?
            .path)
    };
    // Corpus dir default: corpus/<dataset> under the repo root (build_root),
    // where the git-committed corpus lives - NOT project_root (config/data dir).
    // Overridable with --corpus.
    let corpus_dir = |a: &CorpusArchiveArgs, over: &Option<PathBuf>| -> PathBuf {
        over.clone()
            .unwrap_or_else(|| build_root.join("corpus").join(&a.dataset))
    };
    // `--style` default: derived from the corpus dir by the gate's own rule
    // (`corpus_style_path`), never from the build root independently - the two
    // disagree as soon as `--corpus` points elsewhere.
    let style_default = gate::corpus_style_path;
    // The digest's external sort spills under project state, never /tmp.
    let sort_scratch = project_root.state().corpus_sort_dir();

    match cmd {
        PmtilesCorpusCommand::Check { archive, corpus } => {
            let path = resolve(archive)?;
            let (outcome, report) =
                gate::check(&path, &corpus_dir(archive, corpus), &sort_scratch)
                    .map_err(DevError::Io)?;
            emit_corpus(outcome, &report)
        }
        PmtilesCorpusCommand::Bless {
            archive,
            corpus,
            rotate,
            mode,
        } => {
            let path = resolve(archive)?;
            let (outcome, report) =
                gate::bless(
                    &path,
                    &corpus_dir(archive, corpus),
                    *mode,
                    *rotate,
                    &sort_scratch,
                )
                .map_err(DevError::Io)?;
            emit_corpus(outcome, &report)
        }
        PmtilesCorpusCommand::RenderManifest {
            archive,
            corpus,
            style,
        } => {
            let path = resolve(archive)?;
            let dir = corpus_dir(archive, corpus);
            let style_path = style.clone().unwrap_or_else(|| style_default(&dir));
            let (outcome, report) =
                gate::render_manifest(&path, &dir, &style_path, &sort_scratch)
                    .map_err(DevError::Io)?;
            emit_corpus(outcome, &report)
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
            // `render` takes no `--corpus`; the default corpus dir stands in.
            let style_path = style
                .clone()
                .unwrap_or_else(|| style_default(&corpus_dir(archive, &None)));
            let layer_list: Option<Vec<String>> = layers
                .as_ref()
                .map(|l| l.split(',').map(str::to_owned).collect());
            gate::render_tile(
                &path,
                *z,
                *x,
                *y,
                &style_path,
                layer_list.as_deref(),
                output.as_deref(),
            )
            .map_err(DevError::Io)
        }
        PmtilesCorpusCommand::Rings { archive, output } => {
            let path = resolve(archive)?;
            gate::rings(&path, output).map_err(DevError::Io)
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
                    let dir = ElivagarLayout::new(&paths).corpus_calibrands();
                    std::fs::create_dir_all(&dir).ok();
                    // With `--file` and no `--variant`, name the calibrand
                    // after the input file instead.
                    let stem = match &archive.variant {
                        Some(v) => format!("{}-{v}", archive.dataset),
                        None => path.file_stem().map_or_else(
                            || "file".to_owned(),
                            |s| s.to_string_lossy().into_owned(),
                        ),
                    };
                    dir.join(format!("{stem}-{}.pmtiles", op.as_str()))
                }
            };
            let target = tile.as_deref().map(parse_tile).transpose()?;
            gate::mutate::mutate(&path, &out_path, target, *op).map_err(DevError::Io)?;
            crate::output::result_msg(&format!("mutated -> {}", out_path.display()));
            Ok(())
        }
    }
}

/// Print a corpus report and translate the verdict into the process exit code.
fn emit_corpus(
    outcome: super::corpus::Outcome,
    report: &super::corpus::CheckReport,
) -> Result<(), DevError> {
    use super::corpus::Outcome;
    for w in &report.warnings {
        crate::output::warn(w);
    }
    for d in &report.contract_diffs {
        crate::output::run_msg(&format!("contract {d}"));
    }
    if !report.message.is_empty() {
        for line in report.message.lines() {
            crate::output::run_msg(line);
        }
    }
    if report.changed > 0 {
        crate::output::run_msg(&format!("{} changed run(s)", report.changed));
    }
    match outcome {
        Outcome::Pass => {
            crate::output::result_msg("corpus: pass");
            Ok(())
        }
        other => Err(DevError::ExitCode(other.exit_code())),
    }
}

/// Parse a `z/x/y` mutate/render target.
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

#[cfg(test)]
mod operational_tests {
    use super::operational;
    use crate::error::DevError;

    #[test]
    fn a_non_verdict_error_exits_the_given_code() {
        let r = operational(4, || Err(DevError::Config("lock held".into())));
        assert!(matches!(r, Err(DevError::ExitCode(4))));
    }

    #[test]
    fn verdicts_and_interrupts_pass_through() {
        assert!(matches!(
            operational(4, || Err(DevError::ExitCode(1))),
            Err(DevError::ExitCode(1))
        ));
        assert!(matches!(
            operational(4, || Err(DevError::Interrupted)),
            Err(DevError::Interrupted)
        ));
        assert!(operational(4, || Ok(())).is_ok());
    }
}
