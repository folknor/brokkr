//! One verification run of a scenario: the run directory, the request, the
//! harness launch, the checks a process boundary can make, and the verdict
//! (`docs/commands/library.md`, "Running a scenario" onwards).
//!
//! The run holds the brokkr lock from allocation to removal (its caller
//! takes it). Its launches - the harness, then each CLI counterpart's replay
//! and digests ([`super::counterpart`]) - are strictly one at a time: each
//! is released before anything it could have changed is judged and before
//! the next is admitted, so between launches no child exists, and during
//! one the child's lease covers the whole run directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::artefact::{self, CallDoc, Outcome, Seal, Status};
use super::counterpart::{self, Stop};
use super::describe::{RoleKind, RoleSource, Scenario};
use super::identity::{self, Identity};
use crate::error::DevError;
use crate::pbfhogg::binary::{HarnessLaunch, HarnessRun, LibraryHarness, PbfhoggBinary};

/// The request document's name in the run directory.
pub(crate) const REQUEST_FILE: &str = "request.json";
/// The harness's working directory, empty before and after.
pub(crate) const CWD_DIR: &str = "cwd";
/// `BROKKR_HARNESS_ARTEFACT_DIR`.
pub(crate) const ARTEFACTS_DIR: &str = "artefacts";
/// Where output, scratch and generated roles are bound.
pub(crate) const ROLES_DIR: &str = "roles";
/// What brokkr keeps after the harness exited, made only then.
pub(crate) const EVIDENCE_DIR: &str = "evidence";

/// The id brokkr gives its seal result on each call.
const SEAL_RESULT: &str = "brokkr.seal";

/// The id of the stream result brokkr adds to a call whose describe lists
/// stream capabilities.
pub(super) const STREAM_RESULT: &str = "brokkr.stream";

/// Where a run lives.
pub(crate) struct Places<'a> {
    /// `<scratch>/library`: the run directory goes in `<scenario>/run-N/`.
    pub(crate) library_root: &'a Path,
    /// The directory the marker FIFO is made in, outside any run directory.
    pub(crate) fifo_dir: &'a Path,
}

/// The verdict on one run.
pub(crate) struct Report {
    pub(crate) passed: bool,
    /// Every failure, each a line with indented continuation lines.
    pub(crate) failures: Vec<String>,
    /// Every result, harness and brokkr, in report order.
    pub(crate) results: Vec<(Option<String>, Outcome)>,
    /// Each captured diagnostic, one line.
    pub(crate) diagnostics: Vec<String>,
    /// Each call's stream verdict, phases and tail, one line.
    pub(crate) streams: Vec<String>,
    pub(crate) wall: std::time::Duration,
    /// The run directory, kept on failure.
    pub(crate) run_dir: PathBuf,
    /// The directory was kept (a failure, or a refused removal).
    pub(crate) kept: bool,
    /// What the process ran, for a measured run whose documents were read.
    pub(crate) identity: Option<Identity>,
    /// Each call's boundaries and sink account, from its document, in
    /// describe order; empty when the documents were not read.
    pub(crate) spans: Vec<crate::sidecar::CallSpan>,
    /// Each sealed call's snapshot, by call.
    pub(crate) snapshots: BTreeMap<String, BTreeMap<String, u64>>,
    /// A profiled process's hotpath report, once judged valid.
    pub(crate) hotpath: Option<crate::db::HotpathData>,
}

/// What a measured run hands each of its processes
/// (`docs/commands/library.md`, Measuring a scenario).
pub(crate) struct Measured<'a> {
    /// This process's index in the run, from 0: the request's `iteration`.
    pub(crate) iteration: usize,
    /// Iteration 0's identity, which this one must agree with; `None` for
    /// iteration 0 itself.
    pub(crate) baseline: Option<&'a Identity>,
    /// The CLI digests earlier iterations made.
    pub(crate) reuse: &'a mut counterpart::Reuse,
    /// Each dataset role's identity, read once before iteration 0.
    pub(crate) datasets: &'a BTreeMap<String, String>,
    /// The harness build's features.
    pub(crate) features: &'a str,
    /// `--hotpath` or `--alloc`: the process writes a hotpath report into
    /// its run directory, and a valid one is part of its verdict.
    pub(crate) profiled: Option<crate::measure::ProfiledMode>,
}

