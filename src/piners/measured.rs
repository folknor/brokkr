//! Measured `brokkr corpus --hotpath` / `--alloc`: function-level timing and
//! per-function allocation tracking of the parity harness, recorded to
//! `.brokkr/results.db` and queried with `brokkr results` (like every other
//! measurable command).
//!
//! Distinct from the bare parity run ([`crate::piners::cmd::corpus`]): no gate,
//! no `runs.db` ingest, no pre-run runtime ceiling. Selection + hard
//! verification + manifest construction are shared with the parity path; the
//! build goes through [`crate::context::BenchContext::with_build_config`] with
//! the hotpath feature appended, so the run rides the same sidecar + results.db
//! lifecycle as pbfhogg.
//!
//! Only `--hotpath`/`--alloc` are supported. `--bench` (best-of-N wall-clock)
//! would need the harness to emit brokkr's `key=value` stderr timing contract;
//! until it does, that mode is refused with a clear error rather than recording
//! a meaningless number.

use crate::build::{BuildConfig, CargoProfile};
use crate::context::BenchContext;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::output;
use crate::piners::cmd::CorpusArgs;
use crate::piners::manifest::Manifest;
use crate::piners::registry::{self, Registry};
use crate::piners::select::{self, SelectArgs};
use crate::project::{self, Project};

