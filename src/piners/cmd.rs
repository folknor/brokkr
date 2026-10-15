//! `brokkr corpus` - the piners parity-corpus runner.
//!
//! Resolves a keyword-selected slice of the pinned corpus, hard-verifies
//! every selected probe's `strategy.pine` + pinned oracle(s) against the
//! read-only submodule, writes a manifest, builds the harness once, and
//! invokes it with `--manifest <path>`. The harness consumes the manifest
//! and emits one enriched NDJSON disposition line per probe (no trailing
//! summary line); brokkr aggregates the summary and breakdowns itself (see
//! [`crate::piners::report`]).
//!
//! Three correctness checks apply. First, **verification**: a missing path or
//! a hash mismatch aborts before anything is built. Second, **report
//! integrity** ([`crate::piners::integrity`]), independent of the gate and of
//! `--no-gate`: the report is reconciled against the selected ids exactly,
//! and a selected probe with no valid disposition, an invalid record, a
//! repeated record or a line for an unselected probe fails the run - on exit
//! 0 too. Third, the **per-probe expected-disposition gate**
//! ([`crate::piners::gate`]): each probe pins an `expected` label in
//! `pins.toml`, and any deviation - regression or surprise improvement -
//! fails the run, as does a probe never blessed. `--no-gate` downgrades only
//! the gate to informational. The run also fails on any non-zero harness exit
//! the pins do not explain: exit 1 (break(s)) is acceptable only when every
//! break line is a selected probe pinned to that break
//! ([`crate::piners::gate::breaks_all_pinned`]), so a probe pinned to
//! `compile_fail` can pass; any other code or a signal always fails. `--bless`
//! refuses to stamp anything from a run that failed integrity or its exit.
//!
//! All of this reads one shared run assessment
//! ([`crate::piners::integrity::assess`]): termination, stream integrity, the
//! harness line contract ([`crate::piners::contract`]) and the exact
//! reconciliation. A `run_error` fails the run unconditionally and is stored
//! whole on the run row.
//!
//! An abnormal end (exit 2, another unexpected code, a spontaneous signal)
//! with no `run_error` that left selected probes unreported prints the whole
//! harness stderr and,
//! unless `--no-isolate`, runs the diagnostic isolation pass
//! ([`crate::piners::isolate`]) over the unreported probes, after the run is
//! recorded. Its findings are evidence attached to the run, never scored.
//!
//! `--reseed` (re-stamp hashes) and `--bless` (re-stamp dispositions) are the
//! two deliberate writers of `pins.toml`; see [`crate::piners::reseed`] and
//! [`crate::piners::bless`].

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::artefacts::ArtefactDir;
use crate::build::RustProfile;
use crate::config::project_config::piners::PinersConfig;
use crate::error::DevError;
use crate::harness_build;
use crate::lockfile::LockGuard;
use crate::output;
use crate::piners::contract::RunErrorRecord;
use crate::piners::corpus_db::{CorpusDb, RunContext, RunRecord};
use crate::piners::db_paths::corpus_runs_db_path;
use crate::piners::integrity::{self, HarnessEnd, Reconciliation, Trigger};
use crate::piners::isolate::{self, AttemptOutcome, StopReason};
use crate::piners::manifest::Manifest;
use crate::piners::registry::{self, Registry, VerifiedProbe};
use crate::piners::report;
use crate::piners::run_model::{Mode, ModeFlags, RunVerdict};
use crate::piners::select::SelectArgs;

const ARTEFACT_TEST_ID: &str = "corpus";

/// Wall-clock backstop on the harness subprocess. Not a budget - a run is let
/// finish however long its selection takes - but a hang guard: a wedged
/// harness would otherwise hold the global lock until someone runs `brokkr
/// kill`. Set far above any real run (a full pass included), so only a hang
/// reaches it; the run is then recorded as failed.
/// This fixed guard does not estimate from run history or use a dirty-tree
/// bypass; those checks belong to measured commands, not parity runs.
/// Shared with the measured path (`measured.rs`), which applies it per
/// iteration as the sidecar's deadline.
pub(crate) const HARNESS_HANG_BACKSTOP: Duration = Duration::from_secs(60 * 60);

/// Flags lifted off the `Corpus` CLI command.
#[derive(Debug, Default)]
pub struct CorpusArgs {
    pub keywords: Vec<String>,
    pub probe: Vec<String>,
    pub all: bool,
    pub verify_only: bool,
    /// Stamp `pins.toml` from the corpus filesystem instead of running.
    /// Routed to [`crate::piners::reseed`]; see its module docs.
    pub reseed: bool,
    /// Run the selection, then stamp each probe's current disposition into
    /// its `expected` field. Handled inline after the harness run (the run
    /// pipeline is shared); see [`crate::piners::bless`].
    pub bless: bool,
    /// Run + aggregate + report the per-probe gate diff, but never fail on
    /// it. Covers the bless-everything rollout and ad-hoc "just show me"
    /// runs. The harness exit and report integrity still govern pass/fail.
    pub no_gate: bool,
    /// Skip the diagnostic isolation pass after an abnormal harness end.
    /// Integrity reporting and the stderr print still apply.
    pub no_isolate: bool,
    /// The CLI profile choice (`--debug`/`--release`/`--profile`); `None`
    /// takes the config default, else dev for a parity run and release for
    /// a measured one.
    pub profile_override: Option<crate::build::RustProfile>,
    pub keep_artefacts: bool,
    /// Extra flags forwarded verbatim to the harness binary after
    /// `--manifest <path>` (everything after a literal `--` on the CLI).
    /// CLI-conflicted with `--verify-only`/`--reseed`/`--bless`; recorded
    /// in the run row's selector so a perturbed run is never mistaken for
    /// a clean one.
    pub harness_args: Vec<String>,
}

pub(crate) fn missing_harness_config_error() -> DevError {
    DevError::Config(
        "corpus: no [piners.harness] section in brokkr.toml. \
         Declare `[piners.harness]` with `package = \"<crate>\"` \
         (and optional `binary`, `features`, `debug`, `profile`)."
            .into(),
    )
}

