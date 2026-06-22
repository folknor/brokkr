//! `brokkr lint-corpus` - the piners differential-lint runner.
//!
//! Resolves a keyword-selected slice of the pinned lint corpus, hard-verifies
//! every selected snippet against `corpus_root`, builds the piners validator
//! from the dirty tree once, then for each probe runs **piners** (`<bin>
//! validate <file> --format json`) and **pine-lint** offline, diffs their
//! diagnostics on a `(line, col, severity)` grain, and classifies an
//! agreement disposition ([`crate::piners::lint::diff`]). The per-probe
//! expected-disposition gate ([`crate::piners::lint::registry`] pins
//! `expected`) fails the run on any deviation; `--no-gate` downgrades it.
//!
//! `--reanchor` is the periodic network mode: it drives `pine-lint --tv` over
//! the selection and re-stamps each probe's TV fingerprint into `lints.toml`.
//! `--bless` re-stamps `expected` from the current dispositions. Both write
//! the registry via [`crate::piners::lint::lints_write`].

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use crate::config::{DevConfig, HarnessConfig};
use crate::error::DevError;
use crate::lockfile::{self, LockContext};
use crate::output::{self, CapturedOutput};
use crate::piners::lint::db::{LintDb, RunMeta};
use crate::piners::lint::diff::classify;
use crate::piners::lint::registry::{self, LintPin, LintRegistry, TvDiag};
use crate::piners::lint::select::{self, SelectArgs};
use crate::piners::lint::{self, now_rfc3339, validators, DiagSet, ProbeResult};
use crate::ratatoskr::build;
use crate::resolve::lint_runs_db_path;

/// A PID-tracked captured-subprocess runner (`program`, `argv`) -> output.
/// Factored out so [`reanchor`] can borrow it without a clippy-flagged
/// closure type in its signature.
type RunFn<'a> = dyn Fn(&str, &[&str]) -> Result<CapturedOutput, DevError> + 'a;

/// Flags lifted off the `LintCorpus` CLI command.
#[derive(Debug, Default)]
pub struct LintArgs {
    pub keywords: Vec<String>,
    pub probe: Vec<String>,
    pub all: bool,
    pub verify_only: bool,
    /// Refresh the TV anchor (`pine-lint --tv`) for the selection - the
    /// periodic, network-touching registry writer. Conflicts with the run
    /// writers on the CLI.
    pub reanchor: bool,
    /// Run the selection, then stamp each probe's current disposition into
    /// `expected`.
    pub bless: bool,
    /// Report the gate diff but never fail on it.
    pub no_gate: bool,
    /// `Some(true)` = debug, `Some(false)` = release, `None` = default
    /// (debug for this command).
    pub profile_override: Option<bool>,
}

