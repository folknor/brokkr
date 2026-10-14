//! Gate selection, cohort sweeps, and baseline evaluation.

use std::path::Path;

use crate::build::CargoProfile;
use crate::config::{GateConfig, RatatoskrConfig};
use crate::db::gate::{GateDb, GateRow};
use crate::error::DevError;
use crate::git;
use crate::lockfile::{self, LockContext};
use crate::output;
use crate::ratatoskr::gate as gate_eval;
use crate::sidecar;

use super::bench::{SyncBenchRequest, run_sync_bench};
#[cfg(test)]
use super::bench::{read_summary_json, summary_to_kv, sync_span_from_markers};
#[cfg(test)]
use super::list::{DEFAULT_SYNC_SCRIPT_DIR, sync_script_dir};
#[cfg(test)]
use super::runner::render_run_toml;

/// The reserved `--gate` value that means "every configured gate".
pub const GATE_ALL: &str = "all";

/// Reject the two `--gate all` combinations that have no coherent
/// meaning, before anything is built.
///
/// Split out of the dispatch arm so the refusals can be tested directly:
/// the `--as-baseline` bar is a safety property, not a convenience, and
/// it sits behind `project::require` where an end-to-end check would
/// need a ratatoskr checkout to reach.
pub fn validate_gate_selection(
    gate: Option<&str>,
    has_script: bool,
    as_baseline: bool,
) -> Result<(), DevError> {
    if gate != Some(GATE_ALL) {
        return Ok(());
    }
    if as_baseline {
        return Err(DevError::Config(
            "sync: --as-baseline cannot be combined with --gate all. \
             Rebasing a cohort in one command silently re-anchors every \
             baseline-relative rule (max_delta = 0 and equal_to_baseline \
             especially) onto whatever this tree measures, blessing any \
             regression present. Repin one gate at a time, from a tree you \
             have independently confirmed good."
                .into(),
        ));
    }
    if has_script {
        return Err(DevError::Config(
            "sync: --gate all takes no SCRIPT - each gate names its own \
             script in brokkr.toml. Drop the argument, or gate a single \
             script with --gate <name>."
                .into(),
        ));
    }
    Ok(())
}