impl Report {
    /// A run not yet judged, its directory kept until a pass removes it.
    fn kept(wall: std::time::Duration, run_dir: PathBuf) -> Self {
        Self {
            passed: false,
            failures: Vec::new(),
            results: Vec::new(),
            diagnostics: Vec::new(),
            streams: Vec::new(),
            wall,
            run_dir,
            kept: true,
            identity: None,
            spans: Vec::new(),
            snapshots: BTreeMap::new(),
            hotpath: None,
        }
    }
}

/// Run `scenario` once, its dataset roles bound to `datasets` (role id to
/// the resolved file); `cli` replays its calls' CLI counterparts. A
/// measured process gets `measured`: its iteration, the agreement it owes
/// iteration 0, and the reuse cache. Once the harness launch returned,
/// `trajectory` holds its capture (with the calls' boundaries when the
/// documents were read), whatever the run came to, an interrupt included.
pub(crate) fn verify(
    harness: &LibraryHarness,
    cli: &PbfhoggBinary,
    scenario: &Scenario,
    datasets: &BTreeMap<String, PathBuf>,
    places: &Places<'_>,
    measured: Option<Measured<'_>>,
    trajectory: &mut Option<crate::sidecar::SidecarData>,
) -> Result<Report, DevError> {
    let iteration = measured.as_ref().map_or(0, |m| m.iteration);
    let profiled = measured.as_ref().and_then(|m| m.profiled);
    let mut prepared = prepare(scenario, datasets, places.library_root, iteration)?;
    let report_path = prepared.run_dir.join(super::profile::REPORT_FILE);

    let mut fifo = crate::sidecar::SidecarFifo::create(
        places.fifo_dir,
        &crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES,
    )?;
    // One id past the declared calls: a census that overflows it proves an
    // undeclared one.
    fifo.limit_call_census(scenario.calls.len().saturating_add(1));
    let run = harness.run(HarnessLaunch {
        run_dir: &prepared.run_dir,
        request: &prepared.request,
        cwd: &prepared.cwd,
        artefacts: &prepared.artefacts,
        reads: std::mem::take(&mut prepared.reads),
        writes: std::mem::take(&mut prepared.writes),
        deadline: std::time::Duration::from_secs(u64::from(scenario.deadline_s)),
        fifo: &mut fifo,
        iteration,
        hotpath_report: profiled.map(|_| report_path.as_path()),
    });
    drop(fifo);
    let run = run.map_err(|e| {
        DevError::Verify(format!(
            "the library harness did not run: {e}; the run directory {} is kept",
            crate::output::path(&prepared.run_dir)
        ))
    })?;
    // The boundaries are kept once the documents were read, even when the
    // run then stops (a counterpart interrupted, the final stop check).
    let mut spans = Vec::new();
    let judged = judge_run(&run, cli, scenario, &prepared, measured, &mut spans);
    let mut data = run.data;
    data.calls = spans;
    *trajectory = Some(data);
    judged
}

