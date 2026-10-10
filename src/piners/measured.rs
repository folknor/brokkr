//! Measured `brokkr corpus --hotpath` / `--alloc`: function-level timing and
//! per-function allocation tracking of the parity harness, recorded to
//! `.brokkr/results.db` and queried with `brokkr results` (like every other
//! measurable command).
//!
//! Distinct from the bare parity run ([`crate::piners::cmd::corpus`]): no gate,
//! no `runs.db` ingest, no pre-run runtime ceiling, no isolation pass. One
//! integrity rule does apply: an iteration that exits 0 without reporting
//! exactly its selection (every probe validly, no protocol violation) fails
//! the measurement, unless its stop marker actually fired. Selection + hard
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
use crate::piners::integrity::{self, Reconciliation};
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

    let lock_command = if alloc { "corpus --alloc" } else { "corpus --hotpath" };
    // Locked before the registry is read, as the parity path is (see
    // `cmd.rs`): verification must judge the pins and bytes the run uses.
    // `BenchContext` acquires again below; acquisition is re-entrant within
    // the process, so the hold is one and spans both.
    let project_root_str = req.project_root.display().to_string();
    let _lock = crate::lockfile::acquire(&crate::lockfile::LockContext {
        project: "piners",
        command: lock_command,
        project_root: &project_root_str,
    })?;

    // Selection + hard verification, shared with the parity path. No gate,
    // bless, reseed, or runtime ceiling apply to a measured run (those are
    // dispatch-rejected as conflicting flags).
    let registry_dir = req.project_root.join(cfg.registry_dir());
    let corpus_root = req.project_root.join(cfg.corpus_root());
    let reg = Registry::load(&registry_dir, &corpus_root)?;
    reg.lint()?;
    let sel = SelectArgs {
        keywords: args.keywords.clone(),
        probe: args.probe.clone(),
        all: args.all,
        verify_only: false,
    };
    let ids = select::resolve(&reg, &sel)?;

    output::corpus_msg(&format!(
        "verifying {} against {}",
        output::count(ids.len(), "probe"),
        corpus_root.display()
    ));
    let mut verified = Vec::with_capacity(ids.len());
    for id in &ids {
        let (pin, config) = reg.pins.get(id).zip(reg.config(id)).ok_or_else(|| {
            DevError::Config(format!("piners: internal: selected id '{id}' absent from pins"))
        })?;
        verified.push(registry::verify_probe(id, pin, config, &corpus_root, req.project_root)?);
    }
    crate::piners::cmd::verify_selected_feeds(&ids, &reg, &corpus_root, req.project_root)?;
    registry::verify_harness_files(&reg, &corpus_root, req.project_root)?;

    // Measured runs default to release (meaningful timing); `--debug` profiles
    // the dev build instead. Parity runs default debug - see `cmd.rs`.
    //
    // `[piners.harness] debug` is deliberately NOT consulted here. That key
    // sets the *parity* default (`cmd.rs` reads it); piners' config sets it
    // for the edit/run loop, and inheriting it would silently turn every
    // measured run into a dev-build timing. `docs/commands/corpus.md` states
    // the release default. (Reported once as "ignores the config"; it is the
    // documented contract, not an oversight.)
    let debug = args.profile_override.unwrap_or(false);

    let mut build_cfg = BuildConfig::for_harness(harness_cfg, debug);
    build_cfg
        .features
        .push(harness::hotpath_feature(alloc).to_owned());

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
    output::detail(&format!(
        "corpus {label} ({})",
        output::count(verified.len(), "probe")
    ));
    harness::hotpath_alloc_note(alloc);

    let selector = selector_label(args);
    // The profile lives in the `cargo_profile` column below. Rows recorded
    // before `CargoProfile::Dev` existed carry `release` there and a
    // `meta.profile = dev` patch instead.
    let metadata = vec![
        KvPair::int(
            "probe_count",
            i64::try_from(verified.len()).unwrap_or(i64::MAX),
        ),
        KvPair::text("selector", selector.clone()),
    ];

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
        cargo_profile: CargoProfile::for_debug(debug),
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
    // Each measured iteration gets the parity path's hang backstop: a wedged
    // harness would otherwise hold the global lock until `brokkr kill`.
    // Applied through the sidecar's ambient scope because the child is
    // spawned inside `run_hotpath_capture`, which takes no deadline.
    let _deadline = crate::sidecar::DeadlineScope::enter(crate::piners::cmd::HARNESS_HANG_BACKSTOP);
    ctx.harness.run_hotpath(&config, &ctx.binary, |i| {
        // A non-zero exit fails inside the capture, as a subprocess error
        // carrying the whole stderr.
        let capture = harness::run_hotpath_capture_with_stdout(
            &binary_str,
            &subprocess_args,
            &scratch_dir,
            &project_root,
            &env,
            &[0],
            req.stop_marker,
            Some(ctx.harness.lock()),
        )?;
        // A stop marker that actually fired ends the run early by design;
        // every other run, a configured marker that never fired included, is
        // held to its selection.
        if !capture.stopped_by_marker {
            check_iteration_complete(i, &ids, &capture.stdout, &capture.stderr)?;
        }
        Ok((capture.result, capture.sidecar))
    })?;

    Ok(())
}