/// Run every `[ratatoskr.gate.*]` in `brokkr.toml`, one after another.
///
/// Each gate names its own script, so the cohort takes no SCRIPT
/// argument - the config supplies them. Gates run in name order for a
/// stable report, and a breach does **not** stop the sweep: after a
/// refactor the useful answer is the whole blast radius, not the first
/// provider that happened to trip. Every failure is collected and
/// re-reported at the end, and the command exits non-zero if any gate
/// failed. An interrupt (`brokkr kill`, Ctrl-C) is not a breach: it
/// stops the sweep at once and propagates as `Interrupted`.
///
/// `--as-baseline` is refused upstream in `bootstrap.rs`, not here, so
/// the refusal happens before anything is built. Re-recording a whole
/// cohort in one command would silently re-anchor every
/// baseline-relative rule at once - on ratatoskr's own config that is most
/// of its gates carrying zero-drift rules across the provider surface, with
/// no per-gate moment to notice a regression being blessed.
pub fn run_gate_cohort(req: &SyncBenchRequest<'_>) -> Result<(), DevError> {
    let cfg = req.dev_config.ratatoskr.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync: no [ratatoskr] section in brokkr.toml. \
             Required to locate sæhrimnir and the harness binary."
                .into(),
        )
    })?;

    if cfg.gate.is_empty() {
        return Err(DevError::Config(
            "sync --gate all: no [ratatoskr.gate.*] entries in brokkr.toml".into(),
        ));
    }

    // Resolve every script up front. A typo'd path in gate 14 should
    // surface now, not twenty minutes into a sweep.
    let mut plan: Vec<(&str, String)> = Vec::new();
    for (name, gate) in &cfg.gate {
        let script = if gate.script.is_absolute() {
            gate.script.clone()
        } else {
            req.project_root.join(&gate.script)
        };
        if !script.is_file() {
            return Err(DevError::Config(format!(
                "sync --gate all: gate `{name}` names a script that does not exist: {}",
                script.display()
            )));
        }
        // Same reasoning for the rule set: a predicate-free rule in the
        // last gate is a config error, not something to find after every
        // other gate has benched. (Per-gate `run_sync_bench` re-checks.)
        if gate.metrics.is_empty() {
            return Err(DevError::Config(format!(
                "sync --gate all: gate `{name}` has no [ratatoskr.gate.{name}.metrics.*] \
                 rules - an empty rule set silently passes"
            )));
        }
        gate_eval::validate_rules(name, gate)?;
        plan.push((name.as_str(), script.display().to_string()));
    }

    output::ratatoskr_msg(&format!(
        "gate cohort: {} gate(s), {} iteration(s) each",
        plan.len(),
        req.bench
    ));

    // Hold the global measurement lock for the entire sweep. Each
    // `run_sync_bench` below re-enters this same hold through
    // `BenchHarness::new` - `lockfile::acquire` is re-entrant within the
    // process precisely so this hoist doesn't self-deadlock on a second
    // flock fd. Without it the lock dropped between gates, so a concurrent
    // brokkr could run a build or bench in the gap and contaminate the next
    // gate's timing; those numbers still land in gate.db and get compared
    // against the pinned baseline, making a contamination-induced breach
    // indistinguishable from a real regression. Holding across the sweep
    // also keeps the cohort contiguous in time - it can't stall mid-way
    // waiting behind someone else's run.
    let project_root_str = req.project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "ratatoskr",
        command: "sync",
        project_root: &project_root_str,
    })?;

    let mut failures: Vec<(&str, String)> = Vec::new();
    let mut ran = 0usize;
    for (name, script) in &plan {
        output::ratatoskr_msg(&format!("── gate `{name}` ──"));
        ran += 1;
        let one = SyncBenchRequest {
            project_root: req.project_root,
            build_root: req.build_root,
            dev_config: req.dev_config,
            script,
            bench: req.bench,
            force: req.force,
            keep_artefacts: req.keep_artefacts,
            profile_override: req.profile_override,
            brokkr_args: req.brokkr_args.clone(),
            gate: Some(name),
            as_baseline: false,
        };
        match run_sync_bench(&one) {
            Ok(()) => {}
            // `brokkr kill` / Ctrl-C is a request to stop the sweep, not a
            // gate verdict. Recording it as a FAIL and moving on kept the
            // sweep running to completion under the global lock - and each
            // later gate's sidecar guard cleared the request on install, so
            // nothing downstream ever saw it again.
            Err(DevError::Interrupted) => {
                output::ratatoskr_msg(&format!(
                    "gate cohort: interrupted during gate `{name}` ({ran}/{} started, {} failed before it)",
                    plan.len(),
                    failures.len()
                ));
                return Err(DevError::Interrupted);
            }
            Err(e) => {
                output::ratatoskr_msg(&format!("gate `{name}`: FAIL - {e}"));
                failures.push((name, e.to_string()));
            }
        }
    }

    let passed = plan.len() - failures.len();
    output::ratatoskr_msg(&format!("gate cohort: {passed}/{} passed", plan.len()));
    if failures.is_empty() {
        return Ok(());
    }
    let detail = failures
        .iter()
        .map(|(name, msg)| format!("  {name}: {msg}"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(DevError::Config(format!(
        "sync --gate all: {} of {} gate(s) failed:\n{detail}",
        failures.len(),
        plan.len()
    )))
}

/// Inputs for the post-bench gate hook.
pub(super) struct GateHookCtx<'a> {
    pub(super) req: &'a SyncBenchRequest<'a>,
    pub(super) cfg: &'a RatatoskrConfig,
    pub(super) gate_name: &'a str,
    pub(super) rules: &'a gate_eval::GateRules,
    pub(super) project_root: &'a Path,
    pub(super) script_abs: &'a Path,
    pub(super) fixture_name: &'a str,
    pub(super) debug: bool,
    /// The harness build's features, in `canonical_features` form.
    pub(super) features: &'a str,
    pub(super) elapsed_ms: i64,
    pub(super) summary: &'a serde_json::Map<String, serde_json::Value>,
    pub(super) sidecar_data: &'a sidecar::SidecarData,
}

