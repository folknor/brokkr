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
//! Two correctness gates apply. First, **verification**: a missing path or a
//! hash mismatch aborts before anything is built. Second, the **per-probe
//! expected-disposition gate** ([`crate::piners::gate`]): each probe pins an
//! `expected` label in `pins.toml`, and any deviation - regression or
//! surprise improvement - fails the run, as does a probe never blessed.
//! `--no-gate` downgrades the gate to informational. The run also fails on
//! any non-zero harness exit the pins do not explain: exit 1 (break(s)) is
//! acceptable only when every break line is a selected probe pinned to that
//! break ([`crate::piners::gate::breaks_all_pinned`]), so a probe pinned to
//! `compile_fail` can pass; any other code, a signal, or a repeated record
//! always fails. `--bless` refuses to stamp anything from a run that failed
//! this way.
//!
//! `--reseed` (re-stamp hashes) and `--bless` (re-stamp dispositions) are the
//! two deliberate writers of `pins.toml`; see [`crate::piners::reseed`] and
//! [`crate::piners::bless`].

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use crate::artefacts::ArtefactDir;
use crate::config::DevConfig;
use crate::error::DevError;
use crate::lockfile::{self, LockContext};
use crate::output;
use crate::piners::corpus_db::{CorpusDb, RunRecord};
use crate::piners::manifest::Manifest;
use crate::piners::registry::{self, Registry};
use crate::piners::report;
use crate::piners::select::{self, SelectArgs};
use crate::ratatoskr::build;
use crate::resolve::corpus_runs_db_path;

/// Where corpus run dirs live, relative to the project root:
/// `<this>/corpus/run-<run id>/`.
const ARTEFACT_PARENT: &str = ".brokkr/piners";
const ARTEFACT_TEST_ID: &str = "corpus";

/// Pre-run runtime wall, in milliseconds (~270s). A selection whose estimated
/// runtime (the measured wall of a comparable covering run - see
/// [`CorpusDb::estimated_wall_ms`]) exceeds this is refused before building,
/// unless `--force`.
pub(crate) const RUNTIME_CEILING_MS: f64 = 270_000.0;

/// Wall-clock backstop on the harness subprocess. Not a budget - the
/// ~270s budget is the pre-run [`RUNTIME_CEILING_MS`] wall, and a run that
/// outgrows it is let finish - but a hang guard: a wedged harness would
/// otherwise hold the global lock until someone runs `brokkr kill`. Set far
/// above any real run (a `--force`d full pass included), so only a hang
/// reaches it; the run is then recorded as failed.
/// Shared with the measured path (`measured.rs`), which applies it per
/// iteration through `sidecar::DeadlineScope`.
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
    /// runs. The harness exit code still governs pass/fail.
    pub no_gate: bool,
    /// `Some(true)` = debug, `Some(false)` = release, `None` = default
    /// (debug for this command).
    pub profile_override: Option<bool>,
    pub keep_artefacts: bool,
    /// Bypass the pre-run runtime ceiling (the [`RUNTIME_CEILING_MS`] wall).
    pub force: bool,
    /// Extra flags forwarded verbatim to the harness binary after
    /// `--manifest <path>` (everything after a literal `--` on the CLI).
    /// CLI-conflicted with `--verify-only`/`--reseed`/`--bless`; recorded
    /// in the run row's selector so a perturbed run is never mistaken for
    /// a clean one.
    pub harness_args: Vec<String>,
}

