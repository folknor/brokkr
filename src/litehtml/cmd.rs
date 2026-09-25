//! Command dispatch for litehtml visual reference testing.

use std::path::{Path, PathBuf};

use crate::build;
use crate::config::{LitehtmlConfig, LitehtmlFixture};
use crate::error::DevError;
use crate::git;
use crate::output;
use crate::project::{self, Project};
use crate::resolve;

use super::compare;
use super::db::MechanicalDb;

fn open_db(project_root: &Path) -> Result<MechanicalDb, DevError> {
    let db_path = resolve::results_db_path(project_root);
    MechanicalDb::open(&db_path)
}

struct FixtureOutcome {
    pixel_diff_pct: Option<f64>,
    element_match_pct: Option<f64>,
    status: compare::Status,
    /// Worst-first geometry offenders from the element comparison; empty
    /// when the comparison did not run or everything was in tolerance.
    offenders: Vec<compare::Offender>,
    /// An `expected = "fail"` fixture that came out `PASS`. Still a pass
    /// (it does not fail the run), but reported separately: the manifest
    /// entry is now stale, and a silent PASS would leave it that way.
    unexpected_pass: bool,
}

impl FixtureOutcome {
    fn error() -> Self {
        Self {
            pixel_diff_pct: None,
            element_match_pct: None,
            status: compare::Status::Error,
            offenders: Vec::new(),
            unexpected_pass: false,
        }
    }
}

/// The embedded capture script on disk, removed on drop - including when a
/// capture fails partway, which used to leave `.brokkr/capture.js` behind.
struct CaptureScript {
    path: PathBuf,
}

impl CaptureScript {
    fn write(project_root: &Path) -> Result<Self, DevError> {
        Ok(Self {
            path: write_capture_script(project_root)?,
        })
    }
}

impl Drop for CaptureScript {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.path));
    }
}