/// Everything after the harness launch returned: the verdict on `run`.
#[allow(clippy::too_many_lines)]
fn judge_run(
    run: &HarnessRun,
    cli: &PbfhoggBinary,
    scenario: &Scenario,
    prepared: &Prepared,
    measured: Option<Measured<'_>>,
    spans_out: &mut Vec<crate::sidecar::CallSpan>,
) -> Result<Report, DevError> {
    let Prepared {
        run_dir,
        artefacts,
        request_bytes,
        cwd_identity,
        forbidden,
        paths,
        ..
    } = prepared;
    let (run_dir, cwd_identity) = (run_dir.clone(), *cwd_identity);
    let profiled = measured.as_ref().and_then(|m| m.profiled);
    if matches!(run.ending, crate::output::child::Ending::Interrupted) {
        if let Err(e) = &run.released {
            crate::output::warn(&e.to_string());
        }
        return Err(DevError::Interrupted);
    }

    let forbidden: Vec<&str> = forbidden.iter().map(String::as_str).collect();
    let mut report = Report::kept(run.elapsed, run_dir.clone());
    // Whatever the exit and whatever became of the run directory: a record
    // of an undeclared call is a protocol failure no loss can explain, and
    // the census is in memory.
    let undeclared = super::stream::undeclared_failure(scenario, &run.data.calls_seen);
    if let Some(why) = untouchable(run, &run_dir) {
        // A stop seen while the release waited is still this run's.
        if crate::shutdown::should_unwind() {
            return Err(DevError::Interrupted);
        }
        report.failures.push(why);
        report.failures.extend(output_failures(run));
        report.failures.extend(undeclared);
        return Ok(report);
    }

    let layout = layout_failures(
        &run_dir,
        request_bytes,
        cwd_identity,
        scenario,
        &[],
        profiled.is_some(),
    );
    let layout_ok = layout.is_empty();
    report.failures.extend(layout);
    report.failures.extend(output_failures(run));
    report.failures.extend(undeclared);
    let mut untouched = false;
    let mut baseline_note: Option<String> = None;
    if let Some(judged) = judge_ending(run, artefacts, scenario, &forbidden, &mut report) {
        // A profiled process's report, read after the release and the layout
        // check and before any counterpart launch, on exit 0 only: any other
        // exit fails the run already, so its report is not judged (and is
        // kept with the evidence when it was written).
        if let Some(mode) = profiled
            && matches!(run.ending, crate::output::child::Ending::Exited(0))
        {
            match super::profile::read(&run_dir.join(super::profile::REPORT_FILE), mode) {
                Ok(data) => report.hotpath = Some(data),
                Err(why) => report
                    .failures
                    .push(format!("the hotpath report: {}", printable(&why))),
            }
        }
        report.spans = spans(&judged);
        spans_out.clone_from(&report.spans);
        report.snapshots = judged
            .calls
            .iter()
            .filter_map(|(id, doc)| doc.snapshot.clone().map(|s| (id.clone(), s)))
            .collect();
        // A measured process's identity, read before any replay (the reuse
        // key reads it), and its agreement with iteration 0's.
        let mut measured = measured;
        let mut inputs = BTreeMap::new();
        if let Some(m) = &measured
            && layout_ok
        {
            match identify(&judged, scenario, paths, m) {
                Ok(id) => {
                    if let Some(base) = m.baseline
                        && let Some(why) = id.disagreement(base)
                    {
                        report.failures.push(format!(
                            "iteration {} disagrees with iteration 0: {}",
                            m.iteration,
                            printable(&why)
                        ));
                        baseline_note = Some(baseline_text(base, &id));
                    }
                    inputs.extend(id.datasets.clone());
                    inputs.extend(id.generated.clone());
                    report.identity = Some(id);
                }
                Err(why) => report.failures.push(format!(
                    "the process cannot be identified: {}",
                    printable(&why)
                )),
            }
        }
        let reusing = measured.as_mut().and_then(|m| {
            report.identity.as_ref().map(|_| counterpart::Reusing {
                reuse: &mut *m.reuse,
                iteration: m.iteration,
                inputs: &inputs,
            })
        });
        let relayout = |made: &[String]| {
            layout_failures(
                &run_dir,
                request_bytes,
                cwd_identity,
                scenario,
                made,
                profiled.is_some(),
            )
        };
        let cli_results = equality(
            Equal {
                cli,
                scenario,
                run_dir: &run_dir,
                paths,
                layout_ok,
                reusing,
            },
            relayout,
            &mut report,
        )?;
        // An unsafe launch (`None`) leaves the run directory alone, but the
        // documents were read before it and the stream records are in
        // memory: those results stand, without any counterpart's.
        untouched = cli_results.is_none();
        collect_results(
            &judged,
            run,
            scenario,
            &cli_results.unwrap_or_default(),
            &mut report,
        );
    }
    // A stop that landed after the child's ending was seen (while output
    // settled, the lease was released or the artefacts were read) still
    // interrupts the run, which keeps its directory.
    if crate::shutdown::should_unwind() {
        return Err(DevError::Interrupted);
    }
    if untouched {
        return Ok(report);
    }
    report.passed = report.failures.is_empty();
    if !report.passed {
        write_evidence(&run_dir, run, &report, baseline_note.as_deref());
    }
    if report.passed {
        match crate::reservations::remove_dir_all(&run_dir) {
            Ok(()) => report.kept = false,
            Err(e) => crate::output::warn(&format!(
                "kept {} after a passing run: {e}",
                crate::output::path(&run_dir)
            )),
        }
    }
    Ok(report)
}

/// Why nothing in the run directory may be read or written after the
/// harness launch: some process still holds the lease and may still change
/// it, or it is no longer a directory (a symlink put in its place; looked
/// at only after a successful release).
fn untouchable(run: &HarnessRun, run_dir: &Path) -> Option<String> {
    match &run.released {
        Err(e) => Some(format!(
            "the harness launch was not released: {e}; the run directory is left untouched"
        )),
        Ok(()) if !artefact::entry_kind(run_dir).is_ok_and(|k| k.is_some_and(|k| k.is_dir())) => {
            Some(format!(
                "{} is no longer the directory it was made as; nothing in it is read",
                crate::output::path(run_dir)
            ))
        }
        Ok(()) => None,
    }
}