/// Entry point for `brokkr corpus`.
#[allow(clippy::too_many_lines)] // linear orchestration: load, select, verify, build, run, report
pub fn corpus(
    project_root: &Path,
    dev_config: &DevConfig,
    args: &CorpusArgs,
) -> Result<(), DevError> {
    let cfg = dev_config.piners.clone().unwrap_or_default();

    // Reseed stamps pins.toml from the corpus filesystem - no registry to
    // load (it may not exist yet), no build, no harness. Route early.
    if args.reseed {
        return crate::piners::reseed::run(project_root, &cfg, args);
    }

    // The lock comes before anything is read. Reseed and bless write
    // pins.toml under it, so a run that loaded and verified first could wait
    // out a reseed and then run - and bless - against pins and hashes that
    // are no longer the file's. Verify-only too: it would report drift for
    // bytes a reseed had just adopted. Held to the end, through ingest and
    // the bless write.
    let project_root_str = project_root.display().to_string();
    let lock = lockfile::acquire(&LockContext {
        project: "piners",
        command: "corpus",
        project_root: &project_root_str,
    })?;
    let _sigterm = crate::shutdown::SigtermGuard::install();

    let registry_dir = project_root.join(cfg.registry_dir());
    let corpus_root = project_root.join(cfg.corpus_root());
    let mut registry = Registry::load(&registry_dir, &corpus_root)?;
    registry.lint()?;

    let sel_args = SelectArgs {
        keywords: args.keywords.clone(),
        probe: args.probe.clone(),
        all: args.all,
        verify_only: args.verify_only,
    };
    let ids = select::resolve(&registry, &sel_args)?;

    // Hard correctness gate: verify every selected pin before running.
    output::corpus_msg(&format!(
        "verifying {} probe(s) against {}",
        ids.len(),
        corpus_root.display()
    ));
    let mut verified = Vec::with_capacity(ids.len());
    for id in &ids {
        let (pin, config) = registry.pins.get(id).zip(registry.config(id)).ok_or_else(|| {
            DevError::Config(format!("piners: internal: selected id '{id}' absent from pins"))
        })?;
        verified.push(registry::verify_probe(id, pin, config, &corpus_root, project_root)?);
    }
    let feed_count = verify_selected_feeds(&ids, &registry, &corpus_root, project_root)?;
    let file_count = registry::verify_harness_files(&registry, &corpus_root, project_root)?;

    if args.verify_only {
        output::corpus_msg(&format!(
            "verify-only: {} probe(s) + {feed_count} feed group(s) + {file_count} harness \
             file(s) OK",
            verified.len()
        ));
        return Ok(());
    }

    // Required only once something is to be built: verification needs no
    // harness, so a registry-only checkout can still `--verify-only`.
    let harness_cfg = cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(
            "corpus: no [piners.harness] section in brokkr.toml. \
             Declare `[piners.harness]` with `package = \"<crate>\"` \
             (and optional `binary`, `features`, `debug`)."
                .into(),
        )
    })?;

    // Default profile is debug: parity is opt-level-independent, and the
    // debug build keeps the edit/run loop inside the cache-warm window.
    // Resolved before the ceiling, which only estimates from same-profile runs.
    //
    // Pre-run runtime wall: now that the selection has verified, refuse it if
    // its estimated runtime (the measured wall of a comparable covering run)
    // blows the ~270s ceiling, unless --force. Placed after verification so a
    // submodule/hash drift surfaces even on an over-budget selection;
    // verify_only has already returned, so it's naturally exempt.
    let debug = args.profile_override.unwrap_or_else(|| {
        cfg.harness
            .as_ref()
            .and_then(|h| h.debug)
            .unwrap_or(true)
    });
    if !args.force {
        enforce_runtime_ceiling(project_root, &ids, debug)?;
    }
    // Bring the run store to this binary's schema before anything is built or
    // run. The ceiling above does it on its way to a read, but `--force` skips
    // the ceiling, and a store that refuses to migrate would otherwise be
    // found only at ingest - after the whole harness run it was meant to keep.
    let corpus_db_path = corpus_runs_db_path(project_root);
    if corpus_db_path.exists() {
        drop(CorpusDb::open(&corpus_db_path)?);
    }

    // The run's provenance, taken before the build: what HEAD was, whether
    // the tree carried uncommitted work, and when the run began.
    let start = RunStart::capture(project_root);

    let built = build::build_for_harness(
        project_root,
        harness_cfg,
        debug,
        Some(&|pid| lock.set_child_pid(pid)),
        Some(&|| lock.clear_child_pid()),
        true,
    )?;
    output::corpus_msg(&format!(
        "harness build ok (features={}, binary={})",
        built.features_label,
        built.binary.display()
    ));

    // One number per run: the dir is `run-<id>` and the row is stored under
    // the same id. Reserved under the lock, past both every stored run and
    // every dir on disk (an unrecorded run - a SIGKILL, a failed ingest -
    // leaves its dir, which holds the number until a clean removes it).
    let artefact_parent = project_root.join(ARTEFACT_PARENT);
    let run_id = reserve_run_id(&corpus_db_path, &artefact_parent.join(ARTEFACT_TEST_ID))?;
    let artefacts =
        ArtefactDir::allocate_at(&artefact_parent, ARTEFACT_TEST_ID, run_id, args.keep_artefacts)?;
    output::corpus_msg(&format!("run {run_id} -> {}", artefacts.path().display()));
    let selector = selector_json(args, &ids, debug);
    let envelope = Envelope {
        run_id,
        start: &start,
        selector: &selector,
        gated: !args.no_gate && !args.bless,
    };

    let manifest_path = artefacts.path().join("manifest.json");
    let manifest = Manifest::build(&corpus_root, &verified, &registry);
    if let Err(e) = manifest.write(&manifest_path) {
        return Err(record_unfinished(&corpus_db_path, &envelope, artefacts, e, "error", None));
    }
    output::corpus_msg(&format!(
        "manifest: {} probe(s) -> {}",
        verified.len(),
        manifest_path.display()
    ));

    // The ~270s budget is enforced as a pre-run wall (above), not mid-run:
    // once we commit to a run we let it finish, bounded only by the hang
    // backstop. PID is tracked so `brokkr kill` reaches the harness.
    let binary_str = built.binary.display().to_string();
    let manifest_str = manifest_path.display().to_string();
    let artefact_str = artefacts.path().display().to_string();
    let bin_dir_str = built.bin_dir.display().to_string();
    let env_pairs: Vec<(&str, &str)> = vec![
        ("BROKKR_HARNESS_ARTEFACT_DIR", &artefact_str),
        ("BROKKR_TEST_BIN_DIR", &bin_dir_str),
    ];

    let mut harness_argv: Vec<&str> = vec!["--manifest", &manifest_str];
    harness_argv.extend(args.harness_args.iter().map(String::as_str));
    if !args.harness_args.is_empty() {
        output::corpus_msg(&format!(
            "forwarding to harness: {}",
            args.harness_args.join(" ")
        ));
    }

    let capture = match output::run_captured_with_env_and_deadline(
        &binary_str,
        &harness_argv,
        project_root,
        &env_pairs,
        HARNESS_HANG_BACKSTOP,
        Some(&|pid| lock.set_child_pid(pid)),
        true,
    ) {
        Ok(c) => c,
        Err(DevError::Interrupted) => {
            lock.clear_child_pid();
            return Err(record_unfinished(
                &corpus_db_path,
                &envelope,
                artefacts,
                DevError::Interrupted,
                "interrupted",
                None,
            ));
        }
        Err(e) => {
            lock.clear_child_pid();
            let msg = format!("failed to spawn {}: {e}\n", built.binary.display());
            std::fs::write(artefacts.path().join("spawn-error.txt"), &msg).ok();
            // Recorded so it surfaces in `brokkr corpus-results`; the dir is
            // still preserved - a spawn failure is exactly when on-disk
            // forensics matter most, and the row is a convenience index.
            return Err(record_unfinished(
                &corpus_db_path,
                &envelope,
                artefacts,
                e,
                "fail",
                Some(("harness failed to spawn", &msg)),
            ));
        }
    };
    lock.clear_child_pid();

    let killed_on_backstop = capture.killed_on_deadline;
    let captured = capture.captured;
    // Keep stdout/stderr on disk until ingest commits - the pre-ingest safety
    // net if anything panics between here and the DB write.
    std::fs::write(artefacts.path().join("harness.stdout"), &captured.stdout).ok();
    std::fs::write(artefacts.path().join("harness.stderr"), &captured.stderr).ok();

    let mut report = report::parse(&captured.stdout);
    // One record per key from here on, for the gate, bless and ingest alike;
    // a repeat is a harness contract violation that fails the run below.
    let duplicates = report.take_duplicates();
    if !duplicates.is_empty() {
        output::corpus_msg(&format!(
            "harness emitted {} repeated record(s), kept the last of each: {}",
            duplicates.len(),
            duplicates.join(", ")
        ));
    }

    let elapsed_ms = captured.elapsed.as_millis();
    let harness_code = captured.status.code();

    // Evaluate the gate up front. It drives the pass/fail decision, feeds the
    // gate_miss table (selected probes the harness emitted no line for), and
    // selects which per-probe lines render prints: a probe sitting exactly on
    // its pin is folded into a count, so the lines that survive are the
    // deviations. Bless never gates - it ignores the verdict - but the diffs
    // still record and still drive the render filter (what differs from the
    // pins about to be re-stamped).
    let gate_diffs = crate::piners::gate::evaluate(&ids, &registry, &report);
    let gate_blocks = !args.bless && !args.no_gate && !gate_diffs.is_empty();

    // Is the harness exit acceptable? 0 always is. 1 ("break(s)") is when
    // every break is one the pins expect - or, for bless, when the report
    // actually carries the breaks, since recording them is bless's job. A
    // repeated record, 2, any other code, a signal, or the hang backstop is
    // never acceptable.
    let harness_ok = duplicates.is_empty()
        && !killed_on_backstop
        && match harness_code {
            Some(0) => true,
            Some(1) if args.bless => crate::piners::gate::report_has_break(&report),
            Some(1) => crate::piners::gate::breaks_all_pinned(&ids, &registry, &report),
            _ => false,
        };
    let run_pass = harness_ok && !gate_blocks;

    // Render the body now that the deviation set is known. Probes matching
    // their pin collapse to a single count; the survivors are worth reading.
    let deviating: HashSet<&str> = gate_diffs.iter().map(|d| d.probe.as_str()).collect();
    report::render(&report, &deviating);

    // One-line failure classification, mirrored into the DB so a discarded run
    // dir loses nothing. Harness breaks rank ahead of gate deviations.
    let fail_reason: Option<String> = if run_pass {
        None
    } else if harness_ok {
        Some(format!("{} gate deviation(s)", gate_diffs.len()))
    } else if killed_on_backstop {
        Some(format!(
            "harness killed at the {}s hang backstop",
            HARNESS_HANG_BACKSTOP.as_secs()
        ))
    } else if !duplicates.is_empty() {
        Some(format!("{} repeated harness record(s)", duplicates.len()))
    } else {
        Some(match harness_code {
            Some(1) => "parity break(s)".to_owned(),
            Some(2) => "harness error".to_owned(),
            Some(c) => format!("harness exit={c}"),
            None => "harness killed by signal".to_owned(),
        })
    };

    // Persist the run BEFORE any pins mutation or finalize, using the pinned
    // expectations as they stand at run time. An ingest failure preserves the
    // dir (the on-disk stdout is the evidence) and propagates.
    let expected: BTreeMap<String, Option<String>> = ids
        .iter()
        .map(|id| {
            let exp = registry.pins.get(id).and_then(|p| p.expected.clone());
            (id.clone(), exp)
        })
        .collect();
    let stderr_text = String::from_utf8_lossy(&captured.stderr);
    let record = envelope.record(
        if run_pass { "pass" } else { "fail" },
        fail_reason.as_deref(),
        harness_code,
        &stderr_text,
        // brokkr's own measurement of the whole harness subprocess - the real
        // wall the ceiling estimates future runs from.
        Some(elapsed_ms as f64),
    );
    // From here the run is recorded exactly once: a later failure (a bless
    // refusing a changed pins.toml, a finalize error) is the command's error,
    // not a second row under this id.
    if let Err(e) = ingest_run(&corpus_db_path, &record, &report, &expected, &gate_diffs) {
        output::corpus_msg(&format!(
            "warning: failed to persist run to {}: {e}",
            corpus_db_path.display()
        ));
        artefacts.finalize_failure();
        return Err(e);
    }

    // Bless: stamp current dispositions into pins.toml. The run is already
    // persisted, so the dir drops like any other (unless --keep-artefacts).
    // A harness that failed (see harness_ok) blesses nothing: its surviving
    // lines are whatever it emitted before it died, not the dispositions.
    if args.bless {
        if !harness_ok {
            artefacts.finalize_success()?;
            let reason = fail_reason.unwrap_or_else(|| "harness failed".to_owned());
            output::corpus_msg(&format!(
                "FAIL: {reason} in {elapsed_ms}ms - nothing blessed, pins.toml untouched \
                 (recorded; see `brokkr corpus-results`)"
            ));
            return Err(DevError::ExitCode(1));
        }
        let pins_path = registry_dir.join("pins.toml");
        crate::piners::bless::apply(&pins_path, &mut registry, &report, &ids)?;
        artefacts.finalize_success()?;
        return Ok(());
    }

    if !gate_diffs.is_empty() {
        crate::piners::gate::render_diffs(&gate_diffs);
    }

    // Data is durable in the DB; the dir is always dropped (unless
    // --keep-artefacts). No preserve-on-failure - `brokkr corpus-results` is the
    // home for the run's drill-down now.
    if run_pass {
        output::corpus_msg(&format!("PASS in {elapsed_ms}ms"));
        artefacts.finalize_success()?;
        Ok(())
    } else {
        artefacts.finalize_success()?;
        let reason = fail_reason.unwrap_or_else(|| "fail".to_owned());
        output::corpus_msg(&format!(
            "FAIL: {reason} in {elapsed_ms}ms (recorded; see `brokkr corpus-results`)"
        ));
        Err(DevError::ExitCode(1))
    }
}

