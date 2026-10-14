//! Command dispatch for sluggrs visual snapshot testing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::build;
use crate::config::{SluggrsConfig, SluggrsSnapshot};
use crate::error::DevError;
use crate::git;
use crate::output;
use crate::project::{self, Project};
use crate::visual::engine::{self, Subject, SubjectItem, SummaryOptions};
use crate::visual::{compare, status};

use super::db::SnapshotDb;

fn open_db(project_root: &Path) -> Result<SnapshotDb, DevError> {
    engine::open_results_db(project_root, SnapshotDb::open)
}

struct SnapshotOutcome {
    pixel_diff_pct: Option<f64>,
    status: status::Status,
}

fn validate_config(config: &SluggrsConfig) -> Result<(), DevError> {
    let mut ids = HashSet::new();
    for snapshot in &config.snapshots {
        compare::validate_artifact_id(&snapshot.id, "snapshot")?;
        if !ids.insert(snapshot.id.as_str()) {
            return Err(DevError::Config(format!(
                "duplicate sluggrs snapshot ID {:?}",
                snapshot.id
            )));
        }
    }
    Ok(())
}

impl SubjectItem for SluggrsSnapshot {
    const KIND: &'static str = "snapshot";

    fn id(&self) -> &str {
        &self.id
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_pct(v: Option<f64>, decimals: usize) -> String {
    engine::format_pct(v, decimals)
}

fn print_table_header() {
    engine::print_result_header("Snapshot", false, output::sluggrs_msg);
}

fn snapshots_dir(project_root: &Path) -> PathBuf {
    project_root.join("snapshots")
}

fn snapshot_dir(project_root: &Path, id: &str) -> PathBuf {
    snapshots_dir(project_root).join(id)
}

fn resolve_snapshot<'a>(
    config: &'a SluggrsConfig,
    id: &str,
) -> Result<&'a SluggrsSnapshot, DevError> {
    if let Some(snapshot) = config.snapshot_by_id(id) {
        return Ok(snapshot);
    }
    engine::resolve_prefix(&config.snapshots, id)
}

fn resolve_snapshots<'a>(
    config: &'a SluggrsConfig,
    snapshot_id: Option<&str>,
    all: bool,
) -> Result<Vec<&'a SluggrsSnapshot>, DevError> {
    // `--all` used to win silently over an ID.
    if all && snapshot_id.is_some() {
        return Err(DevError::Config(
            "give a snapshot ID or --all, not both".into(),
        ));
    }
    if all {
        Ok(config.snapshots.iter().collect())
    } else if let Some(id) = snapshot_id {
        Ok(vec![resolve_snapshot(config, id)?])
    } else {
        Err(DevError::Config("specify a snapshot ID or --all".into()))
    }
}

fn build_snapshot_binary(build_root: &Path) -> Result<PathBuf, DevError> {
    let config = build::BuildConfig {
        package: None,
        bin: None,
        example: Some("snapshot".into()),
        features: Vec::new(),
        default_features: true,
        all_features: false,
        profile: "release",
    };
    build::cargo_build(&config, build_root)
}

/// Parse the JSON metadata line from snapshot binary stdout.
///
/// The binary emits a single JSON line: `{"adapter":"...","backend":"..."}`
fn parse_snapshot_metadata(stdout: &[u8]) -> Result<SnapshotMeta, DevError> {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('{') {
            let meta: SnapshotMeta = serde_json::from_str(line)
                .map_err(|e| DevError::Verify(format!("invalid snapshot JSON output: {e}")))?;
            return Ok(meta);
        }
    }
    Err(DevError::Verify(
        "snapshot binary did not emit JSON metadata line".into(),
    ))
}

#[derive(serde::Deserialize)]
struct SnapshotMeta {
    adapter: String,
    #[allow(dead_code)]
    backend: Option<String>,
}

