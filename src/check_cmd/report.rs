// What a `brokkr check` run's grouped green lines are built from, and the
// per-run log they are backed by.
//
// A passing run used to print one line per sweep per phase - the sweep's
// shape before each cargo run, a count after it - and a caller that runs
// `check` twenty times a session paid for all of it every time, while the
// only line it acted on was the verdict. Phases now record facts here and
// print one grouped line each when they complete. Streamed per phase rather
// than rendered once at the end: a run the watchdog kills, or one that
// panics, still shows which phases finished.
//
// Nothing is lost, only moved. The per-sweep narration (shapes, full cargo
// argv, build and fan-out times, claims) goes to the run log through
// `output::detail`, written as it happens so an incomplete run keeps
// everything up to the point it stopped.
//
// Global for the same reason `PHASE_CLOCK` is: the facts come from deep
// inside the lanes (post-join parallel reporting, per-resolution test runs),
// and there is one run per process.

/// One convention phase's green result: its name and, where it has them,
/// the counts that make the line falsifiable (a rule whose glob stopped
/// matching shows up as a shrinking number).
struct Pass {
    name: &'static str,
    counts: Option<String>,
}

#[derive(Default)]
struct TestTally {
    passed: usize,
    ignored: usize,
    filtered_out: usize,
    pkg_skipped: usize,
    /// Execution units - one cargo resolution, not one configured sweep, since
    /// a package-mode sweep runs once per package and one of them can be
    /// empty while its siblings are not - that passed having run nothing.
    empty_units: Vec<String>,
    /// (sweep, binary, wall): the slowest binary of any parallel lane.
    slowest_parallel: Option<(String, String, std::time::Duration)>,
}

#[derive(Default)]
struct Report {
    /// `Some` while the convention phases are running and collecting their
    /// passes into one line; `None` before and after, when a pass prints its
    /// own line (a pre-test script-check stage, `--textlint` selection).
    conventions: Option<Vec<Pass>>,
    test: TestTally,
    /// Distinct warning blocks with the sweeps that produced each, in
    /// first-seen order. Keyed by the whole block, never by line: a block is
    /// a diagnostic and splitting it would corrupt it.
    warnings: Vec<(String, Vec<String>)>,
}

static REPORT: std::sync::Mutex<Option<Report>> = std::sync::Mutex::new(None);

fn with_report<R>(f: impl FnOnce(&mut Report) -> R) -> Option<R> {
    let mut guard = REPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.as_mut().map(f)
}

/// Whether a `check` run's report is collecting - false under `brokkr clippy`,
/// which shares the diagnostic pipeline but prints its own verdict.
fn report_active() -> bool {
    with_report(|_| ()).is_some()
}

/// Start a fresh report. Called once per `cmd_check`.
fn report_begin() {
    *REPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Report::default());
}

fn report_end() {
    REPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
}

/// Record a green phase. Collected into the conventions line while that
/// group is open, else printed as `<name>: ok (<counts>)` on its own.
fn phase_ok(name: &'static str, counts: Option<String>) {
    let mut pending = Some(Pass { name, counts });
    with_report(|r| {
        if let Some(passes) = r.conventions.as_mut() {
            passes.extend(pending.take());
        }
    });
    if let Some(p) = pending {
        match &p.counts {
            Some(c) => output::run_msg(&format!("{}: ok ({c})", p.name)),
            None => output::run_msg(&format!("{}: ok", p.name)),
        }
    }
}

fn conventions_open() {
    with_report(|r| r.conventions = Some(Vec::new()));
}