/// Hard-verify the feed groups referenced by the selection, the feed leg of
/// the content gate: the feed is part of each probe's oracle identity (the
/// same script and oracle against the wrong feed gates as a fake
/// regression), so its files get the same hash-or-abort policy as the probe
/// files. Returns the
/// number of groups verified. Shared by the parity and measured paths.
pub(crate) fn verify_selected_feeds(
    ids: &[String],
    registry: &Registry,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<usize, DevError> {
    let referenced: std::collections::BTreeSet<String> = ids
        .iter()
        .filter_map(|id| registry.config(id).and_then(|c| c.feed))
        .collect();
    for name in &referenced {
        // lint guarantees the group exists; the ok_or_else is belt-and-braces.
        let group = registry.feeds.get(name).ok_or_else(|| {
            DevError::Config(format!(
                "piners: internal: referenced feed group '{name}' absent from [feeds]"
            ))
        })?;
        registry::verify_feed_group(name, group, corpus_root, project_root)?;
    }
    Ok(referenced.len())
}

/// Refuse a selection projected to exceed [`RUNTIME_CEILING_MS`]. The estimate
/// is the measured whole-run wall of the most recent comparable run whose
/// selection was a superset of `ids` (see [`CorpusDb::estimated_wall_ms`] for
/// what counts as comparable) - a real wall, not the sum of the harness's
/// overlapping per-probe runtimes. With no covering run recorded (a fresh DB,
/// or a selection no prior run superset-covers) there is no measured basis, so
/// the run proceeds. Opens the DB for reading (an older `runs.db` is migrated
/// first - see [`CorpusDb::open_readonly`]).
fn enforce_runtime_ceiling(project_root: &Path, ids: &[String], debug: bool) -> Result<(), DevError> {
    let db_path = corpus_runs_db_path(project_root);
    if !db_path.exists() {
        return Ok(());
    }
    let Some(est_ms) = CorpusDb::open_readonly(&db_path)?.estimated_wall_ms(ids, debug)? else {
        return Ok(());
    };
    if est_ms > RUNTIME_CEILING_MS {
        return Err(DevError::Preflight(vec![format!(
            "corpus: estimated runtime {:.0}s for {} probe(s) exceeds the {:.0}s ceiling \
             (measured wall of the most recent run covering this selection). \
             Re-run with --force to override.",
            est_ms / 1000.0,
            ids.len(),
            RUNTIME_CEILING_MS / 1000.0,
        )]));
    }
    Ok(())
}

/// Build the `selector` JSON stored on the run row: the resolved probe ids
/// plus the raw selection flags - enough to group by and to reproduce.
/// Forwarded harness flags and the build profile are part of the run's
/// identity (both change what the harness does and how long it takes), so
/// they persist here too; the runtime ceiling only estimates from runs that
/// match on both.
fn selector_json(args: &CorpusArgs, ids: &[String], debug: bool) -> String {
    serde_json::json!({
        "all": args.all,
        "keywords": args.keywords,
        "probe": args.probe,
        "bless": args.bless,
        "harness_args": args.harness_args,
        "debug": debug,
        "ids": ids,
    })
    .to_string()
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
            started_at: crate::piners::lint::now_sqlite_utc(),
            commit_sha: crate::git::resolve_commit(project_root, "HEAD").ok().map(|c| c.full),
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
    gated: bool,
}

impl<'a> Envelope<'a> {
    fn record(
        &self,
        result: &'a str,
        fail_reason: Option<&'a str>,
        harness_exit_code: Option<i32>,
        stderr: &'a str,
        wall_ms: Option<f64>,
    ) -> RunRecord<'a> {
        RunRecord {
            run_id: Some(self.run_id),
            started_at: Some(&self.start.started_at),
            commit_sha: self.start.commit_sha.as_deref(),
            dirty: self.start.dirty,
            selector: self.selector,
            gated: self.gated,
            result,
            fail_reason,
            harness_exit_code,
            stderr,
            wall_ms,
        }
    }
}