// ---------------------------------------------------------------------------
// Per-snapshot test
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn run_snapshot(
    binary: &Path,
    snapshot: &SluggrsSnapshot,
    config: &SluggrsConfig,
    project_root: &Path,
    db: &SnapshotDb,
    run_id: &str,
) -> Result<(SnapshotOutcome, Option<String>), DevError> {
    let snap_dir = snapshot_dir(project_root, &snapshot.id);
    std::fs::create_dir_all(&snap_dir)?;

    let output_png = snap_dir.join("output.png");
    let approved_png = snap_dir.join("approved.png");
    let diff_png = snap_dir.join("diff.png");

    let width_str = config.width.to_string();
    let height_str = config.height.to_string();
    let output_str = output_png.display().to_string();
    let binary_str = binary.display().to_string();

    let mut args: Vec<&str> = vec![
        "--id",
        &snapshot.id,
        "--output",
        &output_str,
        "--width",
        &width_str,
        "--height",
        &height_str,
    ];

    // Pass font paths.
    let font_strs: Vec<String> = snapshot
        .fonts
        .iter()
        .map(|f| project_root.join(f).display().to_string())
        .collect();
    for font in &font_strs {
        args.push("--font");
        args.push(font);
    }

    // Pass optional font paths.
    let opt_font_strs: Vec<String> = snapshot
        .optional_fonts
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|f| project_root.join(f).display().to_string())
        .collect();
    for font in &opt_font_strs {
        args.push("--optional-font");
        args.push(font);
    }

    // Clear the previous run's output first: `output.png` existing is the
    // only evidence the binary wrote it, so a leftover would be scored (and
    // could be approved) as this run's render.
    for stale in [&output_png, &diff_png] {
        match std::fs::remove_file(stale) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }

    output::sluggrs_msg(&format!("  rendering {}", snapshot.id));

    let captured = output::run_captured(&binary_str, &args, project_root)?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        output::sluggrs_msg(&format!(
            "  ERROR: {}",
            stderr.lines().next().unwrap_or("unknown error")
        ));
        db.insert_result(run_id, &snapshot.id, None, status::Status::Error)?;
        return Ok((
            SnapshotOutcome {
                pixel_diff_pct: None,
                status: status::Status::Error,
            },
            None,
        ));
    }

    // Parse adapter name from stdout JSON. A binary that exits 0 without the
    // metadata line is this one snapshot's problem, not the run's: degrade to
    // an ERROR row like every other failure here, rather than propagating and
    // taking the remaining snapshots down with a half-written run.
    let adapter_name = match parse_snapshot_metadata(&captured.stdout) {
        Ok(meta) => meta.adapter,
        Err(e) => {
            output::sluggrs_msg(&format!("  ERROR: {} ({e})", snapshot.id));
            db.insert_result(run_id, &snapshot.id, None, status::Status::Error)?;
            return Ok((
                SnapshotOutcome {
                    pixel_diff_pct: None,
                    status: status::Status::Error,
                },
                None,
            ));
        }
    };

    if !output_png.exists() {
        db.insert_result(run_id, &snapshot.id, None, status::Status::Error)?;
        return Ok((
            SnapshotOutcome {
                pixel_diff_pct: None,
                status: status::Status::Error,
            },
            Some(adapter_name),
        ));
    }

    // No approved baseline yet: NO_BASELINE, not a threshold failure. This is
    // the expected state of every newly registered snapshot - see
    // `status::Status::NoBaseline`.
    let approval = db.get_approval(&snapshot.id)?;
    if !approved_png.exists() {
        // An approval record with no image means the baseline was deleted,
        // not that it was never set; reporting NO_BASELINE would quietly
        // reset the ratchet on the next `approve --all`.
        if approval.is_some() {
            output::sluggrs_msg(&format!(
                "  ERROR: {} has an approval record but approved.png is missing - restore it",
                snapshot.id
            ));
            db.insert_result(run_id, &snapshot.id, None, status::Status::Error)?;
            return Ok((
                SnapshotOutcome {
                    pixel_diff_pct: None,
                    status: status::Status::Error,
                },
                Some(adapter_name),
            ));
        }
        db.insert_result(run_id, &snapshot.id, None, status::Status::NoBaseline)?;
        output::sluggrs_msg(&format!(
            "    no approved baseline for {} - run `brokkr approve {}`",
            snapshot.id, snapshot.id
        ));
        return Ok((
            SnapshotOutcome {
                pixel_diff_pct: None,
                status: status::Status::NoBaseline,
            },
            Some(adapter_name),
        ));
    }

    // Pixel diff against approved baseline.
    let pixel_threshold = config.pixel_diff_threshold;
    let approved_pixel = approval.map(|a| a.pixel_diff_pct);

    let pixel_result = compare::compare_pixels(&output_png, &approved_png, &diff_png);

    let (pixel_diff_pct, status) = match pixel_result {
        Ok(px) => {
            let s = status::determine_status(&status::StatusInput {
                pixel_diff_pct: px.diff_pct,
                element_match_pct: None,
                thresholds: status::ComparisonThresholds {
                    pixel: pixel_threshold,
                    element: None,
                },
                expected_fail: false,
                approved: status::ApprovedScores {
                    pixel: approved_pixel,
                    element: None, // sluggrs has no element comparison
                },
            });
            (Some(px.diff_pct), s)
        }
        Err(e) => {
            // Say why: a bare ERROR row left the decode failure unexplained.
            output::sluggrs_msg(&format!(
                "  ERROR: {} (pixel comparison failed: {e})",
                snapshot.id
            ));
            (None, status::Status::Error)
        }
    };

    db.insert_result(run_id, &snapshot.id, pixel_diff_pct, status)?;

    Ok((
        SnapshotOutcome {
            pixel_diff_pct,
            status,
        },
        Some(adapter_name),
    ))
}

