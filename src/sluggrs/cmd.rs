//! Command dispatch for sluggrs visual snapshot testing.

use std::path::{Path, PathBuf};

use crate::build;
use crate::config::{SluggrsConfig, SluggrsSnapshot};
use crate::error::DevError;
use crate::git;
use crate::litehtml::compare;
use crate::output;
use crate::project::{self, Project};
use crate::resolve;

use super::db::SnapshotDb;

fn open_db(project_root: &Path) -> Result<SnapshotDb, DevError> {
    let db_path = resolve::results_db_path(project_root);
    SnapshotDb::open(&db_path)
}

struct SnapshotOutcome {
    pixel_diff_pct: Option<f64>,
    status: compare::Status,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sluggrs_msg(msg: &str) {
    output::sluggrs_msg(msg);
}

fn format_pct(v: Option<f64>, decimals: usize) -> String {
    match v {
        Some(val) => format!("{val:.decimals$}%"),
        None => "\u{2014}".into(),
    }
}

fn print_table_header() {
    sluggrs_msg(&format!(
        "  {:<25} {:<9} {}", "Snapshot", "Pixels", "Status",
    ));
    sluggrs_msg(&format!("  {}", "\u{2500}".repeat(50)));
}

fn snapshots_dir(project_root: &Path) -> PathBuf {
    project_root.join("snapshots")
}

fn snapshot_dir(project_root: &Path, id: &str) -> PathBuf {
    snapshots_dir(project_root).join(id)
}

fn resolve_snapshot<'a>(config: &'a SluggrsConfig, id: &str) -> Result<&'a SluggrsSnapshot, DevError> {
    if let Some(entry) = config.snapshot_by_id(id) {
        return Ok(entry);
    }
    let matches: Vec<_> = config.snapshots.iter()
        .filter(|s| s.id.starts_with(id))
        .collect();
    match matches.len() {
        0 => Err(DevError::Config(format!("snapshot not found: {id}"))),
        1 => Ok(matches[0]),
        _ => {
            let ids: Vec<&str> = matches.iter().map(|s| s.id.as_str()).collect();
            Err(DevError::Config(format!(
                "ambiguous snapshot prefix '{id}', matches: {}", ids.join(", "),
            )))
        }
    }
}

fn resolve_snapshots<'a>(
    config: &'a SluggrsConfig,
    snapshot_id: Option<&str>,
    all: bool,
) -> Result<Vec<&'a SluggrsSnapshot>, DevError> {
    if all {
        Ok(config.snapshots.iter().collect())
    } else if let Some(id) = snapshot_id {
        Ok(vec![resolve_snapshot(config, id)?])
    } else {
        Err(DevError::Config("specify a snapshot ID or --all".into()))
    }
}

fn build_snapshot_binary(project_root: &Path) -> Result<PathBuf, DevError> {
    let config = build::BuildConfig {
        package: None,
        bin: None,
        example: Some("snapshot".into()),
        features: Vec::new(),
        default_features: true,
        profile: "release",
    };
    build::cargo_build(&config, project_root)
}

/// Parse the JSON metadata line from snapshot binary stdout.
///
/// The binary emits a single JSON line: `{"adapter":"...","backend":"..."}`
fn parse_snapshot_metadata(stdout: &[u8]) -> Result<SnapshotMeta, DevError> {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('{') {
            let meta: SnapshotMeta = serde_json::from_str(line).map_err(|e| {
                DevError::Verify(format!("invalid snapshot JSON output: {e}"))
            })?;
            return Ok(meta);
        }
    }
    Err(DevError::Verify("snapshot binary did not emit JSON metadata line".into()))
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
        "--id", &snapshot.id,
        "--output", &output_str,
        "--width", &width_str,
        "--height", &height_str,
    ];

    // Pass font paths.
    let font_strs: Vec<String> = snapshot.fonts.iter()
        .map(|f| project_root.join(f).display().to_string())
        .collect();
    for font in &font_strs {
        args.push("--font");
        args.push(font);
    }

    // Pass optional font paths.
    let opt_font_strs: Vec<String> = snapshot.optional_fonts.as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|f| project_root.join(f).display().to_string())
        .collect();
    for font in &opt_font_strs {
        args.push("--optional-font");
        args.push(font);
    }

    sluggrs_msg(&format!("  rendering {}", snapshot.id));

    let captured = output::run_captured(&binary_str, &args, project_root)?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        sluggrs_msg(&format!("  ERROR: {}", stderr.lines().next().unwrap_or("unknown error")));
        db.insert_result(run_id, &snapshot.id, None, compare::Status::Error.as_str())?;
        return Ok((SnapshotOutcome {
            pixel_diff_pct: None,
            status: compare::Status::Error,
        }, None));
    }

    // Parse adapter name from stdout JSON.
    let meta = parse_snapshot_metadata(&captured.stdout)?;
    let adapter_name = meta.adapter;

    if !output_png.exists() {
        db.insert_result(run_id, &snapshot.id, None, compare::Status::Error.as_str())?;
        return Ok((SnapshotOutcome {
            pixel_diff_pct: None,
            status: compare::Status::Error,
        }, Some(adapter_name)));
    }

    // If no approved baseline exists, record as FAIL_THRESHOLD (unapproved).
    if !approved_png.exists() {
        db.insert_result(run_id, &snapshot.id, None, compare::Status::FailThreshold.as_str())?;
        sluggrs_msg(&format!("    no approved baseline for {}", snapshot.id));
        return Ok((SnapshotOutcome {
            pixel_diff_pct: None,
            status: compare::Status::FailThreshold,
        }, Some(adapter_name)));
    }

    // Pixel diff against approved baseline.
    let pixel_threshold = config.pixel_diff_threshold;
    let approved_pixel = db.get_approval(&snapshot.id)?.map(|a| a.pixel_diff_pct);

    let pixel_result = compare::compare_pixels(&output_png, &approved_png, &diff_png);

    let (pixel_diff_pct, status) = match pixel_result {
        Ok(px) => {
            let s = compare::determine_status(
                px.diff_pct, None, pixel_threshold, None, false, approved_pixel,
            );
            (Some(px.diff_pct), s)
        }
        Err(_) => (None, compare::Status::Error),
    };

    db.insert_result(run_id, &snapshot.id, pixel_diff_pct, status.as_str())?;

    Ok((SnapshotOutcome { pixel_diff_pct, status }, Some(adapter_name)))
}