/// Entry point for `brokkr corpus`.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // linear orchestration: load, select, verify, build, run, report, diagnose
pub fn corpus(
    project_root: &Path,
    piners: &PinersConfig,
    args: &CorpusArgs,
) -> Result<(), DevError> {
    let cfg = piners.clone();
    crate::piners::layout::ensure_distinct_registry_dirs(project_root, &cfg)?;
    let mode = Mode::resolve(ModeFlags {
        verify_only: args.verify_only,
        reseed: args.reseed,
        reanchor: false,
        bless: args.bless,
        no_gate: args.no_gate,
    })?;

    // Reseed stamps pins.toml from the corpus filesystem - no registry to
    // load (it may not exist yet), no build, no harness. Route early.
    if mode.is_reseed() {
        return crate::piners::reseed::run(project_root, &cfg, args);
    }

    // `--verify-only` deliberately needs no harness config. Every other
    // parity run can refuse a missing harness before hashing the selection.
    let harness_cfg = if mode.is_verify_only() {
        None
    } else {
        Some(
            cfg.harness
                .as_ref()
                .ok_or_else(missing_harness_config_error)?,
        )
    };

    // The lock comes before anything is read. Reseed and bless write
    // pins.toml under it, so a run that loaded and verified first could wait
    // out a reseed and then run - and bless - against pins and hashes that
    // are no longer the file's. Verify-only too: it would report drift for
    // bytes a reseed had just adopted. Held to the end, through ingest and
    // the bless write.
    let lock = crate::context::reenter_command_lock()?;
    let sigterm = crate::shutdown::SigtermGuard::install();

    let sel_args = SelectArgs {
        keywords: args.keywords.clone(),
        probe: args.probe.clone(),
        all: args.all,
        verify_only: mode.is_verify_only(),
    };
    let prepared = crate::piners::prepare::prepare(project_root, &cfg, &lock, &sel_args)?;
    let crate::piners::prepare::PreparedSelection {
        mut registry,
        registry_dir,
        corpus_root,
        ids,
        verified,
        feed_count,
        file_count,
    } = prepared;

    if mode.is_verify_only() {
        output::corpus_msg(&format!(
            "verify-only: {} + {} + {} OK",
            output::count(verified.len(), "probe"),
            output::count(feed_count, "feed group"),
            output::count(file_count, "harness file")
        ));
        return Ok(());
    }

    let harness_cfg = harness_cfg.ok_or_else(missing_harness_config_error)?;

    // Default profile is dev: parity is opt-level-independent, and the dev
    // build keeps the edit/run loop inside the cache-warm window. A custom
    // `[piners.harness] profile` trades a little of that for a faster run.
    // The config default resolves first so its override warning prints
    // whatever the CLI chose.
    let configured = harness_cfg.configured_profile("[piners.harness]");
    let profile = args
        .profile_override
        .clone()
        .or(configured)
        .unwrap_or(RustProfile::Dev);
    // Bring the run store to this binary's schema before anything is built or
    // run: a store that refuses to migrate would otherwise be found only at
    // ingest - after the whole harness run it was meant to keep.
    let corpus_db_path = corpus_runs_db_path(project_root);
    if corpus_db_path.exists() {
        drop(CorpusDb::open(&corpus_db_path)?);
    }

    // The run's provenance, taken before the build: what HEAD was, whether
    // the tree carried uncommitted work, and when the run began.
    let start = RunStart::capture(project_root);

    let built = harness_build::build_for_harness(
        project_root,
        harness_cfg,
        profile,
        Some(&|pid| lock.set_child_pid(pid)),
        Some(&|| lock.clear_child_pid()),
        crate::shutdown::ChildIsolation::ProcessGroup(&sigterm),
    )?;
    output::corpus_msg(&format!(
        "harness build ok (profile={}, features={}, binary={})",
        built.profile,
        built.features_label,
        built.binary.display()
    ));
    // The row records the profile the build returned: a run's wall is
    // comparable only against runs built the same way.
    let context = RunContext {
        profile: built.profile.clone(),
        harness_args: args.harness_args.clone(),
    };

    // One number per run: the dir is `run-<id>` and the row is stored under
    // the same id. Reserved under the lock, past both every stored run and
    // every dir on disk (an unrecorded run - a SIGKILL, a failed ingest -
    // leaves its dir, which holds the number until a clean removes it).
    // Derive the artefact parent from the shared run-store path so the
    // writable run directory stays beside `runs.db` if its root moves.
    let artefact_parent = corpus_db_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            DevError::Config(format!(
                "piners: cannot derive the artefact parent from {}",
                corpus_db_path.display()
            ))
        })?;
    let run_id = reserve_run_id(&corpus_db_path, &artefact_parent.join(ARTEFACT_TEST_ID))?;
    let artefacts = ArtefactDir::allocate_at(
        artefact_parent,
        ARTEFACT_TEST_ID,
        run_id,
        args.keep_artefacts,
    )?;
    output::corpus_msg(&format!("run {run_id} -> {}", artefacts.path().display()));
    let selector = selector_json(args, &ids, &context, mode.is_bless());
    let envelope = Envelope {
        run_id,
        start: &start,
        selector: &selector,
        context: &context,
        gated: mode.gated(),
    };

    let manifest_path = artefacts.path().join("manifest.json");
    let manifest = Manifest::build(&corpus_root, &verified, &registry);
    if let Err(e) = manifest.write(&manifest_path) {
        return Err(record_unfinished(
            &corpus_db_path,
            &envelope,
            artefacts,
            &RunVerdict::error(e.to_string()),
            e,
            None,
        ));
    }
    output::corpus_msg(&format!(
        "manifest: {} -> {}",
        output::count(verified.len(), "probe"),
        manifest_path.display()
    ));

    // A run is let finish, bounded only by the hang backstop. PID is tracked
    // so `brokkr kill` reaches the harness.
    let binary_str = built.binary.display().to_string();
    let manifest_str = manifest_path.display().to_string();
    let harness_env = crate::ratatoskr::harness_env(artefacts.path(), &built.bin_dir);
    let env_pairs = harness_env.as_pairs();

    let mut harness_argv: Vec<&str> = vec!["--manifest", &manifest_str];
    harness_argv.extend(context.harness_args.iter().map(String::as_str));
    if !context.harness_args.is_empty() {
        output::corpus_msg(&format!(
            "forwarding to harness: {}",
            context.harness_args.join(" ")
        ));
    }

    // The one deadline: the hang backstop, counted from this launch, bounds
    // the original attempt and every diagnostic attempt after it together.
    // The shared captured-process runner returns the completed stream as one
    // buffer; integrity reconciliation and isolation use the same original
    // report after the harness exits.
    let launched = Instant::now();
    let capture = match capture_harness(
        &binary_str,
        &harness_argv,
        project_root,
        &env_pairs,
        HARNESS_HANG_BACKSTOP,
        Some(&|pid| lock.set_child_pid(pid)),
        crate::shutdown::ChildIsolation::ProcessGroup(&sigterm),
    ) {
        Ok(c) => c,
        Err(DevError::Interrupted) => {
            lock.clear_child_pid();
            // A requested stop is never diagnosed: the user asked brokkr to stop.
            output::corpus_msg(&format!(
                "harness {}; nothing scored, no isolation",
                HarnessEnd::Interrupted.describe()
            ));
            let original = DevError::Interrupted;
            return Err(record_unfinished(
                &corpus_db_path,
                &envelope,
                artefacts,
                &RunVerdict::interrupted(original.to_string()),
                original,
                None,
            ));
        }
        Err(e) => {
            lock.clear_child_pid();
            let msg = format!("failed to spawn {}: {e}\n", built.binary.display());
            std::fs::write(artefacts.path().join("spawn-error.txt"), &msg).ok();
            output::corpus_msg(&format!(
                "harness {}: {e}",
                HarnessEnd::SpawnFailed.describe()
            ));
            // Recorded so it surfaces in `brokkr corpus-results`; the dir is
            // still preserved - a spawn failure is exactly when on-disk
            // forensics matter most, and the row is a convenience index.
            return Err(record_unfinished(
                &corpus_db_path,
                &envelope,
                artefacts,
                &RunVerdict::error("harness failed to spawn"),
                e,
                Some(&msg),
            ));
        }
    };
    lock.clear_child_pid();

    let end = HarnessEnd::from_ending(&capture.captured.ending);
    let output_cut = capture.output_cut;
    let captured = capture.captured;
    // Keep stdout/stderr on disk until ingest commits - the pre-ingest safety
    // net if anything panics between here and the DB write.
    // Both writes are attempted; a failure does not stop parsing or ingest
    // (the capture is in memory), but it fails the run, refuses bless, counts
    // as a stored violation and skips isolation - see `evidence_failed`.
    let evidence_errors = store_streams(artefacts.path(), &captured.stdout, &captured.stderr);
    for e in &evidence_errors {
        output::warn(&format!("harness evidence could not be stored: {e}"));
    }
    let evidence_failed = !evidence_errors.is_empty();

    // The one shared assessment: termination, stream integrity, the contract
    // and the exact selection reconciliation. Repeats are collapsed in it,
    // so the gate, bless and ingest all see one record per key.
    let assessment = integrity::assess(&ids, &captured.stdout, end, output_cut);
    let report = &assessment.report;
    let rec = &assessment.rec;

    let elapsed_ms = captured.elapsed.as_millis();
    let harness_code = captured.code();

    // Evaluate the gate up front. It drives the pass/fail decision, feeds the
    // gate_miss table (selected probes the harness emitted no line for), and
    // selects which per-probe lines render prints: a probe sitting exactly on
    // its pin is folded into a count, so the lines that survive are the
    // deviations. Bless never gates - it ignores the verdict - but the diffs
    // still record and still drive the render filter (what differs from the
    // pins about to be re-stamped).
    let gate_diffs = crate::piners::gate::evaluate(&ids, &registry, report);
    // Is exit 1 ("break(s)") explained? For a gated run, when every break is
    // one the pins expect; for bless, when the report actually carries the
    // breaks, since recording them is bless's job.
    let exit1_explained = if mode.is_bless() {
        crate::piners::gate::report_has_break(report)
    } else {
        crate::piners::gate::breaks_all_pinned(&ids, &registry, report)
    };
    let mut harness_reasons = match assessment.parity_verdict(exit1_explained) {
        Ok(()) => Vec::new(),
        Err(reasons) => reasons,
    };
    if evidence_failed {
        harness_reasons.push(evidence_reason(&evidence_errors));
    }
    let harness_ok = harness_reasons.is_empty();
    let verdict = RunVerdict::decide(
        (!harness_ok).then(|| harness_reasons.join("; ")),
        mode,
        (!gate_diffs.is_empty()).then(|| output::count(gate_diffs.len(), "gate deviation")),
    );
    let run_pass = verdict.is_pass();

    // Render the body now that the deviation set is known. Probes matching
    // their pin collapse to a single count; the survivors are worth reading.
    // The summary leads with the reconciliation, so an empty report reads as
    // "N selected, 0 scored, N missing", never as "0 total".
    let deviating: HashSet<&str> = gate_diffs.iter().map(|d| d.probe.as_str()).collect();
    // Aggregates and the hidden-match count cover the scored set only.
    let scored: HashSet<&str> = rec.valid.iter().map(String::as_str).collect();
    report::render(report, &scored, &deviating, &rec.summary());
    for line in assessment.detail_lines() {
        output::corpus_msg(&line);
    }
    let stderr_text = String::from_utf8_lossy(&captured.stderr);
    if let Some(e) = &assessment.contract.run_error {
        // The harness named its own failure; every locator it supplied.
        output::corpus_msg(&format!("harness {}", e.render()));
    }
    if !end.completed_status() || !rec.complete() || assessment.contract.run_error.is_some() {
        // How the harness ended against how much it reported, what was in
        // flight (context, never cause), then its whole stderr: the only
        // evidence of an abort.
        output::corpus_msg(&end_line(end, rec));
        for line in assessment.context_lines() {
            output::corpus_msg(&line);
        }
        integrity::print_stderr("harness stderr", &stderr_text);
    }

    // Persist the run BEFORE any pins mutation or finalize, using the pinned
    // expectations as they stand at run time. An ingest failure preserves the
    // dir (the on-disk stdout is the evidence) and propagates.
    let expected: BTreeMap<String, Option<String>> = ids
        .iter()
        .map(|id| {
            let exp = registry
                .pins
                .get(id)
                .and_then(|p| p.expected)
                .map(|disposition| disposition.as_str().to_owned());
            (id.clone(), exp)
        })
        .collect();
    // The run_error is stored whole, as structured JSON, in the run's own
    // transaction; the shared verdict above is its readable projection.
    let run_error_json = assessment
        .contract
        .run_error
        .as_ref()
        .map(RunErrorRecord::to_json);
    let record = envelope.record(
        &verdict,
        harness_code,
        &stderr_text,
        // brokkr's own measurement of the whole harness subprocess - the real
        // wall. The original attempt only: diagnosis below never extends it.
        Some(elapsed_ms as f64),
        StoredIntegrity {
            protocol_violations: Some(stored_violations(&assessment, evidence_failed)),
            run_error: run_error_json.as_deref(),
        },
    );
    // From here the run is recorded exactly once: a later failure (a bless
    // refusing a changed pins.toml, a finalize error) is the command's error,
    // not a second row under this id. Recorded BEFORE any diagnosis, so an
    // interrupted diagnosis cannot lose the original record.
    if let Err(e) = ingest_run(&corpus_db_path, &record, report, &expected, &gate_diffs) {
        output::warn(&format!(
            "failed to persist run to {}: {e}",
            corpus_db_path.display()
        ));
        artefacts.finalize_failure();
        return Err(e);
    }

    // Diagnostic isolation: evidence only, attached to the recorded run.
    // Never on top of evidence the run could not keep: that is an explicit
    // evidence-storage stop, recorded on the run like a diagnosis.
    let trigger = integrity::isolation_trigger(
        end,
        rec,
        assessment.contract.run_error.is_some(),
        args.no_isolate,
    );
    if evidence_failed {
        let note = evidence_note(trigger == Trigger::Isolate, &evidence_errors);
        output::corpus_msg(&note);
        let stored =
            CorpusDb::open(&corpus_db_path).and_then(|db| db.append_diagnosis(run_id, &note));
        if let Err(e) = stored {
            output::warn(&format!(
                "run {run_id}: the evidence-storage failure could not be recorded: {e}"
            ));
        }
    }
    let diagnosis = match trigger {
        _ if evidence_failed => None,
        Trigger::Isolate => {
            let attempts = AttemptEnv {
                run_dir: artefacts.path(),
                corpus_root: &corpus_root,
                verified: &verified,
                registry: &registry,
                binary: &binary_str,
                harness_args: &context.harness_args,
                bin_dir: &built.bin_dir,
                project_root,
                lock: &lock,
                launched,
            };
            Some(diagnose(
                &attempts,
                rec.unreported(&ids),
                &corpus_db_path,
                run_id,
            ))
        }
        Trigger::Disabled => {
            output::corpus_msg("isolation skipped (--no-isolate)");
            None
        }
        _ => None,
    };
    if diagnosis.is_some() {
        output::corpus_msg(&format!(
            "harness wall {elapsed_ms}ms; total elapsed including diagnosis {}ms",
            launched.elapsed().as_millis()
        ));
    }
    let diagnosis_interrupted = diagnosis
        .as_ref()
        .is_some_and(|d| d.stopped == Some(isolate::StopReason::Interrupted));
    // Whenever diagnosis ran, its attempt dirs are the evidence: keep the
    // run dir and say where it is. When the run's own streams could not be
    // written, keep it too and say which evidence it lacks.
    let finish = |artefacts: ArtefactDir| -> Result<(), DevError> {
        if evidence_failed {
            output::corpus_msg(&preserved_incomplete_line(
                artefacts.path(),
                &evidence_errors,
            ));
            crate::artefacts::emit_clean_hint(output::corpus_msg);
            artefacts.finalize_failure();
            Ok(())
        } else if diagnosis.is_some() {
            output::corpus_msg(&format!(
                "artefacts preserved: {}",
                artefacts.path().display()
            ));
            crate::artefacts::emit_clean_hint(output::corpus_msg);
            artefacts.finalize_failure();
            Ok(())
        } else {
            artefacts.finalize_success()
        }
    };
    if diagnosis_interrupted {
        finish(artefacts)?;
        return Err(DevError::Interrupted);
    }

    // Bless: stamp current dispositions into pins.toml. The run is already
    // persisted, so the dir drops like any other (unless --keep-artefacts).
    // A harness that failed or broke the report contract (see harness_ok)
    // blesses nothing: its surviving lines are whatever it emitted before it
    // died, not the dispositions.
    if mode.is_bless() {
        if !harness_ok {
            finish(artefacts)?;
            let reason = verdict.reason().unwrap_or("harness failed");
            output::corpus_msg(&format!(
                "FAIL: {reason} in {elapsed_ms}ms - nothing blessed, pins.toml untouched \
                 (recorded; see `brokkr corpus-results`)"
            ));
            return Err(DevError::ExitCode(1));
        }
        let pins_path = registry_dir.join(registry::PINS_FILE);
        crate::piners::bless::apply(&pins_path, &mut registry, report, &ids)?;
        finish(artefacts)?;
        return Ok(());
    }

    if !gate_diffs.is_empty() {
        crate::piners::gate::render_diffs(&gate_diffs);
    }

    // Data is durable in the DB; the dir is dropped (unless --keep-artefacts)
    // except when diagnosis ran, whose attempt dirs are not in the DB.
    // `brokkr corpus-results` is the home for the run's drill-down.
    finish(artefacts)?;
    if run_pass {
        output::corpus_msg(&format!("PASS in {elapsed_ms}ms"));
        Ok(())
    } else {
        let reason = verdict.reason().unwrap_or("fail");
        output::corpus_msg(&format!(
            "FAIL: {reason} in {elapsed_ms}ms (recorded; see `brokkr corpus-results`)"
        ));
        Err(DevError::ExitCode(1))
    }
}

