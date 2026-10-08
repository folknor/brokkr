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
use crate::piners::lint::{self, validators, DiagSet, ProbeResult};
use crate::piners::time::now_rfc3339;
use crate::piners::registry_io;
use crate::ratatoskr::build;
use crate::resolve::lint_runs_db_path;

/// A PID-tracked captured-subprocess runner (`program`, `argv`, deadline) ->
/// output. Factored out so [`reanchor`] can borrow it without a clippy-flagged
/// closure type in its signature. A child still running at the deadline is
/// killed and reported as an `Err`.
type RunFn<'a> = dyn Fn(&str, &[&str], Duration) -> Result<CapturedOutput, DevError> + 'a;

/// Wall-clock backstop for one offline validator call (`<bin> validate` or
/// offline `pine-lint`) on one snippet. Both finish a snippet in well under a
/// second; this exists so a wedged tool fails that probe as a tool error
/// instead of holding the global lock indefinitely.
const OFFLINE_DEADLINE: Duration = Duration::from_secs(60);

/// Wall-clock limit for one `pine-lint --tv` call during `--reanchor`. The
/// call goes over the network to TradingView, so a stalled connection is the
/// expected failure; it is reported per probe like any transport failure.
const TV_DEADLINE: Duration = Duration::from_secs(30);

/// Dispositions that mean a tool produced no comparable output. They describe
/// the tooling, not the snippet: a run carrying one fails whatever the pin
/// says, and `--bless` never stamps one (with `pine-lint` missing, every
/// probe is `lint_error`, and blessing that would pin the outage).
const TOOL_ERROR_DISPOSITIONS: [&str; 2] = ["piners_error", "lint_error"];

fn is_tool_error(disposition: &str) -> bool {
    TOOL_ERROR_DISPOSITIONS.contains(&disposition)
}

/// Flags lifted off the `LintCorpus` CLI command.
#[derive(Debug, Default)]
pub struct LintArgs {
    pub keywords: Vec<String>,
    pub probe: Vec<String>,
    pub all: bool,
    pub verify_only: bool,
    /// Stamp `lints.toml` from the snippet tree (no build, no run) - the
    /// bootstrap writer. Routed to [`crate::piners::lint::reseed`].
    pub reseed: bool,
    /// Refresh the TV anchor (`pine-lint --tv`) for the selection - the
    /// periodic, network-touching registry writer. Conflicts with the run
    /// writers on the CLI.
    pub reanchor: bool,
    /// Run the selection, then stamp each probe's current disposition into
    /// `expected`.
    pub bless: bool,
    /// Report the gate diff but never fail on it.
    pub no_gate: bool,
    /// Compare type/analysis diagnostics too, not just parser/syntax.
    pub all_stages: bool,
    /// Include warning diagnostics in the gated diff (default: errors only).
    pub warnings: bool,
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

    // Reseed stamps lints.toml from the snippet tree - no registry to load (it
    // may not exist yet), no build, no run. Route early.
    if args.reseed {
        return crate::piners::lint::reseed::run(project_root, &piners_cfg, lint_cfg, args);
    }