// ---------------------------------------------------------------------------
// Test command
// ---------------------------------------------------------------------------

pub(crate) fn test(
    project: Project,
    project_root: &Path,
    sluggrs_config: &SluggrsConfig,
    snapshot_id: Option<&str>,
    all: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Sluggrs, "sluggrs test")?;

    let db = open_db(project_root)?;
    let git_info = git::collect(project_root)?;
    let snapshots = resolve_snapshots(sluggrs_config, snapshot_id, all)?;

    let binary = build_snapshot_binary(project_root)?;

    let run_id = super::generate_run_id()?;
    let short_id = &run_id[..8.min(run_id.len())];
    let dirty = !git_info.is_clean;

    // We'll update adapter_name after the first snapshot runs.
    db.insert_run(&run_id, &git_info.commit, dirty, None)?;

    let dirty_label = if dirty { ", dirty" } else { "" };
    sluggrs_msg(&format!(
        "SNAPSHOT TEST RESULTS  (run {short_id}, commit {}{dirty_label})",
        git_info.commit,
    ));
    sluggrs_msg("");
    print_table_header();

    let mut counts = [0u32; 3]; // pass, fail, error
    let mut first_adapter: Option<String> = None;

    for snapshot in &snapshots {
        let (outcome, adapter) = run_snapshot(
            &binary, snapshot, sluggrs_config, project_root, &db, &run_id,
        )?;

        if first_adapter.is_none() {
            if let Some(ref a) = adapter {
                first_adapter = Some(a.clone());
            }
        }

        let px = format_pct(outcome.pixel_diff_pct, 1);
        sluggrs_msg(&format!(
            "  {:<25} {:<9} {}", snapshot.id, px, outcome.status,
        ));

        match outcome.status {
            compare::Status::Pass => counts[0] += 1,
            compare::Status::Error => counts[2] += 1,
            _ => counts[1] += 1,
        }
    }

    // Update run with adapter name if we got one.
    if let Some(ref adapter) = first_adapter {
        db.update_run_adapter(&run_id, adapter)?;
    }

    print_run_summary(&counts)?;
    Ok(())
}