struct TestContext<'a> {
    binary: &'a Path,
    config: &'a LitehtmlConfig,
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
    fixture: &LitehtmlFixture,
    config: &LitehtmlConfig,
    project_root: &Path,
    artifact_dir: &Path,
) -> Result<(), DevError> {
    let fixture_html = project_root.join(&fixture.path);
    if !fixture_html.exists() {
        return Err(DevError::Config(format!(
            "fixture HTML not found: {}",
            fixture_html.display(),
        )));
    }

    let binary_str = binary.display().to_string();
    let html_str = fixture_html.display().to_string();
    let output_dir_str = artifact_dir.display().to_string();

    let mode = fixture.mode.as_deref().unwrap_or(&config.mode);

    let mut args = vec![&html_str as &str, "--output-dir", &output_dir_str];
    if mode == "ahem" {
        args.push("--fixture");
    }

    output::litehtml_msg(&format!("  rendering {}", fixture.id));

    let captured = output::run_captured(&binary_str, &args, project_root)?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Verify(format!(
            "pipeline failed for {}: {stderr}",
            fixture.id,
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-fixture test
// ---------------------------------------------------------------------------

/// Record one fixture as `ERROR` and say why, then let the run continue.
///
/// A capture or pipeline failure is this fixture's problem, not the run's:
/// propagating it aborted the loop and left a `mechanical_runs` row holding
/// only the fixtures before it, which `report` then showed as a complete run.
/// Sluggrs has always degraded the same failure to one ERROR row.
fn record_error(
    ctx: &TestContext,
    fixture: &LitehtmlFixture,
    reason: &str,
) -> Result<FixtureOutcome, DevError> {
    output::litehtml_msg(&format!("  ERROR {}: {reason}", fixture.id));
    ctx.db.insert_result(
        ctx.run_id,
        &fixture.id,
        None,
        None,
        compare::Status::Error.as_str(),
    )?;
    Ok(FixtureOutcome::error())
}

/// Remove a file if present. Used to clear the previous run's pipeline
/// outputs before rendering, so a pipeline that stops writing one of them
/// cannot be scored against a stale copy.
fn remove_if_present(path: &Path) -> Result<(), DevError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn run_fixture(ctx: &TestContext, fixture: &LitehtmlFixture) -> Result<FixtureOutcome, DevError> {
    let fixture_dir = ctx.project_root.join("fixtures").join(&fixture.id);
    std::fs::create_dir_all(&fixture_dir)?;

    let ref_png = fixture_dir.join("chrome.png");
    let ref_json = fixture_dir.join("chrome.json");

    // Chrome reference: `--recapture` always captures. Otherwise a missing
    // reference is captured only for a fixture nobody has approved yet - its
    // first run. Once an approval exists, its numbers were measured against
    // THAT reference, so silently recapturing a deleted one would swap the
    // baseline's ground truth mid-run; that takes an explicit `--recapture`.
    if let Some(script) = ctx.capture_script {
        if let Err(e) = capture_fixture(fixture, ctx.config, ctx.project_root, script) {
            return record_error(ctx, fixture, &e.to_string());
        }
    } else if !ref_png.exists() || !ref_json.exists() {
        if ctx.db.get_approval(&fixture.id)?.is_some() {
            return record_error(
                ctx,
                fixture,
                "Chrome reference (chrome.png/chrome.json) is missing but the fixture has an \
                 approved baseline measured against it - restore it, or rerun with --recapture",
            );
        }
        let script = CaptureScript::write(ctx.project_root)?;
        if let Err(e) = capture_fixture(fixture, ctx.config, ctx.project_root, &script.path) {
            return record_error(ctx, fixture, &e.to_string());
        }
    }

    if !ref_png.exists() || !ref_json.exists() {
        return record_error(
            ctx,
            fixture,
            "capture did not produce both chrome.png and chrome.json",
        );
    }

    // Clear the previous run's outputs first: existence is the only signal
    // that the pipeline wrote them, so a leftover would be scored as fresh.
    for name in ["pipeline.png", "pipeline.json", "diff.png"] {
        remove_if_present(&fixture_dir.join(name))?;
    }

    // Run the pipeline to produce pipeline.png + pipeline.json.
    if let Err(e) = run_pipeline(
        ctx.binary,
        fixture,
        ctx.config,
        ctx.project_root,
        &fixture_dir,
    ) {
        return record_error(ctx, fixture, &e.to_string());
    }

    if !fixture_dir.join("pipeline.png").exists() || !fixture_dir.join("pipeline.json").exists() {
        return record_error(
            ctx,
            fixture,
            "pipeline exited 0 but did not write both pipeline.png and pipeline.json",
        );
    }

    score_fixture(ctx, fixture, &fixture_dir, &ref_png, &ref_json)
}

/// Compare pipeline output against the Chrome reference, persist the
/// result row and the offenders file. Split from [`run_fixture`] so the
/// capture/render orchestration and the scoring stay readable.
fn score_fixture(
    ctx: &TestContext,
    fixture: &LitehtmlFixture,
    fixture_dir: &Path,
    ref_png: &Path,
    ref_json: &Path,
) -> Result<FixtureOutcome, DevError> {
    let pipeline_png = fixture_dir.join("pipeline.png");
    let pipeline_json = fixture_dir.join("pipeline.json");
    let diff_png = fixture_dir.join("diff.png");

    let pixel_threshold = fixture.resolved_pixel_threshold(ctx.config);
    let element_threshold = if fixture.waive_element_threshold {
        None
    } else {
        Some(fixture.resolved_element_threshold(ctx.config))
    };
    let expected_fail = fixture.expected == "fail";
    let approval = ctx.db.get_approval(&fixture.id)?;
    let approved_pixel = approval.as_ref().map(|a| a.pixel_diff_pct);
    let approved_element = approval.as_ref().and_then(|a| a.element_match_pct);

    // Both comparisons are required. An element comparison that cannot run
    // (unreadable or malformed JSON) is an ERROR, not "no element score":
    // the latter enforced neither the element threshold nor the ratchet, and
    // approving it stored a NULL element baseline that disabled the element
    // ratchet for good.
    let px = match compare::compare_pixels(&pipeline_png, ref_png, &diff_png) {
        Ok(px) => px,
        Err(e) => return record_error(ctx, fixture, &format!("pixel comparison failed: {e}")),
    };
    let em = match compare::compare_elements(&pipeline_json, ref_json) {
        Ok(em) => em,
        Err(e) => return record_error(ctx, fixture, &format!("element comparison failed: {e}")),
    };
    let offenders = em.offenders;
    write_offenders_file(fixture_dir, &offenders)?;

    let status = compare::determine_status(
        px.diff_pct,
        Some(em.match_pct),
        pixel_threshold,
        element_threshold,
        expected_fail,
        approved_pixel,
        approved_element,
    );
    let pixel_diff_pct = Some(px.diff_pct);
    let element_match_pct = Some(em.match_pct);

    ctx.db.insert_result(
        ctx.run_id,
        &fixture.id,
        pixel_diff_pct,
        element_match_pct,
        status.as_str(),
    )?;

    let unexpected_pass = expected_fail && matches!(status, compare::Status::Pass);
    Ok(FixtureOutcome {
        pixel_diff_pct,
        element_match_pct,
        status,
        offenders,
        unexpected_pass,
    })
}

/// Persist the full offender list next to diff.png so triage survives
/// the run (the table prints only the top few). Removes a stale file
/// when the current run has no offenders, so it can't lie.
fn write_offenders_file(
    fixture_dir: &Path,
    offenders: &[compare::Offender],
) -> Result<(), DevError> {
    let path = fixture_dir.join("offenders.txt");
    if offenders.is_empty() {
        drop(std::fs::remove_file(&path));
        return Ok(());
    }

    let mut body = String::from(
        "# path-matched elements outside geometry tolerance, worst first\n\
         # deltas are pipeline-minus-reference; x/y parent-relative where possible\n",
    );
    for o in offenders {
        body.push_str(&format_offender(o));
        body.push('\n');
    }
    std::fs::write(&path, body)?;
    Ok(())
}

fn format_offender(o: &compare::Offender) -> String {
    format!(
        "dx {:+7.1}  dy {:+7.1}  dw {:+7.1}  dh {:+7.1}  {}",
        o.dx, o.dy, o.dw, o.dh, o.path,
    )
}

/// How many offenders the test table prints per fixture; the rest are in
/// offenders.txt.
const OFFENDER_PRINT_LIMIT: usize = 10;

fn print_offenders(fixture_id: &str, offenders: &[compare::Offender]) {
    for o in offenders.iter().take(OFFENDER_PRINT_LIMIT) {
        output::litehtml_msg(&format!("      {}", format_offender(o)));
    }

    if offenders.len() > OFFENDER_PRINT_LIMIT {
        output::litehtml_msg(&format!(
            "      ... and {} more (fixtures/{fixture_id}/offenders.txt)",
            offenders.len() - OFFENDER_PRINT_LIMIT,
        ));
    }
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
        "  {:<25} {:<9} {:<11} {}",
        "Fixture", "Pixels", "Elements", "Status",
    ));
    output::litehtml_msg(&format!("  {}", "\u{2500}".repeat(60)));
}

fn resolve_fixture<'a>(
    manifest: &'a LitehtmlConfig,
    id: &str,
) -> Result<&'a LitehtmlFixture, DevError> {
    if let Some(entry) = manifest.fixture_by_id(id) {
        return Ok(entry);
    }
    let matches: Vec<_> = manifest
        .fixtures
        .iter()
        .filter(|f| f.id.starts_with(id))
        .collect();
    match matches.len() {
        0 => Err(DevError::Config(format!("fixture not found: {id}"))),
        1 => Ok(matches[0]),
        _ => {
            let ids: Vec<&str> = matches.iter().map(|f| f.id.as_str()).collect();
            Err(DevError::Config(format!(
                "ambiguous fixture prefix '{id}', matches: {}",
                ids.join(", "),
            )))
        }
    }
}

