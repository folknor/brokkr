//! `brokkr corpus` - the piners parity-corpus runner.
//!
//! Resolves a keyword-selected slice of the pinned corpus, hard-verifies
//! every selected probe's `strategy.pine` + `tv_trades.csv` against the
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
//! `--no-gate` downgrades the gate to informational. The run also fails on a
//! real break (`compile_fail`/`runtime_fail`) or any non-zero harness exit;
//! the exit code stays authoritative for breaks.
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
/// `<this>/corpus/run-N/`.
const ARTEFACT_PARENT: &str = ".brokkr/piners";

/// Pre-run runtime wall, in milliseconds (~270s). A selection whose estimated
/// runtime (the sum over selected probes of each probe's most recent recorded
/// `runtime_ms`) exceeds this is refused before building, unless `--force`.
pub(crate) const RUNTIME_CEILING_MS: f64 = 270_000.0;

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

    let registry_dir = project_root.join(cfg.registry_dir());
    let mut registry = Registry::load(&registry_dir)?;
    registry.lint()?;

    let sel_args = SelectArgs {
        keywords: args.keywords.clone(),
        probe: args.probe.clone(),
        all: args.all,
        verify_only: args.verify_only,
    };
    let ids = select::resolve(&registry, &sel_args)?;

    // Hard correctness gate: verify every selected pin before running.
    let corpus_root = project_root.join(cfg.corpus_root());
    output::corpus_msg(&format!(
        "verifying {} probe(s) against {}",
        ids.len(),
        corpus_root.display()
    ));
    let mut verified = Vec::with_capacity(ids.len());
    for id in &ids {
        let pin = registry.pins.get(id).ok_or_else(|| {
            DevError::Config(format!("piners: internal: selected id '{id}' absent from pins"))
        })?;
        verified.push(registry::verify_probe(id, pin, &corpus_root, project_root)?);
    }
    let feed_count = verify_selected_feeds(&ids, &registry, &corpus_root, project_root)?;

    if args.verify_only {
        output::corpus_msg(&format!(
            "verify-only: {} probe(s) + {feed_count} feed group(s) OK",
            verified.len()
        ));
        return Ok(());
    }

    // Pre-run runtime wall: now that the selection has verified, refuse it if
    // its estimated runtime (sum of each probe's most recent recorded runtime)
    // blows the ~270s ceiling, unless --force. Placed after verification so a
    // submodule/hash drift surfaces even on an over-budget selection;
    // verify_only has already returned, so it's naturally exempt.
    if !args.force {
        enforce_runtime_ceiling(project_root, &ids)?;
    }

    let harness_cfg = cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(
            "corpus: no [piners.harness] section in brokkr.toml. \
             Declare `[piners.harness]` with `package = \"<crate>\"` \
             (and optional `binary`, `features`, `debug`)."
                .into(),
        )
    })?;

    let project_root_str = project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "piners",
        command: "corpus",
        project_root: &project_root_str,
    })?;
    let _sigterm = crate::shutdown::SigtermGuard::install();

    // Default profile is debug: parity is opt-level-independent, and the
    // debug build keeps the edit/run loop inside the cache-warm window.
    let debug = args
        .profile_override
        .unwrap_or_else(|| harness_cfg.debug.unwrap_or(true));
    let built = build::build_for_harness(
        project_root,
        harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true,
    )?;
    output::corpus_msg(&format!(
        "harness build ok (features={}, binary={})",
        built.features_label,
        built.binary.display()
    ));

    let artefact_parent = project_root.join(ARTEFACT_PARENT);
    let artefacts = ArtefactDir::allocate(&artefact_parent, "corpus", args.keep_artefacts)?;
    let corpus_db_path = corpus_runs_db_path(project_root);

    let manifest_path = artefacts.path().join("manifest.json");
    let manifest = Manifest::build(&corpus_root, &verified, &registry);
    manifest.write(&manifest_path)?;
    output::corpus_msg(&format!(
        "manifest: {} probe(s) -> {}",
        verified.len(),
        manifest_path.display()
    ));

    // The ~270s budget is enforced as a pre-run wall (above), not mid-run:
    // once we commit to a run we let it finish. PID is tracked so `brokkr
    // kill` reaches the harness.
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
        Duration::MAX,
        Some(&|pid| _lock.set_child_pid(pid)),
        true,
    ) {
        Ok(c) => c,
        Err(DevError::Interrupted) => {
            _lock.clear_child_pid();
            artefacts.finalize_failure();
            return Err(DevError::Interrupted);
        }
        Err(e) => {
            let msg = format!("failed to spawn {}: {e}\n", built.binary.display());
            std::fs::write(artefacts.path().join("spawn-error.txt"), &msg).ok();
            // Record the failed run so it surfaces in `brokkr corpus-results`, then
            // still preserve the dir - a spawn failure is exactly when on-disk
            // forensics matter most, and the DB row is a convenience index.
            let selector = selector_json(args, &ids);
            let record = RunRecord {
                selector: &selector,
                gated: !args.no_gate,
                result: "fail",
                fail_reason: Some("harness failed to spawn"),
                harness_exit_code: None,
                stderr: &msg,
                // Never ran -> no measured wall.
                wall_ms: None,
            };
            ingest_run(
                &corpus_db_path,
                &record,
                &report::HarnessReport::default(),
                &BTreeMap::new(),
                &[],
            )
            .ok();
            artefacts.finalize_failure();
            return Err(e);
        }
    };
    _lock.clear_child_pid();

    let captured = capture.captured;
    // Keep stdout/stderr on disk until ingest commits - the pre-ingest safety
    // net if anything panics between here and the DB write.
    std::fs::write(artefacts.path().join("harness.stdout"), &captured.stdout).ok();
    std::fs::write(artefacts.path().join("harness.stderr"), &captured.stderr).ok();

    let report = report::parse(&captured.stdout);

    let elapsed_ms = captured.elapsed.as_millis();
    let harness_code = captured.status.code();
    let harness_ok = harness_code == Some(0);

    // Evaluate the gate up front. It drives the pass/fail decision, feeds the
    // gate_miss table (selected probes the harness emitted no line for), and
    // selects which per-probe lines render prints: a probe sitting exactly on
    // its pin is folded into a count, so the lines that survive are the
    // deviations. Bless never gates - it ignores the verdict - but the diffs
    // still record and still drive the render filter (what differs from the
    // pins about to be re-stamped).
    let gate_diffs = crate::piners::gate::evaluate(&ids, &registry, &report);
    let gate_blocks = !args.bless && !args.no_gate && !gate_diffs.is_empty();
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
    let selector = selector_json(args, &ids);
    let stderr_text = String::from_utf8_lossy(&captured.stderr);
    let record = RunRecord {
        selector: &selector,
        gated: !args.no_gate,
        result: if run_pass { "pass" } else { "fail" },
        fail_reason: fail_reason.as_deref(),
        harness_exit_code: harness_code,
        stderr: &stderr_text,
        // brokkr's own measurement of the whole harness subprocess - the real
        // wall the ceiling estimates future runs from.
        wall_ms: Some(elapsed_ms as f64),
    };
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
    if args.bless {
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
/// the content gate: the feed is part of each probe's oracle identity (same
/// pine + csv against the wrong feed gates as a fake regression), so its
/// files get the same hash-or-abort policy as `pine`/`csv`. Returns the
/// number of groups verified. Shared by the parity and measured paths.
pub(crate) fn verify_selected_feeds(
    ids: &[String],
    registry: &Registry,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<usize, DevError> {
    let referenced: std::collections::BTreeSet<&str> = ids
        .iter()
        .filter_map(|id| registry.pins.get(id).and_then(|p| p.feed.as_deref()))
        .collect();
    for name in &referenced {
        // lint guarantees the group exists; the ok_or_else is belt-and-braces.
        let group = registry.feeds.get(*name).ok_or_else(|| {
            DevError::Config(format!(
                "piners: internal: referenced feed group '{name}' absent from [feeds]"
            ))
        })?;
        registry::verify_feed_group(name, group, corpus_root, project_root)?;
    }
    Ok(referenced.len())
}

/// Refuse a selection projected to exceed [`RUNTIME_CEILING_MS`]. The estimate
/// is the measured whole-run wall of the most recent run whose selection was a
/// superset of `ids` (see [`CorpusDb::estimated_wall_ms`]) - a real wall, not
/// the sum of the harness's overlapping per-probe runtimes. With no covering
/// run recorded (a fresh DB, or a selection no prior run superset-covers) there
/// is no measured basis, so the run proceeds. Read-only DB open - never writes.
fn enforce_runtime_ceiling(project_root: &Path, ids: &[String]) -> Result<(), DevError> {
    let db_path = corpus_runs_db_path(project_root);
    if !db_path.exists() {
        return Ok(());
    }
    let Some(est_ms) = CorpusDb::open_readonly(&db_path)?.estimated_wall_ms(ids)? else {
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
/// Forwarded harness flags are part of the run's identity (they perturb
/// harness behavior), so they persist here too.
fn selector_json(args: &CorpusArgs, ids: &[String]) -> String {
    serde_json::json!({
        "all": args.all,
        "keywords": args.keywords,
        "probe": args.probe,
        "bless": args.bless,
        "harness_args": args.harness_args,
        "ids": ids,
    })
    .to_string()
}

/// Open the corpus DB and persist one run. Separate so both the spawn-error
/// path and the normal path share the open+record sequence.
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