fn print_run_summary(counts: &[u32; 3]) -> Result<(), DevError> {
    sluggrs_msg(&format!("  {}", "\u{2500}".repeat(50)));

    let labels = ["passed", "failed", "error"];
    let parts: Vec<String> = counts.iter().zip(labels.iter())
        .filter(|&(&c, _)| c > 0)
        .map(|(&c, &l)| format!("{c} {l}"))
        .collect();
    sluggrs_msg(&format!("  {}", parts.join(", ")));

    if counts[1] > 0 || counts[2] > 0 {
        return Err(DevError::ExitCode(1));
    }
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
    project::require(project, Project::Sluggrs, "sluggrs list")?;

    let db = open_db(project_root)?;

    sluggrs_msg(&format!(
        "  {:<25} {:<40} {}", "ID", "Description", "Approved",
    ));
    sluggrs_msg(&format!("  {}", "\u{2500}".repeat(75)));

    for snapshot in &sluggrs_config.snapshots {
        let approved = match db.get_approval(&snapshot.id)? {
            Some(a) => format!("{:.1}%", a.pixel_diff_pct),
            None => "\u{2014}".into(),
        };
        sluggrs_msg(&format!(
            "  {:<25} {:<40} {}", snapshot.id, snapshot.description, approved,
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Approve command
// ---------------------------------------------------------------------------

pub(crate) fn approve(
    project: Project,
    project_root: &Path,
    sluggrs_config: &SluggrsConfig,
    snapshot_id: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Sluggrs, "sluggrs approve")?;

    let git_info = git::collect(project_root)?;
    if !git_info.is_clean {
        return Err(DevError::Verify("sluggrs approve requires a clean git tree".into()));
    }

    let db = open_db(project_root)?;
    let snapshot = resolve_snapshot(sluggrs_config, snapshot_id)?;

    let snap_dir = snapshot_dir(project_root, &snapshot.id);
    let output_png = snap_dir.join("output.png");
    let approved_png = snap_dir.join("approved.png");

    if !output_png.exists() {
        return Err(DevError::Verify(format!(
            "no output.png for snapshot '{}' \u{2014} run `brokkr sluggrs test` first",
            snapshot.id,
        )));
    }

    // Compute pixel diff of current output against itself (0.0%) for the
    // approval record. If there was already an approved.png we diff against
    // that to capture the actual approved delta.
    let pixel_pct = if approved_png.exists() {
        let diff_png = snap_dir.join("diff.png");
        match compare::compare_pixels(&output_png, &approved_png, &diff_png) {
            Ok(px) => px.diff_pct,
            Err(_) => 0.0,
        }
    } else {
        0.0
    };

    // Copy output.png to approved.png.
    std::fs::copy(&output_png, &approved_png)?;

    db.set_approval(&snapshot.id, &git_info.commit, pixel_pct)?;

    sluggrs_msg(&format!(
        "approved '{}' at pixel={pixel_pct:.1}% (commit {})",
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
    project::require(project, Project::Sluggrs, "sluggrs status")?;

    let db = open_db(project_root)?;
    let approvals = db.all_approvals()?;

    sluggrs_msg(&format!(
        "  {:<25} {:<11} {:<11} {:<9} {}", "Snapshot", "Approved", "Last Run", "Delta", "Status",
    ));
    sluggrs_msg(&format!("  {}", "\u{2500}".repeat(70)));

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
    let approval = approvals.iter().find(|a| a.snapshot_id == snapshot.id);
    let latest = db.latest_result_for_snapshot(&snapshot.id)?;

    let approved_str = approval
        .map(|a| format!("{:.1}%", a.pixel_diff_pct))
        .unwrap_or_else(|| "\u{2014}".into());

    let (last_run_str, delta_str, status_extra) = format_status_columns(&latest, approval);

    let base_status = latest.as_ref().map_or("\u{2014}", |r| r.status.as_str());
    sluggrs_msg(&format!(
        "  {:<25} {:<11} {:<11} {:<9} {}{}",
        snapshot.id, approved_str, last_run_str, delta_str, base_status, status_extra,
    ));

    Ok(())
}

fn format_status_columns(
    latest: &Option<super::db::ResultRow>,
    approval: Option<&super::db::Approval>,
) -> (String, String, String) {
    let Some(r) = latest else {
        return ("\u{2014}".into(), "\u{2014}".into(), String::new());
    };

    let px = format_pct(r.pixel_diff_pct, 1);

    let delta = match (r.pixel_diff_pct, approval) {
        (Some(current), Some(appr)) => {
            let d = current - appr.pixel_diff_pct;
            if d.abs() < 0.05 { "\u{2014}".into() } else { format!("{d:+.1}%") }
        }
        _ => "\u{2014}".into(),
    };

    let extra = match r.status.as_str() {
        "PASS" => {
            if let (Some(current), Some(appr)) = (r.pixel_diff_pct, approval) {
                if current < appr.pixel_diff_pct - 0.5 {
                    " (improved)".into()
                } else {
                    String::new()
                }
            } else if approval.is_none() {
                " (unapproved)".into()
            } else {
                String::new()
            }
        }
        s => format!(" ({s})"),
    };

    (px, delta, extra)
}

// ---------------------------------------------------------------------------
// Report command
// ---------------------------------------------------------------------------

pub(crate) fn report(
    project: Project,
    project_root: &Path,
    run_id: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Sluggrs, "sluggrs report")?;

    let db = open_db(project_root)?;
    let results = db.run_results(run_id)?;

    if results.is_empty() {
        return Err(DevError::Config(format!("no results found for run '{run_id}'")));
    }

    if let Some(summary) = db.run_summary(run_id)? {
        let adapter = summary.adapter_name.as_deref().unwrap_or("unknown");
        let dirty_label = if summary.dirty { ", dirty" } else { "" };
        sluggrs_msg(&format!(
            "Run {} \u{2014} commit {}{dirty_label}, adapter: {adapter}",
            &summary.run_id[..8.min(summary.run_id.len())],
            summary.commit,
        ));
        sluggrs_msg("");
    }

    print_table_header();

    for row in &results {
        let px = format_pct(row.pixel_diff_pct, 1);
        sluggrs_msg(&format!(
            "  {:<25} {:<9} {}", row.snapshot_id, px, row.status,
        ));
    }

    Ok(())
}