fn resolve_fixtures<'a>(
    manifest: &'a LitehtmlConfig,
    fixture_id: Option<&str>,
    suite: Option<&str>,
    all: bool,
) -> Result<Vec<&'a LitehtmlFixture>, DevError> {
    // One selector, not a precedence order: `--all` used to win silently
    // over `--suite` and an ID, so a typo'd narrow run became a full one.
    let selectors =
        usize::from(all) + usize::from(suite.is_some()) + usize::from(fixture_id.is_some());
    if selectors > 1 {
        return Err(DevError::Config(
            "give exactly one of a fixture ID, --suite, or --all".into(),
        ));
    }
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
        Err(DevError::Config(
            "specify a fixture ID, --suite, or --all".into(),
        ))
    }
}

fn build_pipeline(build_root: &Path) -> Result<PathBuf, DevError> {
    let config = build::BuildConfig::release(None);
    build::cargo_build(&config, build_root)
}

// ---------------------------------------------------------------------------
// Test command
// ---------------------------------------------------------------------------

/// `project_root` is the config dir (fixtures, `.brokkr/`, results.db);
/// `build_root` is the code tree cargo and git run in. They differ only under
/// the one-level-up layout, where using `project_root` for the build and the
/// commit stamp built and described the wrong tree.
#[allow(clippy::too_many_arguments)]
pub(crate) fn test(
    project: Project,
    project_root: &Path,
    build_root: &Path,
    litehtml_config: &LitehtmlConfig,
    fixture_id: Option<&str>,
    suite: Option<&str>,
    all: bool,
    recapture: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "visual")?;

    let db = open_db(project_root)?;
    let git_info = git::collect(build_root)?;
    let fixtures = resolve_fixtures(litehtml_config, fixture_id, suite, all)?;

    // Build the pipeline binary once.
    let binary = build_pipeline(build_root)?;

    // Write the capture script once if --recapture was requested; the guard
    // removes it however the run ends.
    let recapture_script = if recapture {
        Some(CaptureScript::write(project_root)?)
    } else {
        None
    };

    let run_id = super::generate_run_id()?;
    let short_id = &run_id[..8.min(run_id.len())];
    let dirty = !git_info.is_clean;

    db.insert_run(&run_id, &git_info.commit, dirty)?;

    let dirty_label = if dirty { ", dirty" } else { "" };
    output::litehtml_msg(&format!(
        "MECHANICAL TEST RESULTS  (run {short_id}, commit {}{dirty_label})",
        git_info.commit,
    ));
    output::litehtml_msg("");
    print_table_header();

    let ctx = TestContext {
        binary: &binary,
        config: litehtml_config,
        project_root,
        db: &db,
        run_id: &run_id,
        capture_script: recapture_script.as_ref().map(|s| s.path.as_path()),
    };

    let mut counts = [0u32; 5]; // pass, fail, expected_fail, error, unexpected pass

    for fixture in &fixtures {
        let outcome = run_fixture(&ctx, fixture)?;

        let px = format_pct(outcome.pixel_diff_pct, 1);
        let el = format_pct(outcome.element_match_pct, 0);
        let note = if outcome.unexpected_pass {
            " (unexpected pass: drop `expected = \"fail\"`)"
        } else {
            ""
        };
        output::litehtml_msg(&format!(
            "  {:<25} {:<9} {:<11} {}{note}",
            fixture.id, px, el, outcome.status,
        ));

        // The single most useful debugging output for a bad element
        // score: which elements, and how far off. Quiet on PASS - a
        // within-tolerance run's stragglers are noise.
        if matches!(
            outcome.status,
            compare::Status::FailThreshold | compare::Status::Regression
        ) {
            print_offenders(&fixture.id, &outcome.offenders);
        }

        match outcome.status {
            compare::Status::Pass if outcome.unexpected_pass => counts[4] += 1,
            compare::Status::Pass => counts[0] += 1,
            compare::Status::ExpectedFail => counts[2] += 1,
            compare::Status::Error => counts[3] += 1,
            _ => counts[1] += 1,
        }
    }

    drop(recapture_script);

    print_run_summary(&counts)?;
    Ok(())
}