/// The next run id: one past the highest of every stored run and every
/// `run-N` dir on disk. Called under the lock, so no other corpus run can
/// take the same number between this read and the dir's creation.
fn reserve_run_id(db_path: &Path, run_dirs: &Path) -> Result<i64, DevError> {
    let stored = if db_path.exists() {
        CorpusDb::open_readonly(db_path)?.latest_run_id()?.unwrap_or(0)
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
/// record is reported beside it, never in its place.
fn record_unfinished(
    db_path: &Path,
    envelope: &Envelope<'_>,
    artefacts: ArtefactDir,
    original: DevError,
    result: &str,
    detail: Option<(&str, &str)>,
) -> DevError {
    let reason = match detail {
        Some((reason, _)) => reason.to_owned(),
        None => original.to_string(),
    };
    let stderr = detail.map_or("", |(_, stderr)| stderr);
    // Never ran -> no measured wall, no exit code.
    let record = envelope.record(result, Some(&reason), None, stderr, None);
    if let Err(e) = ingest_run(db_path, &record, &report::HarnessReport::default(), &BTreeMap::new(), &[]) {
        output::corpus_msg(&format!(
            "warning: run {} could not be recorded in {}: {e}",
            envelope.run_id,
            db_path.display()
        ));
    }
    output::corpus_msg(&format!("artefacts preserved: {}", artefacts.path().display()));
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