/// Run the isolation pass over `unreported`, print what it found, and attach
/// its note to the already-recorded run `run_id`. A failure to record the
/// note is reported, never fatal: the original row stands either way.
fn diagnose(
    attempts: &AttemptEnv<'_>,
    unreported: Vec<String>,
    db_path: &Path,
    run_id: i64,
) -> isolate::Diagnosis {
    output::corpus_msg(&format!(
        "isolating: bisecting only the {} the original run left unreported \
         (same binary, env, flags and forwarded args; attempt dirs under {})",
        output::count(unreported.len(), "selected probe"),
        attempts.run_dir.display()
    ));
    let d = isolate::bisect(unreported, |subset, n| attempts.run(subset, n));
    for line in d.lines() {
        output::corpus_msg(&line);
    }
    let stored = CorpusDb::open(db_path)
        .and_then(|db| db.append_diagnosis(run_id, &d.note(attempts.run_dir)));
    if let Err(e) = stored {
        output::warn(&format!(
            "run {run_id}: the diagnosis could not be recorded: {e}"
        ));
    }
    d
}

/// The console line for a run that ended abnormally or left probes
/// unreported.
fn end_line(end: HarnessEnd, rec: &Reconciliation) -> String {
    if end.completed_status() {
        format!(
            "harness {} without a valid disposition for {} of {} selected probes",
            end.describe(),
            rec.unscored(),
            rec.selected
        )
    } else {
        integrity::abnormal_end_line(end, rec)
    }
}