/// Run a measured corpus selection (`--hotpath`/`--alloc`). Routed here from
/// the `Corpus` dispatch when a measurement flag is set; bare/parity runs go to
/// [`crate::piners::cmd::corpus`] instead.
#[allow(clippy::too_many_lines)] // linear orchestration: select, verify, build, measure
pub(crate) fn run(req: &MeasureRequest, args: &CorpusArgs) -> Result<(), DevError> {
    project::require(req.project, Project::Piners, "corpus")?;

    let alloc = match req.mode {
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            matches!(req.mode, MeasureMode::Alloc { .. })
        }
        MeasureMode::Bench { .. } => {
            return Err(DevError::Config(
                "corpus --bench is not supported yet: the parity harness emits NDJSON \
                 dispositions, not brokkr's key=value timing contract. Use --hotpath or \
                 --alloc (function-level timing / allocation tracking)."
                    .to_owned(),
            ));
        }
        MeasureMode::Run => unreachable!("Run mode routes to the parity path in dispatch"),
    };

    let cfg = req.dev_config.piners.clone().unwrap_or_default();
    let harness_cfg = cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(
            "corpus: no [piners.harness] section in brokkr.toml. \
             Declare `[piners.harness]` with `package = \"<crate>\"` \
             (and optional `binary`, `features`, `debug`)."
                .to_owned(),
        )
    })?;

    // Selection + hard verification, shared with the parity path. No gate,
    // bless, reseed, or runtime ceiling apply to a measured run (those are
    // dispatch-rejected as conflicting flags).
    let registry_dir = req.project_root.join(cfg.registry_dir());
    let reg = Registry::load(&registry_dir)?;
    reg.lint()?;
    let sel = SelectArgs {
        keywords: args.keywords.clone(),
        probe: args.probe.clone(),
        all: args.all,
        verify_only: false,
    };
    let ids = select::resolve(&reg, &sel)?;

    let corpus_root = req.project_root.join(cfg.corpus_root());
    output::corpus_msg(&format!(
        "verifying {} probe(s) against {}",
        ids.len(),
        corpus_root.display()
    ));
    let mut verified = Vec::with_capacity(ids.len());
    for id in &ids {
        let pin = reg.pins.get(id).ok_or_else(|| {
            DevError::Config(format!("piners: internal: selected id '{id}' absent from pins"))
        })?;
        verified.push(registry::verify_probe(id, pin, &corpus_root, req.project_root)?);
    }
    crate::piners::cmd::verify_selected_feeds(&ids, &reg, &corpus_root, req.project_root)?;

    // Measured runs default to release (meaningful timing); `--debug` profiles
    // the dev build instead. Parity runs default debug - see `cmd.rs`.
    let debug = args.profile_override.unwrap_or(false);

    let mut build_cfg = BuildConfig::for_harness(harness_cfg, debug);
    build_cfg
        .features
        .push(harness::hotpath_feature(alloc).to_owned());

    let lock_command = if alloc { "corpus --alloc" } else { "corpus --hotpath" };
    let ctx = BenchContext::with_build_config(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        &build_cfg,
        lock_command,
        req.force,        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    // Manifest into the bench scratch dir. The harness writes its (ignored)
    // NDJSON there via BROKKR_HARNESS_ARTEFACT_DIR, and run_hotpath_capture
    // drops the hotpath JSON report beside it.
    let manifest_path = ctx.paths.scratch_dir.join("manifest.json");
    Manifest::build(&corpus_root, &verified, &reg).write(&manifest_path)?;

    let bin_dir = ctx.binary.parent().ok_or_else(|| {
        DevError::Build(format!(
            "binary path {} has no parent directory",
            ctx.binary.display()
        ))
    })?;

    let binary_str = ctx.binary.display().to_string();
    let manifest_str = manifest_path.display().to_string();
    let scratch_str = ctx.paths.scratch_dir.display().to_string();
    let bin_dir_str = bin_dir.display().to_string();

    let label = harness::hotpath_feature(alloc);
    output::hotpath_msg(&format!(
        "=== corpus {label} ({} probe(s)) ===",
        verified.len()
    ));
    if alloc {
        output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
    }

    let selector = selector_label(args);
    let mut metadata = vec![
        KvPair::int(
            "probe_count",
            i64::try_from(verified.len()).unwrap_or(i64::MAX),
        ),
        KvPair::text("selector", selector.clone()),
    ];
    if debug {
        // The profile column only models release; record the dev override here.
        metadata.push(KvPair::text("profile", "dev"));
    }

    // Forwarded harness flags (everything after `--`) ride along here too -
    // profiling with a scan toggle enabled is a legitimate measured run. They
    // land in the result row via `cli_args` below.
    let mut subprocess_args: Vec<&str> = vec!["--manifest", manifest_str.as_str()];
    subprocess_args.extend(args.harness_args.iter().map(String::as_str));
    let config = BenchConfig {
        command: "corpus".to_owned(),
        mode: None,
        input_file: Some(selector),
        input_mb: None,
        cargo_features: None,
        cargo_profile: CargoProfile::Release,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(&binary_str, &subprocess_args)),
        brokkr_args: None,
        metadata,
    };

    let env = [
        ("BROKKR_HARNESS_ARTEFACT_DIR", scratch_str.as_str()),
        ("BROKKR_TEST_BIN_DIR", bin_dir_str.as_str()),
    ];
    let scratch_dir = ctx.paths.scratch_dir.clone();
    let project_root = req.project_root.to_path_buf();
    ctx.harness.run_hotpath(&config, &ctx.binary, |_i| {
        let (result, _stderr, sidecar) = harness::run_hotpath_capture(
            &binary_str,
            &subprocess_args,
            &scratch_dir,
            &project_root,
            &env,
            &[0],
            req.stop_marker,
            Some(ctx.harness.lock()),
        )?;
        Ok((result, sidecar))
    })?;

    Ok(())
}

/// A compact label for the measured selection, stored in the result row's
/// `input_file` column and a `selector` metadata key. Mirrors the intent
/// rendering used by the corpus run-store views (`all` / `kw=…` / `probe=…`).
fn selector_label(args: &CorpusArgs) -> String {
    if args.all {
        "all".to_owned()
    } else if !args.probe.is_empty() {
        format!("probe={}", args.probe.join(","))
    } else if !args.keywords.is_empty() {
        format!("kw={}", args.keywords.join(","))
    } else {
        "selection".to_owned()
    }
}
