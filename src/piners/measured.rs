//! Measured `brokkr corpus --hotpath` / `--alloc`: function-level timing and
//! per-function allocation tracking of the parity harness, recorded to
//! `.brokkr/results.db` and queried with `brokkr results` (like every other
//! measurable command).
//!
//! Distinct from the bare parity run ([`crate::piners::cmd::corpus`]): no gate,
//! no `runs.db` ingest, no isolation pass. One
//! integrity rule does apply: an iteration that exits 0 without reporting
//! exactly its selection (every probe validly, no protocol violation) fails
//! the measurement, unless its stop marker actually fired. Selection + hard
//! verification + manifest construction are shared with the parity path; the
//! build goes through [`crate::context::MeasurementSession::with_build_config`] with
//! the hotpath feature appended, so the run rides the same sidecar + results.db
//! lifecycle as pbfhogg.
//!
//! Only `--hotpath`/`--alloc` are supported. `--bench` (best-of-N wall-clock)
//! would need the harness to emit brokkr's `key=value` stderr timing contract;
//! until it does, that mode is refused with a clear error rather than recording
//! a meaningless number.

use crate::build::BuildConfig;
use crate::context::MeasurementSession;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::output;
use crate::piners::cmd::CorpusArgs;
use crate::piners::integrity::{self, HarnessEnd};
use crate::piners::manifest::Manifest;
use crate::piners::prepare::{self, PreparedSelection};
use crate::piners::select::SelectArgs;
use crate::project::{self, Project};

/// Run a measured corpus selection (`--hotpath`/`--alloc`). Routed here from
/// the `Corpus` dispatch when a measurement flag is set; bare/parity runs go to
/// [`crate::piners::cmd::corpus`] instead.
#[allow(clippy::too_many_lines)] // linear orchestration: select, verify, build, measure
pub(crate) fn run(req: &MeasureRequest, args: &CorpusArgs) -> Result<(), DevError> {
    project::require(req.project, Project::Piners, "corpus")?;

    let profiling = match req.mode {
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => req
            .mode
            .profiled()
            .ok_or_else(|| DevError::Internal("profiled mode without a profile".into()))?,
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

    let cfg = req.project_config.piners()?.clone();
    crate::piners::layout::ensure_distinct_registry_dirs(req.project_root.as_path(), &cfg)?;
    let harness_cfg = cfg
        .harness
        .as_ref()
        .ok_or_else(crate::piners::cmd::missing_harness_config_error)?;

    // Locked before the registry is read, as the parity path is (see
    // `cmd.rs`): verification must judge the pins and bytes the run uses.
    // `MeasurementSession` acquires again below; acquisition is re-entrant within
    // the process, so the hold is one and spans both.
    let lock = crate::context::reenter_command_lock()?;

    // Selection and hash verification use the same preparation path as a
    // parity run. The measured lifecycle still owns its benchmark context.
    let sel = SelectArgs {
        keywords: args.keywords.clone(),
        probe: args.probe.clone(),
        all: args.all,
        verify_only: false,
    };
    let PreparedSelection {
        registry: reg,
        corpus_root,
        ids,
        verified,
        ..
    } = prepare::prepare(req.project_root.as_path(), &cfg, &lock, &sel)?;

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
    build_cfg.features.push(profiling.feature().to_owned());

    let ctx = MeasurementSession::with_build_config_for_request(req, &build_cfg)?;

    // Manifest into the bench scratch dir. The harness writes its (ignored)
    // NDJSON there via BROKKR_HARNESS_ARTEFACT_DIR, and run_hotpath_capture
    // drops the hotpath JSON report beside it.
    let manifest_path = ctx.paths.scratch_dir.join("manifest.json");
    Manifest::build(&corpus_root, &verified, &reg).write(&manifest_path)?;

    let binary = ctx.binary()?;
    let bin_dir = binary.parent().ok_or_else(|| {
        DevError::Build(format!(
            "binary path {} has no parent directory",
            binary.display()
        ))
    })?;

    let binary_str = binary.display().to_string();
    let manifest_str = manifest_path.display().to_string();
    let scratch_str = ctx.paths.scratch_dir.display().to_string();
    let bin_dir_str = bin_dir.display().to_string();

    output::detail(&format!(
        "corpus {} ({})",
        profiling.feature(),
        output::count(verified.len(), "probe")
    ));
    profiling.print_note();

    let selector = selector_label(args);
    // The profile lives in the row's `cargo_profile` column, stamped from the
    // session's build. Rows recorded before `CargoProfile::Dev` existed carry
    // `release` there and a `meta.profile = dev` patch instead.
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
        input_file: Some(selector),
        input_mb: None,
        cargo_features: None,
        external_build: None,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(&binary_str, &subprocess_args)),
        metadata,
    };

    let env = [
        ("BROKKR_HARNESS_ARTEFACT_DIR", scratch_str.as_str()),
        ("BROKKR_TEST_BIN_DIR", bin_dir_str.as_str()),
    ];
    let project_root = req.project_root.as_path().to_path_buf();
    ctx.harness.run_hotpath(&config, binary, |i, fifo| {
        let mut iteration = ctx.harness.capture_hotpath(
            binary,
            &subprocess_args,
            &project_root,
            &env,
            &[0],
            fifo,
            i,
            Some(crate::piners::cmd::HARNESS_HANG_BACKSTOP),
        )?;
        if let Some(ending) = &iteration.ending
            && !matches!(ending, crate::output::Ending::StopMarker(_))
        {
            let end = HarnessEnd::from_ending(ending);
            if let Err(error) = check_iteration(i, &ids, end, &iteration.stdout, &iteration.stderr)
            {
                // The protocol assessment explains the failure more precisely
                // than the generic child verdict; keep its captured evidence.
                iteration.error = Some(error);
            }
        }
        Ok(iteration)
    })?;

    Ok(())
}