    // Locked before the registry is read: the lint registry's writers
    // (reseed, bless, reanchor) write under this lock, so verification must
    // judge the pins and bytes the run will use, not ones a writer replaced
    // while this waited. Verify-only included.
    let project_root_str = project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "piners",
        command: if args.reanchor { "lint-reanchor" } else { "lint-corpus" },
        project_root: &project_root_str,
    })?;
    let _sigterm = crate::shutdown::SigtermGuard::install();

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

    // A captured subprocess run, PID-tracked so `brokkr kill` reaches it, and
    // killed at `deadline` (reported as an error naming the limit).
    let run = |program: &str, argv: &[&str], deadline: Duration| -> Result<CapturedOutput, DevError> {
        let r = output::run_captured_with_env_and_deadline(
            program,
            argv,
            project_root,
            &[],
            deadline,
            Some(&|pid| _lock.set_child_pid(pid)),
            false,
        );
        _lock.clear_child_pid();
        let r = r?;
        if r.killed_on_deadline {
            return Err(DevError::Verify(format!(
                "{program} did not finish within {}s and was killed",
                deadline.as_secs()
            )));
        }
        Ok(r.captured)
    };

    let scope = validators::Scope {
        include_warnings: args.warnings,
        syntax_only: !args.all_stages,
    };

    // --reanchor: refresh the TV fingerprint via `pine-lint --tv`, write the
    // registry, and return. No validator build, no run store. The anchor is
    // filtered to the run's scope, which is recorded beside it (`tv_scope`).
    if args.reanchor {
        return reanchor(
            &registry_dir,
            &mut registry,
            &ids,
            &abs_paths,
            lint_cfg.pine_lint_bin(),
            scope,
            &run,
        );
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
        // A `brokkr kill` interrupts the in-flight call, which then reads as a
        // tool error; stop here rather than spawn (and kill) every later probe.
        if crate::shutdown::is_shutdown_requested() {
            return Err(DevError::Interrupted);
        }
        let abs = &abs_paths[id];
        let pin = &registry.pins[id];

        let piners_set = match run(&validator, &[&subcommand, "--format", "json", abs], OFFLINE_DEADLINE) {
            Ok(cap) => validators::parse_piners(&cap.stdout, scope),
            Err(e) => Err(format!("piners validate failed: {e}")),
        };
        let lint_set = match run(&pine_lint, &[abs], OFFLINE_DEADLINE) {
            Ok(cap) => validators::parse_pine_lint(&cap.stdout, scope),
            Err(e) => Err(format!("pine-lint failed: {e}")),
        };

        let outcome = classify(
            piners_set.as_ref().map_err(String::as_str),
            lint_set.as_ref().map_err(String::as_str),
        );
        results.push(build_result(id, pin, &outcome, piners_set.as_ref().ok(), scope));
    }

    render(&results, &registry, args.no_gate, scope);

    // Persist the run before any registry mutation. A tool error fails the
    // run whatever the pin says: it describes the tooling, not the snippet
    // (see TOOL_ERROR_DISPOSITIONS), so it is never a state a pin can accept.
    let tool_errors: Vec<&str> = results
        .iter()
        .filter(|r| is_tool_error(&r.disposition))
        .map(|r| r.probe.as_str())
        .collect();
    let tool_error = !tool_errors.is_empty();
    let deviations = results.iter().filter(|r| !r.gate_ok).count();
    let gate_blocks = !args.bless && !args.no_gate && deviations > 0;
    let run_pass = !tool_error && !gate_blocks;
    let fail_reason: Option<String> = if run_pass {
        None
    } else if tool_error {
        Some(format!("{} validator error(s)", tool_errors.len()))
    } else {
        Some(format!("{deviations} gate deviation(s)"))
    };

    let selector = selector_json(args, &ids);
    let meta = RunMeta {
        started_at: &now_rfc3339(),
        selector: &selector,
        // A bless run ignores the gate verdict, so it is not a gated run.
        gated: !args.no_gate && !args.bless,
        result: if run_pass { "pass" } else { "fail" },
        fail_reason: fail_reason.as_deref(),
        probe_count: results.len(),
        stderr: "",
    };
    let db_path = lint_runs_db_path(project_root);
    let mut db = LintDb::open(&db_path)?;
    db.record_run(&meta, &results)?;

    // --bless: stamp current dispositions (and the scope they were computed
    // under) into `expected`, write the registry. A tool-error probe is not
    // stamped - its disposition says nothing about the snippet - and makes the
    // bless fail once the rest are written, so a missing or broken tool is
    // loud rather than pinned.
    if args.bless {
        let recorded_scope = scope.recorded_label();
        let mut blessed = 0usize;
        let mut changed = 0usize;
        write_registry(&registry_dir, &mut registry, true, |pins| {
            for r in &results {
                if is_tool_error(&r.disposition) || !lint::is_disposition(&r.disposition) {
                    continue;
                }
                let Some(pin) = pins.get_mut(&r.probe) else {
                    continue; // unreachable: results come from the loaded pins being edited
                };
                blessed += 1;
                if pin.expected.as_deref() != Some(r.disposition.as_str())
                    || pin.expected_scope != recorded_scope
                {
                    changed += 1;
                    pin.expected = Some(r.disposition.clone());
                    pin.expected_scope = recorded_scope.clone();
                }
            }
        })?;
        output::lint_msg(&format!(
            "blessed {blessed} (changed {changed}) under scope {}",
            scope.label()
        ));
        if tool_error {
            output::lint_msg(&format!(
                "FAIL: {} probe(s) hit a validator error and were not blessed: {}",
                tool_errors.len(),
                tool_errors.join(", ")
            ));
            return Err(DevError::ExitCode(1));
        }
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
///
/// `expected` holds only under the scope it was blessed in: a pin blessed
/// under another scope is a gate deviation (rendered as a scope mismatch),
/// not a comparison of dispositions computed from different diagnostics.
/// Likewise a TV anchor filtered to another scope is not compared at all
/// (`tv_divergent = None`), since the difference would be the filter, not TV.
fn build_result(
    id: &str,
    pin: &LintPin,
    outcome: &lint::diff::LintOutcome,
    piners_set: Option<&DiagSet>,
    scope: validators::Scope,
) -> ProbeResult {
    let disposition = outcome.disposition.to_owned();
    let expected = pin.expected.clone();
    let gate_ok = expected.as_deref() == Some(disposition.as_str())
        && scope.matches_recorded(pin.expected_scope.as_deref());
    let anchor = pin
        .tv_anchor()
        .filter(|_| scope.matches_recorded(pin.tv_scope.as_deref()));
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
/// fingerprint + `tv_anchored_at` + `tv_scope` into `lints.toml`. Per-probe
/// transport failures (including a call killed at [`TV_DEADLINE`]) are
/// reported, not fatal; the run succeeds unless every probe failed.
fn reanchor(
    registry_dir: &Path,
    registry: &mut LintRegistry,
    ids: &[String],
    abs_paths: &BTreeMap<String, String>,
    pine_lint: &str,
    scope: validators::Scope,
    run: &RunFn,
) -> Result<(), DevError> {
    let now = now_rfc3339();
    let recorded_scope = scope.recorded_label();
    let mut fresh: BTreeMap<String, Vec<TvDiag>> = BTreeMap::new();
    let mut failed = 0usize;
    for id in ids {
        if crate::shutdown::is_shutdown_requested() {
            return Err(DevError::Interrupted);
        }
        let abs = &abs_paths[id];
        let set = match run(pine_lint, &["--tv", abs], TV_DEADLINE) {
            Ok(cap) => validators::parse_pine_lint(&cap.stdout, scope),
            Err(e) => Err(format!("pine-lint --tv failed: {e}")),
        };
        match set {
            Ok(diags) => {
                fresh.insert(id.clone(), diags.iter().map(diag_to_tv).collect());
            }
            Err(e) => {
                output::lint_msg(&format!("reanchor {id}: {e}"));
                failed += 1;
            }
        }
    }
    let anchored = fresh.len();
    if anchored > 0 {
        write_registry(registry_dir, registry, false, |pins| {
            for (id, tv) in &fresh {
                if let Some(pin) = pins.get_mut(id) {
                    pin.tv = tv.clone();
                    pin.tv_anchored_at = Some(now.clone());
                    pin.tv_scope = recorded_scope.clone();
                }
            }
        })?;
    }
    output::lint_msg(&format!(
        "reanchored {anchored} probe(s) under scope {} ({failed} failed)",
        scope.label()
    ));
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

/// Edit the `lints.toml` text the run loaded: let `apply` change the fields
/// this writer owns on a copy of the loaded pins, render that into
/// [`LintRegistry::lints_text`] (preserving comments), and replace the file
/// atomically.
///
/// The command holds the lock from before its load, so no brokkr writer can
/// have touched the file since. If the bytes on disk no longer equal the
/// loaded text, a hand edit landed during the run: the write is refused
/// rather than revert it, or stamp a disposition or TV fingerprint measured
/// against a snippet pin that is no longer the file's. `run_recorded` only
/// shapes the message (a bless run is already in the run store when this is
/// called; a reanchor records none). The check is not atomic with the
/// replace - an edit landing between the two is still overwritten.
///
/// A read failure propagates (the file was readable at load; treating it as
/// absent would rewrite it without its comments). On success the registry's
/// pins and text become what was written, so a later write in this process
/// compares against it.
fn write_registry(
    registry_dir: &Path,
    registry: &mut LintRegistry,
    run_recorded: bool,
    apply: impl FnOnce(&mut BTreeMap<String, LintPin>),
) -> Result<(), DevError> {
    let path = registry_dir.join("lints.toml");
    let on_disk = std::fs::read_to_string(&path).map_err(|e| {
        DevError::Config(format!("piners lint: failed to re-read {}: {e}", path.display()))
    })?;
    if on_disk != registry.lints_text {
        let tail = if run_recorded {
            "the run is recorded; re-run the bless against the file as it is now"
        } else {
            "re-run the reanchor against the file as it is now"
        };
        return Err(DevError::Config(format!(
            "piners lint: {} changed during the run - nothing written ({tail})",
            path.display()
        )));
    }
    let mut pins = registry.pins.clone();
    apply(&mut pins);
    let rendered = lint::lints_write::render_lints(Some(&registry.lints_text), &pins)?;
    // Parse our own output against the loader's rules before replacing, so a
    // write can never produce a file the next load refuses.
    registry::parse_lints(&rendered, &path)?;
    registry_io::write_atomic(&path, &rendered)?;
    registry.pins = pins;
    registry.lints_text = rendered;
    Ok(())
}

/// Render the run: a disposition summary, the surviving deviation lines (a
/// probe sitting on its pin is folded into a count when gated), and the TV
/// shared-but-wrong advisory.
fn render(
    results: &[ProbeResult],
    registry: &LintRegistry,
    no_gate: bool,
    scope: validators::Scope,
) {
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
        let pin = registry.pins.get(&r.probe);
        let blessed_scope = pin.and_then(|p| p.expected_scope.as_deref());
        let line = match (&r.expected, r.error.as_deref()) {
            (_, Some(err)) => format!("{}: {} ({err})", r.probe, r.disposition),
            (None, None) => format!("{}: not blessed (got {}) - run --bless", r.probe, r.disposition),
            (Some(_), None) if !scope.matches_recorded(blessed_scope) => format!(
                "{}: blessed under scope {}, this run is {} - rerun with the flags it \
                 was blessed under, or re-bless (got {})",
                r.probe,
                blessed_scope.unwrap_or(validators::DEFAULT_SCOPE_LABEL),
                scope.label(),
                r.disposition
            ),
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
    // Anchors stamped under another scope are skipped by build_result, not
    // compared; say so, so a quiet advisory is not read as "TV agrees".
    let other_scope = results
        .iter()
        .filter_map(|r| registry.pins.get(&r.probe))
        .filter(|p| p.tv_anchor().is_some() && !scope.matches_recorded(p.tv_scope.as_deref()))
        .count();
    if other_scope > 0 {
        output::lint_msg(&format!(
            "TV advisory: {other_scope} anchor(s) were stamped under another scope than {} \
             and were not compared",
            scope.label()
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const LOADED: &str = "# keep me\n[probes.a]\npine = { path = \"lint/a.pine\", xxh128 = \"00\" }\n";

    fn loaded(dir: &Path, on_disk: &str) -> LintRegistry {
        std::fs::write(dir.join("lints.toml"), LOADED).unwrap();
        let mut reg = LintRegistry::load(dir).unwrap();
        std::fs::write(dir.join("lints.toml"), on_disk).unwrap();
        reg.lints_text = LOADED.to_owned();
        reg
    }

    #[test]
    fn write_succeeds_on_unchanged_file_and_keeps_comments() {
        let dir = crate::test_scratch::scratch("piners_lint_cmd", "write_unchanged");
        let mut reg = loaded(&dir, LOADED);
        write_registry(&dir, &mut reg, true, |pins| {
            pins.get_mut("a").unwrap().expected = Some("agree_clean".to_owned());
        })
        .unwrap();

        let written = std::fs::read_to_string(dir.join("lints.toml")).unwrap();
        assert!(written.contains("# keep me"));
        let data = registry::parse_lints(&written, &dir).unwrap();
        assert_eq!(data.probes["a"].expected.as_deref(), Some("agree_clean"));
        assert_eq!(reg.lints_text, written);
        assert_eq!(reg.pins["a"].expected.as_deref(), Some("agree_clean"));

        // A second write in the same process compares against the first.
        write_registry(&dir, &mut reg, true, |pins| {
            pins.get_mut("a").unwrap().expected = Some("divergent".to_owned());
        })
        .unwrap();
        let again = std::fs::read_to_string(dir.join("lints.toml")).unwrap();
        assert!(again.contains("divergent"));
    }

    #[test]
    fn write_refuses_and_leaves_file_untouched_when_it_changed() {
        let dir = crate::test_scratch::scratch("piners_lint_cmd", "write_changed");
        let edited = LOADED.replace("lint/a.pine", "lint/b.pine");
        let mut reg = loaded(&dir, &edited);
        let err = write_registry(&dir, &mut reg, true, |pins| {
            pins.get_mut("a").unwrap().expected = Some("agree_clean".to_owned());
        })
        .unwrap_err();

        let msg = format!("{err:?}");
        assert!(msg.contains("changed during the run"), "{msg}");
        assert!(msg.contains("lints.toml"), "{msg}");
        assert_eq!(std::fs::read_to_string(dir.join("lints.toml")).unwrap(), edited);
        assert_eq!(reg.lints_text, LOADED);
        assert!(reg.pins["a"].expected.is_none());
    }
}