/// Record the gated run into `gate.db`, then either: print the
/// `--as-baseline` paste-line and return, or look up the configured
/// per-hostname baseline and evaluate every metric rule. Returns
/// `DevError::Config` on any rule failure so the run exits non-zero.
pub(super) fn run_gate_hook(ctx: &GateHookCtx<'_>) -> Result<(), DevError> {
    let gate = ctx
        .cfg
        .gate
        .get(ctx.gate_name)
        .expect("gate existence already validated upstream");

    // Validate the gate's configured script matches the current
    // invocation. The gate is bound to a specific script; mixing them
    // up would silently compare against a different fixture's baseline.
    let configured_script = if gate.script.is_absolute() {
        gate.script.clone()
    } else {
        ctx.project_root.join(&gate.script)
    };
    let configured_canon = configured_script
        .canonicalize()
        .unwrap_or(configured_script.clone());
    if configured_canon != ctx.script_abs {
        return Err(DevError::Config(format!(
            "gate `{}`: configured script `{}` does not match invocation script `{}`",
            ctx.gate_name,
            gate.script.display(),
            ctx.script_abs.display(),
        )));
    }

    let hostname = crate::config::hostname()?;
    // Git state comes from the tree that was built, so a `--commit` row
    // records the commit it measured rather than whatever HEAD happens to
    // be. `gate.db` itself stays under the main `project_root` below - a
    // worktree is disposable, and the baselines are not.
    let git_info = git::collect(ctx.req.build_root.unwrap_or(ctx.project_root))?;
    let profile_name = CargoProfile::for_debug(ctx.debug).as_str();

    let sidecar_blob = sidecar_to_json(ctx.sidecar_data);
    let meta_blob = meta_to_json(ctx.summary);

    let row = GateRow {
        git_commit: git_info.commit.clone(),
        dirty: !git_info.is_clean,
        hostname: hostname.clone(),
        gate_name: ctx.gate_name.to_owned(),
        script: stored_script_path(ctx.project_root, ctx.script_abs),
        fixture: ctx.fixture_name.to_owned(),
        profile: profile_name.into(),
        elapsed_ms: ctx.elapsed_ms,
        // This hook runs only after every iteration exited successfully;
        // failures return before gate recording. These two stored metrics
        // are therefore constants for rows produced by this path.
        exit_code: 0,
        success: true,
        sidecar: serde_json::to_string(&sidecar_blob).unwrap_or_else(|_| "{}".into()),
        meta: serde_json::to_string(&meta_blob).unwrap_or_else(|_| "{}".into()),
        features: ctx.features.to_owned(),
    };

    let db_path = ctx.project_root.join(".brokkr/ratatoskr/gate.db");
    let db = GateDb::open(&db_path)?;
    let uuid = db.insert(&row)?;
    let short = &uuid[..8.min(uuid.len())];

    // Say which database. The harness's earlier "dirty tree - results will
    // NOT be stored" warning is about results.db and does not govern
    // gate.db, which records every gated run so a failure is inspectable.
    // Unqualified, the two lines read as a contradiction.
    output::ratatoskr_msg(&format!(
        "gate `{}` recorded run {short} in gate.db on host `{hostname}`",
        ctx.gate_name
    ));
    if row.dirty {
        output::ratatoskr_msg(
            "  [warn] this row is tagged dirty (--force); it is stored, but \
             pinning it as a baseline pins numbers from an uncommitted tree",
        );
    }

    if ctx.req.as_baseline {
        warn_if_rebasing(ctx.gate_name, gate, ctx.rules, &hostname);
        output::ratatoskr_msg("--as-baseline: skipping evaluation. Add this line to brokkr.toml:");
        output::ratatoskr_msg(&format!(
            "    [ratatoskr.gate.{}.baseline]\n    {hostname} = \"{uuid}\"",
            ctx.gate_name
        ));
        return Ok(());
    }

    evaluate_against_baseline(&db, &hostname, ctx.gate_name, gate, ctx.rules, &row, &uuid)
}

/// Keep in-project script identity stable when a checkout moves. Older rows
/// retain canonical absolute paths and are handled by `GateIdentity`.
fn stored_script_path(project_root: &Path, script_abs: &Path) -> String {
    let Ok(project_root) = project_root.canonicalize() else {
        return script_abs.display().to_string();
    };
    script_abs.strip_prefix(&project_root).map_or_else(
        |_| script_abs.display().to_string(),
        |path| path.display().to_string(),
    )
}

/// Warn, on the `--as-baseline` path, when recording is a *rebase* rather
/// than a first recording.
///
/// The missing-baseline error already says `--as-baseline` is unsafe, but
/// that text only reaches whoever hits the error. Someone re-recording on
/// purpose sees nothing, and re-recording is exactly where the damage
/// happens: a rule that compares against the baseline is re-anchored onto
/// the numbers in front of it, so a regression present at that moment
/// becomes the new definition of correct. `max_delta = 0` and
/// `equal_to_baseline` are the sharp cases - they permit no drift at all,
/// so the rebase is the only way they can ever be silenced.
///
/// This warns rather than refuses: recording a new baseline after a
/// deliberate change is the command's normal purpose, and brokkr cannot
/// tell that from an accident. The paste-line still has to be moved into
/// `brokkr.toml` by hand, so the warning arrives before anything is
/// actually rebased.
fn warn_if_rebasing(
    gate_name: &str,
    gate: &GateConfig,
    rules: &gate_eval::GateRules,
    hostname: &str,
) {
    let Some(pinned) = gate.baseline.get(hostname) else {
        return; // First baseline for this host: nothing to overwrite.
    };

    let strict = rules.no_drift_metrics();

    // Char-based, not a byte slice: the pin is hand-pasted TOML and may
    // hold a multi-byte character, which a byte slice would panic on.
    let short: String = pinned.chars().take(8).collect();
    output::ratatoskr_msg(&format!(
        "  [warn] gate `{gate_name}` already pins baseline {short} for host \
         `{hostname}` - repinning rebases every baseline-relative rule onto \
         this run's numbers"
    ));
    if !strict.is_empty() {
        let subject = if strict.len() == 1 {
            format!("rule `{}` admits", strict[0])
        } else {
            format!("rules `{}` admit", strict.join("`, `"))
        };
        output::ratatoskr_msg(&format!(
            "  [warn] {subject} no drift at all; if this tree has regressed, \
             repinning blesses the regression and the gate goes blind to it. \
             Repin only from a tree you have independently confirmed good."
        ));
    }
}