/// Entry point for `brokkr lint-corpus`.
#[allow(clippy::too_many_lines)] // linear orchestration: load, select, verify, build, run, report
pub fn lint_corpus(
    project_root: &Path,
    dev_config: &DevConfig,
    args: &LintArgs,
) -> Result<(), DevError> {
    let piners_cfg = dev_config.piners.clone().unwrap_or_default();
    let lint_cfg = piners_cfg.lint.as_ref().ok_or_else(|| {
        DevError::Config(
            "lint-corpus: no [piners.lint] section in brokkr.toml. Declare \
             `[piners.lint]` with `package = \"<crate>\"` (and optional `binary`, \
             `subcommand`, `features`, `registry_dir`, `pine_lint_bin`)."
                .into(),
        )
    })?;

    let registry_dir = project_root.join(lint_cfg.registry_dir());
    let mut registry = LintRegistry::load(&registry_dir)?;
    registry.lint()?;

    let sel = SelectArgs {
        keywords: args.keywords.clone(),
        probe: args.probe.clone(),
        all: args.all,
        all_universe: args.verify_only || (args.reanchor && args.all),
    };
    let ids = select::resolve(&registry, &sel)?;

    // Hard correctness gate: verify every selected snippet, recording its
    // absolute path for the validator runners.
    let corpus_root = project_root.join(piners_cfg.corpus_root());
    output::lint_msg(&format!(
        "verifying {} snippet(s) against {}",
        ids.len(),
        corpus_root.display()
    ));
    let mut abs_paths: BTreeMap<String, String> = BTreeMap::new();
    for id in &ids {
        let pin = registry.pins.get(id).ok_or_else(|| {
            DevError::Config(format!("piners lint: internal: selected id '{id}' absent from pins"))
        })?;
        let abs = registry::verify_probe(id, pin, &corpus_root, project_root)?;
        abs_paths.insert(id.clone(), abs.display().to_string());
    }

    if args.verify_only {
        output::lint_msg(&format!("verify-only: {} snippet(s) OK", ids.len()));
        return Ok(());
    }

    let project_root_str = project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "piners",
        command: if args.reanchor { "lint-reanchor" } else { "lint-corpus" },
        project_root: &project_root_str,
    })?;
    let _sigterm = crate::shutdown::SigtermGuard::install();

    // A captured subprocess run, PID-tracked so `brokkr kill` reaches it.
    let run = |program: &str, argv: &[&str]| -> Result<CapturedOutput, DevError> {
        let r = output::run_captured_with_env_and_deadline(
            program,
            argv,
            project_root,
            &[],
            Duration::MAX,
            Some(&|pid| _lock.set_child_pid(pid)),
            false,
        )?;
        _lock.clear_child_pid();
        Ok(r.captured)
    };

    // --reanchor: refresh the TV fingerprint via `pine-lint --tv`, write the
    // registry, and return. No validator build, no run store.
    if args.reanchor {
        return reanchor(&registry_dir, &mut registry, &ids, &abs_paths, lint_cfg.pine_lint_bin(), &run);
    }

    // Build the piners validator from the dirty tree (debug by default - lint
    // is opt-level-independent and the fast build keeps the loop cache-warm).
    let harness_cfg = HarnessConfig {
        package: lint_cfg.package.clone(),
        binary: lint_cfg.binary.clone(),
        features: lint_cfg.features.clone(),
        debug: lint_cfg.debug,
    };
    let debug = args
        .profile_override
        .unwrap_or_else(|| lint_cfg.debug.unwrap_or(true));
    let built = build::build_for_harness(
        project_root,
        &harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true,
    )?;
    let validator = built.binary.display().to_string();
    let subcommand = lint_cfg.subcommand().to_owned();
    let pine_lint = lint_cfg.pine_lint_bin().to_owned();
    output::lint_msg(&format!(
        "validator build ok (features={}, binary={})",
        built.features_label,
        built.binary.display()
    ));

    // Run both validators on each probe and classify.
    let mut results: Vec<ProbeResult> = Vec::with_capacity(ids.len());
    for id in &ids {
        let abs = &abs_paths[id];
        let pin = &registry.pins[id];

        let piners_set = match run(&validator, &[&subcommand, abs, "--format", "json"]) {
            Ok(cap) => validators::parse_piners(&cap.stdout),
            Err(e) => Err(format!("piners validate failed to spawn: {e}")),
        };
        let lint_set = match run(&pine_lint, &[abs]) {
            Ok(cap) => validators::parse_pine_lint(&cap.stdout),
            Err(e) => Err(format!("pine-lint failed to spawn: {e}")),
        };

        let outcome = classify(
            piners_set.as_ref().map_err(String::as_str),
            lint_set.as_ref().map_err(String::as_str),
        );
        results.push(build_result(id, pin, &outcome, piners_set.as_ref().ok()));
    }

    render(&results, args.no_gate);

    // Persist the run before any registry mutation.
    let tool_error = results
        .iter()
        .any(|r| r.disposition == "piners_error" || r.disposition == "lint_error");
    let deviations = results.iter().filter(|r| !r.gate_ok).count();
    let gate_blocks = !args.bless && !args.no_gate && deviations > 0;
    let run_pass = !tool_error && !gate_blocks;
    let fail_reason: Option<String> = if run_pass {
        None
    } else if tool_error {
        let n = results
            .iter()
            .filter(|r| r.disposition == "piners_error" || r.disposition == "lint_error")
            .count();
        Some(format!("{n} validator error(s)"))
    } else {
        Some(format!("{deviations} gate deviation(s)"))
    };

    let selector = selector_json(args, &ids);
    let meta = RunMeta {
        started_at: &now_rfc3339(),
        selector: &selector,
        gated: !args.no_gate,
        result: if run_pass { "pass" } else { "fail" },
        fail_reason: fail_reason.as_deref(),
        probe_count: results.len(),
        stderr: "",
    };
    let db_path = lint_runs_db_path(project_root);
    let mut db = LintDb::open(&db_path)?;
    db.record_run(&meta, &results)?;

    // --bless: stamp current dispositions into `expected`, write the registry.
    if args.bless {
        for r in &results {
            if let Some(pin) = registry.pins.get_mut(&r.probe) {
                pin.expected = Some(r.disposition.clone());
            }
        }
        write_registry(&registry_dir, &registry)?;
        let changed = results.iter().filter(|r| !r.gate_ok).count();
        output::lint_msg(&format!("blessed {} (changed {changed})", results.len()));
        return Ok(());
    }

    if run_pass {
        output::lint_msg(&format!("PASS: {} probe(s)", results.len()));
        Ok(())
    } else {
        let reason = fail_reason.unwrap_or_else(|| "fail".to_owned());
        output::lint_msg(&format!(
            "FAIL: {reason} (recorded; see `brokkr lint-results`)"
        ));
        Err(DevError::ExitCode(1))
    }
}