struct SnapshotRunner<'a> {
    binary: &'a Path,
    config: &'a SluggrsConfig,
    project_root: &'a Path,
    db: &'a SnapshotDb,
    run_id: &'a str,
    first_adapter: Option<String>,
}

impl Subject for SnapshotRunner<'_> {
    type Item = SluggrsSnapshot;
    type Outcome = SnapshotOutcome;

    fn run_item(&mut self, snapshot: &Self::Item) -> Result<Self::Outcome, DevError> {
        let (outcome, adapter) = run_snapshot(
            self.binary,
            snapshot,
            self.config,
            self.project_root,
            self.db,
            self.run_id,
        )?;
        if self.first_adapter.is_none()
            && let Some(adapter) = adapter
        {
            self.first_adapter = Some(adapter);
        }
        Ok(outcome)
    }

    fn status(outcome: &Self::Outcome) -> status::Status {
        outcome.status
    }

    fn print_result(&mut self, snapshot: &Self::Item, outcome: &Self::Outcome) {
        let pixels = engine::format_pct(outcome.pixel_diff_pct, 1);
        output::sluggrs_msg(&format!(
            "  {:<25} {:<9} {}",
            snapshot.id, pixels, outcome.status,
        ));
    }

    fn finish(&mut self, tally: &status::StatusTally) -> Result<(), DevError> {
        if let Some(adapter) = &self.first_adapter {
            self.db.update_run_adapter(self.run_id, adapter)?;
        }
        engine::print_run_summary(
            tally,
            &SummaryOptions {
                divider_width: 50,
                show_unexpected_passes: false,
                no_baseline_hint: Some("  approve new snapshots with `brokkr approve --all`"),
            },
            output::sluggrs_msg,
        )
    }
}

// ---------------------------------------------------------------------------
// Test command
// ---------------------------------------------------------------------------