/// Each call's boundaries and sink account, from its document.
fn spans(judged: &Judged) -> Vec<crate::sidecar::CallSpan> {
    let ts = |us: u64| i64::try_from(us).unwrap_or(i64::MAX);
    judged
        .calls
        .iter()
        .map(|(id, doc)| crate::sidecar::CallSpan {
            call: id.clone(),
            entry_us: ts(doc.entry_us),
            return_us: ts(doc.return_us),
            seal_us: ts(doc.seal_us),
            stream_state: doc.stream.state.name().to_owned(),
            stream_drops: doc.stream.drops,
            run_idx: 0,
        })
        .collect()
}

/// A measured process's identity from its documents: the canonical plan
/// and contexts, the generated roles as the harness left them, the dataset
/// identities and the features. Any role that cannot be identified fails.
fn identify(
    judged: &Judged,
    scenario: &Scenario,
    paths: &BTreeMap<String, PathBuf>,
    m: &Measured<'_>,
) -> Result<Identity, String> {
    let plan = identity::Document::of("plan", &judged.plan)?;
    let mut contexts = BTreeMap::new();
    for (id, doc) in &judged.calls {
        contexts.insert(id.clone(), identity::Document::of("context", &doc.context)?);
    }
    let mut generated = BTreeMap::new();
    for role in &scenario.roles {
        if role.source != RoleSource::Generated {
            continue;
        }
        let path = paths
            .get(&role.id)
            .ok_or_else(|| format!("generated role {} was never bound", role.id))?;
        generated.insert(role.id.clone(), identity::generated(path, role.kind)?);
    }
    Ok(Identity {
        plan,
        contexts,
        datasets: m.datasets.clone(),
        generated,
        features: m.features.to_owned(),
    })
}

/// What `evidence/baseline.txt` holds after a disagreement: iteration 0's
/// identity and documents beside this iteration's, since iteration 0's run
/// directory is gone.
fn baseline_text(base: &Identity, this: &Identity) -> String {
    let mut text = String::new();
    for (what, id) in [("iteration 0", base), ("this iteration", this)] {
        text.push_str(&format!("# {what}\nidentity {}\n", id.qualifier()));
        text.push_str(&format!("{} {}\n", id.plan.digest, id.plan.canonical));
        for (call, doc) in &id.contexts {
            text.push_str(&format!("{call} {} {}\n", doc.digest, doc.canonical));
        }
    }
    text
}

/// What the counterparts are judged with.
struct Equal<'a> {
    cli: &'a PbfhoggBinary,
    scenario: &'a Scenario,
    run_dir: &'a Path,
    paths: &'a BTreeMap<String, PathBuf>,
    layout_ok: bool,
    reusing: Option<counterpart::Reusing<'a>>,
}

/// The counterparts, run once the harness's documents were read, whatever
/// the run's other results. Every counterpart's result by call, after the
/// layout check `relayout`
/// ran again over what the CLI could have changed beside its own
/// directories. `None`: a launch left the run directory to some other
/// process, the failure recorded, and nothing more may touch it.
fn equality(
    on: Equal<'_>,
    relayout: impl Fn(&[String]) -> Vec<String>,
    report: &mut Report,
) -> Result<Option<BTreeMap<String, Outcome>>, DevError> {
    let Equal {
        cli,
        scenario,
        run_dir,
        paths,
        layout_ok,
        reusing,
    } = on;
    let equality = match counterpart::judge(cli, scenario, run_dir, paths, layout_ok, reusing) {
        Ok(equality) => equality,
        Err(Stop::Interrupted) => return Err(DevError::Interrupted),
        Err(Stop::Unsafe(_)) if crate::shutdown::should_unwind() => {
            return Err(DevError::Interrupted);
        }
        Err(Stop::Unsafe(why)) => {
            report.failures.push(why);
            return Ok(None);
        }
    };
    if !equality.made.is_empty() {
        report.failures.extend(relayout(&equality.made));
    }
    Ok(Some(equality.results))
}

/// A laid-out run directory, before the launch.
struct Prepared {
    run_dir: PathBuf,
    cwd: PathBuf,
    artefacts: PathBuf,
    request: PathBuf,
    request_bytes: Vec<u8>,
    cwd_identity: (u64, u64),
    reads: Vec<PathBuf>,
    writes: Vec<PathBuf>,
    /// Spellings no plan or context string may contain.
    forbidden: Vec<String>,
    /// Every role's bound path.
    paths: BTreeMap<String, PathBuf>,
}