/// Build the failure message for a pinned baseline UUID that `gate.db`
/// does not have under this host.
///
/// The old text asserted "recorded on a different machine", which is an
/// inference the lookup cannot support: all it established was absence
/// under this hostname. The distinction matters because the remedy is not
/// the same. A pin someone deliberately made and a missing row point at
/// local `gate.db` loss - and re-recording there is destructive, since it
/// rebases every rule (`max_delta = 0` especially) onto current numbers
/// and blesses whatever regression is in the tree.
fn missing_baseline_error(
    db: &GateDb,
    gate_name: &str,
    hostname: &str,
    baseline_uuid: &str,
    current_uuid: &str,
) -> DevError {
    let hosts = db.hosts_with_uuid(baseline_uuid).unwrap_or_default();
    let others: Vec<&str> = hosts
        .iter()
        .filter(|h| h.as_str() != hostname)
        .map(String::as_str)
        .collect();
    let siblings = db
        .count_for_gate(gate_name, hostname, current_uuid)
        .unwrap_or(0);

    let cause = if others.is_empty() {
        if siblings == 0 {
            format!(
                "gate.db has no other rows for `{gate_name}` on this host either, \
                 so the local database looks reset, relocated or newly created - \
                 not a provenance problem."
            )
        } else {
            format!(
                "gate.db does have {siblings} other row(s) for `{gate_name}` on this \
                 host, so the pinned row specifically is gone (pruned, or written to \
                 a different .brokkr/ratatoskr/gate.db)."
            )
        }
    } else {
        format!(
            "that UUID *is* present in gate.db, but under host(s): {}. \
             The pin is filed under the wrong hostname.",
            others.join(", ")
        )
    };

    DevError::Config(format!(
        "gate `{gate_name}`: baseline UUID `{baseline_uuid}` is pinned for host \
         `{hostname}` in brokkr.toml, but no such row exists in gate.db under that \
         host.\n  {cause}\n  Recovery: restore the gate.db that holds this row \
         (it is the only copy of the reference numbers).\n  [warn] --as-baseline \
         is NOT a safe fix here: someone pinned this UUID deliberately, and \
         re-recording silently rebases every rule onto the current tree's numbers. \
         If the current tree has regressed, the gate is blessed blind. Only \
         re-record from a tree you have independently confirmed is good."
    ))
}

/// Look up the pinned per-hostname baseline UUID in `brokkr.toml`,
/// fetch the row from `gate.db`, validate gate/script/fixture/profile/features identity,
/// run rule evaluation, and emit a report. Returns
/// `DevError::Config("gate failed: ...")` on any rule failure.
fn evaluate_against_baseline(
    db: &GateDb,
    hostname: &str,
    gate_name: &str,
    gate: &GateConfig,
    rules: &gate_eval::GateRules,
    current_row: &GateRow,
    current_uuid: &str,
) -> Result<(), DevError> {
    let baseline_uuid = gate.baseline.get(hostname).ok_or_else(|| {
        DevError::Config(format!(
            "gate `{gate_name}`: no baseline pinned for host `{hostname}` in \
             [ratatoskr.gate.{gate_name}.baseline]. Record one with --as-baseline \
             and add the printed line to brokkr.toml."
        ))
    })?;
    // Exact-or-unambiguous-prefix, never the current row (already
    // inserted above): see `GateDb::lookup_baseline`.
    let baseline_entry = db
        .lookup_baseline(baseline_uuid, hostname, current_uuid)
        .map_err(|e| match e {
            DevError::Config(msg) => DevError::Config(format!("gate `{gate_name}`: {msg}")),
            other => other,
        })?
        .ok_or_else(|| {
            missing_baseline_error(db, gate_name, hostname, baseline_uuid, current_uuid)
        })?;
    let baseline_identity = gate_eval::GateIdentity::new(
        baseline_entry.gate_name.clone(),
        baseline_entry.script.clone(),
        baseline_entry.fixture.clone(),
        identity_profile(gate_name, "baseline", &baseline_entry.profile)?,
        baseline_entry
            .features
            .as_deref()
            .map(gate_eval::CanonicalFeatures::new),
    );
    let current_identity = gate_eval::GateIdentity::new(
        gate_name.to_owned(),
        current_row.script.clone(),
        current_row.fixture.clone(),
        identity_profile(gate_name, "current", &current_row.profile)?,
        Some(gate_eval::CanonicalFeatures::new(
            current_row.features.clone(),
        )),
    );
    let features_unrecorded = baseline_identity.features_unrecorded();

    let verdict = match baseline_identity.mismatch(&current_identity) {
        Some(mismatch) => gate_eval::GateVerdict::incomparable(mismatch),
        None => {
            let current_run = gate_eval::GateRun::from_parts(
                current_row.elapsed_ms,
                current_row.exit_code,
                current_row.success,
                serde_json::from_str(&current_row.sidecar).unwrap_or_default(),
                serde_json::from_str(&current_row.meta).unwrap_or_default(),
            );
            let baseline_run = gate_eval::GateRun::from_entry(&baseline_entry)?;
            gate_eval::GateVerdict::from_outcomes(rules.evaluate(&current_run, &baseline_run)?)
        }
    };
    if let Some(mismatch) = verdict.mismatch() {
        return Err(DevError::Config(format!("gate `{gate_name}`: {mismatch}")));
    }
    let report = verdict.format_report();
    let label_suffix = gate
        .baseline_label
        .as_deref()
        .map(|l| format!(" ({l})"))
        .unwrap_or_default();
    output::ratatoskr_msg(&format!(
        "gate `{gate_name}` vs baseline {}{label_suffix} ({} rules):",
        &baseline_entry.uuid[..8.min(baseline_entry.uuid.len())],
        verdict.outcomes().len(),
    ));
    if baseline_entry.dirty {
        output::ratatoskr_msg(
            "  [warn] baseline was recorded on a dirty git tree (--force); \
             consider re-recording on a clean checkout",
        );
    }
    if features_unrecorded {
        output::ratatoskr_msg(&format!(
            "  [warn] baseline predates gate.db's features column, so the features \
             it was built with are unknown; this run is {}. Compared on trust - \
             repin from a tree you have confirmed good to make the identity checkable.",
            features_label(&current_row.features)
        ));
    }
    if !report.is_empty() {
        for line in report.lines() {
            output::ratatoskr_msg(line);
        }
    }
    if !verdict.passed() {
        return Err(DevError::Config(format!(
            "gate `{gate_name}` FAILED: one or more rules breached"
        )));
    }
    output::ratatoskr_msg(&format!("gate `{gate_name}` PASSED"));
    Ok(())
}