/// `project_root` is the config dir (snapshots, results.db); `build_root` is
/// the code tree cargo and git run in - they differ only under the
/// one-level-up layout. `suite`/`recapture` are litehtml's flags on the
/// shared `visual` command; sluggrs has neither concept, so they are refused
/// rather than silently dropped.
#[allow(clippy::too_many_arguments)]
pub(crate) fn test(
    project: Project,
    project_root: &Path,
    build_root: &Path,
    sluggrs_config: &SluggrsConfig,
    snapshot_id: Option<&str>,
    suite: Option<&str>,
    all: bool,
    recapture: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Sluggrs, "visual")?;
    validate_config(sluggrs_config)?;
    if suite.is_some() || recapture {
        return Err(DevError::Config(
            "--suite and --recapture are litehtml-only; sluggrs snapshots take an ID or --all"
                .into(),
        ));
    }

    let db = open_db(project_root)?;
    let git_info = git::collect(build_root)?;
    let snapshots = resolve_snapshots(sluggrs_config, snapshot_id, all)?;

    let binary = build_snapshot_binary(build_root)?;

    let run_id = crate::db::generate_uuid()?;
    let short_id = crate::db::short_uuid(&run_id);
    let dirty = !git_info.is_clean;

    // We'll update adapter_name after the first snapshot runs.
    db.insert_run(&run_id, &git_info.commit, dirty, None)?;

    let dirty_label = if dirty { ", dirty" } else { "" };
    output::sluggrs_msg(&format!(
        "SNAPSHOT TEST RESULTS  (run {short_id}, commit {}{dirty_label})",
        git_info.commit,
    ));
    output::sluggrs_msg("");
    print_table_header();

    let mut runner = SnapshotRunner {
        binary: &binary,
        config: sluggrs_config,
        project_root,
        db: &db,
        run_id: &run_id,
        first_adapter: None,
    };
    engine::run(&mut runner, &snapshots)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// List command
// ---------------------------------------------------------------------------