/// Everything a diagnostic attempt reuses from the original run: the same
/// built binary, verified probes, env, cwd and forwarded harness args, under
/// the same lock and the same deadline.
struct AttemptEnv<'a> {
    run_dir: &'a Path,
    corpus_root: &'a Path,
    verified: &'a [VerifiedProbe],
    registry: &'a Registry,
    binary: &'a str,
    harness_args: &'a [String],
    bin_dir: &'a Path,
    project_root: &'a Path,
    lock: &'a LockGuard,
    /// When the original harness launched; the backstop counts from here.
    launched: Instant,
}

impl AttemptEnv<'_> {
    /// Run one diagnostic attempt over `subset` in `<run dir>/attempt-<n>/`:
    /// its own manifest (built from the same verified probes), stdout,
    /// stderr and `BROKKR_HARNESS_ARTEFACT_DIR`. Judged by the same run
    /// assessment as the original run; see [`isolate::classify_attempt`].
    fn run(&self, subset: &[String], n: usize) -> AttemptOutcome {
        if crate::shutdown::is_shutdown_requested() {
            return AttemptOutcome::Stop(StopReason::Interrupted);
        }
        let Some(remaining) = HARNESS_HANG_BACKSTOP
            .checked_sub(self.launched.elapsed())
            .filter(|d| !d.is_zero())
        else {
            return AttemptOutcome::Stop(StopReason::Deadline);
        };
        let dir = self.run_dir.join(format!("attempt-{n}"));
        let manifest_path = dir.join("manifest.json");
        let wanted: HashSet<&str> = subset.iter().map(String::as_str).collect();
        let probes: Vec<VerifiedProbe> = self
            .verified
            .iter()
            .filter(|v| wanted.contains(v.id.as_str()))
            .cloned()
            .collect();
        let prepared = std::fs::create_dir_all(&dir)
            .map_err(DevError::Io)
            .and_then(|()| {
                Manifest::build(self.corpus_root, &probes, self.registry).write(&manifest_path)
            });
        if let Err(e) = prepared {
            return AttemptOutcome::Stop(StopReason::Setup(format!("{}: {e}", dir.display())));
        }
        let manifest_str = manifest_path.display().to_string();
        let harness_env = crate::ratatoskr::harness_env(&dir, self.bin_dir);
        let env = harness_env.as_pairs();
        let mut argv: Vec<&str> = vec!["--manifest", &manifest_str];
        argv.extend(self.harness_args.iter().map(String::as_str));
        let lock = self.lock;
        // Guards nest; this one is the borrow the process-group isolation needs.
        let signal_guard = crate::shutdown::SigtermGuard::install();
        let capture = output::run_captured_with_env_and_deadline_allow_output_cut(
            self.binary,
            &argv,
            self.project_root,
            &env,
            remaining,
            Some(&|pid| lock.set_child_pid(pid)),
            crate::shutdown::ChildIsolation::ProcessGroup(&signal_guard),
            output::child::ChildOptions::LARGE,
        );
        drop(signal_guard);
        lock.clear_child_pid();
        let capture = match capture {
            Ok(c) => c,
            Err(DevError::Interrupted) => return AttemptOutcome::Stop(StopReason::Interrupted),
            Err(e @ DevError::ChildFailure { .. }) => {
                return AttemptOutcome::Stop(StopReason::CaptureIncomplete(e.to_string()));
            }
            Err(e) => return AttemptOutcome::Stop(StopReason::SpawnFailed(e.to_string())),
        };
        // The streams are the evidence the run will claim to preserve: a
        // failure to store them stops diagnosis rather than go on unrecorded.
        if let Err(reason) =
            store_attempt_streams(&dir, &capture.captured.stdout, &capture.captured.stderr)
        {
            return AttemptOutcome::Stop(reason);
        }
        if capture.killed_on_deadline() {
            // Never attributed to a probe: the subset ran out of shared time.
            return AttemptOutcome::Stop(StopReason::Deadline);
        }
        let end = HarnessEnd::from_ending(&capture.captured.ending);
        // The same assessment the original run gets, over this subset.
        let a = integrity::assess(subset, &capture.captured.stdout, end, capture.output_cut);
        output::corpus_msg(&format!(
            "attempt {n}: {} -> {}, {}",
            output::count(subset.len(), "probe"),
            end.short(),
            a.rec.summary()
        ));
        match isolate::classify_attempt(&a) {
            isolate::AttemptVerdict::Stop(stop) => AttemptOutcome::Stop(stop),
            isolate::AttemptVerdict::Completed => AttemptOutcome::Completed,
            isolate::AttemptVerdict::Failed(failure) => AttemptOutcome::Failed {
                end,
                failure,
                stderr: String::from_utf8_lossy(&capture.captured.stderr).into_owned(),
                dir,
            },
        }
    }
}