/// The measured-iteration policy comes from the shared assessment: only an
/// exit-0 completed run without a validly reported `harness_abort` is a sample.
/// On failure prints the reconciliation, context and whole stderr. No
/// isolation on this path.
fn check_iteration(
    iteration: usize,
    ids: &[String],
    end: HarnessEnd,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(), DevError> {
    let a = integrity::assess(ids, stdout, end, false);
    let why = match a.measured_verdict() {
        Ok(()) => return Ok(()),
        Err(why) => why,
    };
    output::corpus_msg(&format!("iteration {iteration}: {why}"));
    for line in a.detail_lines().into_iter().chain(a.context_lines()) {
        output::corpus_msg(&line);
    }
    integrity::print_stderr("harness stderr", &String::from_utf8_lossy(stderr));
    Err(DevError::Verify(format!(
        "corpus measurement: iteration {iteration}: {why}; the timing is not of the selected workload"
    )))
}

/// A compact label for the measured selection, stored in the result row's
/// `input_file` column and a `selector` metadata key. Mirrors the intent
/// rendering used by the corpus run-store views (`all` / `kw=...` / `probe=...`).
fn selector_label(args: &CorpusArgs) -> String {
    let raw =
        crate::piners::selector::json(args.all, &args.keywords, &args.probe, false, &[], None);
    let label = crate::piners::selector::compact(&raw);
    if label == raw {
        "selection".to_owned()
    } else {
        label
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

    const OK: HarnessEnd = HarnessEnd::Code(0);

    #[test]
    fn a_complete_clean_iteration_passes() {
        assert!(check_iteration(0, &ids(), OK, A.as_bytes(), b"").is_ok());
    }

    #[test]
    fn a_partial_iteration_fails() {
        let both = vec!["a".to_owned(), "b".to_owned()];
        assert!(check_iteration(0, &both, OK, A.as_bytes(), b"").is_err());
    }

    #[test]
    fn complete_coverage_with_protocol_violations_fails() {
        let dup = format!("{A}{A}");
        assert!(check_iteration(0, &ids(), OK, dup.as_bytes(), b"").is_err());
        let extra = format!("{A}{{\"probe\":\"zz\",\"outcome\":\"no_tv_data\"}}\n");
        assert!(check_iteration(0, &ids(), OK, extra.as_bytes(), b"").is_err());
        let invalid = format!("{A}not json\n");
        let err = check_iteration(0, &ids(), OK, invalid.as_bytes(), b"").unwrap_err();
        assert!(err.to_string().contains("1 invalid"), "{err}");
    }

    #[test]
    fn a_run_error_fails_the_iteration_with_the_rendered_record() {
        let nd = "{\"kind\":\"run_error\",\"version\":1,\"stage\":\"feed_load\",\"feed\":\"eth\",\"role\":\"base\",\"error\":\"no such file\"}\n";
        let err = check_iteration(3, &ids(), HarnessEnd::Code(2), nd.as_bytes(), b"").unwrap_err();
        assert!(
            err.to_string().contains(
                "iteration 3: run_error at stage feed_load (feed eth, role base): no such file"
            ),
            "{err}"
        );
    }

    #[test]
    fn a_harness_abort_is_never_a_timing_sample_even_on_exit_0() {
        let crash = "{\"probe\":\"a\",\"outcome\":\"harness_abort\",\"error\":\"signal 11\"}\n";
        // Legacy disposition-only stream, exit 0.
        assert!(check_iteration(0, &ids(), OK, crash.as_bytes(), b"").is_err());
        // A contract stream whose run_end says 0.
        let v = "\"version\":1";
        let nd = format!(
            "{{\"kind\":\"probe_start\",{v},\"probe\":\"a\"}}\n{crash}{{\"kind\":\"probe_end\",{v},\"probe\":\"a\"}}\n{{\"kind\":\"run_end\",{v},\"exit\":0}}\n"
        );
        let err = check_iteration(0, &ids(), OK, nd.as_bytes(), b"").unwrap_err();
        assert!(
            err.to_string().contains("harness_abort in the run"),
            "{err}"
        );
    }

    #[test]
    fn a_backstop_kill_after_a_run_error_is_not_a_contract_violation() {
        let nd = "{\"kind\":\"run_start\",\"version\":1}\n{\"kind\":\"run_error\",\"version\":1,\"stage\":\"x\",\"error\":\"y\"}\n";
        let a = integrity::assess(&ids(), nd.as_bytes(), HarnessEnd::Backstop, false);
        assert!(
            a.contract.violations.is_empty(),
            "{:?}",
            a.contract.violations
        );
        // The same SIGKILL read as spontaneous would be one.
        let a = integrity::assess(&ids(), nd.as_bytes(), HarnessEnd::Signal(9), false);
        assert!(!a.contract.violations.is_empty());
    }

    #[test]
    fn a_failed_status_is_read_before_it_is_rejected() {
        // Exit 1 with a complete stream (a harness_abort) is still no sample.
        let crash = "{\"probe\":\"a\",\"outcome\":\"harness_abort\",\"error\":\"signal 11\"}\n";
        let err =
            check_iteration(0, &ids(), HarnessEnd::Code(1), crash.as_bytes(), b"").unwrap_err();
        assert!(
            err.to_string().contains("harness_abort in the run (a)"),
            "{err}"
        );
        // A signal names itself and what was reported.
        let err = check_iteration(0, &ids(), HarnessEnd::Signal(9), b"", b"").unwrap_err();
        assert!(
            err.to_string()
                .contains("killed by signal 9 (1 selected, 0 scored, 1 missing)"),
            "{err}"
        );
    }
}