pub(crate) fn list(
    project: Project,
    project_root: &Path,
    sluggrs_config: &SluggrsConfig,
) -> Result<(), DevError> {
    project::require(project, Project::Sluggrs, "list")?;
    validate_config(sluggrs_config)?;

    let db = open_db(project_root)?;

    output::sluggrs_msg(&format!(
        "  {:<25} {:<40} {}",
        "ID", "Description", "Approved",
    ));
    output::sluggrs_msg(&format!("  {}", "\u{2500}".repeat(75)));

    for snapshot in &sluggrs_config.snapshots {
        let approved = match db.get_approval(&snapshot.id)? {
            Some(a) => format!("{:.1}%", a.pixel_diff_pct),
            None => "\u{2014}".into(),
        };
        output::sluggrs_msg(&format!(
            "  {:<25} {:<40} {}",
            snapshot.id, snapshot.description, approved,
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Approve command
// ---------------------------------------------------------------------------

/// `build_root` is the code tree whose cleanliness and commit the approval
/// is pinned to; `project_root` holds the snapshots and results.db.
pub(crate) fn approve(
    project: Project,
    project_root: &Path,
    build_root: &Path,
    sluggrs_config: &SluggrsConfig,
    snapshot_id: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Sluggrs, "approve")?;
    validate_config(sluggrs_config)?;

    let git_info = engine::collect_clean_approval("sluggrs", build_root)?;

    let db = open_db(project_root)?;
    let snapshot = resolve_snapshot(sluggrs_config, snapshot_id)?;

    let snap_dir = snapshot_dir(project_root, &snapshot.id);
    let output_png = snap_dir.join("output.png");
    let approved_png = snap_dir.join("approved.png");

    // Only approve an output the latest run actually produced and scored:
    // an ERROR row means output.png is missing or untrustworthy.
    match db.latest_result_for_snapshot(&snapshot.id)? {
        Some(r) if r.status == status::Status::Error => {
            return Err(DevError::Verify(format!(
                "latest run of snapshot '{}' is ERROR \u{2014} nothing to approve; fix it and \
                 rerun `brokkr visual {}`",
                snapshot.id, snapshot.id,
            )));
        }
        Some(_) if output_png.exists() => {}
        _ => {
            return Err(DevError::Verify(format!(
                "no output.png for snapshot '{}' \u{2014} run `brokkr visual {}` first",
                snapshot.id, snapshot.id,
            )));
        }
    }

    // Approving makes this output the image every later run is compared
    // against, so the ratchet value that belongs with it is its diff against
    // itself: 0. Recording the diff against the *old* baseline (as this
    // used to) judged later runs, compared to the new image, against the
    // old image's number - loosening the ratchet by exactly the change
    // being approved - and a compare error recorded 0.0 by accident.
    let pixel_pct = 0.0;

    // Stage the image, record the approval, then swap the image in. A DB
    // failure leaves the old baseline image and its record untouched; the
    // remaining window is a same-directory rename, not a copy.
    // Dropping `staged` on either error removes the staged copy.
    let staged = crate::atomic_write::Staged::stable(&approved_png)?;
    std::fs::copy(&output_png, staged.tmp_path())?;
    db.set_approval(&snapshot.id, &git_info.commit, pixel_pct)?;
    staged.commit()?;

    output::sluggrs_msg(&format!(
        "approved '{}' (commit {})",
        snapshot.id, git_info.commit,
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Status command
// ---------------------------------------------------------------------------

pub(crate) fn status(
    project: Project,
    project_root: &Path,
    sluggrs_config: &SluggrsConfig,
) -> Result<(), DevError> {
    project::require(project, Project::Sluggrs, "visual-status")?;
    validate_config(sluggrs_config)?;

    let db = open_db(project_root)?;
    let approvals = db.all_approvals()?;

    output::sluggrs_msg(&format!(
        "  {:<25} {:<11} {:<11} {:<9} {}",
        "Snapshot", "Approved", "Last Run", "Delta", "Status",
    ));
    output::sluggrs_msg(&format!("  {}", "\u{2500}".repeat(70)));

    for snapshot in &sluggrs_config.snapshots {
        print_snapshot_status(snapshot, &approvals, &db)?;
    }

    Ok(())
}

fn print_snapshot_status(
    snapshot: &SluggrsSnapshot,
    approvals: &[super::db::Approval],
    db: &SnapshotDb,
) -> Result<(), DevError> {
    let approval = approvals.iter().find(|a| a.subject_id == snapshot.id);
    let latest = db.latest_result_for_snapshot(&snapshot.id)?;

    let approved_str = approval
        .map(|a| format!("{:.1}%", a.pixel_diff_pct))
        .unwrap_or_else(|| "\u{2014}".into());

    let columns = status::status_columns(
        latest.as_ref().map(|row| row.status),
        latest.as_ref().and_then(|row| row.pixel_diff_pct),
        approval.map(|entry| entry.pixel_diff_pct),
        false,
    );
    output::sluggrs_msg(&engine::format_status_row(
        &snapshot.id,
        &approved_str,
        &columns.pixel,
        &columns.delta,
        &columns.status,
        &columns.annotation,
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Report command
// ---------------------------------------------------------------------------

pub(crate) fn report(project: Project, project_root: &Path, run_id: &str) -> Result<(), DevError> {
    project::require(project, Project::Sluggrs, "report")?;

    let db = open_db(project_root)?;
    let results = db.run_results(run_id)?;

    if results.is_empty() {
        return Err(DevError::Config(format!(
            "no results found for run '{run_id}'"
        )));
    }

    if let Some(summary) = db.run_summary(run_id)? {
        let adapter = summary.adapter_name.as_deref().unwrap_or("unknown");
        let dirty_label = if summary.dirty { ", dirty" } else { "" };
        let short_run_id = crate::db::short_uuid(&summary.run_id);
        output::sluggrs_msg(&format!(
            "Run {} \u{2014} commit {}{dirty_label}, adapter: {adapter}",
            short_run_id, summary.commit,
        ));
        output::sluggrs_msg("");
    }

    print_table_header();

    for row in &results {
        let px = format_pct(row.pixel_diff_pct, 1);
        output::sluggrs_msg(&format!(
            "  {:<25} {:<9} {}",
            row.subject_id, px, row.status,
        ));
    }

    Ok(())
}