/// Write captured streams into `dir` as `harness.stdout`/`harness.stderr`.
/// Both writes are attempted; returns one path-specific message per failure
/// (empty when both landed). Used for the original run and every attempt.
fn store_streams(dir: &Path, stdout: &[u8], stderr: &[u8]) -> Vec<String> {
    let mut failed = Vec::new();
    for (name, bytes) in [("harness.stdout", stdout), ("harness.stderr", stderr)] {
        let path = dir.join(name);
        if let Err(e) = std::fs::write(&path, bytes) {
            failed.push(format!("{}: {e}", path.display()));
        }
    }
    failed
}

/// The fail reason for run evidence that could not be stored.
fn evidence_reason(errors: &[String]) -> String {
    format!("evidence storage failed: {}", errors.join("; "))
}

/// The note printed and stored on the run when its own streams could not be
/// written: an explicit evidence-storage stop in place of isolation.
fn evidence_note(would_isolate: bool, errors: &[String]) -> String {
    let what = if would_isolate {
        "isolation stopped before its first attempt on an evidence-storage failure"
    } else {
        "evidence-storage failure"
    };
    format!("{what}: {}", errors.join("; "))
}

/// `artefacts preserved with incomplete evidence (failed: harness.stdout): DIR`.
fn preserved_incomplete_line(dir: &Path, errors: &[String]) -> String {
    let failed: Vec<&str> = ["harness.stdout", "harness.stderr"]
        .into_iter()
        .filter(|name| errors.iter().any(|e| e.contains(name)))
        .collect();
    format!(
        "artefacts preserved with incomplete evidence (failed: {}): {}",
        failed.join(", "),
        dir.display()
    )
}

/// [`store_streams`] for a diagnostic attempt: any failure stops diagnosis
/// with the evidence-storage reason naming every failed path.
fn store_attempt_streams(dir: &Path, stdout: &[u8], stderr: &[u8]) -> Result<(), StopReason> {
    let failed = store_streams(dir, stdout, stderr);
    if failed.is_empty() {
        Ok(())
    } else {
        Err(StopReason::EvidenceStorage(failed.join("; ")))
    }
}

/// The integrity count stored on the run row: reconciliation and contract
/// violations, plus one for a stdout cut short, plus one when the run's own
/// stream evidence could not be stored.
fn stored_violations(a: &integrity::RunAssessment, evidence_failed: bool) -> i64 {
    let n = a.stored_violations() + usize::from(evidence_failed);
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Build the `selector` JSON stored on the run row: the resolved probe ids
/// plus the raw selection flags - enough to group by and to reproduce.
/// Forwarded harness flags and the build profile are part of the run's
/// identity (both change what the harness does and how long it takes), so
/// they remain in this selection record too; `RunContext` also stores them in
/// dedicated columns for strict comparison.
fn selector_json(args: &CorpusArgs, ids: &[String], context: &RunContext, bless: bool) -> String {
    crate::piners::selector::json(
        args.all,
        &args.keywords,
        &args.probe,
        bless,
        ids,
        Some((&context.profile, &context.harness_args)),
    )
}

/// What a run knows about itself before it executes: when it started, and
/// the checkout it was taken from.
pub(crate) struct RunStart {
    /// `YYYY-MM-DD HH:MM:SS`, UTC - the store's `started_at` convention.
    pub started_at: String,
    /// Full `HEAD` hash; `None` outside git or when git fails.
    pub commit_sha: Option<String>,
    /// Uncommitted changes outside `.brokkr/` ([`crate::git::has_uncommitted`]).
    pub dirty: Option<bool>,
}

impl RunStart {
    pub(crate) fn capture(project_root: &Path) -> Self {
        Self {
            started_at: crate::piners::time::now_sqlite_utc(),
            commit_sha: crate::git::resolve_commit(project_root, "HEAD")
                .ok()
                .map(|c| c.as_str().to_owned()),
            dirty: crate::git::has_uncommitted(project_root),
        }
    }
}

/// The run-row fields fixed before the harness runs, shared by the normal
/// ingest and the unfinished-run record so both store the same identity.
struct Envelope<'a> {
    run_id: i64,
    start: &'a RunStart,
    selector: &'a str,
    context: &'a RunContext,
    gated: bool,
}