/// Assemble a [`ProbeResult`] from a probe's classification, pin, and (when
/// piners parsed) its diagnostic set for the TV-anchor comparison.
fn build_result(
    id: &str,
    pin: &LintPin,
    outcome: &lint::diff::LintOutcome,
    piners_set: Option<&DiagSet>,
) -> ProbeResult {
    let disposition = outcome.disposition.to_owned();
    let expected = pin.expected.clone();
    let gate_ok = expected.as_deref() == Some(disposition.as_str());
    let anchor = pin.tv_anchor();
    let tv_divergent = anchor.as_ref().map(|a| match piners_set {
        Some(set) => set != a,
        None => true, // piners produced nothing comparable => divergent from truth
    });
    ProbeResult {
        probe: id.to_owned(),
        disposition,
        signature: outcome.signature.map(|s| s.as_str().to_owned()),
        expected,
        gate_ok,
        piners_count: outcome.piners_count,
        lint_count: outcome.lint_count,
        error: outcome.error.clone(),
        tv_anchored_at: pin.tv_anchored_at.clone(),
        tv_divergent,
    }
}

/// Drive `pine-lint --tv` over the selection and re-stamp each probe's TV
/// fingerprint + `tv_anchored_at` into `lints.toml`. Per-probe transport
/// failures are reported, not fatal; the run succeeds unless every probe
/// failed.
fn reanchor(
    registry_dir: &Path,
    registry: &mut LintRegistry,
    ids: &[String],
    abs_paths: &BTreeMap<String, String>,
    pine_lint: &str,
    run: &RunFn,
) -> Result<(), DevError> {
    let now = now_rfc3339();
    let mut anchored = 0usize;
    let mut failed = 0usize;
    for id in ids {
        let abs = &abs_paths[id];
        let set = match run(pine_lint, &["--tv", abs]) {
            Ok(cap) => validators::parse_pine_lint(&cap.stdout),
            Err(e) => Err(format!("pine-lint --tv failed to spawn: {e}")),
        };
        match set {
            Ok(diags) => {
                if let Some(pin) = registry.pins.get_mut(id) {
                    pin.tv = diags.iter().map(diag_to_tv).collect();
                    pin.tv_anchored_at = Some(now.clone());
                }
                anchored += 1;
            }
            Err(e) => {
                output::lint_msg(&format!("reanchor {id}: {e}"));
                failed += 1;
            }
        }
    }
    if anchored > 0 {
        write_registry(registry_dir, registry)?;
    }
    output::lint_msg(&format!("reanchored {anchored} probe(s) ({failed} failed)"));
    if anchored == 0 && failed > 0 {
        return Err(DevError::ExitCode(1));
    }
    Ok(())
}

/// Convert a normalized diagnostic key to its registry `TvDiag` form.
fn diag_to_tv(key: &lint::DiagKey) -> TvDiag {
    TvDiag {
        line: key.line,
        col: key.col,
        severity: key.severity.as_str().to_owned(),
    }
}

/// Write the in-memory registry back to `lints.toml`, preserving comments.
fn write_registry(registry_dir: &Path, registry: &LintRegistry) -> Result<(), DevError> {
    let path = registry_dir.join("lints.toml");
    let existing = std::fs::read_to_string(&path).ok();
    let rendered = lint::lints_write::render_lints(existing.as_deref(), &registry.pins)?;
    std::fs::write(&path, rendered).map_err(DevError::Io)?;
    Ok(())
}

/// Render the run: a disposition summary, the surviving deviation lines (a
/// probe sitting on its pin is folded into a count when gated), and the TV
/// shared-but-wrong advisory.
fn render(results: &[ProbeResult], no_gate: bool) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in results {
        *counts.entry(r.disposition.as_str()).or_default() += 1;
    }
    let summary = counts
        .iter()
        .map(|(k, n)| format!("{k}={n}"))
        .collect::<Vec<_>>()
        .join(", ");
    output::lint_msg(&format!("dispositions: {summary}"));

    let mut hidden = 0usize;
    for r in results {
        if r.gate_ok && !no_gate {
            hidden += 1;
            continue;
        }
        let line = match (&r.expected, r.error.as_deref()) {
            (_, Some(err)) => format!("{}: {} ({err})", r.probe, r.disposition),
            (None, None) => format!("{}: not blessed (got {}) - run --bless", r.probe, r.disposition),
            (Some(exp), None) if *exp == r.disposition => {
                format!("{}: {}", r.probe, r.disposition)
            }
            (Some(exp), None) => format!("{}: expected {exp}, got {}", r.probe, r.disposition),
        };
        output::lint_msg(&format!("  {line}"));
    }
    if hidden > 0 {
        output::lint_msg(&format!("  {hidden} probe(s) match their pin (hidden)"));
    }

    let tv_div = results.iter().filter(|r| r.tv_divergent == Some(true)).count();
    if tv_div > 0 {
        output::lint_msg(&format!(
            "TV advisory: {tv_div} probe(s) diverge from their TV anchor (re-investigate or --reanchor)"
        ));
    }
}

/// The `selector` JSON stored on the run row.
fn selector_json(args: &LintArgs, ids: &[String]) -> String {
    serde_json::json!({
        "all": args.all,
        "keywords": args.keywords,
        "probe": args.probe,
        "bless": args.bless,
        "ids": ids,
    })
    .to_string()
}