/// Close the conventions group and print its one line. Silent when no
/// convention phase ran (all skipped or inert), and on a failing group - the
/// failing phase already printed its own detail, and the verdict names it.
fn conventions_close(passed: bool, elapsed: std::time::Duration) {
    let Some(passes) = with_report(|r| r.conventions.take()).flatten() else {
        return;
    };
    if !passed || passes.is_empty() {
        return;
    }
    let parts: Vec<String> = passes
        .iter()
        .map(|p| match &p.counts {
            Some(c) => format!("{} ({c})", p.name),
            None => p.name.to_owned(),
        })
        .collect();
    output::run_msg(&format!(
        "{}: ok in {}",
        parts.join(", "),
        fmt_wall(elapsed)
    ));
}

fn note_tests(passed: usize, ignored: usize, filtered_out: usize) {
    with_report(|r| {
        r.test.passed += passed;
        r.test.ignored += ignored;
        r.test.filtered_out += filtered_out;
    });
}

fn note_pkg_skipped(n: usize) {
    with_report(|r| r.test.pkg_skipped += n);
}

fn note_empty_unit(label: String) {
    with_report(|r| r.test.empty_units.push(label));
}

fn note_parallel_slowest(sweep: &str, binary: &str, wall: std::time::Duration) {
    with_report(|r| {
        if r.test
            .slowest_parallel
            .as_ref()
            .is_none_or(|(_, _, w)| wall > *w)
        {
            r.test.slowest_parallel = Some((sweep.to_owned(), binary.to_owned(), wall));
        }
    });
}

/// Hold a warning block for the end of its phase, merged with any identical
/// block another sweep produced. Printed at once when no report is active.
fn note_warning(block: &str, sweep: &str) {
    let held = with_report(|r| match r.warnings.iter_mut().find(|(b, _)| b == block) {
        Some((_, sweeps)) => {
            if !sweeps.iter().any(|s| s == sweep) {
                sweeps.push(sweep.to_owned());
            }
        }
        None => r.warnings.push((block.to_owned(), vec![sweep.to_owned()])),
    })
    .is_some();
    if !held {
        output::warn(block);
    }
}

/// Print every held warning once, with the sweeps that produced it when more
/// than one sweep ran - `every sweep` when that is all of them, so a reader
/// can tell a property of the code from a property of one build shape.
fn flush_warnings(sweeps_run: usize) {
    let held = with_report(|r| std::mem::take(&mut r.warnings)).unwrap_or_default();
    for (block, sweeps) in held {
        let from = if sweeps_run <= 1 {
            String::new()
        } else if sweeps.len() == sweeps_run {
            "\n  (every sweep)".to_owned()
        } else {
            format!("\n  (sweeps: {})", sweeps.join(", "))
        };
        output::warn(&format!("{block}{from}"));
    }
}

/// The test phase's grouped green line.
fn print_test_line(sweeps_run: usize, elapsed: std::time::Duration) {
    let Some(t) = with_report(|r| std::mem::take(&mut r.test)) else {
        return;
    };
    let mut extra = String::new();
    if t.ignored > 0 {
        extra.push_str(&format!(", {} ignored", t.ignored));
    }
    if t.filtered_out > 0 {
        extra.push_str(&format!(", {} filtered out", t.filtered_out));
    }
    if t.pkg_skipped > 0 {
        extra.push_str(&format!(", {} pkg-skipped", t.pkg_skipped));
    }
    let mut detail = vec![output::count(sweeps_run, "sweep")];
    if let Some((sweep, binary, wall)) = &t.slowest_parallel {
        detail.push(format!(
            "slowest parallel binary {binary} {:.1}s in {sweep}",
            wall.as_secs_f64()
        ));
    }
    // Named, never folded into the total: a lane that validated nothing is
    // invisible behind thousands of tests from its siblings.
    if !t.empty_units.is_empty() {
        detail.push(format!("no tests ran in {}", t.empty_units.join(", ")));
    }
    output::run_msg(&format!(
        "test: {} passed{extra} ({}) in {}",
        t.passed,
        detail.join("; "),
        fmt_wall(elapsed)
    ));
}