/// The integrity fields a run row stores beside its result: the violation
/// count and the structured `run_error`. Both `None` for
/// a run that produced no report.
#[derive(Default, Clone, Copy)]
struct StoredIntegrity<'a> {
    protocol_violations: Option<i64>,
    run_error: Option<&'a str>,
}

impl<'a> Envelope<'a> {
    fn record<'b>(
        &'b self,
        verdict: &'b RunVerdict,
        harness_exit_code: Option<i32>,
        stderr: &'b str,
        wall_ms: Option<f64>,
        integrity: StoredIntegrity<'b>,
    ) -> RunRecord<'b>
    where
        'a: 'b,
    {
        RunRecord {
            run_id: Some(self.run_id),
            started_at: Some(&self.start.started_at),
            commit_sha: self.start.commit_sha.as_deref(),
            dirty: self.start.dirty,
            selector: self.selector,
            context: Some(self.context),
            gated: self.gated,
            verdict,
            harness_exit_code,
            stderr,
            wall_ms,
            protocol_violations: integrity.protocol_violations,
            run_error: integrity.run_error,
        }
    }
}

/// Run one harness attempt under its caller's wall-clock deadline.
///
/// Kept as the corpus command's boundary so its timeout policy can be tested
/// with a short deadline without changing the production backstop.
fn capture_harness(
    program: &str,
    argv: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    deadline: Duration,
    on_spawn: Option<&dyn Fn(u32)>,
    isolation: crate::shutdown::ChildIsolation<'_>,
) -> Result<output::DeadlineCapture, DevError> {
    output::run_captured_with_env_and_deadline_allow_output_cut(
        program,
        argv,
        cwd,
        env,
        deadline,
        on_spawn,
        isolation,
        output::child::ChildOptions::LARGE,
    )
}

/// The next run id: one past the highest of every stored run and every
/// `run-N` dir on disk. Called under the lock, so no other corpus run can
/// take the same number between this read and the dir's creation.
fn reserve_run_id(db_path: &Path, run_dirs: &Path) -> Result<i64, DevError> {
    let stored = if db_path.exists() {
        CorpusDb::open_readonly(db_path)?
            .latest_run_id()?
            .unwrap_or(0)
    } else {
        0
    };
    let on_disk = crate::artefacts::highest_run_number(run_dirs)?;
    stored
        .max(on_disk)
        .checked_add(1)
        .ok_or_else(|| DevError::Database("corpus: run ids exhausted".to_owned()))
}

/// Record a run that ended before its harness output could be ingested
/// (interrupted, failed to spawn, could not write its manifest), so its id
/// names a row and not only a dir. The dir is preserved either way. Returns
/// the error the command fails with - always `original`; a failure to
/// record is reported beside it, never in its place. Prints where the
/// preserved artefacts are, then the `brokkr clean` hint
/// ([`crate::artefacts::emit_clean_hint`], through the corpus printer).
fn record_unfinished(
    db_path: &Path,
    envelope: &Envelope<'_>,
    artefacts: ArtefactDir,
    verdict: &RunVerdict,
    original: DevError,
    detail: Option<&str>,
) -> DevError {
    let stderr = detail.unwrap_or("");
    // Never ran -> no measured wall, no exit code.
    let record = envelope.record(verdict, None, stderr, None, StoredIntegrity::default());
    if let Err(e) = ingest_run(
        db_path,
        &record,
        &report::HarnessReport::default(),
        &BTreeMap::new(),
        &[],
    ) {
        output::warn(&format!(
            "run {} could not be recorded in {}: {e}",
            envelope.run_id,
            db_path.display()
        ));
    }
    output::corpus_msg(&format!(
        "artefacts preserved: {}",
        artefacts.path().display()
    ));
    crate::artefacts::emit_clean_hint(output::corpus_msg);
    artefacts.finalize_failure();
    original
}