/// An unexpected pass is reported but does not fail the run: the render got
/// better, and the only thing wrong is the manifest's stale expectation.
fn print_run_summary(counts: &[u32; 5]) -> Result<(), DevError> {
    output::litehtml_msg(&format!("  {}", "\u{2500}".repeat(60)));

    let labels = ["passed", "failed", "expected fail", "error", "unexpected pass"];
    let parts: Vec<String> = counts
        .iter()
        .zip(labels.iter())
        .filter(|&(&c, _)| c > 0)
        .map(|(&c, &l)| format!("{c} {l}"))
        .collect();
    output::litehtml_msg(&format!("  {}", parts.join(", ")));

    if counts[1] > 0 || counts[3] > 0 {
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
    litehtml_config: &LitehtmlConfig,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "list")?;

    let db = open_db(project_root)?;

    output::litehtml_msg(&format!(
        "  {:<25} {:<30} {:<10} {}",
        "ID", "Tags", "Expected", "Approved",
    ));
    output::litehtml_msg(&format!("  {}", "\u{2500}".repeat(75)));

    for fixture in &litehtml_config.fixtures {
        let tags = fixture.tags.join(", ");
        let approved = match db.get_approval(&fixture.id)? {
            Some(a) => format!("{:.1}%", a.pixel_diff_pct),
            None => "\u{2014}".into(),
        };
        output::litehtml_msg(&format!(
            "  {:<25} {:<30} {:<10} {}",
            fixture.id, tags, fixture.expected, approved,
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
  // 30s: multi-MB data-URI emails get close to the old 10s budget.
  await page.goto('file://' + absPath, { waitUntil: 'networkidle0', timeout: 30000 });
  // networkidle0 does not cover data-URI @font-face decoding; without
  // this the measurement pass can run against fallback-font geometry.
  await page.evaluate(() => document.fonts.ready.then(() => true));

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

  // Sidecar so a silent Chrome update changing fractional line heights
  // is attributable instead of looking like mysterious drift across
  // every fixture at once. Separate file: neither JSON parser changes.
  const meta = {
    browser: await browser.version(),
    puppeteer: require('puppeteer/package.json').version,
    viewport: page.viewport(),
    timestamp: new Date().toISOString(),
  };
  const metaPath = path.join(path.dirname(jsonPath), 'chrome.meta.json');
  fs.writeFileSync(metaPath, JSON.stringify(meta, null, 2) + '\n');

  // fullPage captures the whole scrollable document; the viewport stays
  // at its measurement-pass size so viewport-dependent layout cannot
  // shift between the JSON dump and the screenshot.
  const bodyHeight = await page.evaluate(() => document.body.scrollHeight);
  fs.mkdirSync(path.dirname(pngPath), { recursive: true });
  await page.screenshot({ path: pngPath, fullPage: true });
  console.error(`Screenshot: ${bodyHeight}px`);

  await browser.close();
})();
"#;

fn write_capture_script(project_root: &Path) -> Result<PathBuf, DevError> {
    let script_path = project_root.join(".brokkr").join("capture.js");
    std::fs::create_dir_all(
        script_path
            .parent()
            .ok_or_else(|| DevError::Config("cannot determine .brokkr directory".into()))?,
    )?;
    std::fs::write(&script_path, CAPTURE_JS)?;
    Ok(script_path)
}

fn npm_global_root() -> Result<String, DevError> {
    let out = std::process::Command::new("npm")
        .args(["root", "-g"])
        .output()
        .map_err(|e| DevError::Verify(format!("cannot run `npm root -g`: {e}")))?;

    if !out.status.success() {
        return Err(DevError::Verify("npm root -g failed".into()));
    }

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn capture_fixture(
    fixture: &LitehtmlFixture,
    config: &LitehtmlConfig,
    project_root: &Path,
    capture_script: &Path,
) -> Result<(), DevError> {
    let fixture_dir = project_root.join("fixtures").join(&fixture.id);
    std::fs::create_dir_all(&fixture_dir)?;

    let viewport = fixture.viewport_width.unwrap_or(config.viewport_width);
    output::litehtml_msg(&format!("  capturing {} ({viewport}px)", fixture.id));

    let fixture_html = project_root.join(&fixture.path);
    if !fixture_html.exists() {
        return Err(DevError::Config(format!(
            "fixture HTML not found: {}",
            fixture_html.display(),
        )));
    }

    let chrome_png = fixture_dir.join("chrome.png");
    let chrome_json = fixture_dir.join("chrome.json");

    let node_path = npm_global_root()?;
    let script_str = capture_script.display().to_string();
    let html_str = fixture_html.display().to_string();
    let png_str = chrome_png.display().to_string();
    let json_str = chrome_json.display().to_string();
    let vp_str = viewport.to_string();

    let captured = output::run_captured_with_env(
        "node",
        &[&script_str, &html_str, &png_str, &json_str, &vp_str],
        project_root,
        &[("NODE_PATH", &node_path)],
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        // Puppeteer pins a browser build per version; a puppeteer bump
        // quietly invalidates the previously installed Chrome.
        if stderr.contains("Could not find Chrome") {
            output::litehtml_msg(
                "  puppeteer's pinned Chrome is not installed (a puppeteer bump changes the pinned build)",
            );
            output::litehtml_msg("  install it with: npx puppeteer browsers install chrome");
        }
        return Err(DevError::Verify(format!(
            "Chrome capture failed for {}: {stderr}",
            fixture.id,
        )));
    }

    output::litehtml_msg(&format!("    \u{2192} {}", fixture_dir.display()));
    Ok(())
}

// ---------------------------------------------------------------------------
// Approve command
// ---------------------------------------------------------------------------

/// `build_root` is the code tree whose cleanliness and commit the approval
/// is pinned to; `project_root` holds results.db. See [`test()`].
pub(crate) fn approve(
    project: Project,
    project_root: &Path,
    build_root: &Path,
    litehtml_config: &LitehtmlConfig,
    fixture_id: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "approve")?;

    let git_info = git::collect(build_root)?;
    if !git_info.is_clean {
        return Err(DevError::Verify(
            "litehtml approve requires a clean git tree".into(),
        ));
    }

    let db = open_db(project_root)?;

    let fixture = resolve_fixture(litehtml_config, fixture_id)?;

    let result = db.latest_result_for_fixture(&fixture.id)?.ok_or_else(|| {
        DevError::Verify(format!(
            "no test results for fixture '{}' \u{2014} run `brokkr visual {}` first",
            fixture.id, fixture.id,
        ))
    })?;

    // Both scores or nothing. An errored run has no pixel score, and
    // defaulting it to 0.0 turned "could not render" into a perfect
    // baseline (`approve --all` did it to every errored fixture); a missing
    // element score stored a NULL that disabled the element ratchet for good.
    let (Some(pixel_pct), Some(element_pct)) = (result.pixel_diff_pct, result.element_match_pct)
    else {
        return Err(DevError::Verify(format!(
            "latest result for fixture '{}' is {} without both a pixel and an element score \
             \u{2014} nothing to approve; fix it and rerun `brokkr visual {}`",
            fixture.id, result.status, fixture.id,
        )));
    };

    db.set_approval(&fixture.id, &git_info.commit, pixel_pct, Some(element_pct))?;

    output::litehtml_msg(&format!(
        "approved '{}' at pixel={pixel_pct:.1}%, elements={element_pct:.0}% (commit {})",
        fixture.id, git_info.commit,
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Report command
// ---------------------------------------------------------------------------

pub(crate) fn report(project: Project, project_root: &Path, run_id: &str) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "report")?;

    let db = open_db(project_root)?;
    let results = db.run_results(run_id)?;

    if results.is_empty() {
        return Err(DevError::Config(format!(
            "no results found for run '{run_id}'"
        )));
    }

    print_table_header();

    for row in &results {
        let px = format_pct(row.pixel_diff_pct, 1);
        let el = format_pct(row.element_match_pct, 0);
        output::litehtml_msg(&format!(
            "  {:<25} {:<9} {:<11} {}",
            row.fixture_id, px, el, row.status,
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Status command
// ---------------------------------------------------------------------------

pub(crate) fn status(
    project: Project,
    project_root: &Path,
    litehtml_config: &LitehtmlConfig,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "visual-status")?;

    let db = open_db(project_root)?;
    let approvals = db.all_approvals()?;

    output::litehtml_msg(&format!(
        "  {:<25} {:<11} {:<11} {:<9} {}",
        "Fixture", "Approved", "Last Run", "Delta", "Status",
    ));
    output::litehtml_msg(&format!("  {}", "\u{2500}".repeat(70)));

    for fixture in &litehtml_config.fixtures {
        print_fixture_status(fixture, &approvals, &db)?;
    }

    Ok(())
}

fn print_fixture_status(
    fixture: &LitehtmlFixture,
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
            if d.abs() < 0.05 {
                "\u{2014}".into()
            } else {
                format!("{d:+.1}%")
            }
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
// Script infrastructure
// ---------------------------------------------------------------------------

/// Directory containing the prepare/extract Node.js scripts.
fn scripts_dir() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .join("scripts")
        .join("litehtml-prepare")
}

/// Ensure pnpm dependencies are installed, then return path to prepare.js.
fn ensure_prepare_script(project_root: &Path) -> Result<PathBuf, DevError> {
    let dir = scripts_dir();
    let script = dir.join("prepare.js");

    if !script.exists() {
        return Err(DevError::Config(format!(
            "prepare script not found: {}",
            script.display(),
        )));
    }

    let node_modules = dir.join("node_modules");
    if !node_modules.exists() {
        output::litehtml_msg("installing prepare dependencies...");
        let dir_str = dir.display().to_string();
        let captured = output::run_captured("pnpm", &["install", "--dir", &dir_str], project_root)?;
        if !captured.status.success() {
            let stderr = String::from_utf8_lossy(&captured.stderr);
            return Err(DevError::Verify(format!("pnpm install failed: {stderr}",)));
        }
    }

    Ok(script)
}

// ---------------------------------------------------------------------------
// Prepare command
// ---------------------------------------------------------------------------

pub(crate) fn prepare(
    project: Project,
    project_root: &Path,
    litehtml_config: &LitehtmlConfig,
    input: &str,
    output_path: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "prepare")?;

    let script = ensure_prepare_script(project_root)?;
    let script_str = script.display().to_string();

    let input_path = project_root.join(input);
    if !input_path.exists() {
        return Err(DevError::Config(format!(
            "input file not found: {}",
            input_path.display(),
        )));
    }

    let input_str = input_path.display().to_string();
    let output_resolved = project_root.join(output_path);
    let output_str = output_resolved.display().to_string();

    let cache_dir = project_root.join(".brokkr").join("prepare-cache");
    std::fs::create_dir_all(&cache_dir)?;
    let cache_str = cache_dir.display().to_string();

    // Resolve Ahem font path: look for fixtures/Ahem.ttf in project root.
    let ahem_path = project_root.join("fixtures").join("Ahem.ttf");
    let ahem_str = ahem_path.display().to_string();
    if !ahem_path.exists() {
        return Err(DevError::Config(format!(
            "Ahem font not found: {}",
            ahem_path.display(),
        )));
    }

    let ratio = litehtml_config.fallback_aspect_ratio.unwrap_or(2.0);
    let ratio_str = ratio.to_string();

    let args = vec![
        script_str.as_str(),
        "prepare",
        &input_str,
        &output_str,
        "--cache-dir",
        &cache_str,
        "--ahem-font",
        &ahem_str,
        "--fallback-aspect-ratio",
        &ratio_str,
    ];

    output::litehtml_msg(&format!("preparing {input}"));

    let captured = output::run_captured("node", &args, project_root)?;

    // Print warnings from stderr.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    for line in stderr.lines() {
        if !line.is_empty() {
            output::litehtml_msg(&format!("  {line}"));
        }
    }

    if !captured.status.success() {
        return Err(DevError::Verify(format!("prepare failed for {input}",)));
    }

    output::litehtml_msg(&format!("wrote {output_path}"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Extract command
// ---------------------------------------------------------------------------

pub(crate) fn extract(
    project: Project,
    project_root: &Path,
    input: &str,
    selector: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    output_path: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "html-extract")?;

    let script = ensure_prepare_script(project_root)?;
    let script_str = script.display().to_string();

    let input_path = project_root.join(input);
    if !input_path.exists() {
        return Err(DevError::Config(format!(
            "input file not found: {}",
            input_path.display(),
        )));
    }

    let input_str = input_path.display().to_string();
    let output_resolved = project_root.join(output_path);
    let output_str = output_resolved.display().to_string();

    let mut args = vec![script_str.as_str(), "extract", &input_str, &output_str];

    let label: String;
    if let Some(sel) = selector {
        args.push("--selector");
        args.push(sel);
        label = format!("extracting '{sel}' from {input}");
    } else if let (Some(f), Some(t)) = (from, to) {
        args.push("--from");
        args.push(f);
        args.push("--to");
        args.push(t);
        label = format!("extracting range from {input}");
    } else {
        return Err(DevError::Config("specify --selector or --from/--to".into()));
    }

    output::litehtml_msg(&label);

    let captured = output::run_captured("node", &args, project_root)?;

    let stderr = String::from_utf8_lossy(&captured.stderr);
    for line in stderr.lines() {
        if !line.is_empty() {
            output::litehtml_msg(&format!("  {line}"));
        }
    }

    if !captured.status.success() {
        return Err(DevError::Verify(format!("extract failed for {input}",)));
    }

    output::litehtml_msg(&format!("wrote {output_path}"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Outline command
// ---------------------------------------------------------------------------

pub(crate) fn outline(
    project: Project,
    project_root: &Path,
    input: &str,
    depth: usize,
    full: bool,
    selectors: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Litehtml, "outline")?;

    let script = ensure_prepare_script(project_root)?;
    let script_str = script.display().to_string();

    let input_path = project_root.join(input);
    if !input_path.exists() {
        return Err(DevError::Config(format!(
            "input file not found: {}",
            input_path.display(),
        )));
    }

    let input_str = input_path.display().to_string();
    let depth_str = depth.to_string();

    let mut args = vec![
        script_str.as_str(),
        "outline",
        &input_str,
        "--depth",
        &depth_str,
    ];
    if full {
        args.push("--full");
    }
    if selectors {
        args.push("--selectors");
    }

    let captured = output::run_captured("node", &args, project_root)?;

    // Outline output goes to stdout from the script.
    let stdout = String::from_utf8_lossy(&captured.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }

    let stderr = String::from_utf8_lossy(&captured.stderr);
    for line in stderr.lines() {
        if !line.is_empty() {
            output::litehtml_msg(&format!("  {line}"));
        }
    }

    if !captured.status.success() {
        return Err(DevError::Verify(format!("outline failed for {input}",)));
    }

    Ok(())
}