/// Fail a measured iteration that exited 0 without reporting exactly its
/// selection - a probe without a valid disposition, or any invalid, repeated
/// or report-only extra record: its timing would describe a different
/// workload. Prints the reconciliation and the whole stderr. No isolation on
/// this path.
fn check_iteration_complete(
    iteration: usize,
    ids: &[String],
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(), DevError> {
    let mut report = crate::piners::report::parse(stdout);
    let duplicates = report.take_duplicates();
    let rec = Reconciliation::reconcile(ids, &report, duplicates);
    if rec.complete() && rec.protocol_violations() == 0 {
        return Ok(());
    }
    output::corpus_msg(&format!(
        "iteration {iteration}: harness exited 0 with {} of {} selected probes unscored \
         and {} ({})",
        rec.unscored(),
        rec.selected,
        output::count(rec.protocol_violations(), "protocol violation"),
        rec.summary()
    ));
    for line in rec.detail_lines() {
        output::corpus_msg(&line);
    }
    integrity::print_stderr("harness stderr", &String::from_utf8_lossy(stderr));
    Err(DevError::Verify(format!(
        "corpus measurement: iteration {iteration} did not report exactly its selection \
         ({}); the timing is not of the selected workload",
        rec.summary()
    )))
}

/// A compact label for the measured selection, stored in the result row's
/// `input_file` column and a `selector` metadata key. Mirrors the intent
/// rendering used by the corpus run-store views (`all` / `kw=...` / `probe=...`).
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const A: &str = "{\"probe\":\"a\",\"outcome\":\"no_tv_data\"}\n";

    fn ids() -> Vec<String> {
        vec!["a".to_owned()]
    }

    #[test]
    fn a_complete_clean_iteration_passes() {
        assert!(check_iteration_complete(0, &ids(), A.as_bytes(), b"").is_ok());
    }

    #[test]
    fn a_partial_iteration_fails() {
        let both = vec!["a".to_owned(), "b".to_owned()];
        assert!(check_iteration_complete(0, &both, A.as_bytes(), b"").is_err());
    }

    #[test]
    fn complete_coverage_with_protocol_violations_fails() {
        let dup = format!("{A}{A}");
        assert!(check_iteration_complete(0, &ids(), dup.as_bytes(), b"").is_err());
        let extra = format!("{A}{{\"probe\":\"zz\",\"outcome\":\"no_tv_data\"}}\n");
        assert!(check_iteration_complete(0, &ids(), extra.as_bytes(), b"").is_err());
        let invalid = format!("{A}not json\n");
        let err = check_iteration_complete(0, &ids(), invalid.as_bytes(), b"").unwrap_err();
        assert!(err.to_string().contains("1 invalid"), "{err}");
    }
}