/// Allocate the run directory and lay it out: `cwd/`, `artefacts/`,
/// `roles/` with each role in its initial state, and the request binding
/// every role.
fn prepare(
    scenario: &Scenario,
    datasets: &BTreeMap<String, PathBuf>,
    library_root: &Path,
    iteration: usize,
) -> Result<Prepared, DevError> {
    let allocated = crate::artefacts::ArtefactDir::allocate(library_root, &scenario.id, true)?;
    let lexical = allocated.path().to_path_buf();
    let run_dir = std::fs::canonicalize(&lexical)?;
    let (cwd, artefacts, roles) = (
        run_dir.join(CWD_DIR),
        run_dir.join(ARTEFACTS_DIR),
        run_dir.join(ROLES_DIR),
    );
    for dir in [&cwd, &artefacts, &roles] {
        std::fs::create_dir(dir)?;
    }

    let mut bindings: BTreeMap<String, String> = BTreeMap::new();
    let mut paths: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut forbidden: Vec<String> = Vec::new();
    for spelling in [&lexical, &run_dir] {
        push_spelling(&mut forbidden, spelling)?;
    }
    let (mut reads, mut writes) = (Vec::new(), Vec::new());
    for role in &scenario.roles {
        let path = match &role.source {
            RoleSource::Dataset { .. } => {
                let given = datasets.get(&role.id).ok_or_else(|| {
                    DevError::Internal(format!("dataset role {} was not bound", role.id))
                })?;
                push_spelling(&mut forbidden, given)?;
                let canonical = std::fs::canonicalize(given).map_err(|e| {
                    DevError::Config(format!(
                        "dataset file {} for role {}: {e}",
                        given.display(),
                        role.id
                    ))
                })?;
                push_spelling(&mut forbidden, &canonical)?;
                reads.push(canonical.clone());
                canonical
            }
            RoleSource::Generated | RoleSource::Output { .. } | RoleSource::Scratch { .. } => {
                let path = roles.join(&role.id);
                let starts_empty_dir = role.kind == RoleKind::Directory
                    && !matches!(role.source, RoleSource::Output { .. });
                if starts_empty_dir {
                    std::fs::create_dir(&path)?;
                }
                writes.push(path.clone());
                path
            }
        };
        bindings.insert(role.id.clone(), crate::utf8_path::path_string(&path)?);
        paths.insert(role.id.clone(), path);
    }
    let request_bytes = request_document(&scenario.id, iteration, &bindings)?;
    let request = run_dir.join(REQUEST_FILE);
    std::fs::write(&request, &request_bytes)?;
    let cwd_identity = identity(&cwd)?;

    Ok(Prepared {
        run_dir,
        cwd,
        artefacts,
        request,
        request_bytes,
        cwd_identity,
        reads,
        writes,
        forbidden,
        paths,
    })
}

fn push_spelling(out: &mut Vec<String>, path: &Path) -> Result<(), DevError> {
    let spelled = crate::utf8_path::path_string(path)?;
    if !out.contains(&spelled) {
        out.push(spelled);
    }
    Ok(())
}