/// A canonical features string for a message: the empty string is a
/// default-features build.
fn features_label(canonical: &str) -> String {
    gate_eval::CanonicalFeatures::new(canonical).to_string()
}

fn identity_profile(
    gate_name: &str,
    row_label: &str,
    profile: &str,
) -> Result<CargoProfile, DevError> {
    // Earlier gate rows and the previous writer spelled Cargo's `dev`
    // profile `debug`.
    let parsed = if profile == "debug" {
        Ok(CargoProfile::Dev)
    } else {
        CargoProfile::from_db(profile)
    };
    parsed.map_err(|unknown| {
        DevError::Config(format!(
            "gate `{gate_name}`: {row_label} row has unknown cargo profile `{}`",
            unknown.0
        ))
    })
}

/// Project a `SidecarData` into a flat JSON object usable by gate
/// `sidecar.<key>` selectors. Surfaces the summary fields plus a few
/// derived peaks/totals from the sample stream so common rules
/// (`sidecar.rss_peak_kb`, `sidecar.read_bytes`) work without the
/// gate doing its own aggregation.
fn sidecar_to_json(data: &sidecar::SidecarData) -> serde_json::Value {
    let mut peak_rss_kb: i64 = data.summary.vm_hwm_kb;
    let mut max_threads: i64 = 0;
    let mut last = data.samples.last();
    for s in &data.samples {
        peak_rss_kb = peak_rss_kb.max(s.rss_kb);
        max_threads = max_threads.max(s.num_threads);
        last = Some(s);
    }
    let (read_bytes, write_bytes, vol_cs, nonvol_cs, minflt, majflt) = match last {
        Some(s) => (
            s.read_bytes,
            s.write_bytes,
            s.vol_cs,
            s.nonvol_cs,
            s.minflt,
            s.majflt,
        ),
        None => (0, 0, 0, 0, 0, 0),
    };
    serde_json::json!({
        "vm_hwm_kb": data.summary.vm_hwm_kb,
        "rss_peak_kb": peak_rss_kb,
        "sample_count": data.summary.sample_count,
        "marker_count": data.summary.marker_count,
        "wall_time_ms": data.summary.wall_time_ms,
        "num_threads_max": max_threads,
        "read_bytes": read_bytes,
        "write_bytes": write_bytes,
        "vol_cs": vol_cs,
        "nonvol_cs": nonvol_cs,
        "minflt": minflt,
        "majflt": majflt,
    })
}

