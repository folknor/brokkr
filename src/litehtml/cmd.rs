//! Command dispatch for litehtml visual reference testing.

use std::path::{Path, PathBuf};

use crate::build;
use crate::error::DevError;
use crate::git;
use crate::output;
use crate::project::{self, Project};
use crate::resolve;

use super::compare;
use super::db::MechanicalDb;
use super::manifest::{Defaults, FixtureEntry, Manifest};

fn open_db(project_root: &Path) -> Result<MechanicalDb, DevError> {
    let db_path = resolve::results_db_path(project_root);
    MechanicalDb::open(&db_path)
}

struct FixtureOutcome {
    pixel_diff_pct: Option<f64>,
    element_match_pct: Option<f64>,
    status: compare::Status,
}

struct TestContext<'a> {
    binary: &'a Path,
    defaults: &'a Defaults,
    project_root: &'a Path,
    db: &'a MechanicalDb,
    run_id: &'a str,
    capture_script: Option<&'a Path>,
}

// ---------------------------------------------------------------------------
// Pipeline execution
// ---------------------------------------------------------------------------

fn run_pipeline(
    binary: &Path,
    fixture: &FixtureEntry,
    defaults: &Defaults,
    project_root: &Path,
    artifact_dir: &Path,
) -> Result<(), DevError> {
    let fixture_html = project_root.join(&fixture.path);
    if !fixture_html.exists() {
        return Err(DevError::Config(format!(
            "fixture HTML not found: {}", fixture_html.display(),
        )));
    }

    let binary_str = binary.display().to_string();
    let html_str = fixture_html.display().to_string();
    let output_dir_str = artifact_dir.display().to_string();

    let mode = fixture.mode.as_deref()
        .unwrap_or(&defaults.mode);

    let mut args = vec![&html_str as &str, "--output-dir", &output_dir_str];
    if mode == "ahem" {
        args.push("--fixture");
    }

    output::litehtml_msg(&format!("  rendering {}", fixture.id));

    let captured = output::run_captured(&binary_str, &args, project_root)?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Verify(format!(
            "pipeline failed for {}: {stderr}", fixture.id,
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-fixture test
// ---------------------------------------------------------------------------

fn run_fixture(
    ctx: &TestContext,
    fixture: &FixtureEntry,
    artifact_dir: &Path,
) -> Result<FixtureOutcome, DevError> {
    let reference_dir = ctx.project_root.join("fixtures/reference").join(&fixture.id);
    let ref_png = reference_dir.join("chrome.png");
    let ref_json = reference_dir.join("chrome.json");

    // Auto-capture or recapture Chrome reference if needed.
    let needs_capture = !ref_png.exists() || ctx.capture_script.is_some();
    if needs_capture {
        if let Some(script) = ctx.capture_script {
            capture_fixture(fixture, ctx.defaults, ctx.project_root, script)?;
        } else {
            let script = write_capture_script(ctx.project_root)?;
            capture_fixture(fixture, ctx.defaults, ctx.project_root, &script)?;
            drop(std::fs::remove_file(&script));
        }
    }

    if !ref_png.exists() {
        ctx.db.insert_result(
            ctx.run_id, &fixture.id, None, None, compare::Status::Error.as_str(),
            Some(&artifact_dir.display().to_string()),
        )?;
        return Ok(FixtureOutcome {
            pixel_diff_pct: None, element_match_pct: None, status: compare::Status::Error,
        });
    }

    // Run the pipeline to produce pipeline.png + pipeline.json.
    run_pipeline(ctx.binary, fixture, ctx.defaults, ctx.project_root, artifact_dir)?;

    let pipeline_png = artifact_dir.join("pipeline.png");
    let pipeline_json = artifact_dir.join("pipeline.json");
    let diff_png = artifact_dir.join("diff.png");

    if !pipeline_png.exists() {
        ctx.db.insert_result(
            ctx.run_id, &fixture.id, None, None, compare::Status::Error.as_str(),
            Some(&artifact_dir.display().to_string()),
        )?;
        return Ok(FixtureOutcome {
            pixel_diff_pct: None, element_match_pct: None, status: compare::Status::Error,
        });
    }

    // Compare pipeline output against Chrome reference.
    let pixel_threshold = fixture.resolved_pixel_threshold(ctx.defaults);
    let element_threshold = if fixture.waive_element_threshold {
        None
    } else {
        Some(fixture.resolved_element_threshold(ctx.defaults))
    };
    let expected_fail = fixture.expected == "fail";
    let approved_pixel = ctx.db.get_approval(&fixture.id)?.map(|a| a.pixel_diff_pct);

    let pixel_result = compare::compare_pixels(&pipeline_png, &ref_png, &diff_png);
    let element_result = if ref_json.exists() && pipeline_json.exists() {
        Some(compare::compare_elements(&pipeline_json, &ref_json))
    } else {
        None
    };

    let (pixel_diff_pct, element_match_pct, status) = match pixel_result {
        Ok(px) => {
            let elem_pct = match element_result {
                Some(Ok(ref em)) => Some(em.match_pct),
                _ => None,
            };
            let s = compare::determine_status(
                px.diff_pct, elem_pct, pixel_threshold, element_threshold,
                expected_fail, approved_pixel,
            );
            (Some(px.diff_pct), elem_pct, s)
        }
        Err(_) => (None, None, compare::Status::Error),
    };

    ctx.db.insert_result(
        ctx.run_id, &fixture.id, pixel_diff_pct, element_match_pct,
        status.as_str(), Some(&artifact_dir.display().to_string()),
    )?;

    Ok(FixtureOutcome { pixel_diff_pct, element_match_pct, status })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_pct(v: Option<f64>, decimals: usize) -> String {
    match v {
        Some(val) => format!("{val:.decimals$}%"),
        None => "\u{2014}".into(),
    }
}

fn print_table_header() {
    output::litehtml_msg(&format!(
        "  {:<25} {:<9} {:<11} {}", "Fixture", "Pixels", "Elements", "Status",
    ));
    output::litehtml_msg(&format!("  {}", "\u{2500}".repeat(60)));
}

fn resolve_fixture<'a>(manifest: &'a Manifest, id: &str) -> Result<&'a FixtureEntry, DevError> {
    if let Some(entry) = manifest.fixture_by_id(id) {
        return Ok(entry);
    }
    let matches: Vec<_> = manifest.fixtures.iter()
        .filter(|f| f.id.starts_with(id))
        .collect();
    match matches.len() {
        0 => Err(DevError::Config(format!("fixture not found: {id}"))),
        1 => Ok(matches[0]),
        _ => {
            let ids: Vec<&str> = matches.iter().map(|f| f.id.as_str()).collect();
            Err(DevError::Config(format!(
                "ambiguous fixture prefix '{id}', matches: {}", ids.join(", "),
            )))
        }
    }
}

fn resolve_fixtures<'a>(
    manifest: &'a Manifest,
    fixture_id: Option<&str>,
    suite: Option<&str>,
    all: bool,
) -> Result<Vec<&'a FixtureEntry>, DevError> {
    if all {
        Ok(manifest.fixtures.iter().collect())
    } else if let Some(suite) = suite {
        let f = manifest.fixtures_for_suite(suite);
        if f.is_empty() {
            return Err(DevError::Config(format!("no fixtures tagged '{suite}'")));
        }
        Ok(f)
    } else if let Some(id) = fixture_id {
        Ok(vec![resolve_fixture(manifest, id)?])
    } else {
        Err(DevError::Config("specify a fixture ID, --suite, or --all".into()))
    }
}