/// The request: `protocol`, `scenario`, `iteration` and `bindings`.
fn request_document(
    scenario: &str,
    iteration: usize,
    bindings: &BTreeMap<String, String>,
) -> Result<Vec<u8>, DevError> {
    let doc = serde_json::json!({
        "protocol": super::describe::PROTOCOL,
        "scenario": scenario,
        "iteration": iteration,
        "bindings": bindings,
    });
    let mut bytes = serde_json::to_vec_pretty(&doc)
        .map_err(|e| DevError::Internal(format!("cannot write the request: {e}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn identity(path: &Path) -> Result<(u64, u64), DevError> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::symlink_metadata(path)?;
    Ok((meta.dev(), meta.ino()))
}

/// The run directory against its layout, after a released launch: exactly
/// the request (bytes unchanged), the same empty `cwd/`, `artefacts/` and
/// `roles/`, plus the directories named in `made` (the replay directories,
/// after the counterparts ran; each checked by its own launch, so only its
/// type here), and in a `profiled` run the hotpath report, absent or a
/// regular file; in `roles/` only bound role names, each absent or of its
/// kind.
fn layout_failures(
    run_dir: &Path,
    request: &[u8],
    cwd_identity: (u64, u64),
    scenario: &Scenario,
    made: &[String],
    profiled: bool,
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut fail = |why: String| failures.push(format!("run directory: {why}"));
    // Nothing is traversed through a symlink: a directory that is not one
    // is a failure, never listed.
    match artefact::entry_kind(run_dir) {
        Ok(Some(kind)) if kind.is_dir() => {}
        Ok(_) => {
            fail(format!("{} is no longer a directory", run_dir.display()));
            return failures;
        }
        Err(e) => {
            fail(e);
            return failures;
        }
    }
    let listed = match artefact::entries(run_dir) {
        Ok(listed) => listed,
        Err(e) => {
            fail(e);
            return failures;
        }
    };
    let is_dir = |name: &str| listed.iter().any(|(n, kind)| n == name && kind.is_dir());
    for (name, kind) in &listed {
        let fits = match name.as_str() {
            REQUEST_FILE => kind.is_file(),
            CWD_DIR | ARTEFACTS_DIR | ROLES_DIR => kind.is_dir(),
            super::profile::REPORT_FILE if profiled => kind.is_file(),
            other if made.iter().any(|m| m == other) => kind.is_dir(),
            other => {
                fail(format!("an unexpected entry {other:?}"));
                continue;
            }
        };
        if !fits {
            fail(format!("{name} has the wrong type"));
        }
    }
    for name in [REQUEST_FILE, CWD_DIR, ARTEFACTS_DIR, ROLES_DIR] {
        if !listed.iter().any(|(n, _)| n == name) {
            fail(format!("{name} is missing"));
        }
    }
    match artefact::read_regular(&run_dir.join(REQUEST_FILE), request.len()) {
        Ok(bytes) if bytes == request => {}
        Ok(_) | Err(_) => fail(format!("{REQUEST_FILE} was changed")),
    }
    let cwd = run_dir.join(CWD_DIR);
    let same_cwd = is_dir(CWD_DIR) && identity(&cwd).is_ok_and(|id| id == cwd_identity);
    if same_cwd {
        match artefact::entries(&cwd) {
            Ok(inside) if inside.is_empty() => {}
            Ok(_) => fail(format!(
                "{CWD_DIR}/ is not empty: the harness left something in its working directory"
            )),
            Err(e) => fail(e),
        }
    } else {
        fail(format!("{CWD_DIR}/ is not the directory brokkr made"));
    }
    if !is_dir(ROLES_DIR) {
        return failures;
    }
    let inside = match artefact::entries(&run_dir.join(ROLES_DIR)) {
        Ok(inside) => inside,
        Err(e) => {
            fail(e);
            return failures;
        }
    };
    for (name, kind) in inside {
        let role = scenario
            .roles
            .iter()
            .find(|r| r.id == name && !matches!(r.source, RoleSource::Dataset { .. }));
        let Some(role) = role else {
            fail(format!("{ROLES_DIR}/{name:?} is not a bound role"));
            continue;
        };
        let (fits, what) = match role.kind {
            RoleKind::Pbf | RoleKind::File => (kind.is_file(), "regular file"),
            RoleKind::Directory => (kind.is_dir(), "directory"),
        };
        if !fits {
            fail(format!("{ROLES_DIR}/{name} is not a {what}"));
        }
    }
    failures
}

/// stdout and stderr must be empty and complete.
fn output_failures(run: &HarnessRun) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, bytes) in [("stdout", &run.stdout), ("stderr", &run.stderr)] {
        if !bytes.is_empty() {
            failures.push(format!(
                "the harness wrote to {name}, which must stay empty:\n{}",
                text_block(&String::from_utf8_lossy(bytes))
            ));
        }
    }
    if run.output_truncated {
        failures.push("the harness output ran past what brokkr keeps".to_owned());
    }
    if run.output_cut {
        failures.push(
            "the harness output did not close after it exited, so it is not known to be empty"
                .to_owned(),
        );
    }
    failures
}

/// The harness's documents once read: the scenario-level results, the
/// plan and each call's document, in describe order.
struct Judged {
    scenario_results: Vec<Outcome>,
    plan: super::json::Json,
    calls: Vec<(String, CallDoc)>,
}

/// The exit, then on 0 and 1 the artefacts, on 2 the refusal. `Some` when
/// every document was read, so the per-call results can be collected.
fn judge_ending(
    run: &HarnessRun,
    artefacts: &Path,
    scenario: &Scenario,
    forbidden: &[&str],
    report: &mut Report,
) -> Option<Judged> {
    use crate::output::child::Ending;
    let code = match &run.ending {
        Ending::Exited(code) => *code,
        Ending::Deadline(limit) => {
            report.failures.push(format!(
                "the harness was killed at its {} deadline",
                crate::output::format_duration(*limit)
            ));
            return None;
        }
        Ending::Signalled(signal) => {
            report
                .failures
                .push(format!("the harness was killed by signal {signal}"));
            return None;
        }
        other => {
            report
                .failures
                .push(format!("the harness ended as {other:?}"));
            return None;
        }
    };
    match code {
        0 | 1 => judge_artefacts(run, code, artefacts, scenario, forbidden, report),
        2 => {
            let refusal = artefacts.join(artefact::REFUSAL_FILE);
            // Never read through an artefact directory that is not one.
            let real = artefact::entry_kind(artefacts).is_ok_and(|k| k.is_some_and(|k| k.is_dir()));
            let why = if real {
                match artefact::entry_kind(&refusal) {
                    Ok(None) => "exit 2, without a refusal document".to_owned(),
                    Ok(Some(_)) => match artefact::read_refusal(&refusal) {
                        Ok(reason) => format!("exit 2: {}", printable(&reason)),
                        Err(e) => format!("exit 2, and its refusal document is invalid: {e}"),
                    },
                    Err(e) => format!("exit 2: {e}"),
                }
            } else {
                "exit 2, and its artefact directory is not a directory".to_owned()
            };
            report
                .failures
                .push(format!("the harness could not run the request, {why}"));
            None
        }
        other => {
            report.failures.push(format!(
                "the harness exited {other}, which the protocol does not define"
            ));
            None
        }
    }
}

fn judge_artefacts(
    run: &HarnessRun,
    code: i32,
    artefacts: &Path,
    scenario: &Scenario,
    forbidden: &[&str],
    report: &mut Report,
) -> Option<Judged> {
    let invalid = |why: String| format!("the artefacts are invalid: {}", printable(&why));
    let tree = match artefact::read_tree(artefacts, scenario) {
        Ok(tree) => tree,
        Err(e) => {
            report.failures.push(invalid(e));
            return None;
        }
    };
    let scenario_doc = match artefact::parse_scenario(&tree.scenario, scenario, forbidden) {
        Ok(doc) => doc,
        Err(e) => {
            report.failures.push(invalid(e));
            return None;
        }
    };
    let mut calls: Vec<(String, CallDoc)> = Vec::new();
    for (id, bytes) in &tree.calls {
        match artefact::parse_call(bytes, id, forbidden) {
            Ok(doc) => calls.push((id.clone(), doc)),
            Err(e) => {
                report.failures.push(invalid(e));
                return None;
            }
        }
    }

    let artefact::ScenarioDoc {
        results: scenario_results,
        plan,
    } = scenario_doc;
    let call_results: Vec<(String, Vec<Outcome>)> = calls
        .iter()
        .map(|(id, doc)| (id.clone(), doc.results.clone()))
        .collect();
    let inventory = artefact::inventory_failures(scenario, &scenario_results, &call_results);
    let complete = inventory.is_empty();
    report
        .failures
        .extend(inventory.iter().map(|why| printable(why)));

    // The wall bound: each call's readings share the FIFO clock, so their
    // differences fit the process wall brokkr observed, plus a second.
    let limit_us = u64::try_from(run.elapsed.as_micros())
        .unwrap_or(u64::MAX)
        .saturating_add(1_000_000);
    for (id, doc) in &calls {
        for (what, end) in [("return_us", doc.return_us), ("seal_us", doc.seal_us)] {
            let span = end - doc.entry_us;
            if span > limit_us {
                report.failures.push(format!(
                    "call {id}: {what} - entry_us is {span} us, longer than the process ran \
                     ({} us observed, plus 1 s)",
                    limit_us - 1_000_000
                ));
            }
        }
    }

    let harness_nonpass = scenario_results
        .iter()
        .chain(calls.iter().flat_map(|(_, doc)| doc.results.iter()))
        .any(|r| r.status != Status::Pass);
    if complete {
        if code == 0 && harness_nonpass {
            report
                .failures
                .push("the harness exited 0 but reported a result that did not pass".to_owned());
        }
        if code == 1 && !harness_nonpass {
            report
                .failures
                .push("the harness exited 1 but every result it reported passed".to_owned());
        }
    }
    Some(Judged {
        scenario_results,
        plan,
        calls,
    })
}

/// Every result in report order, brokkr's seal, stream and CLI results among
/// them (`cli` holds each counterpart's, by call), each call's stream line,
/// diagnostics and returned outcome, and a failure line per result that did
/// not pass.
fn collect_results(
    judged: &Judged,
    run: &HarnessRun,
    scenario: &Scenario,
    cli: &BTreeMap<String, Outcome>,
    report: &mut Report,
) {
    let Judged {
        scenario_results,
        calls,
        ..
    } = judged;
    for result in scenario_results {
        report.results.push((None, result.clone()));
    }
    for (id, doc) in calls {
        for result in &doc.results {
            report.results.push((Some(id.clone()), result.clone()));
        }
        let (status, detail) = match &doc.seal {
            Seal::Sealed => (Status::Pass, String::new()),
            Seal::Outstanding(n) => (
                Status::Fail,
                format!(
                    "{} still outstanding",
                    crate::output::count(usize::try_from(*n).unwrap_or(usize::MAX), "block")
                ),
            ),
            Seal::Refused(reason) => (Status::Fail, format!("refused: {reason}")),
        };
        report.results.push((
            Some(id.clone()),
            Outcome {
                id: SEAL_RESULT.to_owned(),
                status,
                detail,
            },
        ));
        let stream = super::stream::judge(id, doc, &run.data.records, &run.data.summary.loss);
        let judged = scenario
            .calls
            .iter()
            .any(|c| c.id == *id && !c.stream.is_empty());
        report
            .streams
            .push(super::stream::summary_line(id, &stream, judged));
        if judged {
            report.results.push((
                Some(id.clone()),
                Outcome {
                    id: STREAM_RESULT.to_owned(),
                    status: stream.status,
                    detail: stream.detail,
                },
            ));
        }
        if let Some(result) = cli.get(id) {
            report.results.push((Some(id.clone()), result.clone()));
        }
        for d in &doc.diagnostics {
            report.diagnostics.push(format!(
                "{id} {} {}: {}{}",
                printable(&d.kind),
                printable(&d.key),
                printable(&d.message),
                if d.truncated { " (truncated)" } else { "" }
            ));
        }
        if let artefact::CallOutcome::Error { kind, message } = &doc.outcome {
            report.diagnostics.push(format!(
                "{id} returned an error, {kind}: {}",
                printable(message)
            ));
        } else if doc.outcome == artefact::CallOutcome::Cancelled {
            report.diagnostics.push(format!("{id} returned cancelled"));
        }
    }
    for (call, result) in &report.results {
        if result.status != Status::Pass {
            let name = call
                .as_ref()
                .map_or_else(|| result.id.clone(), |c| format!("{c}/{}", result.id));
            let mut line = format!("{name}: {}", result.status.name());
            if !result.detail.is_empty() {
                line.push('\n');
                line.push_str(&text_block(&result.detail));
            }
            report.failures.push(line);
        }
    }
}

/// Make evidence/ beside the run's documents and write what brokkr saw
/// there; never following an entry that already exists. A failure to write
/// it is a warning: the verdict does not depend on it.
fn write_evidence(run_dir: &Path, run: &HarnessRun, report: &Report, baseline: Option<&str>) {
    let dir = run_dir.join(EVIDENCE_DIR);
    let written = (|| -> std::io::Result<()> {
        std::fs::create_dir(&dir)?;
        let put = |name: &str, bytes: &[u8]| -> std::io::Result<()> {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dir.join(name))?;
            file.write_all(bytes)
        };
        put("stdout", &run.stdout)?;
        put("stderr", &run.stderr)?;
        put("stream.txt", stream_text(run).as_bytes())?;
        if let Some(baseline) = baseline {
            put("baseline.txt", baseline.as_bytes())?;
        }
        let mut verdict = String::new();
        for line in &report.failures {
            verdict.push_str(line);
            verdict.push('\n');
        }
        put("failures.txt", verdict.as_bytes())
    })();
    if let Err(e) = written {
        crate::output::warn(&format!(
            "could not write the run's evidence to {}: {e}",
            crate::output::path(&dir)
        ));
    }
}

/// The kept FIFO records in the wire's own spelling and in arrival order,
/// after one comment line per cause of capture loss.
fn stream_text(run: &HarnessRun) -> String {
    let mut text = String::new();
    for cause in run.data.summary.loss.causes() {
        text.push_str(&format!("# {cause}\n"));
    }
    text.push_str("# records, in arrival order\n");
    for record in &run.data.records {
        text.push_str(&record.wire());
        text.push('\n');
    }
    text
}

/// `text` safe to print on one line: every control character but tab
/// escaped, newlines included, so harness-supplied text can neither rewrite
/// the terminal nor pass for a line of brokkr's own.
pub(crate) fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() && c != '\t' {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// `text` as printable lines, each indented and marked, so a multi-line
/// text stays visibly inside the result it belongs to.
fn text_block(text: &str) -> String {
    text.trim_end_matches('\n')
        .split('\n')
        .map(|line| format!("    | {}", printable(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