/// Open the corpus DB and persist one run. Shared by the normal path and the
/// unfinished-run record.
fn ingest_run(
    db_path: &Path,
    record: &RunRecord<'_>,
    report: &report::HarnessReport,
    expected: &BTreeMap<String, Option<String>>,
    gate_diffs: &[crate::piners::gate::GateDiff],
) -> Result<(), DevError> {
    let db = CorpusDb::open(db_path)?;
    db.record_run(record, report, expected, gate_diffs)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        crate::test_scratch::scratch("piners_cmd", name)
    }

    fn record_with_id<'a>(run_id: i64) -> RunRecord<'a> {
        RunRecord {
            run_id: Some(run_id),
            started_at: None,
            commit_sha: None,
            dirty: None,
            selector: "{}",
            context: None,
            gated: true,
            verdict: &RunVerdict::Pass,
            harness_exit_code: None,
            stderr: "",
            wall_ms: None,
            protocol_violations: None,
            run_error: None,
        }
    }

    /// Create a store at `db_path` holding one row under `run_id`.
    fn store_run(db_path: &Path, run_id: i64) {
        let db = CorpusDb::open(db_path).unwrap();
        let rec = record_with_id(run_id);
        db.record_run(
            &rec,
            &report::HarnessReport::default(),
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
    }

    fn start() -> RunStart {
        RunStart {
            started_at: "2026-01-02 03:04:05".to_owned(),
            commit_sha: Some("abc123".to_owned()),
            dirty: Some(true),
        }
    }

    #[test]
    fn reserve_missing_db_and_missing_dir_is_1() {
        let root = scratch("reserve_missing");
        let id = reserve_run_id(&root.join("runs.db"), &root.join("corpus")).unwrap();
        assert_eq!(id, 1);
    }

    #[test]
    fn reserve_empty_db_and_empty_dir_is_1() {
        let root = scratch("reserve_empty");
        let db_path = root.join("runs.db");
        drop(CorpusDb::open(&db_path).unwrap());
        let dirs = root.join("corpus");
        fs::create_dir_all(&dirs).unwrap();
        assert_eq!(reserve_run_id(&db_path, &dirs).unwrap(), 1);
    }

    #[test]
    fn reserve_db_max_above_dir_max() {
        let root = scratch("reserve_db_higher");
        let db_path = root.join("runs.db");
        store_run(&db_path, 9);
        let dirs = root.join("corpus");
        fs::create_dir_all(dirs.join("run-3")).unwrap();
        assert_eq!(reserve_run_id(&db_path, &dirs).unwrap(), 10);
    }

    #[test]
    fn reserve_dir_max_above_db_max() {
        let root = scratch("reserve_dir_higher");
        let db_path = root.join("runs.db");
        store_run(&db_path, 2);
        let dirs = root.join("corpus");
        fs::create_dir_all(dirs.join("run-7")).unwrap();
        assert_eq!(reserve_run_id(&db_path, &dirs).unwrap(), 8);
    }

    #[test]
    fn reserve_counts_a_file_named_like_a_run_dir() {
        let root = scratch("reserve_file_counts");
        let dirs = root.join("corpus");
        fs::create_dir_all(&dirs).unwrap();
        fs::write(dirs.join("run-42"), "not a dir").unwrap();
        assert_eq!(reserve_run_id(&root.join("runs.db"), &dirs).unwrap(), 43);
    }

    #[test]
    fn reserve_errors_instead_of_overflowing() {
        let root = scratch("reserve_overflow");
        let db_path = root.join("runs.db");
        store_run(&db_path, i64::MAX);
        let result = reserve_run_id(&db_path, &root.join("corpus"));
        assert!(matches!(result, Err(DevError::Database(_))), "{result:?}");
    }

    #[test]
    fn harness_deadline_is_reported_as_the_backstop() {
        let capture = capture_harness(
            "/bin/sleep",
            &["30"],
            Path::new("."),
            &[],
            Duration::from_millis(250),
            None,
            crate::shutdown::ChildIsolation::Inherit,
        )
        .unwrap();

        assert!(capture.killed_on_deadline());
        assert_eq!(
            HarnessEnd::from_ending(&capture.captured.ending),
            HarnessEnd::Backstop
        );
    }

    #[test]
    fn envelope_record_carries_the_run_identity() {
        let start = start();
        let context = RunContext {
            profile: RustProfile::Dev,
            harness_args: vec!["--flag".to_owned()],
        };
        let envelope = Envelope {
            run_id: 17,
            start: &start,
            selector: r#"{"all":true}"#,
            context: &context,
            gated: false,
        };
        let integrity = StoredIntegrity {
            protocol_violations: Some(3),
            run_error: Some("{}"),
        };
        let verdict = RunVerdict::Fail("why".to_owned());
        let rec = envelope.record(&verdict, Some(2), "err", Some(1.5), integrity);
        assert_eq!(rec.protocol_violations, Some(3));
        assert_eq!(rec.run_error, Some("{}"));
        assert_eq!(rec.run_id, Some(17));
        assert_eq!(rec.started_at, Some("2026-01-02 03:04:05"));
        assert_eq!(rec.commit_sha, Some("abc123"));
        assert_eq!(rec.dirty, Some(true));
        assert_eq!(rec.selector, r#"{"all":true}"#);
        assert_eq!(rec.context, Some(&context));
        assert!(!rec.gated);
        assert_eq!(rec.verdict.result(), "fail");
        assert_eq!(rec.verdict.reason(), Some("why"));
        assert_eq!(rec.harness_exit_code, Some(2));
        assert_eq!(rec.stderr, "err");
        assert_eq!(rec.wall_ms, Some(1.5));
    }

    /// The shared assessment of an NDJSON literal (no contract lines unless
    /// the literal carries them).
    fn assessed(ids: &[&str], nd: &str, end: HarnessEnd, cut: bool) -> integrity::RunAssessment {
        let ids: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
        integrity::assess(&ids, nd.as_bytes(), end, cut)
    }

    fn reasons(ids: &[&str], nd: &str, end: HarnessEnd, cut: bool, exit1: bool) -> Vec<String> {
        match assessed(ids, nd, end, cut).parity_verdict(exit1) {
            Ok(()) => Vec::new(),
            Err(reasons) => reasons,
        }
    }

    const A_OK: &str =
        "{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"accepted\"}}\n";

    #[test]
    fn a_clean_exit_with_a_missing_probe_fails() {
        let r = reasons(&["a", "b"], A_OK, HarnessEnd::Code(0), false, false);
        assert_eq!(
            r,
            vec!["1 of 2 selected probes without a valid disposition".to_owned()]
        );
        assert!(reasons(&["a"], A_OK, HarnessEnd::Code(0), false, false).is_empty());
    }

    #[test]
    fn an_abnormal_end_names_how_much_went_unreported() {
        let r = reasons(&["a", "b", "c"], A_OK, HarnessEnd::Code(2), false, false);
        assert_eq!(
            r,
            vec!["harness exited 2 before reporting 2 of 3".to_owned()]
        );
        let r = reasons(&["a"], A_OK, HarnessEnd::Signal(11), false, false);
        assert_eq!(
            r,
            vec!["harness killed by signal 11 after reporting all 1".to_owned()]
        );
    }

    #[test]
    fn protocol_violations_fail_a_complete_clean_run() {
        let nd = format!("{A_OK}{A_OK}{{\"probe\":\"zz\",\"outcome\":\"no_tv_data\"}}\nnoise\n");
        let r = reasons(&["a"], &nd, HarnessEnd::Code(0), false, false);
        assert_eq!(
            r,
            vec![
                "1 invalid harness record".to_owned(),
                "1 repeated harness record".to_owned(),
                "1 report-only extra record".to_owned(),
            ]
        );
    }

    #[test]
    fn exit_one_needs_explaining_and_a_cut_output_fails() {
        assert!(reasons(&["a"], A_OK, HarnessEnd::Code(1), false, true).is_empty());
        assert_eq!(
            reasons(&["a"], A_OK, HarnessEnd::Code(1), false, false),
            vec!["parity breaks".to_owned()]
        );
        assert_eq!(
            reasons(&["a"], A_OK, HarnessEnd::Code(0), true, false).len(),
            1
        );
        let backstop = reasons(&["a", "b"], A_OK, HarnessEnd::Backstop, false, false);
        assert_eq!(backstop.len(), 2, "{backstop:?}");
    }

    #[test]
    fn a_truncated_stream_is_stored_as_an_integrity_violation() {
        let a = assessed(&["a"], A_OK, HarnessEnd::Code(0), false);
        assert_eq!(stored_violations(&a, false), 0);
        let cut = assessed(&["a"], A_OK, HarnessEnd::Code(0), true);
        assert_eq!(stored_violations(&cut, false), 1);
        let dup = assessed(&["a"], &format!("{A_OK}{A_OK}"), HarnessEnd::Code(0), true);
        assert_eq!(stored_violations(&dup, false), 2);
    }

    #[test]
    fn contract_violations_are_stored_and_fail_the_run() {
        // A completion claim with a lifecycle breach.
        let nd = format!(
            "{{\"kind\":\"probe_end\",\"version\":1,\"probe\":\"a\"}}\n{A_OK}{{\"kind\":\"run_end\",\"version\":1,\"exit\":0}}\n"
        );
        let a = assessed(&["a"], &nd, HarnessEnd::Code(0), false);
        assert!(stored_violations(&a, false) >= 1);
        assert!(
            reasons(&["a"], &nd, HarnessEnd::Code(0), false, false)
                .iter()
                .any(|r| r.ends_with("contract violations") || r.ends_with("contract violation"))
        );
    }

    #[test]
    fn unstored_run_evidence_names_every_failed_file_and_counts_as_a_violation() {
        let root = scratch("run_streams");
        assert!(store_streams(&root, b"out", b"err").is_empty());
        // Both writes are attempted and both failures named, not just the first.
        let gone = root.join("no-such-dir");
        let errors = store_streams(&gone, b"out", b"err");
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("harness.stdout") && errors[1].contains("harness.stderr"));
        assert_eq!(
            stored_violations(&assessed(&["a"], A_OK, HarnessEnd::Code(0), false), true),
            1
        );
        assert!(evidence_reason(&errors).starts_with("evidence storage failed: "));
        assert!(evidence_note(true, &errors).starts_with(
            "isolation stopped before its first attempt on an evidence-storage failure: "
        ));
        let line = preserved_incomplete_line(&gone, &errors[1..]);
        assert!(
            line.starts_with(
                "artefacts preserved with incomplete evidence (failed: harness.stderr): "
            ),
            "{line}"
        );
    }

    #[test]
    fn attempts_tell_aborts_from_incomplete_and_protocol_invalid() {
        let classify = |ids: &[&str], nd: &str, end, cut| {
            isolate::classify_attempt(&assessed(ids, nd, end, cut))
        };
        assert_eq!(
            classify(&["a"], A_OK, HarnessEnd::Code(0), false),
            isolate::AttemptVerdict::Completed
        );
        assert_eq!(
            classify(&["a"], A_OK, HarnessEnd::Code(1), false),
            isolate::AttemptVerdict::Completed
        );
        assert_eq!(
            classify(&["a"], A_OK, HarnessEnd::Code(2), false),
            isolate::AttemptVerdict::Failed(isolate::Failure::Aborted)
        );
        assert_eq!(
            classify(&["a"], A_OK, HarnessEnd::Signal(11), false),
            isolate::AttemptVerdict::Failed(isolate::Failure::Aborted)
        );
        assert!(matches!(
            classify(&["a", "b"], A_OK, HarnessEnd::Code(0), false),
            isolate::AttemptVerdict::Failed(isolate::Failure::Incomplete(_))
        ));
        // Complete coverage with an extra line is not a completed attempt, and
        // a broken report is protocol-invalid even on an abnormal end - never
        // classified as a crash first.
        let extra = format!("{A_OK}{{\"probe\":\"zz\",\"outcome\":\"no_tv_data\"}}\n");
        assert!(matches!(
            classify(&["a"], &extra, HarnessEnd::Code(0), false),
            isolate::AttemptVerdict::Failed(isolate::Failure::ProtocolInvalid(_))
        ));
        assert!(matches!(
            classify(&["a"], &extra, HarnessEnd::Signal(11), false),
            isolate::AttemptVerdict::Failed(isolate::Failure::ProtocolInvalid(_))
        ));
        assert!(matches!(
            classify(&["a"], A_OK, HarnessEnd::Code(0), true),
            isolate::AttemptVerdict::Failed(isolate::Failure::ProtocolInvalid(w))
                if w.contains("cut short")
        ));
        // A harness_abort the harness attributed counts as a completed attempt.
        let crash = "{\"probe\":\"a\",\"outcome\":\"harness_abort\",\"error\":\"signal 11\"}\n";
        assert_eq!(
            classify(&["a"], crash, HarnessEnd::Code(1), false),
            isolate::AttemptVerdict::Completed
        );
        // A run_error stops diagnosis with the harness's own failure.
        let run_error =
            "{\"kind\":\"run_error\",\"version\":1,\"stage\":\"feed_load\",\"error\":\"oom\"}\n";
        assert_eq!(
            classify(&["a"], run_error, HarnessEnd::Code(2), false),
            isolate::AttemptVerdict::Stop(StopReason::HarnessFailure(
                "run_error at stage feed_load: oom".to_owned()
            ))
        );
    }

    #[test]
    fn an_unwritable_attempt_dir_stops_diagnosis_with_a_named_failure() {
        let root = scratch("attempt_streams");
        assert!(store_attempt_streams(&root, b"out", b"err").is_ok());
        assert_eq!(fs::read(root.join("harness.stderr")).unwrap(), b"err");
        let gone = root.join("no-such-dir");
        let err = store_attempt_streams(&gone, b"out", b"err").unwrap_err();
        assert!(
            matches!(&err, StopReason::EvidenceStorage(m) if m.contains("harness.stdout")),
            "{err:?}"
        );
    }

    #[test]
    fn record_unfinished_failed_ingest_returns_original_and_keeps_dir() {
        let root = scratch("unfinished_ingest_fails");
        // A regular file as the db's parent: opening the store must fail.
        let blocker = root.join("blocker");
        fs::write(&blocker, "file").unwrap();
        let db_path = blocker.join("runs.db");

        let parent = root.join("artefacts");
        let artefacts = ArtefactDir::allocate_at(&parent, ARTEFACT_TEST_ID, 4, false).unwrap();
        let sentinel = artefacts.path().join("sentinel.txt");
        fs::write(&sentinel, "evidence").unwrap();

        let start = start();
        let context = RunContext {
            profile: RustProfile::Dev,
            harness_args: Vec::new(),
        };
        let envelope = Envelope {
            run_id: 4,
            start: &start,
            selector: "{}",
            context: &context,
            gated: true,
        };
        let err = record_unfinished(
            &db_path,
            &envelope,
            artefacts,
            &RunVerdict::error("sentinel-error"),
            DevError::Config("sentinel-error".into()),
            None,
        );
        assert!(
            matches!(&err, DevError::Config(m) if m == "sentinel-error"),
            "{err:?}"
        );
        assert!(sentinel.exists(), "preserved dir lost its contents");
    }

    #[test]
    fn record_unfinished_success_stores_the_row_and_keeps_dir() {
        let root = scratch("unfinished_ingest_ok");
        let db_path = root.join("runs.db");
        let parent = root.join("artefacts");
        let artefacts = ArtefactDir::allocate_at(&parent, ARTEFACT_TEST_ID, 6, false).unwrap();
        let dir = artefacts.path().to_owned();
        fs::write(dir.join("manifest.json"), "{}").unwrap();

        let start = start();
        let context = RunContext {
            profile: RustProfile::Dev,
            harness_args: Vec::new(),
        };
        let envelope = Envelope {
            run_id: 6,
            start: &start,
            selector: "{}",
            context: &context,
            gated: true,
        };
        let err = record_unfinished(
            &db_path,
            &envelope,
            artefacts,
            &RunVerdict::error("harness failed to spawn"),
            DevError::Config("sentinel-error".into()),
            Some("boom"),
        );
        assert!(
            matches!(&err, DevError::Config(m) if m == "sentinel-error"),
            "{err:?}"
        );

        let db = CorpusDb::open_readonly(&db_path).unwrap();
        let row = db.run(6).unwrap().unwrap();
        assert_eq!(row.result, "error");
        assert_eq!(row.fail_reason.as_deref(), Some("harness failed to spawn"));
        assert_eq!(row.commit_sha.as_deref(), Some("abc123"));
        assert!(dir.join("manifest.json").exists(), "dir must be preserved");
    }
}