/// Log a sweep's shape and full cargo argv, and show it as the status. Under
/// `--commands` the argv also prints, before the run, exactly as it always
/// did; otherwise a green run prints nothing for it - the shape is config
/// plus invocation, identical run to run, and a failing sweep reprints it.
fn announce_sweep(line_shape: &str, command: Option<&str>, commands: bool) {
    output::detail(line_shape);
    if let Some(cmd) = command {
        cargo_line(commands, cmd);
    }
    output::status(line_shape);
}

/// A cargo invocation about to run: printed under `--commands`, logged always.
fn cargo_line(commands: bool, line: &str) {
    if commands {
        output::run_msg(line);
    } else {
        output::detail(line);
    }
}

/// How many recent run logs to keep, per kind. Enough that a watchdog-killed
/// run survives the retries that follow it; bounded so the directory cannot
/// grow without limit on a machine that runs `check` all day. Per kind, so a
/// burst of `brokkr test` runs cannot evict the `check` log being investigated.
const RUN_LOGS_KEPT: usize = 10;

/// Open this run's log under `<state_root>/.brokkr/check-logs/` as
/// `<kind>-<ms>.log`, pruning the oldest of that kind so that at most
/// [`RUN_LOGS_KEPT`] remain. Best-effort: a log that cannot be opened costs the
/// record, never the run.
fn open_run_log(state_root: &Path, kind: &str) {
    let dir = state_root.join(".brokkr").join("check-logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    // `create_new`, stepping the stamp past a name already taken: two runs
    // resolving the same millisecond must not truncate one another's log.
    // Stepping (rather than a suffix) keeps the name all digits, so name
    // order stays age order for retention.
    let opened = (0..1000u128).find_map(|step| {
        let path = dir.join(format!("{kind}-{:015}.log", millis + step));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()
            .map(|file| (path, file))
    });
    if let Some((current, file)) = opened {
        let existing: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        // Prune only after creating it: a failed open must not evict history.
        for oldest in logs_to_prune(existing, kind, &current) {
            std::fs::remove_file(oldest).ok();
        }
        output::open_run_log(file);
        let argv: Vec<String> = std::env::args().collect();
        output::detail(&format!("argv: {}", argv.join(" ")));
        if let Ok(cwd) = std::env::current_dir() {
            output::detail(&format!("cwd: {}", cwd.display()));
        }
    }
}

/// Reserve one retention slot for the current file, even after a clock
/// rollback. Other kinds never enter this kind's retention budget.
fn logs_to_prune(mut existing: Vec<PathBuf>, kind: &str, current: &Path) -> Vec<PathBuf> {
    existing.retain(|p| is_run_log_name(p, kind) && p != current);
    existing.sort();
    let count = existing.len().saturating_sub(RUN_LOGS_KEPT - 1);
    existing.into_iter().take(count).collect()
}

/// The kinds of run log, one name prefix (and one retention budget) each.
pub(crate) mod run_log_kind {
    pub(crate) const CHECK: &str = "check";
    pub(crate) const TEST: &str = "test";
    /// Measured runs (`--bench`/`--hotpath`/`--alloc` and plain runs through
    /// the measurement harness, `sync --bench`).
    pub(crate) const MEASURE: &str = "measure";
    /// `brokkr bench`, the criterion runner.
    pub(crate) const BENCH: &str = "bench";
}

/// Reserved before the lock, activated under it, and dropped after the
/// watchdog joins. Reserving touches no files: waiting commands cannot prune
/// the current holder's log. Nested reservations borrow the outer log.
pub(crate) struct RunLog {
    owner: bool,
}

struct LogReservation {
    root: PathBuf,
    kind: String,
    activated: bool,
}

static LOG_RESERVATION: std::sync::Mutex<Option<LogReservation>> = std::sync::Mutex::new(None);

impl RunLog {
    pub(crate) fn begin(state_root: &Path, kind: &str) -> Self {
        let owner = {
            let mut reservation = LOG_RESERVATION
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if reservation.is_some() {
                false
            } else {
                *reservation = Some(LogReservation {
                    root: state_root.to_owned(),
                    kind: kind.to_owned(),
                    activated: false,
                });
                true
            }
        };
        // A reservation made inside an existing hold needs no fresh acquisition.
        if crate::lockfile::current_hold().is_some() {
            activate_run_log();
        }
        Self { owner }
    }
}

/// Called by the real lock's drain hook, after flock and before any toolchain
/// narration. Re-entrant acquisitions never reopen or prune the log.
pub(crate) fn activate_run_log() {
    let mut reservation = LOG_RESERVATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(log) = reservation.as_mut()
        && !log.activated
    {
        log.activated = true;
        open_run_log(&log.root, &log.kind);
    }
}

impl Drop for RunLog {
    fn drop(&mut self) {
        if self.owner {
            let mut reservation = LOG_RESERVATION
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            output::close_run_log();
            reservation.take();
        }
    }
}

/// Holds a check's report and status line open. The run log is the caller's
/// ([`RunLog`], reserved before the lock).
struct RunScope;

impl RunScope {
    fn begin() -> Self {
        report_begin();
        output::enable_status_line();
        RunScope
    }
}

impl Drop for RunScope {
    fn drop(&mut self) {
        output::disable_status_line();
        report_end();
    }
}

/// `<kind>-<digits>.log`, the only name [`open_run_log`] constructs for `kind`
/// - and the only one its pruning may delete.
fn is_run_log_name(p: &Path, kind: &str) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix(kind))
        .and_then(|n| n.strip_prefix('-'))
        .and_then(|n| n.strip_suffix(".log"))
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod report_tests {
    use super::*;

    #[test]
    fn retention_protects_current_log_and_other_kinds_during_a_burst() {
        // The current clock has rolled back behind every historical stamp.
        let current = PathBuf::from("/x/test-000000000000001.log");
        let mut logs = vec![
            current.clone(),
            PathBuf::from("/x/check-000000000000002.log"),
        ];
        logs.extend((10..40).map(|stamp| PathBuf::from(format!("/x/test-{stamp:015}.log"))));
        let pruned = logs_to_prune(logs.clone(), run_log_kind::TEST, &current);
        assert_eq!(pruned.len(), 21);
        assert_eq!(
            pruned.first(),
            Some(&PathBuf::from("/x/test-000000000000010.log"))
        );
        assert_eq!(
            pruned.last(),
            Some(&PathBuf::from("/x/test-000000000000030.log"))
        );
        assert!(!pruned.contains(&current));
        let kept: Vec<_> = logs.into_iter().filter(|p| !pruned.contains(p)).collect();
        assert_eq!(
            kept.iter()
                .filter(|p| is_run_log_name(p, run_log_kind::TEST))
                .count(),
            RUN_LOGS_KEPT
        );
        assert!(kept.contains(&PathBuf::from("/x/check-000000000000002.log")));
        assert!(logs_to_prune(kept, run_log_kind::TEST, &current).is_empty());
    }

    #[test]
    fn run_log_names_are_recognised_exactly() {
        let check = run_log_kind::CHECK;
        assert!(is_run_log_name(
            Path::new("/x/check-000001727000000.log"),
            check
        ));
        assert!(!is_run_log_name(Path::new("/x/check-.log"), check));
        assert!(!is_run_log_name(Path::new("/x/check-12a.log"), check));
        assert!(!is_run_log_name(Path::new("/x/notes.log"), check));
        assert!(!is_run_log_name(Path::new("/x/check-12.log.bak"), check));
        // Each kind prunes only its own: a test log is not a check log.
        assert!(!is_run_log_name(
            Path::new("/x/test-000001727000000.log"),
            check
        ));
        assert!(is_run_log_name(
            Path::new("/x/test-000001727000000.log"),
            run_log_kind::TEST
        ));
        assert!(!is_run_log_name(Path::new("/x/checkx-12.log"), check));
    }
}
