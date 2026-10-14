//! Run a discovered sync-script cohort.

use std::path::Path;

use crate::artefacts;
use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::error::DevError;
use crate::output;
use crate::ratatoskr::build;
use crate::ratatoskr::discover::{self, ScriptInfo, Selection};

use super::list::sync_script_dir;
use super::runner::{SyncRunner, resolve_info, validate_sync_config};

// ---------------------------------------------------------------------------
// sync --all - run the whole discovered cohort
// ---------------------------------------------------------------------------

/// The cohort `sync --all` runs: the discovered scripts narrowed by the name
/// filter and, unless asked, stripped of `expected = "ignored"` ones. An empty
/// discovery or an empty selection is a refusal, not a vacuous pass.
fn select_cohort<'a>(
    scripts: &'a [ScriptInfo],
    req: &SyncAllRequest<'_>,
    dir: &Path,
) -> Result<Vec<&'a ScriptInfo>, DevError> {
    if scripts.is_empty() {
        return Err(DevError::Config(format!(
            "sync --all: no sync-test scripts found under {}",
            dir.display()
        )));
    }
    let selected: Vec<&ScriptInfo> = scripts
        .iter()
        .filter(|s| req.filter.is_none_or(|f| s.name.contains(f)))
        .filter(|s| req.include_ignored || s.selection != Selection::Skip)
        .collect();
    if selected.is_empty() {
        return Err(DevError::Config(format!(
            "sync --all: no scripts matched (filter: {}, {} discovered)",
            req.filter.unwrap_or("none"),
            scripts.len()
        )));
    }
    Ok(selected)
}

/// `brokkr sync --all [--filter SUB]` - run every discovered sync script
/// unmeasured, in discovery order.
///
/// The sync-side answer to `service --all`, and the reason a directory
/// argument can now mean the same thing in both families. Scripts marked
/// `expected = ignored` in frontmatter are skipped unless
/// `include_ignored` is set - they reproduce known-broken behaviour and
/// would block an otherwise clean sweep.
///
/// Default is keep-going: a cohort exists to tell you what is broken, so
/// it reports every failure and exits non-zero if any script failed.
/// That is the opposite of `service --all`'s stop-on-first-failure
/// default, and deliberately so - each sync script owns its own artefact
/// dir, so a later failure never overwrites an earlier one's triage
/// material.
///
/// The harness is built once, before the loop - the cohort varies the
/// script, never the binary - and each script then reports exactly one
/// line. The alternative (loop over [`super::smoke::run_sync_smoke`]) meant one no-op
/// cargo invocation and four lines of repeated build boilerplate per
/// script; a 160-script sweep is read by scanning verdicts, so nothing
/// invariant belongs in the loop.
pub fn run_sync_all(req: &SyncAllRequest<'_>) -> Result<(), DevError> {
    let dir = sync_script_dir(req.project_root, Some(req.ratatoskr));
    let scripts = discover::discover_sync_at(&dir)?;
    let selected = select_cohort(&scripts, req, &dir)?;

    let (cfg, harness_cfg, mock_binary, fixtures_dir) =
        validate_sync_config(req.project_root, req.ratatoskr)?;
    let resolved = selected
        .iter()
        .map(|script| resolve_info(script, &fixtures_dir))
        .collect::<Result<Vec<_>, _>>()?;

    output::ratatoskr_msg(&format!("sync cohort: {} script(s)", selected.len()));

    // Hold the global lock for the whole sweep, so another brokkr can't
    // interleave a build or bench between two scripts and the sweep can't
    // stall mid-way waiting behind one. With the build hoisted out of the
    // loop this is the path's only acquire; the re-entrant nesting story
    // lives with `--gate all` (see `lockfile::acquire`).
    let _lock = crate::context::reenter_command_lock()?;
    // One cooperative-SIGTERM guard for the whole sweep: the build and
    // every script run poll the same shutdown flag. See run_sync_smoke
    // for the mechanism.
    let _sigterm = crate::shutdown::SigtermGuard::install();

    let debug = req
        .profile_override
        .unwrap_or_else(|| harness_cfg.debug.unwrap_or(false));
    let built = build::build_for_harness(
        req.build_root.as_path(),
        harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true, // isolate_pg: SigtermGuard above bridges terminal signals
    )?;

    let runner = SyncRunner {
        project_root: req.project_root,
        cfg,
        built: &built,
        mock_binary: &mock_binary,
        keep_artefacts: req.keep_artefacts,
    };

    let mut failures: Vec<&str> = Vec::new();
    let mut any_preserved = false;
    let mut interrupted_at: Option<usize> = None;
    for (idx, (script, resolved)) in selected.iter().zip(&resolved).enumerate() {
        // Keep-going covers script failures, not `brokkr kill` / Ctrl-C.
        // The flag stays set for the rest of the sweep (one guard for the
        // whole cohort), so continuing would spawn sæhrimnir for every
        // remaining script only to kill its harness at once, preserving
        // an artefact dir each time. Checked before the spawn as well as
        // on the result, since a request can land between scripts.
        if crate::shutdown::is_shutdown_requested() {
            interrupted_at = Some(idx);
            break;
        }
        let outcome = runner.run_resolved(resolved, &_lock);
        match outcome.result {
            Ok(()) => {
                output::ratatoskr_msg(&format!("{}: PASS{}", script.name, outcome.summary));
            }
            Err(DevError::Interrupted) => {
                output::ratatoskr_msg(&format!("{}: INTERRUPTED{}", script.name, outcome.summary));
                if let Some(path) = outcome.preserved {
                    output::ratatoskr_msg(&format!("  artefacts preserved at {}", path.display()));
                    any_preserved = true;
                }
                interrupted_at = Some(idx);
                break;
            }
            Err(e) => {
                output::ratatoskr_msg(&format!("{}: FAIL{} - {e}", script.name, outcome.summary));
                if let Some(path) = outcome.preserved {
                    output::ratatoskr_msg(&format!("  artefacts preserved at {}", path.display()));
                    any_preserved = true;
                }
                failures.push(script.name.as_str());
            }
        }
    }

    if any_preserved {
        artefacts::emit_clean_hint(output::ratatoskr_msg);
    }
    if let Some(idx) = interrupted_at {
        output::ratatoskr_msg(&format!(
            "sync cohort: interrupted at script {}/{} ({} passed, {} failed before it)",
            idx + 1,
            selected.len(),
            idx - failures.len(),
            failures.len(),
        ));
        return Err(DevError::Interrupted);
    }
    let passed = selected.len() - failures.len();
    output::ratatoskr_msg(&format!("sync cohort: {passed}/{} passed", selected.len()));
    if failures.is_empty() {
        return Ok(());
    }
    // Each failure already printed its full error inline; the exit error
    // names them without repeating the messages.
    Err(DevError::Reported(format!(
        "sync --all: {} of {} script(s) failed: {}",
        failures.len(),
        selected.len(),
        failures.join(", "),
    )))
}

/// CLI inputs for `brokkr sync --all`.
pub struct SyncAllRequest<'a> {
    pub project_root: &'a Path,
    /// The code tree the harness is built from (cwd); differs from
    /// `project_root` only when `brokkr.toml` lives one level up.
    pub build_root: &'a crate::project::BuildRoot,
    pub ratatoskr: &'a RatatoskrConfig,
    /// Substring match against the script's discovered name.
    pub filter: Option<&'a str>,
    /// Include scripts whose frontmatter says `expected: ignored`.
    pub include_ignored: bool,
    pub keep_artefacts: bool,
    pub profile_override: Option<bool>,
}
