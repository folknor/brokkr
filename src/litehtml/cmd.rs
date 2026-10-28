//! Command dispatch for litehtml visual reference testing.

use std::path::{Path, PathBuf};

use crate::build;
use crate::cli::{LitehtmlVisualArgs, VisualArgs};
use crate::config::project_config::litehtml::{
    ExpectedOutcome, LitehtmlConfig, LitehtmlFixture, PipelineMode,
};
use crate::error::DevError;
use crate::git;
use crate::output;
use crate::user_dirs;
use crate::visual::engine::{self, Subject, SubjectItem, SummaryOptions};

use super::compare;
use super::db::{MechanicalDb, RunCompletion};

fn open_db(project_root: &Path) -> Result<MechanicalDb, DevError> {
    engine::open_results_db(project_root, MechanicalDb::open)
}

fn open_db_read_only(project_root: &Path) -> Result<MechanicalDb, DevError> {
    engine::open_results_db(project_root, MechanicalDb::open_read_only)
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

/// The bundled capture script's path in the per-user runtime cache.
struct CaptureScript {
    path: PathBuf,
}

impl CaptureScript {
    fn write(project_root: &Path) -> Result<Self, DevError> {
        ensure_pinned_chrome(project_root)?;
        Ok(Self {
            path: write_capture_script()?,
        })
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

impl Subject for TestContext<'_> {
    type Item = LitehtmlFixture;
    type Outcome = FixtureOutcome;

    fn run_item(&mut self, fixture: &Self::Item) -> Result<Self::Outcome, DevError> {
        run_fixture(self, fixture)
    }

    fn status(outcome: &Self::Outcome) -> compare::Status {
        outcome.status
    }

    fn is_unexpected_pass(outcome: &Self::Outcome) -> bool {
        outcome.unexpected_pass
    }

    fn print_result(&mut self, fixture: &Self::Item, outcome: &Self::Outcome) {
        let px = engine::format_pct(outcome.pixel_diff_pct, 1);
        let el = engine::format_pct(outcome.element_match_pct, 0);
        let note = if outcome.unexpected_pass {
            " (unexpected pass: drop `expected = \"fail\"`)"
        } else {
            ""
        };
        output::litehtml_msg(&format!(
            "  {:<25} {:<9} {:<11} {}{note}",
            fixture.id, px, el, outcome.status,
        ));

        if matches!(
            outcome.status,
            compare::Status::FailThreshold | compare::Status::Regression
        ) {
            print_offenders(&fixture.id, &outcome.offenders);
        }
    }

    fn finish(&mut self, tally: &compare::StatusTally) -> Result<(), DevError> {
        // Stamp before the summary: the summary errors when fixtures failed,
        // and a run whose every fixture ran is complete however it scored.
        self.db.mark_run_complete(self.run_id)?;
        print_run_summary(tally)
    }
}

impl SubjectItem for LitehtmlFixture {
    const KIND: &'static str = "fixture";

    fn id(&self) -> &str {
        &self.id
    }
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

    let mode = fixture.resolved_mode(config);

    let mut args = vec![&html_str as &str, "--output-dir", &output_dir_str];
    if mode == PipelineMode::Ahem {
        args.push("--fixture");
    }

    output::litehtml_msg(&format!("  rendering {}", fixture.id));

    let captured = output::run_captured(&binary_str, &args, project_root)?;

    if !captured.success() {
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
/// propagating it would abort the loop and leave the run holding only the
/// fixtures before it (now stamped incomplete, so `report` labels it, but
/// still a run that tested nothing after the failure). Sluggrs has always
/// degraded the same failure to one ERROR row.
fn record_error(
    ctx: &TestContext,
    fixture: &LitehtmlFixture,
    reason: &str,
) -> Result<FixtureOutcome, DevError> {
    output::litehtml_msg(&format!("  ERROR {}: {reason}", fixture.id));
    ctx.db
        .insert_result(ctx.run_id, &fixture.id, None, None, compare::Status::Error)?;
    Ok(FixtureOutcome::error())
}

/// [`record_error`] for a failed step's error, except that a stop request is not
/// this fixture's failure: it propagates, so the run ends instead of recording
/// an ERROR row per remaining fixture, each interrupted at once.
fn record_step_error(
    ctx: &TestContext,
    fixture: &LitehtmlFixture,
    error: &DevError,
) -> Result<FixtureOutcome, DevError> {
    if matches!(error, DevError::Interrupted) {
        return Err(DevError::Interrupted);
    }
    record_error(ctx, fixture, &error.to_string())
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
            return record_step_error(ctx, fixture, &e);
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
            return record_step_error(ctx, fixture, &e);
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
        return record_step_error(ctx, fixture, &e);
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
    let expected_fail = fixture.expected == ExpectedOutcome::Fail;
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

    let status = compare::determine_status(&compare::StatusInput {
        pixel_diff_pct: px.diff_pct,
        element_match_pct: Some(em.match_pct),
        thresholds: compare::ComparisonThresholds {
            pixel: pixel_threshold,
            element: element_threshold,
        },
        expected_fail,
        approved: compare::ApprovedScores {
            pixel: approved_pixel,
            element: approved_element,
        },
    });
    let pixel_diff_pct = Some(px.diff_pct);
    let element_match_pct = Some(em.match_pct);

    ctx.db.insert_result(
        ctx.run_id,
        &fixture.id,
        pixel_diff_pct,
        element_match_pct,
        status,
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

/// Presentation limit for each table row; comparison tolerances live in
/// `compare.rs`, and the complete offender list is persisted separately.
const OFFENDER_PRINT_LIMIT: usize = 10;

fn print_offenders(fixture_id: &str, offenders: &[compare::Offender]) {
    for line in offender_messages(fixture_id, offenders, OFFENDER_PRINT_LIMIT) {
        output::litehtml_msg(&line);
    }
}

fn offender_messages(
    fixture_id: &str,
    offenders: &[compare::Offender],
    limit: usize,
) -> Vec<String> {
    let mut lines = offenders
        .iter()
        .take(limit)
        .map(|offender| format!("      {}", format_offender(offender)))
        .collect::<Vec<_>>();

    if offenders.len() > limit {
        lines.push(format!(
            "      ... and {} more (fixtures/{fixture_id}/offenders.txt)",
            offenders.len() - limit,
        ));
    }
    lines
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_pct(v: Option<f64>, decimals: usize) -> String {
    engine::format_pct(v, decimals)
}

fn print_table_header() {
    engine::print_result_header("Fixture", true, output::litehtml_msg);
}

fn resolve_fixture<'a>(
    manifest: &'a LitehtmlConfig,
    id: &str,
) -> Result<&'a LitehtmlFixture, DevError> {
    if let Some(fixture) = manifest.fixture_by_id(id) {
        return Ok(fixture);
    }
    engine::resolve_prefix(&manifest.fixtures, id)
}

fn approval_scores(
    fixture_id: &str,
    result: &super::db::ResultRow,
) -> Result<(f64, f64), DevError> {
    if result.status == compare::Status::Error {
        return Err(DevError::Verify(format!(
            "latest result for fixture '{fixture_id}' is ERROR - nothing to approve; fix it and \
             rerun `brokkr visual {fixture_id}`"
        )));
    }
    match (result.pixel_diff_pct, result.element_match_pct) {
        (Some(pixel), Some(element)) => Ok((pixel, element)),
        _ => Err(DevError::Verify(format!(
            "latest result for fixture '{fixture_id}' is {} without both a pixel and an element score \
             - nothing to approve; fix it and rerun `brokkr visual {fixture_id}`",
            result.status,
        ))),
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
    build::cargo_build(&config, build_root).map(|artifact| artifact.path)
}

// ---------------------------------------------------------------------------
// Test command
// ---------------------------------------------------------------------------

/// `project_root` is the config dir (fixtures, `.brokkr/`, the visual store);
/// `build_root` is the code tree cargo and git run in. They differ only under
/// the one-level-up layout, where using `project_root` for the build and the
/// commit stamp built and described the wrong tree.
pub(crate) fn test(
    project_root: &Path,
    build_root: &Path,
    included_config_files: &[PathBuf],
    litehtml_config: &LitehtmlConfig,
    args: &VisualArgs,
    litehtml: &LitehtmlVisualArgs,
) -> Result<(), DevError> {
    let db = open_db(project_root)?;
    let git_info = git::collect_with_config_paths(build_root, included_config_files)?;
    let fixtures = resolve_fixtures(
        litehtml_config,
        args.fixture.as_deref(),
        litehtml.suite.as_deref(),
        args.all,
    )?;

    // Build the pipeline binary once.
    let binary = build_pipeline(build_root)?;

    // Materialize the capture script once if --recapture was requested; it
    // stays in the versioned per-user cache for later runs.
    let recapture_script = if litehtml.recapture {
        Some(CaptureScript::write(project_root)?)
    } else {
        None
    };

    let run_id = crate::db::generate_uuid()?;
    let short_id = crate::db::short_uuid(&run_id);
    let dirty = !git_info.is_clean;

    db.insert_run(&run_id, git_info.commit.as_str(), dirty)?;

    let dirty_label = if dirty { ", dirty" } else { "" };
    output::litehtml_msg(&format!(
        "MECHANICAL TEST RESULTS  (run {short_id}, commit {}{dirty_label})",
        git_info.commit,
    ));
    output::litehtml_msg("");
    print_table_header();

    let mut ctx = TestContext {
        binary: &binary,
        config: litehtml_config,
        project_root,
        db: &db,
        run_id: &run_id,
        capture_script: recapture_script.as_ref().map(|s| s.path.as_path()),
    };
    engine::run(&mut ctx, &fixtures)?;

    drop(recapture_script);
    Ok(())
}

/// An unexpected pass is reported but does not fail the run: the render got
/// better, and the only thing wrong is the manifest's stale expectation.
fn print_run_summary(counts: &compare::StatusTally) -> Result<(), DevError> {
    engine::print_run_summary(
        counts,
        &SummaryOptions {
            divider_width: 60,
            show_unexpected_passes: true,
            no_baseline_hint: None,
        },
        output::litehtml_msg,
    )
}

// ---------------------------------------------------------------------------
// List command
// ---------------------------------------------------------------------------

pub(crate) fn list(project_root: &Path, litehtml_config: &LitehtmlConfig) -> Result<(), DevError> {
    let db = open_db_read_only(project_root)?;

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
        if let Some(notes) = fixture.notes.as_deref().filter(|notes| !notes.is_empty()) {
            output::litehtml_msg(&format!("    note: {notes}"));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Chrome capture script, bundled as a source asset
// ---------------------------------------------------------------------------

const CAPTURE_JS: &str = include_str!("capture.js");

fn write_capture_script() -> Result<PathBuf, DevError> {
    let dir = runtime_scripts_dir()?;
    let script_key = xxhash_rust::xxh3::xxh3_128(CAPTURE_JS.as_bytes());
    let script_path = dir.join(format!("capture-{script_key:032x}.js"));
    write_cached_file(&script_path, CAPTURE_JS)?;
    Ok(script_path)
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

    let script_str = capture_script.display().to_string();
    let html_str = fixture_html.display().to_string();
    let png_str = chrome_png.display().to_string();
    let json_str = chrome_json.display().to_string();
    let vp_str = viewport.to_string();
    let Some(cache_dir) = capture_script.parent() else {
        return Err(DevError::Internal(
            "capture script has no parent directory".into(),
        ));
    };
    let browser_cache = cache_dir.join("puppeteer").display().to_string();

    let captured = output::run_captured_with_env(
        "node",
        &[&script_str, &html_str, &png_str, &json_str, &vp_str],
        project_root,
        &[("PUPPETEER_CACHE_DIR", &browser_cache)],
    )?;

    if !captured.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        // Puppeteer pins a browser build per version; a puppeteer bump
        // quietly invalidates the previously installed Chrome.
        if stderr.contains("Could not find Chrome") {
            output::litehtml_msg(
                "  puppeteer's pinned Chrome is not installed (a puppeteer bump changes the pinned build)",
            );
            if let Some(cache_dir) = capture_script.parent() {
                output::litehtml_msg(&format!(
                    "  from {}, run `PUPPETEER_CACHE_DIR=puppeteer pnpm exec puppeteer browsers install chrome`",
                    cache_dir.display()
                ));
            }
        }
        return Err(DevError::Verify(format!(
            "Chrome capture failed for {}: {stderr}",
            fixture.id,
        )));
    }

    output::litehtml_msg(&format!("    \u{2192} {}", output::path(&fixture_dir)));
    Ok(())
}

// ---------------------------------------------------------------------------
// Approve command
// ---------------------------------------------------------------------------

/// `build_root` is the code tree whose cleanliness and commit the approval
/// is pinned to; `project_root` holds the visual store. See [`test()`].
pub(crate) fn approve(
    project_root: &Path,
    build_root: &Path,
    included_config_files: &[PathBuf],
    litehtml_config: &LitehtmlConfig,
    fixture_id: &str,
) -> Result<(), DevError> {
    let git_info = engine::collect_clean_approval("litehtml", build_root, included_config_files)?;

    let db = open_db(project_root)?;

    let fixture = resolve_fixture(litehtml_config, fixture_id)?;

    let result = db.latest_result_for_fixture(&fixture.id)?.ok_or_else(|| {
        DevError::Verify(format!(
            "no test results for fixture '{}' \u{2014} run `brokkr visual {}` first",
            fixture.id, fixture.id,
        ))
    })?;

    // Both scores or nothing. An error row is never an approval candidate,
    // and a missing element score would disable the element ratchet for good.
    let (pixel_pct, element_pct) = approval_scores(&fixture.id, &result)?;
    let run = db.run_summary(&result.run_id)?;
    engine::validate_approval_origin(
        "litehtml",
        &result.run_id,
        run.as_ref(),
        git_info.commit.as_str(),
    )?;

    db.set_approval(
        &fixture.id,
        git_info.commit.as_str(),
        pixel_pct,
        Some(element_pct),
    )?;

    output::litehtml_msg(&format!(
        "approved '{}' at pixel={pixel_pct:.1}%, elements={element_pct:.0}% (commit {})",
        fixture.id, git_info.commit,
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Report command
// ---------------------------------------------------------------------------

pub(crate) fn report(project_root: &Path, run_id: &str) -> Result<(), DevError> {
    let db = open_db_read_only(project_root)?;
    let results = db.run_results(run_id)?;

    if results.is_empty() {
        return Err(DevError::Config(format!(
            "no results found for run '{run_id}'"
        )));
    }

    match db.run_completion(run_id)? {
        RunCompletion::Incomplete => output::litehtml_msg(
            "  INCOMPLETE RUN: interrupted or aborted before every fixture ran; \
             the rows below are partial, not a full result",
        ),
        RunCompletion::Unknown => output::litehtml_msg(
            "  note: this run predates completion tracking; whether it ran every \
             fixture is unknown",
        ),
        RunCompletion::Complete => {}
    }
    print_table_header();

    for row in &results {
        let px = format_pct(row.pixel_diff_pct, 1);
        let el = format_pct(row.element_match_pct, 0);
        output::litehtml_msg(&format!(
            "  {:<25} {:<9} {:<11} {}",
            row.subject_id, px, el, row.status,
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Status command
// ---------------------------------------------------------------------------

pub(crate) fn status(
    project_root: &Path,
    litehtml_config: &LitehtmlConfig,
) -> Result<(), DevError> {
    let db = open_db_read_only(project_root)?;
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
    let approval = approvals.iter().find(|a| a.subject_id == fixture.id);
    let latest = db.latest_result_for_fixture(&fixture.id)?;

    let approved_str = approval
        .map(|a| format!("{:.1}%", a.pixel_diff_pct))
        .unwrap_or_else(|| "\u{2014}".into());

    let columns = compare::status_columns(
        latest.as_ref().map(|row| row.status),
        latest.as_ref().and_then(|row| row.pixel_diff_pct),
        approval.map(|entry| entry.pixel_diff_pct),
        true,
    );
    output::litehtml_msg(&engine::format_status_row(
        &fixture.id,
        &approved_str,
        &columns.pixel,
        &columns.delta,
        &columns.status,
        &columns.annotation,
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Script infrastructure
// ---------------------------------------------------------------------------

const PREPARE_JS: &str = include_str!("../../scripts/litehtml-prepare/prepare.js");
const PREPARE_PACKAGE_JSON: &str = include_str!("../../scripts/litehtml-prepare/package.json");
const PREPARE_PNPM_LOCK: &str = include_str!("../../scripts/litehtml-prepare/pnpm-lock.yaml");

/// Wall bound on installing the bundled scripts' package dependencies.
const PNPM_INSTALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);
/// Wall bound on downloading Puppeteer's pinned Chrome build.
const PUPPETEER_INSTALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

/// `$XDG_CACHE_HOME/brokkr/litehtml-prepare`: one subdirectory per bundled
/// script version. Regenerable - a swept cache costs one reinstall.
fn script_cache_root() -> Result<PathBuf, DevError> {
    user_dirs::litehtml_scripts_dir().ok_or_else(|| {
        DevError::Config("cannot resolve an XDG cache directory for litehtml scripts".into())
    })
}

/// The cache key of the scripts this binary carries.
fn script_cache_key() -> String {
    let cache_material =
        format!("{PREPARE_PACKAGE_JSON}\0{PREPARE_PNPM_LOCK}\0{PREPARE_JS}\0{CAPTURE_JS}");
    format!(
        "{:032x}",
        xxhash_rust::xxh3::xxh3_128(cache_material.as_bytes())
    )
}

/// Cache directories left by other brokkr builds' scripts, for `brokkr
/// clean`. Only 32-hex-digit names are candidates (the names this module
/// constructs); anything else under the root is not ours to remove.
pub(crate) fn stale_script_cache_dirs() -> Vec<PathBuf> {
    let Ok(root) = script_cache_root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let current = script_cache_key();
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let ours = name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit());
            (ours && name != current).then(|| entry.path())
        })
        .collect()
}

/// Private script/dependency cache keyed by the bundled package, lockfile,
/// and script contents. An installed brokkr binary therefore carries its
/// Node entry points instead of reaching back into its source checkout.
fn runtime_scripts_dir() -> Result<PathBuf, DevError> {
    let dir = script_cache_root()?.join(script_cache_key());
    std::fs::create_dir_all(&dir)?;
    write_cached_file(&dir.join("package.json"), PREPARE_PACKAGE_JSON)?;
    write_cached_file(&dir.join("pnpm-lock.yaml"), PREPARE_PNPM_LOCK)?;
    write_cached_file(&dir.join("prepare.js"), PREPARE_JS)?;
    Ok(dir)
}

fn write_cached_file(path: &Path, contents: &str) -> Result<(), DevError> {
    if std::fs::read(path).is_ok_and(|old| old.as_slice() == contents.as_bytes()) {
        return Ok(());
    }
    crate::atomic_write::replace(path, contents.as_bytes())?;
    Ok(())
}

/// Ensure package dependencies for the bundled litehtml scripts are installed
/// in the per-user cache, then return the prepare path.
fn ensure_litehtml_scripts(project_root: &Path) -> Result<PathBuf, DevError> {
    let dir = runtime_scripts_dir()?;
    let script = dir.join("prepare.js");
    let node_modules = dir.join("node_modules");
    let ready = dir.join(".pnpm-deps-ready");
    let installed = || ready.is_file() && node_modules.is_dir();
    if installed() {
        return Ok(script);
    }

    // The cache is per user, not per project, so two brokkr runs in different
    // trees can reach here at once. An exclusive lock on a file in the
    // version's own dir serialises the install; the loser re-checks and finds
    // it done. Released when `install_lock` drops.
    let install_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".install.lock"))?;
    install_lock.lock()?;
    if installed() {
        return Ok(script);
    }

    output::litehtml_msg("installing litehtml script dependencies...");
    let dir_str = dir.display().to_string();
    let run = output::run_captured_with_env_and_deadline(
        "pnpm",
        &["install", "--frozen-lockfile", "--dir", &dir_str],
        project_root,
        &[("PUPPETEER_SKIP_DOWNLOAD", "true")],
        PNPM_INSTALL_DEADLINE,
        None,
        crate::shutdown::ChildIsolation::Inherit,
    )?;
    if run.killed_on_deadline() || !run.captured.success() {
        drop(std::fs::remove_dir_all(&node_modules));
        let why = if run.killed_on_deadline() {
            format!("timed out after {}s", PNPM_INSTALL_DEADLINE.as_secs())
        } else {
            String::from_utf8_lossy(&run.captured.stderr)
                .trim()
                .to_owned()
        };
        return Err(DevError::Verify(format!(
            "pnpm install failed in {dir_str}: {why}"
        )));
    }

    crate::atomic_write::replace(&ready, b"ready\n")?;
    drop(install_lock);

    Ok(script)
}

/// Install Puppeteer's pinned Chrome build into this version of the script
/// cache. Preparation uses the same package cache without downloading Chrome.
fn ensure_pinned_chrome(project_root: &Path) -> Result<(), DevError> {
    let dir = runtime_scripts_dir()?;
    ensure_litehtml_scripts(project_root)?;

    let browser_cache = dir.join("puppeteer");
    let ready = dir.join(".puppeteer-browser-ready");
    let installed = || ready.is_file() && browser_cache.is_dir();
    if installed() {
        return Ok(());
    }

    let install_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".browser-install.lock"))?;
    install_lock.lock()?;
    if installed() {
        return Ok(());
    }

    output::litehtml_msg("installing Puppeteer's pinned Chrome build...");
    let dir_str = dir.display().to_string();
    let browser_cache_str = browser_cache.display().to_string();
    let browser = output::run_captured_with_env_and_deadline(
        "pnpm",
        &[
            "--dir",
            &dir_str,
            "exec",
            "puppeteer",
            "browsers",
            "install",
            "chrome",
        ],
        project_root,
        &[("PUPPETEER_CACHE_DIR", &browser_cache_str)],
        PUPPETEER_INSTALL_DEADLINE,
        None,
        crate::shutdown::ChildIsolation::Inherit,
    )?;
    if browser.killed_on_deadline() || !browser.captured.success() {
        let why = if browser.killed_on_deadline() {
            format!("timed out after {}s", PUPPETEER_INSTALL_DEADLINE.as_secs())
        } else {
            String::from_utf8_lossy(&browser.captured.stderr)
                .trim()
                .to_owned()
        };
        return Err(DevError::Verify(format!(
            "installing litehtml's pinned Chrome failed in {dir_str}: {why}"
        )));
    }
    crate::atomic_write::replace(&ready, b"ready\n")?;
    drop(install_lock);

    Ok(())
}

// ---------------------------------------------------------------------------
// Prepare command
// ---------------------------------------------------------------------------

pub(crate) fn prepare(
    project_root: &Path,
    litehtml_config: &LitehtmlConfig,
    input: &str,
    output_path: &str,
) -> Result<(), DevError> {
    let ratio = litehtml_config.effective_fallback_aspect_ratio();

    let script = ensure_litehtml_scripts(project_root)?;
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

    let state = crate::clean::layout::ProjectState::new(project_root);
    let cache_dir = state.litehtml().prepare_cache().to_path_buf();
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

    if !captured.success() {
        return Err(DevError::Verify(format!("prepare failed for {input}",)));
    }

    output::litehtml_msg(&format!("wrote {output_path}"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Extract command
// ---------------------------------------------------------------------------

pub(crate) fn extract(
    project_root: &Path,
    input: &str,
    selector: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    output_path: &str,
) -> Result<(), DevError> {
    let script = ensure_litehtml_scripts(project_root)?;
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

    if !captured.success() {
        return Err(DevError::Verify(format!("extract failed for {input}",)));
    }

    output::litehtml_msg(&format!("wrote {output_path}"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Outline command
// ---------------------------------------------------------------------------

#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
pub(crate) fn outline(
    project_root: &Path,
    input: &str,
    depth: usize,
    full: bool,
    selectors: bool,
) -> Result<(), DevError> {
    let script = ensure_litehtml_scripts(project_root)?;
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

    if !captured.success() {
        return Err(DevError::Verify(format!("outline failed for {input}",)));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::panic, clippy::unwrap_used)]

    use super::*;

    fn fixture(id: &str) -> LitehtmlFixture {
        LitehtmlFixture {
            id: id.into(),
            path: format!("fixtures/{id}.html"),
            tags: Vec::new(),
            viewport_width: None,
            mode: None,
            pixel_diff_threshold: None,
            element_match_threshold: None,
            expected: ExpectedOutcome::Pass,
            waive_element_threshold: false,
            notes: None,
        }
    }

    fn config(fixtures: Vec<LitehtmlFixture>) -> LitehtmlConfig {
        LitehtmlConfig {
            viewport_width: 800,
            mode: PipelineMode::Default,
            pixel_diff_threshold: 0.5,
            element_match_threshold: 95.0,
            fallback_aspect_ratio: None,
            fixtures,
        }
    }

    fn result(
        status: compare::Status,
        pixel: Option<f64>,
        element: Option<f64>,
    ) -> crate::litehtml::db::ResultRow {
        crate::litehtml::db::ResultRow {
            run_id: "run".into(),
            subject_id: "fixture".into(),
            pixel_diff_pct: pixel,
            element_match_pct: element,
            status,
        }
    }

    #[test]
    fn fixture_resolution_prefers_exact_ids_and_rejects_ambiguous_prefixes() {
        let cfg = config(vec![fixture("alpha"), fixture("alphabet"), fixture("beta")]);

        assert_eq!(resolve_fixture(&cfg, "alpha").unwrap().id, "alpha");
        assert_eq!(resolve_fixture(&cfg, "beta").unwrap().id, "beta");
        let error = resolve_fixture(&cfg, "alph").unwrap_err().to_string();
        assert!(error.contains("ambiguous fixture prefix 'alph'"));
        assert!(error.contains("alpha, alphabet"));
    }

    #[test]
    fn approval_requires_a_non_error_result_with_both_scores() {
        let scored = result(compare::Status::Pass, Some(1.5), Some(98.0));
        assert_eq!(approval_scores("fixture", &scored).unwrap(), (1.5, 98.0));

        let error_with_scores = result(compare::Status::Error, Some(0.0), Some(100.0));
        assert!(approval_scores("fixture", &error_with_scores).is_err());

        let missing_element = result(compare::Status::Pass, Some(1.5), None);
        assert!(approval_scores("fixture", &missing_element).is_err());
    }

    #[test]
    fn offender_display_limit_is_injectable() {
        let offenders = [
            compare::Offender {
                path: "html>body[0]>a[0]".into(),
                dx: 3.0,
                dy: 0.0,
                dw: 0.0,
                dh: 0.0,
            },
            compare::Offender {
                path: "html>body[0]>b[0]".into(),
                dx: 0.0,
                dy: 4.0,
                dw: 0.0,
                dh: 0.0,
            },
        ];

        let lines = offender_messages("sample", &offenders, 1);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("html>body[0]>a[0]"));
        assert!(lines[1].contains("... and 1 more (fixtures/sample/offenders.txt)"));
    }
}