fn build_pipeline(project_root: &Path) -> Result<PathBuf, DevError> {
    let config = build::BuildConfig::release(None);
    build::cargo_build(&config, project_root)
}

// ---------------------------------------------------------------------------
// Test command
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn test(
    project: Project,
    project_root: &Path,
    fixture_id: Option<&str>,
    suite: Option<&str>,
    all: bool,
    recapture: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "litehtml test")?;

    let manifest = Manifest::load(project_root)?;
    let db = open_db(project_root)?;
    let git_info = git::collect(project_root)?;
    let fixtures = resolve_fixtures(&manifest, fixture_id, suite, all)?;

    // Build the pipeline binary once.
    let binary = build_pipeline(project_root)?;

    // Write the capture script once if --recapture was requested.
    let recapture_script = if recapture {
        Some(write_capture_script(project_root)?)
    } else {
        None
    };

    let run_id = super::generate_run_id()?;
    let short_id = &run_id[..8.min(run_id.len())];
    let dirty = !git_info.is_clean;

    db.insert_run(&run_id, &git_info.commit, dirty)?;

    let artifact_base = project_root.join(".brokkr").join("mechanical").join(short_id);
    std::fs::create_dir_all(&artifact_base)?;

    let dirty_label = if dirty { ", dirty" } else { "" };
    output::litehtml_msg(&format!(
        "MECHANICAL TEST RESULTS  (run {short_id}, commit {}{dirty_label})",
        git_info.commit,
    ));
    output::litehtml_msg("");
    print_table_header();

    let ctx = TestContext {
        binary: &binary,
        defaults: &manifest.defaults,
        project_root,
        db: &db,
        run_id: &run_id,
        capture_script: recapture_script.as_deref(),
    };

    let mut counts = [0u32; 4]; // pass, fail, expected_fail, error

    for fixture in &fixtures {
        let fixture_dir = artifact_base.join(&fixture.id);
        std::fs::create_dir_all(&fixture_dir)?;

        let outcome = run_fixture(&ctx, fixture, &fixture_dir)?;

        let px = format_pct(outcome.pixel_diff_pct, 1);
        let el = format_pct(outcome.element_match_pct, 0);
        output::litehtml_msg(&format!(
            "  {:<25} {:<9} {:<11} {}", fixture.id, px, el, outcome.status,
        ));

        match outcome.status {
            compare::Status::Pass => counts[0] += 1,
            compare::Status::ExpectedFail => counts[2] += 1,
            compare::Status::Error => counts[3] += 1,
            _ => counts[1] += 1,
        }
    }

    if let Some(ref script) = recapture_script {
        drop(std::fs::remove_file(script));
    }

    print_run_summary(&counts, short_id)?;
    Ok(())
}