/// Project scalar harness metrics into the gate database. JSON booleans
/// remain booleans here; the results database projection omits them because
/// its `KvValue` schema has no boolean variant. Nested values are dropped.
fn meta_to_json(map: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (k, v) in map {
        match v {
            serde_json::Value::Number(_)
            | serde_json::Value::String(_)
            | serde_json::Value::Bool(_) => {
                out.insert(k.clone(), v.clone());
            }
            _ => {}
        }
    }
    serde_json::Value::Object(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::config::HarnessConfig;
    use crate::db::KvValue;
    use crate::ratatoskr::build::HarnessBuild;
    use crate::ratatoskr::saehrimnir::{Endpoints, MockOutcome, endpoint_env_pairs};
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    /// The load-bearing refusal: one command must not be able to rebase
    /// every gate at once. On ratatoskr's own config that would
    /// re-anchor 14 zero-drift rules in a single sweep.
    #[test]
    fn gate_all_refuses_as_baseline() {
        let err = validate_gate_selection(Some(GATE_ALL), false, true)
            .expect_err("--gate all + --as-baseline must be refused");
        let msg = err.to_string();
        assert!(msg.contains("--as-baseline"), "got: {msg}");
        assert!(msg.contains("one gate at a time"), "got: {msg}");
    }

    #[test]
    fn gate_all_refuses_a_script_argument() {
        let err = validate_gate_selection(Some(GATE_ALL), true, false)
            .expect_err("--gate all + SCRIPT must be refused");
        assert!(err.to_string().contains("takes no SCRIPT"));
    }

    /// A single named gate keeps every combination it had before the
    /// sweep existed - the refusals are scoped to the reserved value.
    #[test]
    fn single_gate_still_allows_as_baseline_with_a_script() {
        validate_gate_selection(Some("jmap_steady_state_delta"), true, true).unwrap();
        validate_gate_selection(None, true, false).unwrap();
        validate_gate_selection(None, false, false).unwrap();
    }

    #[test]
    fn features_identity_refuses_a_different_build() {
        let msg = gate_identity(Some("a,b"))
            .mismatch(&gate_identity(Some("a")))
            .unwrap()
            .to_string();
        assert!(msg.contains("features `a,b`"), "got: {msg}");
        assert!(msg.contains("features `a`"), "got: {msg}");
        let mismatch = gate_identity(Some(""))
            .mismatch(&gate_identity(Some("a")))
            .unwrap();
        assert!(mismatch.to_string().contains("default features"));
    }

    #[test]
    fn features_identity_passes_same_and_flags_legacy() {
        assert!(
            gate_identity(Some("a,b"))
                .mismatch(&gate_identity(Some("a,b")))
                .is_none()
        );
        assert!(
            gate_identity(Some(""))
                .mismatch(&gate_identity(Some("")))
                .is_none()
        );
        // A pre-v2 row: compared, but reported as unchecked.
        let legacy = gate_identity(None);
        assert!(legacy.features_unrecorded());
        assert!(legacy.mismatch(&gate_identity(Some("a"))).is_none());
    }

    #[test]
    fn legacy_debug_profile_maps_to_dev() {
        assert_eq!(
            identity_profile("g", "baseline", "debug").unwrap(),
            CargoProfile::Dev
        );
        assert_eq!(
            identity_profile("g", "baseline", "dev").unwrap(),
            CargoProfile::Dev
        );
    }

    #[test]
    fn in_project_script_paths_are_stored_relative_to_the_checkout() {
        let root = crate::test_scratch::scratch("ratatoskr-bench-gate", "stored_script_path");
        let root = root.canonicalize().unwrap();
        let script = root.join("crates/app/tests/sync/run.lua");

        assert_eq!(
            stored_script_path(&root, &script),
            "crates/app/tests/sync/run.lua"
        );
    }

    fn gate_identity(features: Option<&str>) -> gate_eval::GateIdentity {
        gate_eval::GateIdentity::new(
            "g",
            "script.lua",
            "fixture.toml",
            CargoProfile::Release,
            features.map(gate_eval::CanonicalFeatures::new),
        )
    }

    fn cfg_with_endpoints() -> RatatoskrConfig {
        RatatoskrConfig {
            harness: Some(HarnessConfig {
                package: "app".into(),
                binary: None,
                features: Vec::new(),
                debug: None,
            }),
            mock_server_binary: None,
            fixtures_dir: None,
            test_endpoint_env_jmap: Some("RATATOSKR_TEST_JMAP_ENDPOINT".into()),
            test_endpoint_env_imap: Some("RATATOSKR_TEST_IMAP_ENDPOINT".into()),
            test_endpoint_env_smtp: None,
            test_endpoint_env_graph: None,
            test_endpoint_env_gmail: Some("RATATOSKR_TEST_GMAIL_ENDPOINT".into()),
            test_endpoint_env_caldav: Some("RATATOSKR_TEST_CALDAV_ENDPOINT".into()),
            test_endpoint_env_carddav: Some("RATATOSKR_TEST_CARDDAV_ENDPOINT".into()),
            test_endpoint_env_people: Some("RATATOSKR_TEST_PEOPLE_ENDPOINT".into()),
            test_endpoint_env_gcal: Some("RATATOSKR_TEST_GCAL_ENDPOINT".into()),
            sync_script_dir: None,
            gate: std::collections::BTreeMap::new(),
        }
    }

    fn endpoints() -> Endpoints {
        Endpoints::from_ordered([1001, 1002, 1003, 1004, 1005, 1006, 1009, 1007, 1008])
    }

    #[test]
    fn endpoint_envs_only_emit_for_configured_protocols() {
        let cfg = cfg_with_endpoints();
        let pairs = endpoint_env_pairs(&cfg, &endpoints());
        let names: Vec<&str> = pairs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "RATATOSKR_TEST_JMAP_ENDPOINT",
                "RATATOSKR_TEST_IMAP_ENDPOINT",
                "RATATOSKR_TEST_GMAIL_ENDPOINT",
                "RATATOSKR_TEST_CALDAV_ENDPOINT",
                "RATATOSKR_TEST_CARDDAV_ENDPOINT",
                "RATATOSKR_TEST_PEOPLE_ENDPOINT",
                "RATATOSKR_TEST_GCAL_ENDPOINT",
            ]
        );
    }

    #[test]
    fn endpoint_envs_use_http_for_json_over_http() {
        let cfg = cfg_with_endpoints();
        let pairs = endpoint_env_pairs(&cfg, &endpoints());
        let jmap = pairs
            .iter()
            .find(|(n, _)| n == "RATATOSKR_TEST_JMAP_ENDPOINT")
            .unwrap();
        assert_eq!(jmap.1, "http://127.0.0.1:1001");
        let gmail = pairs
            .iter()
            .find(|(n, _)| n == "RATATOSKR_TEST_GMAIL_ENDPOINT")
            .unwrap();
        assert_eq!(gmail.1, "http://127.0.0.1:1005");
    }

    #[test]
    fn endpoint_envs_use_host_port_for_stream_protocols() {
        let cfg = cfg_with_endpoints();
        let pairs = endpoint_env_pairs(&cfg, &endpoints());
        let imap = pairs
            .iter()
            .find(|(n, _)| n == "RATATOSKR_TEST_IMAP_ENDPOINT")
            .unwrap();
        assert_eq!(imap.1, "127.0.0.1:1002");
    }

    #[test]
    fn endpoint_envs_empty_when_nothing_configured() {
        let cfg = RatatoskrConfig::default();
        let pairs = endpoint_env_pairs(&cfg, &endpoints());
        assert!(pairs.is_empty());
    }

    #[test]
    fn sync_script_dir_falls_back_to_default() {
        let root = Path::new("/proj");
        let resolved = sync_script_dir(root, None);
        assert_eq!(resolved, root.join(DEFAULT_SYNC_SCRIPT_DIR));
    }

    #[test]
    fn sync_script_dir_respects_relative_override() {
        let root = Path::new("/proj");
        let cfg = RatatoskrConfig {
            sync_script_dir: Some(PathBuf::from("custom/sync")),
            ..RatatoskrConfig::default()
        };
        let resolved = sync_script_dir(root, Some(&cfg));
        assert_eq!(resolved, root.join("custom/sync"));
    }

    fn marker(name: &str, ts_us: i64) -> sidecar::Marker {
        sidecar::Marker {
            marker_idx: 0,
            timestamp_us: ts_us,
            name: name.to_owned(),
        }
    }

    #[test]
    fn sync_span_uses_last_start_and_first_following_end() {
        // Warmup-then-measure: SYNC_START fires twice; the LAST one is
        // the measured start. SYNC_END after that is the measured end.
        let markers = vec![
            marker("SYNC_START", 1_000_000),
            marker("warmup_done", 2_000_000),
            marker("SYNC_START", 5_000_000),
            marker("SYNC_END", 12_500_000),
        ];
        assert_eq!(sync_span_from_markers(&markers), Some(7500));
    }

    #[test]
    fn sync_span_returns_none_without_markers() {
        let markers: Vec<sidecar::Marker> = Vec::new();
        assert_eq!(sync_span_from_markers(&markers), None);
    }

    #[test]
    fn sync_span_returns_none_with_only_start() {
        let markers = vec![marker("SYNC_START", 1_000_000)];
        assert_eq!(sync_span_from_markers(&markers), None);
    }

    #[test]
    fn sync_span_returns_none_with_end_before_start() {
        // Pathological: SYNC_END before any SYNC_START. Treat as
        // missing rather than negative.
        let markers = vec![marker("SYNC_END", 500_000), marker("SYNC_START", 1_000_000)];
        assert_eq!(sync_span_from_markers(&markers), None);
    }

    #[test]
    fn summary_to_kv_emits_int_real_text() {
        let mut map = serde_json::Map::new();
        map.insert("count".into(), serde_json::json!(42));
        map.insert("ratio".into(), serde_json::json!(2.5));
        map.insert("provider".into(), serde_json::json!("jmap"));
        map.insert("nested".into(), serde_json::json!({"skipped": true}));
        map.insert("flag".into(), serde_json::json!(true));
        map.insert("nullable".into(), serde_json::Value::Null);
        let kv = summary_to_kv(&map);
        // Only int + real + string survive; nested/bool/null skipped.
        assert_eq!(kv.len(), 3);
        let by_key = |k: &str| kv.iter().find(|p| p.key == k).map(|p| p.value.clone());
        assert!(matches!(by_key("meta.count"), Some(KvValue::Int(42))));
        match by_key("meta.ratio") {
            Some(KvValue::Real(v)) => assert!((v - 2.5).abs() < 1e-9),
            _ => panic!("expected meta.ratio=Real"),
        }
        match by_key("meta.provider") {
            Some(KvValue::Text(s)) => assert_eq!(s, "jmap"),
            _ => panic!("expected meta.provider=Text(jmap)"),
        }
    }

    /// A fresh scratch directory for one test. `name` must be unique in
    /// this module - the allocator asserts it.
    fn sum_tmp(name: &str) -> PathBuf {
        crate::test_scratch::scratch("sync-summary", name)
    }

    #[test]
    fn read_summary_json_missing_returns_empty_map() {
        let dir = sum_tmp("missing");
        let map = read_summary_json(&dir).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn read_summary_json_parses_object() {
        let dir = sum_tmp("ok");
        fs::write(
            dir.join("summary.json"),
            r#"{"messages_synced":100,"final_db_size_bytes":4096}"#,
        )
        .unwrap();
        let map = read_summary_json(&dir).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("messages_synced")
                .and_then(serde_json::Value::as_i64),
            Some(100)
        );
    }

    #[test]
    fn read_summary_json_rejects_non_object_root() {
        let dir = sum_tmp("array");
        fs::write(dir.join("summary.json"), "[1,2,3]").unwrap();
        let err = read_summary_json(&dir).unwrap_err();
        assert!(
            err.to_string().contains("must be a JSON object"),
            "got: {err}"
        );
    }

    #[test]
    fn read_summary_json_surfaces_parse_error() {
        let dir = sum_tmp("malformed");
        fs::write(dir.join("summary.json"), "{not valid json").unwrap();
        let err = read_summary_json(&dir).unwrap_err();
        assert!(err.to_string().contains("parse"), "got: {err}");
    }

    /// The sync `run.toml` used to be `format!`ed, so a `"` or `\` in a
    /// path or the features label produced a file no TOML reader could
    /// open. It must round-trip whatever the strings hold.
    #[test]
    fn run_toml_round_trips_hostile_strings() {
        use std::os::unix::process::ExitStatusExt;
        let built = HarnessBuild {
            binary: PathBuf::from("/odd \"dir\"/back\\slash/app"),
            bin_dir: PathBuf::from("/odd \"dir\"/back\\slash"),
            features_label: "a\"b\\c".to_owned(),
        };
        let dc = output::DeadlineCapture {
            captured: output::CapturedOutput {
                status: std::process::ExitStatus::from_raw(3 << 8),
                stdout: Vec::new(),
                stderr: Vec::new(),
                elapsed: Duration::from_millis(42),
            },
            killed_on_deadline: true,
            output_cut: false,
        };
        let mock = MockOutcome {
            exit_code: None,
            signal: Some(9),
            killed_after_budget: true,
            shutdown_elapsed: Duration::ZERO,
        };
        let git = git::GitInfo {
            commit: "abc1234".to_owned(),
            subject: "fix \"quoted\" \\ thing".to_owned(),
            is_clean: false,
        };
        let body = render_run_toml(
            Path::new("/scripts/we\"ird.lua"),
            &built,
            &dc,
            &mock,
            Some(&git),
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        assert_eq!(
            parsed["binary"].as_str(),
            Some("/odd \"dir\"/back\\slash/app")
        );
        assert_eq!(parsed["features"].as_str(), Some("a\"b\\c"));
        assert_eq!(parsed["script"].as_str(), Some("/scripts/we\"ird.lua"));
        assert_eq!(
            parsed["git_subject"].as_str(),
            Some("fix \"quoted\" \\ thing")
        );
        assert_eq!(parsed["exit_code"].as_integer(), Some(3));
        assert_eq!(parsed["elapsed_ms"].as_integer(), Some(42));
        assert_eq!(parsed["killed_on_deadline"].as_bool(), Some(true));
        assert_eq!(parsed["mock"]["signal"].as_integer(), Some(9));
        assert_eq!(parsed["mock"]["killed_after_budget"].as_bool(), Some(true));
        assert!(parsed["mock"].get("exit_code").is_none());
    }

    #[test]
    fn sync_script_dir_keeps_absolute_override() {
        let cfg = RatatoskrConfig {
            sync_script_dir: Some(PathBuf::from("/srv/scripts")),
            ..RatatoskrConfig::default()
        };
        let resolved = sync_script_dir(Path::new("/proj"), Some(&cfg));
        assert_eq!(resolved, PathBuf::from("/srv/scripts"));
    }
}
