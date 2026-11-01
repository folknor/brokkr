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
use super::db::{MechanicalDb, ReferenceStamp, RunCompletion, recorded_width};
use super::reference::{self, Inspection, ReferenceFiles};

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

/// The pipeline's argv for one fixture. The width is always passed, even when
/// it is the pipeline's own default of 800: the Chrome reference is captured
/// at the same resolved width, and a comparison of two different widths
/// scores silently (the pixel diff pads the smaller image) rather than
/// failing. `--width` also drives the pipeline's media-query evaluation.
fn pipeline_args(html: &str, output_dir: &str, width: u32, mode: PipelineMode) -> Vec<String> {
    let mut args = vec![
        html.to_owned(),
        "--output-dir".to_owned(),
        output_dir.to_owned(),
        "--width".to_owned(),
        width.to_string(),
    ];
    if mode == PipelineMode::Ahem {
        args.push("--fixture".to_owned());
    }
    args
}

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
    let width = fixture.resolved_viewport_width(config);
    let args = pipeline_args(
        &fixture_html.display().to_string(),
        &artifact_dir.display().to_string(),
        width,
        fixture.resolved_mode(config),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    output::litehtml_msg(&format!("  rendering {} ({width}px)", fixture.id));

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
    ctx.db.insert_result(
        ctx.run_id,
        &fixture.id,
        None,
        None,
        compare::Status::Error,
        None,
    )?;
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
    if let Err(e) = std::fs::create_dir_all(&fixture_dir) {
        return record_step_error(ctx, fixture, &e.into());
    }

    let width = fixture.resolved_viewport_width(ctx.config);
    let reference_id = match ensure_reference(ctx, fixture, &fixture_dir, width) {
        Ok(Ok(id)) => id,
        Ok(Err(reason)) => return record_error(ctx, fixture, &reason),
        Err(e) => return record_step_error(ctx, fixture, &e),
    };

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

    let stamp = ReferenceStamp {
        viewport_width: width,
        reference_id: &reference_id,
    };
    score_fixture(ctx, fixture, &fixture_dir, stamp)
}

/// Make sure `fixture_dir` holds a usable Chrome reference at `width` and
/// return its id, or the reason this fixture is an `ERROR` instead.
///
/// `--recapture` always captures. Otherwise an unusable reference (missing,
/// half-published, or captured at another width) is captured only for a
/// fixture with no approval at all - its first run. Once any approval exists,
/// its numbers were measured against the reference that is there, so silently
/// replacing it would swap the baseline's ground truth; that takes an explicit
/// `--recapture`. That holds for an approval measured at another width too:
/// the approval no longer ratchets this comparison, but it is the only record
/// of the reference it was measured against.
fn ensure_reference(
    ctx: &TestContext,
    fixture: &LitehtmlFixture,
    fixture_dir: &Path,
    width: u32,
) -> Result<Result<String, String>, DevError> {
    if let Some(script) = ctx.capture_script {
        return capture_fixture(fixture, width, ctx.project_root, script).map(Ok);
    }
    match reference::inspect(fixture_dir, width)? {
        Inspection::Usable { reference_id } => Ok(Ok(reference_id)),
        Inspection::Unidentified => reference::adopt(fixture_dir, width).map(Ok),
        Inspection::Unusable(reason) => {
            if ctx.db.get_approval(&fixture.id)?.is_some() {
                return Ok(Err(format!(
                    "Chrome reference is not usable ({reason}) but the fixture has an approved \
                     baseline measured against the reference that is there - restore it, or \
                     rerun with --recapture"
                )));
            }
            let script = CaptureScript::write(ctx.project_root)?;
            capture_fixture(fixture, width, ctx.project_root, &script.path).map(Ok)
        }
    }
}