fn print_run_summary(counts: &[u32; 4], short_id: &str) -> Result<(), DevError> {
    output::litehtml_msg(&format!("  {}", "\u{2500}".repeat(60)));

    let labels = ["passed", "failed", "expected fail", "error"];
    let parts: Vec<String> = counts.iter().zip(labels.iter())
        .filter(|&(&c, _)| c > 0)
        .map(|(&c, &l)| format!("{c} {l}"))
        .collect();
    output::litehtml_msg(&format!("  {}", parts.join(", ")));
    output::litehtml_msg(&format!("\n  Artifacts: .brokkr/mechanical/{short_id}/"));

    if counts[1] > 0 || counts[3] > 0 {
        return Err(DevError::ExitCode(1));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// List command
// ---------------------------------------------------------------------------

pub(crate) fn list(project: Project, project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "litehtml list")?;

    let manifest = Manifest::load(project_root)?;
    let db = open_db(project_root)?;

    output::litehtml_msg(&format!(
        "  {:<25} {:<30} {:<10} {}", "ID", "Tags", "Expected", "Approved",
    ));
    output::litehtml_msg(&format!("  {}", "\u{2500}".repeat(75)));

    for fixture in &manifest.fixtures {
        let tags = fixture.tags.join(", ");
        let approved = match db.get_approval(&fixture.id)? {
            Some(a) => format!("{:.1}%", a.pixel_diff_pct),
            None => "\u{2014}".into(),
        };
        output::litehtml_msg(&format!(
            "  {:<25} {:<30} {:<10} {}", fixture.id, tags, fixture.expected, approved,
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Chrome capture (embedded JS)
// ---------------------------------------------------------------------------

const CAPTURE_JS: &str = r#"
const puppeteer = require('puppeteer');
const path = require('path');
const fs = require('fs');

const htmlPath = process.argv[2];
const pngPath = process.argv[3];
const jsonPath = process.argv[4];
const viewportWidth = parseInt(process.argv[5] || '800', 10);

if (!htmlPath || !pngPath || !jsonPath) {
  console.error('Usage: node capture.js <html> <png> <json> <width>');
  process.exit(1);
}

const absPath = path.resolve(htmlPath);

(async () => {
  const browser = await puppeteer.launch({
    headless: 'new',
    args: ['--no-sandbox', '--disable-setuid-sandbox'],
  });
  const page = await browser.newPage();
  await page.setViewport({ width: viewportWidth, height: 600 });
  await page.goto('file://' + absPath, { waitUntil: 'networkidle0', timeout: 10000 });

  const elements = await page.evaluate(() => {
    const results = [];
    function walk(node, parentPath) {
      if (node.nodeType !== Node.ELEMENT_NODE) return;
      const cs = window.getComputedStyle(node);
      const tag = node.tagName.toLowerCase();
      if (cs.display === 'none') return;

      let sibIdx = 0;
      let sib = node.previousElementSibling;
      while (sib) {
        if (sib.tagName === node.tagName) sibIdx++;
        sib = sib.previousElementSibling;
      }
      const nodePath = parentPath ? `${parentPath}>${tag}[${sibIdx}]` : tag;
      const rect = node.getBoundingClientRect();

      results.push({
        path: nodePath,
        tag,
        x: Math.round(rect.left * 10) / 10,
        y: Math.round(rect.top * 10) / 10,
        w: Math.round(rect.width * 10) / 10,
        h: Math.round(rect.height * 10) / 10,
      });

      for (const child of node.children) {
        walk(child, nodePath);
      }
    }
    walk(document.documentElement, '');
    return results;
  });

  fs.mkdirSync(path.dirname(jsonPath), { recursive: true });
  fs.writeFileSync(jsonPath, JSON.stringify(elements));
  console.error(`Extracted ${elements.length} elements`);

  const bodyHeight = await page.evaluate(() => document.body.scrollHeight);
  const height = Math.min(bodyHeight, 10000);
  await page.setViewport({ width: viewportWidth, height });
  fs.mkdirSync(path.dirname(pngPath), { recursive: true });
  await page.screenshot({ path: pngPath, fullPage: true });
  console.error(`Screenshot: ${height}px`);

  await browser.close();
})();
"#;

fn write_capture_script(project_root: &Path) -> Result<PathBuf, DevError> {
    let script_path = project_root.join(".brokkr").join("capture.js");
    std::fs::create_dir_all(script_path.parent().ok_or_else(|| {
        DevError::Config("cannot determine .brokkr directory".into())
    })?)?;
    std::fs::write(&script_path, CAPTURE_JS)?;
    Ok(script_path)
}

fn capture_fixture(
    fixture: &FixtureEntry,
    defaults: &Defaults,
    project_root: &Path,
    capture_script: &Path,
) -> Result<(), DevError> {
    let reference_dir = project_root.join("fixtures/reference").join(&fixture.id);
    std::fs::create_dir_all(&reference_dir)?;

    let viewport = fixture.viewport_width.unwrap_or(defaults.viewport_width);
    output::litehtml_msg(&format!("  capturing {} ({viewport}px)", fixture.id));

    let fixture_html = project_root.join(&fixture.path);
    if !fixture_html.exists() {
        return Err(DevError::Config(format!(
            "fixture HTML not found: {}", fixture_html.display(),
        )));
    }

    let chrome_png = reference_dir.join("chrome.png");
    let chrome_json = reference_dir.join("chrome.json");

    let script_str = capture_script.display().to_string();
    let html_str = fixture_html.display().to_string();
    let png_str = chrome_png.display().to_string();
    let json_str = chrome_json.display().to_string();
    let vp_str = viewport.to_string();

    let captured = output::run_captured(
        "node", &[&script_str, &html_str, &png_str, &json_str, &vp_str], project_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Verify(format!(
            "Chrome capture failed for {}: {stderr}", fixture.id,
        )));
    }

    output::litehtml_msg(&format!("    \u{2192} {}", reference_dir.display()));
    Ok(())
}

// ---------------------------------------------------------------------------
// Approve command
// ---------------------------------------------------------------------------

pub(crate) fn approve(
    project: Project,
    project_root: &Path,
    fixture_id: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "litehtml approve")?;

    let git_info = git::collect(project_root)?;
    if !git_info.is_clean {
        return Err(DevError::Verify("litehtml approve requires a clean git tree".into()));
    }

    let manifest = Manifest::load(project_root)?;
    let db = open_db(project_root)?;

    let fixture = resolve_fixture(&manifest, fixture_id)?;

    let result = db.latest_result_for_fixture(&fixture.id)?.ok_or_else(|| {
        DevError::Verify(format!(
            "no test results for fixture '{}' \u{2014} run `brokkr litehtml test` first",
            fixture.id,
        ))
    })?;

    let pixel_pct = result.pixel_diff_pct.unwrap_or(0.0);
    let element_pct = result.element_match_pct;

    db.set_approval(&fixture.id, &git_info.commit, pixel_pct, element_pct)?;

    output::litehtml_msg(&format!(
        "approved '{}' at pixel={pixel_pct:.1}%{} (commit {})",
        fixture.id,
        element_pct.map(|e| format!(", elements={e:.0}%")).unwrap_or_default(),
        git_info.commit,
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Report command
// ---------------------------------------------------------------------------

pub(crate) fn report(
    project: Project,
    project_root: &Path,
    run_id: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "litehtml report")?;

    let db = open_db(project_root)?;
    let results = db.run_results(run_id)?;

    if results.is_empty() {
        return Err(DevError::Config(format!("no results found for run '{run_id}'")));
    }

    print_table_header();

    for row in &results {
        let px = format_pct(row.pixel_diff_pct, 1);
        let el = format_pct(row.element_match_pct, 0);
        output::litehtml_msg(&format!(
            "  {:<25} {:<9} {:<11} {}", row.fixture_id, px, el, row.status,
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Status command
// ---------------------------------------------------------------------------

pub(crate) fn status(project: Project, project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "litehtml status")?;

    let manifest = Manifest::load(project_root)?;
    let db = open_db(project_root)?;
    let approvals = db.all_approvals()?;

    output::litehtml_msg(&format!(
        "  {:<25} {:<11} {:<11} {:<9} {}", "Fixture", "Approved", "Last Run", "Delta", "Status",
    ));
    output::litehtml_msg(&format!("  {}", "\u{2500}".repeat(70)));

    for fixture in &manifest.fixtures {
        print_fixture_status(fixture, &approvals, &db)?;
    }

    Ok(())
}

fn print_fixture_status(
    fixture: &FixtureEntry,
    approvals: &[super::db::Approval],
    db: &MechanicalDb,
) -> Result<(), DevError> {
    let approval = approvals.iter().find(|a| a.fixture_id == fixture.id);
    let latest = db.latest_result_for_fixture(&fixture.id)?;

    let approved_str = approval
        .map(|a| format!("{:.1}%", a.pixel_diff_pct))
        .unwrap_or_else(|| "\u{2014}".into());

    let (last_run_str, delta_str, status_extra) = format_status_columns(&latest, approval);

    let base_status = latest.as_ref().map_or("\u{2014}", |r| r.status.as_str());
    output::litehtml_msg(&format!(
        "  {:<25} {:<11} {:<11} {:<9} {}{}",
        fixture.id, approved_str, last_run_str, delta_str, base_status, status_extra,
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