/// Compare pipeline output against the Chrome reference, persist the
/// result row and the offenders file. Split from [`run_fixture`] so the
/// capture/render orchestration and the scoring stay readable.
fn score_fixture(
    ctx: &TestContext,
    fixture: &LitehtmlFixture,
    fixture_dir: &Path,
    stamp: ReferenceStamp<'_>,
) -> Result<FixtureOutcome, DevError> {
    let reference = ReferenceFiles::in_dir(fixture_dir);
    let (ref_png, ref_json) = (reference.png.as_path(), reference.json.as_path());
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
    // An approval ratchets only the comparison it measured: one taken at
    // another width scores this run as unapproved, and says so.
    let approval = ctx.db.get_approval(&fixture.id)?.filter(|approval| {
        let approved_width = recorded_width(approval.viewport_width);
        let applies = approved_width == stamp.viewport_width;
        if !applies {
            output::litehtml_msg(&format!(
                "  note: {} was approved at {approved_width}px; at {}px that approval does not \
                 apply - approve a run at this width",
                fixture.id, stamp.viewport_width,
            ));
        }
        applies
    });
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
        Some(stamp),
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

/// An approval pins the comparison its result measured, so that result must
/// be the comparison a run would make now: scored at the width the fixture
/// resolves to, against the reference that is published now. A result from
/// before results recorded their reference, or one scored before a later
/// `--recapture` replaced it (an interrupted run's rows are not the latest
/// result, so the replaced reference's scores still are), is refused.
fn approval_comparison(
    fixture_id: &str,
    result: &super::db::ResultRow,
    width: u32,
    current: &Inspection,
) -> Result<(), DevError> {
    let rerun = format!("rerun `brokkr visual {fixture_id}` and approve that");
    let measured = recorded_width(result.viewport_width);
    if measured != width {
        return Err(DevError::Verify(format!(
            "latest result for fixture '{fixture_id}' was measured at {measured}px but the \
             fixture now renders at {width}px - {rerun}"
        )));
    }
    let Some(scored) = result.reference_id.as_deref() else {
        return Err(DevError::Verify(format!(
            "latest result for fixture '{fixture_id}' does not record which Chrome reference it \
             was scored against - {rerun}"
        )));
    };
    match current {
        Inspection::Usable { reference_id } if reference_id == scored => Ok(()),
        Inspection::Usable { .. } | Inspection::Unidentified => Err(DevError::Verify(format!(
            "the Chrome reference for fixture '{fixture_id}' was replaced after its latest \
             result was scored - {rerun}"
        ))),
        Inspection::Unusable(reason) => Err(DevError::Verify(format!(
            "the Chrome reference for fixture '{fixture_id}' is not usable ({reason}) - restore \
             it and approve again, or run `brokkr visual {fixture_id} --recapture` and approve \
             that"
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

/// Capture the Chrome reference at `width` and publish it into the fixture's
/// directory, returning its new id. The capture lands in a staging directory
/// under `.brokkr/` first, so a failed or interrupted capture never touches
/// the published reference; see `reference.rs` for the publication order.
fn capture_fixture(
    fixture: &LitehtmlFixture,
    width: u32,
    project_root: &Path,
    capture_script: &Path,
) -> Result<String, DevError> {
    let fixture_dir = project_root.join("fixtures").join(&fixture.id);
    std::fs::create_dir_all(&fixture_dir)?;

    output::litehtml_msg(&format!("  capturing {} ({width}px)", fixture.id));

    let fixture_html = project_root.join(&fixture.path);
    if !fixture_html.exists() {
        return Err(DevError::Config(format!(
            "fixture HTML not found: {}",
            fixture_html.display(),
        )));
    }

    let staging = crate::clean::layout::ProjectState::new(project_root)
        .litehtml()
        .capture_staging()
        .join(&fixture.id);
    match std::fs::remove_dir_all(&staging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    std::fs::create_dir_all(&staging)?;
    let staged = ReferenceFiles::in_dir(&staging);

    let script_str = capture_script.display().to_string();
    let html_str = fixture_html.display().to_string();
    // capture.js writes chrome.meta.json beside the JSON path it is given.
    let png_str = staged.png.display().to_string();
    let json_str = staged.json.display().to_string();
    let vp_str = width.to_string();
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

    let reference_id = reference::publish(&staging, &fixture_dir, width)
        .map_err(|e| DevError::Verify(format!("Chrome capture for {}: {e}", fixture.id)))?;
    // The staging directory is empty once published; a leftover is cleared
    // by the next capture of this fixture, so failing to remove it is not
    // this capture's failure.
    drop(std::fs::remove_dir(&staging));

    output::litehtml_msg(&format!("    \u{2192} {}", output::path(&fixture_dir)));
    Ok(reference_id)
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
    let width = fixture.resolved_viewport_width(litehtml_config);
    let fixture_dir = project_root.join("fixtures").join(&fixture.id);
    let current = reference::inspect(&fixture_dir, width).map_err(|e| {
        DevError::Verify(format!(
            "cannot read the Chrome reference for fixture '{id}': {e} - restore it and approve \
             again, or run `brokkr visual {id} --recapture` and approve that",
            id = fixture.id
        ))
    })?;
    approval_comparison(&fixture.id, &result, width, &current)?;

    db.set_approval(
        &fixture.id,
        git_info.commit.as_str(),
        pixel_pct,
        Some(element_pct),
        width,
    )?;

    output::litehtml_msg(&format!(
        "approved '{}' at pixel={pixel_pct:.1}%, elements={element_pct:.0}%, {width}px (commit {})",
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
        let width = fixture.resolved_viewport_width(litehtml_config);
        print_fixture_status(fixture, width, &approvals, &db)?;
    }

    Ok(())
}

/// A delta needs the approval, the latest result and the configuration to
/// agree on the width; anything measured at another width is shown with its
/// width and never compared, since a config edit alone does not make an old
/// result describe the current comparison.
fn print_fixture_status(
    fixture: &LitehtmlFixture,
    width: u32,
    approvals: &[super::db::Approval],
    db: &MechanicalDb,
) -> Result<(), DevError> {
    let approval = approvals.iter().find(|a| a.subject_id == fixture.id);
    let latest = db.latest_result_for_fixture(&fixture.id)?;
    let approved_width = approval.map(|a| recorded_width(a.viewport_width));
    // An ERROR row scored nothing and records no width, so it is never stale:
    // it is the current state at whatever width the run attempted.
    let latest_width = latest
        .as_ref()
        .filter(|row| row.status != compare::Status::Error)
        .map(|row| recorded_width(row.viewport_width));

    let approved_str = match (approval, approved_width) {
        (Some(a), Some(w)) if w != width => format!("{:.1}% {w}px", a.pixel_diff_pct),
        (Some(a), _) => format!("{:.1}%", a.pixel_diff_pct),
        _ => "\u{2014}".into(),
    };
    let applicable_approval = approval.filter(|_| approved_width == Some(width));

    let columns = match latest_width {
        Some(measured) if measured != width => crate::visual::status::StatusColumns {
            pixel: "\u{2014}".into(),
            delta: "\u{2014}".into(),
            status: "STALE".into(),
            annotation: format!(
                " (last run at {measured}px, fixture renders at {width}px - rerun)"
            ),
        },
        _ => compare::status_columns(
            latest.as_ref().map(|row| row.status),
            latest.as_ref().and_then(|row| row.pixel_diff_pct),
            applicable_approval.map(|entry| entry.pixel_diff_pct),
            true,
        ),
    };
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
            viewport_width: Some(800),
            reference_id: Some("reference".into()),
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

    /// The pipeline is always told the width, so it renders the comparison
    /// the Chrome reference was captured for.
    #[test]
    fn pipeline_argv_carries_the_resolved_width() {
        let mut narrow = fixture("narrow");
        narrow.viewport_width = Some(720);
        let cfg = config(vec![narrow.clone(), fixture("plain")]);

        let args = pipeline_args(
            "a.html",
            "out",
            narrow.resolved_viewport_width(&cfg),
            PipelineMode::Ahem,
        );
        assert_eq!(
            args,
            [
                "a.html",
                "--output-dir",
                "out",
                "--width",
                "720",
                "--fixture"
            ]
        );
        let args = pipeline_args(
            "b.html",
            "out",
            cfg.fixtures[1].resolved_viewport_width(&cfg),
            PipelineMode::Default,
        );
        assert_eq!(args, ["b.html", "--output-dir", "out", "--width", "800"]);
    }

    #[test]
    fn approval_needs_the_current_width_and_the_published_reference() {
        let scored = result(compare::Status::Pass, Some(1.5), Some(98.0));
        let current = Inspection::Usable {
            reference_id: "reference".into(),
        };
        assert!(approval_comparison("f", &scored, 800, &current).is_ok());

        let error = approval_comparison("f", &scored, 720, &current).unwrap_err();
        assert!(error.to_string().contains("measured at 800px"), "{error}");

        let replaced = Inspection::Usable {
            reference_id: "other".into(),
        };
        let error = approval_comparison("f", &scored, 800, &replaced).unwrap_err();
        assert!(error.to_string().contains("was replaced"), "{error}");
        assert!(approval_comparison("f", &scored, 800, &Inspection::Unidentified).is_err());
        assert!(
            approval_comparison("f", &scored, 800, &Inspection::Unusable("gone".into())).is_err()
        );

        let mut legacy = result(compare::Status::Pass, Some(1.5), Some(98.0));
        legacy.viewport_width = None;
        legacy.reference_id = None;
        let error = approval_comparison("f", &legacy, 800, &current).unwrap_err();
        assert!(error.to_string().contains("does not record"), "{error}");
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
