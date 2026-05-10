//! Shared streaming runner for cargo libtest invocations.
//!
//! The runner keeps the captured stdout/stderr buffers used by the existing
//! cargo parsers, while also watching libtest's partial `test name ... `
//! progress marker before the terminating newline arrives.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::DevError;
use crate::output::CapturedOutput;
use crate::ratatoskr::process::snapshot_proc;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);
const WATCHDOG_POLL: Duration = Duration::from_millis(250);

pub(crate) struct LibtestRun {
    pub(crate) captured: CapturedOutput,
    pub(crate) outcome: LibtestOutcome,
    /// Time from cargo spawn to the `Finished` stderr line (build phase).
    /// `None` if cargo never emitted `Finished` (build failed or no rebuild
    /// happened - though cargo emits `Finished` even on cache hits).
    pub(crate) build_elapsed: Option<Duration>,
    /// (test name, wall-clock duration) for every test the tracker observed
    /// running to completion. Order is observation order (libtest runs
    /// `--test-threads=1`, so this is also start order). Note: the
    /// deferred-observe state machine clears pending on the *next* start
    /// marker or `test result:` summary, so a test whose first println
    /// looks like a bare status (`println!("ok")`) will have its duration
    /// inflated by the gap until the next test starts.
    pub(crate) completed: Vec<(String, Duration)>,
}

pub(crate) enum LibtestOutcome {
    Completed,
    HungTest(HungTest),
}

#[derive(Clone, Debug)]
pub(crate) struct HungTest {
    pub(crate) test: String,
    pub(crate) elapsed: Duration,
    pub(crate) ceiling: Duration,
    pub(crate) snapshot_dir: PathBuf,
    pub(crate) cargo_pid: u32,
    pub(crate) test_pids: Vec<u32>,
    pub(crate) snapshot_pid: Option<u32>,
    pub(crate) wchan: Option<String>,
    pub(crate) stack: Option<String>,
    pub(crate) snapshot_error: Option<String>,
}

#[derive(Default)]
struct TestTracker {
    current: HashMap<String, Instant>,
    completed: Vec<(String, Duration)>,
}

impl TestTracker {
    fn observe_start(&mut self, name: String) {
        self.current.entry(name).or_insert_with(Instant::now);
    }

    fn observe_result(&mut self, name: &str) {
        if let Some(started) = self.current.remove(name) {
            self.completed.push((name.to_owned(), started.elapsed()));
        }
    }

    fn timed_out(&self, timeout: Duration) -> Option<(String, Duration)> {
        self.current
            .iter()
            .filter_map(|(name, started)| {
                let elapsed = started.elapsed();
                (elapsed >= timeout).then(|| (name.clone(), elapsed))
            })
            .max_by_key(|(_, elapsed)| *elapsed)
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn streaming_run_libtest<Out, Err, Fin>(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    forward_stdout_line: Out,
    forward_stderr_line: Err,
    on_build_finished: Fin,
) -> Result<LibtestRun, DevError>
where
    Out: FnMut(&str) + Send + 'static,
    Err: FnMut(&str) + Send + 'static,
    Fin: FnOnce(Duration) + Send + 'static,
{
    enforce_single_threaded(args)?;

    let start = Instant::now();
    let mut child = spawn_cargo_process_group(args, cwd, env)?;
    let cargo_pid = child.id();

    let Some(stdout_pipe) = child.stdout.take() else {
        return Err(DevError::Build("cargo stdout was not piped".into()));
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        return Err(DevError::Build("cargo stderr was not piped".into()));
    };

    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let done = Arc::new(AtomicBool::new(false));
    let hung = Arc::new(Mutex::new(None::<HungTest>));

    let stdout_buf_t = Arc::clone(&stdout_buf);
    let tracker_t = Arc::clone(&tracker);
    let stdout_thread = thread::spawn(move || {
        drain_stdout(stdout_pipe, &stdout_buf_t, &tracker_t, forward_stdout_line);
    });

    let stderr_buf_t = Arc::clone(&stderr_buf);
    let build_elapsed = Arc::new(Mutex::new(None::<Duration>));
    let build_elapsed_t = Arc::clone(&build_elapsed);
    let stderr_thread = thread::spawn(move || {
        drain_stderr(
            stderr_pipe,
            &stderr_buf_t,
            forward_stderr_line,
            start,
            move |elapsed| {
                if let Ok(mut slot) = build_elapsed_t.lock() {
                    *slot = Some(elapsed);
                }
                on_build_finished(elapsed);
            },
        );
    });

    let cwd_t = cwd.to_path_buf();
    let tracker_t = Arc::clone(&tracker);
    let done_t = Arc::clone(&done);
    let hung_t = Arc::clone(&hung);
    let watchdog_thread = thread::spawn(move || {
        watchdog_loop(cwd_t, cargo_pid, tracker_t, done_t, hung_t);
    });

    let status = child.wait().map_err(|e| DevError::Subprocess {
        program: "cargo".into(),
        code: None,
        stderr: e.to_string(),
    })?;
    done.store(true, Ordering::SeqCst);

    stdout_thread.join().ok();
    stderr_thread.join().ok();
    watchdog_thread.join().ok();

    let elapsed = start.elapsed();
    let stdout = clone_buffer(&stdout_buf, "stdout")?;
    let stderr = clone_buffer(&stderr_buf, "stderr")?;
    let hung_outcome = clone_hung(&hung)?;
    let build_elapsed = build_elapsed
        .lock()
        .map_err(|_| DevError::Build("build_elapsed mutex poisoned".into()))?
        .take();
    let completed = tracker
        .lock()
        .map(|mut t| std::mem::take(&mut t.completed))
        .map_err(|_| DevError::Build("test tracker mutex poisoned".into()))?;

    Ok(LibtestRun {
        captured: CapturedOutput {
            status,
            stdout,
            stderr,
            elapsed,
        },
        outcome: match hung_outcome {
            Some(h) => LibtestOutcome::HungTest(h),
            None => LibtestOutcome::Completed,
        },
        build_elapsed,
        completed,
    })
}

fn spawn_cargo_process_group(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
) -> Result<std::process::Child, DevError> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new("cargo");
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    for &(key, value) in env {
        cmd.env(key, value);
    }
    crate::oom::protect_child(&mut cmd);

    cmd.spawn().map_err(|e| DevError::Subprocess {
        program: "cargo".into(),
        code: None,
        stderr: e.to_string(),
    })
}

fn clone_buffer(buf: &Arc<Mutex<Vec<u8>>>, label: &str) -> Result<Vec<u8>, DevError> {
    buf.lock()
        .map(|v| v.clone())
        .map_err(|_| DevError::Build(format!("{label} buffer poisoned")))
}

fn clone_hung(hung: &Arc<Mutex<Option<HungTest>>>) -> Result<Option<HungTest>, DevError> {
    hung.lock()
        .map(|h| h.clone())
        .map_err(|_| DevError::Build("hung-test state poisoned".into()))
}

/// State of partial-marker tracking under `--nocapture --test-threads=1`.
///
/// Libtest writes `test NAME ... ` (no newline, flushed) before running
/// each test, then the test's own stdout glues onto that partial line,
/// then libtest writes the bare status (`ok\n`/`FAILED\n`/`ignored\n`,
/// optionally with a `<X.Xs>` suffix when `--report-time` is set).
/// We strip the partial marker and track which name is still running
/// so the watchdog can age it; the trailing bare-status line, when
/// it arrives unambiguously (no preceding test output), is consumed
/// as the terminator.
///
/// The earlier implementation gated partial-marker detection on
/// "no pending test" and cleared pending unconditionally on the first
/// bare-status-shaped line. Two failure modes:
/// - `print!("hi")` glues with libtest's `ok\n` -> arrives as `hiok`,
///   not bare status; pending never cleared; the *next* test's partial
///   marker is then ignored (gated out), and the watchdog blames the
///   wrong test.
/// - `println!("ok")` arrives as a real bare-status line *before*
///   libtest's terminator; pending cleared early, real hang in same
///   test goes unnoticed.
///
/// The state machine here fixes Trigger A (the next-start-marker case)
/// and narrows Trigger B (`intermediate_output_seen` suppresses the
/// bare-status shortcut once any non-status output has flowed).
#[derive(Default)]
enum PartialState {
    #[default]
    Idle,
    AwaitingTerminator {
        name: String,
        /// True once a non-blank, non-status-shaped line has been
        /// forwarded for this pending test. Suppresses the
        /// bare-status terminator shortcut: at that point the next
        /// `ok`/`FAILED`/`ignored` line is more likely test output
        /// than libtest framing, so we wait for the *next* partial
        /// start marker (or `test result:` summary) to clear instead.
        intermediate_output_seen: bool,
    },
}

fn drain_stdout<F>(
    mut pipe: ChildStdout,
    buf: &Mutex<Vec<u8>>,
    tracker: &Mutex<TestTracker>,
    mut forward_line: F,
) where
    F: FnMut(&str),
{
    let mut read_buf = [0_u8; 4096];
    let mut line = Vec::<u8>::new();
    let mut state = PartialState::Idle;

    while let Ok(n) = pipe.read(&mut read_buf) {
        if n == 0 {
            break;
        }
        if let Ok(mut out) = buf.lock() {
            out.extend_from_slice(&read_buf[..n]);
        }
        for &byte in &read_buf[..n] {
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                handle_stdout_line(&line, tracker, &mut forward_line, true, &mut state);
                line.clear();
            } else {
                line.push(byte);
                // Detect the partial `test NAME ... ` start marker before
                // the newline arrives, so the watchdog can age the test
                // even if it never produces output. Note: no `Idle`-only
                // gate - a new start marker while a previous test is
                // pending is the strongest signal that the previous test
                // ended without an explicit terminator (Trigger A).
                if byte == b' '
                    && line.len() >= "test x ... ".len()
                    && let Ok(text) = std::str::from_utf8(&line)
                    && let Some(name) = parse_start_marker(text)
                {
                    if let PartialState::AwaitingTerminator { name: prev, .. } = &state
                        && let Ok(mut t) = tracker.lock()
                    {
                        t.observe_result(prev);
                    }
                    if let Ok(mut t) = tracker.lock() {
                        t.observe_start(name.clone());
                    }
                    state = PartialState::AwaitingTerminator {
                        name,
                        intermediate_output_seen: false,
                    };
                    line.clear();
                }
            }
        }
    }

    if !line.is_empty() {
        handle_stdout_line(&line, tracker, &mut forward_line, false, &mut state);
    }
}

fn drain_stderr<F, G>(
    mut pipe: ChildStderr,
    buf: &Mutex<Vec<u8>>,
    mut forward_line: F,
    start: Instant,
    on_build_finished: G,
) where
    F: FnMut(&str),
    G: FnOnce(Duration),
{
    let mut on_build_finished = Some(on_build_finished);
    let mut read_buf = [0_u8; 4096];
    let mut line = Vec::<u8>::new();

    while let Ok(n) = pipe.read(&mut read_buf) {
        if n == 0 {
            break;
        }
        if let Ok(mut out) = buf.lock() {
            out.extend_from_slice(&read_buf[..n]);
        }
        for &byte in &read_buf[..n] {
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let text = String::from_utf8_lossy(&line).into_owned();
                if on_build_finished.is_some() && is_cargo_finished_line(&text)
                    && let Some(cb) = on_build_finished.take()
                {
                    cb(start.elapsed());
                }
                forward_line(&text);
                line.clear();
            } else {
                line.push(byte);
            }
        }
    }

    if !line.is_empty() {
        let text = String::from_utf8_lossy(&line).into_owned();
        forward_line(&text);
    }
}

fn is_cargo_finished_line(line: &str) -> bool {
    line.trim_start().starts_with("Finished ")
}

fn handle_stdout_line<F>(
    line: &[u8],
    tracker: &Mutex<TestTracker>,
    forward_line: &mut F,
    terminated: bool,
    state: &mut PartialState,
) where
    F: FnMut(&str),
{
    let text = String::from_utf8_lossy(line).into_owned();

    // libtest's per-suite summary is the universal pending-clear:
    // `test result: ok. ...` or `test result: FAILED. ...`. Match
    // both verb forms specifically so a test that does
    // `println!("test result:")` doesn't accidentally clear pending
    // and leave the watchdog blind to a subsequent hang.
    if is_libtest_result_summary(&text) {
        if let PartialState::AwaitingTerminator { name, .. } = state
            && let Ok(mut t) = tracker.lock()
        {
            t.observe_result(name);
        }
        *state = PartialState::Idle;
    }

    if let PartialState::AwaitingTerminator { name: _, intermediate_output_seen } = state {
        if !*intermediate_output_seen && is_bare_status_line(&text) {
            // No test output has been seen yet, and the line looks like
            // libtest's terminator (`ok` / `FAILED` / `ignored`,
            // optionally `<X.Xs>`). Two real shapes match here:
            //
            // 1. The legitimate libtest terminator for a silent test.
            // 2. A test whose *first* println! was literally one of
            //    those words - the watchdog can't tell which.
            //
            // Drop the line from display either way (so we don't print
            // a stray `ok` next to libtest's real one), but DO NOT
            // call observe_result. If the test then hangs after
            // `println!("ok")`, the watchdog must still fire. Pending
            // is cleared by either the next `test NAME ... ` start
            // marker or by the `test result:` summary - both happen
            // well inside the watchdog timeout for a normal completion.
            return;
        }
        if !text.trim().is_empty() {
            *intermediate_output_seen = true;
        }
    }

    if let Some(name) = parse_result_marker(&text)
        && let Ok(mut t) = tracker.lock()
    {
        t.observe_result(&name);
    }

    if terminated || parse_start_marker(&text).is_none() {
        forward_line(&text);
    }
}

/// True for libtest's per-suite summary line.
///
/// Libtest emits exactly one of:
/// - `test result: ok. N passed; M failed; ...`
/// - `test result: FAILED. N passed; M failed; ...`
///
/// Match both verbs explicitly so a user `println!("test result:")`
/// in test output cannot accidentally clear the watchdog's pending
/// state - which would silently let a subsequent hang go undetected.
fn is_libtest_result_summary(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("test result: ok.") || t.starts_with("test result: FAILED.")
}

/// True for libtest's standalone status lines (`ok`, `FAILED`, `ignored`,
/// optionally followed by ` <X.Xs>` when `--report-time` is enabled).
fn is_bare_status_line(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(head) = parts.next() else {
        return false;
    };
    if !matches!(head, "ok" | "FAILED" | "ignored") {
        return false;
    }
    // Allow at most one trailing token, and only if it looks like a
    // libtest timing suffix: `<0.001s>`.
    match parts.next() {
        None => true,
        Some(tail) => {
            parts.next().is_none()
                && tail.starts_with('<')
                && tail.ends_with("s>")
        }
    }
}

fn parse_start_marker(line: &str) -> Option<String> {
    let rest = line.strip_prefix("test ")?;
    let name = rest.strip_suffix(" ... ")?;
    (!name.is_empty()).then(|| name.to_owned())
}

fn parse_result_marker(line: &str) -> Option<String> {
    let rest = line.strip_prefix("test ")?;
    let (name, status) = rest.rsplit_once(" ... ")?;
    let is_result = ["ok", "FAILED", "ignored"]
        .iter()
        .any(|s| status == *s || status.strip_prefix(s).is_some_and(|tail| tail.starts_with(' ')));
    (is_result && !name.is_empty()).then(|| name.to_owned())
}

#[allow(clippy::needless_pass_by_value)] // The Arcs are moved into a spawned thread.
fn watchdog_loop(
    cwd: PathBuf,
    cargo_pid: u32,
    tracker: Arc<Mutex<TestTracker>>,
    done: Arc<AtomicBool>,
    hung: Arc<Mutex<Option<HungTest>>>,
) {
    watchdog_loop_with_timing(cwd, cargo_pid, tracker, done, hung, TEST_TIMEOUT, WATCHDOG_POLL);
}

#[allow(clippy::needless_pass_by_value)] // The Arcs are moved into a spawned thread.
fn watchdog_loop_with_timing(
    cwd: PathBuf,
    cargo_pid: u32,
    tracker: Arc<Mutex<TestTracker>>,
    done: Arc<AtomicBool>,
    hung: Arc<Mutex<Option<HungTest>>>,
    timeout: Duration,
    poll: Duration,
) {
    loop {
        if done.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(poll);
        if done.load(Ordering::SeqCst) {
            return;
        }

        let timed_out = tracker
            .lock()
            .ok()
            .and_then(|t| t.timed_out(timeout));
        let Some((test, elapsed)) = timed_out else {
            continue;
        };
        if done.load(Ordering::SeqCst) {
            return;
        }

        let hung_test = capture_hung_test(&cwd, cargo_pid, &test, elapsed, timeout);
        if let Ok(mut slot) = hung.lock() {
            *slot = Some(hung_test);
        }
        kill_process_group(cargo_pid).ok();
        return;
    }
}

fn capture_hung_test(
    cwd: &Path,
    cargo_pid: u32,
    test: &str,
    elapsed: Duration,
    ceiling: Duration,
) -> HungTest {
    let test_pids = direct_child_pids(cargo_pid);
    let snapshot_pid = test_pids.first().copied().or(Some(cargo_pid));
    let snapshot_dir = cwd
        .join(".brokkr")
        .join("test-hung")
        .join(format!(
            "{}-{}-{}",
            unix_secs(),
            cargo_pid,
            sanitize_path_component(test)
        ));

    let mut snapshot_error = None;
    if let Err(err) = fs::create_dir_all(&snapshot_dir) {
        snapshot_error = Some(err.to_string());
    } else if let Some(pid) = snapshot_pid
        && let Err(err) = snapshot_proc(pid, &snapshot_dir)
    {
        snapshot_error = Some(err.to_string());
    }

    let wchan = snapshot_error
        .is_none()
        .then(|| first_line(snapshot_dir.join("proc-wchan.txt")))
        .flatten();
    let stack = snapshot_error
        .is_none()
        .then(|| first_line(snapshot_dir.join("proc-stack.txt")))
        .flatten();

    HungTest {
        test: test.to_owned(),
        elapsed,
        ceiling,
        snapshot_dir,
        cargo_pid,
        test_pids,
        snapshot_pid,
        wchan,
        stack,
        snapshot_error,
    }
}

fn kill_process_group(pgid: u32) -> Result<(), DevError> {
    let pgid = i32::try_from(pgid)
        .map_err(|_| DevError::Build(format!("process group id {pgid} does not fit pid_t")))?;
    let target: libc::pid_t = -pgid;
    let ret = unsafe { libc::kill(target, libc::SIGKILL) };
    if ret == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(DevError::Io(err))
}

fn direct_child_pids(parent: u32) -> Vec<u32> {
    let task_children = Path::new("/proc")
        .join(parent.to_string())
        .join("task")
        .join(parent.to_string())
        .join("children");
    let mut out = fs::read_to_string(task_children)
        .ok()
        .map(|s| parse_pid_list(&s))
        .unwrap_or_default();
    if out.is_empty() {
        out = scan_proc_children(parent);
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn parse_pid_list(text: &str) -> Vec<u32> {
    text.split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}

fn scan_proc_children(parent: u32) -> Vec<u32> {
    // Some kernels/configurations do not expose task/<tid>/children. Walking
    // /proc is slower but only happens on the hang path.
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let pid = name.to_str()?.parse::<u32>().ok()?;
            let status = fs::read_to_string(entry.path().join("status")).ok()?;
            (status_ppid(&status) == Some(parent)).then_some(pid)
        })
        .collect()
}

fn status_ppid(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("PPid:")?;
        rest.trim().parse::<u32>().ok()
    })
}

fn first_line(path: PathBuf) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned())
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn sanitize_path_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(120));
    let mut prev_sep = false;
    for ch in name.chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_');
        if keep {
            out.push(ch);
            prev_sep = false;
        } else if !prev_sep {
            out.push('_');
            prev_sep = true;
        }
        if out.len() >= 120 {
            break;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "unknown".into()
    } else {
        trimmed.to_owned()
    }
}

pub(crate) fn format_hung_test(hung: &HungTest, cwd: &Path) -> String {
    let snapshot_path = hung
        .snapshot_dir
        .strip_prefix(cwd)
        .unwrap_or(&hung.snapshot_dir)
        .display()
        .to_string();
    let child_text = match hung.test_pids.as_slice() {
        [] => "no test child found".to_owned(),
        [pid] => format!("test child (pid {pid})"),
        pids => format!(
            "test children (pids {})",
            pids.iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    };
    let proc_pid = hung
        .snapshot_pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "?".into());
    let wchan = hung.wchan.as_deref().unwrap_or("unavailable");
    let stack = hung.stack.as_deref().unwrap_or("unavailable");
    let snapshot_line = match &hung.snapshot_error {
        Some(err) => format!("snapshot failed: {err}"),
        None => format!("full snapshot: {snapshot_path}"),
    };

    format!(
        "test {} did not finish within {}s after libtest started it\n  per-test timeout: cargo build time excluded\n  killed cargo process group (pgid {}) and {}\n  /proc/{}/wchan: {}\n  /proc/{}/stack: {}\n  {}",
        hung.test,
        hung.ceiling.as_secs(),
        hung.cargo_pid,
        child_text,
        proc_pid,
        wchan,
        proc_pid,
        stack,
        snapshot_line,
    )
}

pub(crate) fn effective_test_threads(args: &[String]) -> Result<Option<u32>, DevError> {
    effective_test_threads_from(args)
}

fn enforce_single_threaded(args: &[&str]) -> Result<(), DevError> {
    match effective_test_threads_from(args)? {
        Some(1) => Ok(()),
        Some(n) => Err(DevError::Config(format!(
            "libtest watchdog requires --test-threads=1, got --test-threads={n}"
        ))),
        None => Err(DevError::Config(
            "libtest watchdog requires --test-threads=1, but no --test-threads flag was passed".into(),
        )),
    }
}

fn effective_test_threads_from<T: AsRef<str>>(args: &[T]) -> Result<Option<u32>, DevError> {
    let mut current = None;
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_ref();
        if let Some(value) = arg.strip_prefix("--test-threads=") {
            current = Some(parse_thread_count(value)?);
        } else if arg == "--test-threads" {
            let Some(value) = args.get(idx + 1) else {
                return Err(DevError::Config(
                    "--test-threads requires a numeric value".into(),
                ));
            };
            current = Some(parse_thread_count(value.as_ref())?);
            idx += 1;
        }
        idx += 1;
    }
    Ok(current)
}

fn parse_thread_count(value: &str) -> Result<u32, DevError> {
    value.parse::<u32>().map_err(|_| {
        DevError::Config(format!(
            "--test-threads requires a numeric value, got {value:?}"
        ))
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used
    )]

    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    /// Drive `drain_stdout` against a synthetic libtest stream and
    /// return (forwarded_lines, names_observed_finished).
    /// The stream is the concatenation of the byte slices the libtest
    /// stdout pipe would have produced, in chunk order. The tracker
    /// records `observe_start` / `observe_result` calls so we can
    /// inspect which tests the watchdog still believes are running.
    fn drive_drain(chunks: &[&[u8]]) -> (Vec<String>, Vec<String>) {
        use std::io::Write;
        // Build a real pipe: one end gets the chunks, the other is fed
        // to drain_stdout. This exercises the same byte-by-byte path as
        // production and avoids forking the loop under test.
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cat");
        let mut stdin = child.stdin.take().expect("cat stdin");
        let stdout = child.stdout.take().expect("cat stdout");

        let chunks_owned: Vec<Vec<u8>> = chunks.iter().map(|c| c.to_vec()).collect();
        let writer = std::thread::spawn(move || {
            for c in &chunks_owned {
                stdin.write_all(c).ok();
            }
            drop(stdin);
        });

        let buf = Mutex::new(Vec::<u8>::new());
        let tracker = Mutex::new(TestTracker::default());
        let forwarded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let forwarded_t = Arc::clone(&forwarded);

        drain_stdout(stdout, &buf, &tracker, move |line: &str| {
            forwarded_t.lock().unwrap().push(line.to_owned());
        });

        writer.join().ok();
        child.wait().ok();

        // Names still in the tracker are tests the watchdog believes
        // are running. Names absent from the tracker had observe_result
        // called - they're "finished" from the watchdog's view.
        let still_running: Vec<String> =
            tracker.lock().unwrap().current.keys().cloned().collect();
        let forwarded = forwarded.lock().unwrap().clone();
        (forwarded, still_running)
    }

    #[test]
    fn watchdog_clears_when_test_prints_without_newline() {
        // Trigger A from the review: `print!("hello")` glues with the
        // libtest `ok\n` -> arrives as `hellook\n`. The next test's
        // partial marker must clear the previous pending; otherwise
        // the watchdog times the wrong test.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"hellook\n",
            b"test bar ... ",
            b"ok\n",
            b"\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (forwarded, still_running) = drive_drain(&stream);
        // "hellook" should have been forwarded as test output (intermediate).
        assert!(forwarded.iter().any(|l| l == "hellook"), "got: {forwarded:?}");
        // After the result summary, no test should still be pending.
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn is_libtest_result_summary_matches_real_shapes_only() {
        // Exact libtest summary forms (with leading whitespace tolerated).
        assert!(is_libtest_result_summary(
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
        ));
        assert!(is_libtest_result_summary(
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"
        ));
        // A user `println!("test result:")` with no verb is NOT a
        // summary - reviewer flagged that the prefix-only check let
        // tests accidentally clear watchdog pending state.
        assert!(!is_libtest_result_summary("test result:"));
        assert!(!is_libtest_result_summary("test result: foo"));
        assert!(!is_libtest_result_summary("test result: ok bar"));
    }

    #[test]
    fn watchdog_keeps_pending_when_test_prints_test_result_prefix_then_hangs() {
        // Item 4 from the second review: a test that prints
        // `println!("test result:")` and then hangs must NOT have
        // pending cleared by the summary detector.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"test result: maybe later\n",
            // ... and then the test hangs.
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert_eq!(still_running, vec!["foo".to_string()]);
    }

    #[test]
    fn watchdog_keeps_pending_when_test_prints_ok_then_hangs() {
        // Reviewer-flagged regression: a test whose *first* output is
        // `println!("ok")` and which then hangs forever must NOT be
        // declared finished by the bare-status shortcut. Previously
        // the watchdog cleared pending on that line and the timeout
        // never fired. With the deferred-observe fix the test stays
        // pending; the real watchdog timer (20s in production, not
        // simulated here) will eventually trigger.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"ok\n",
            // ... and then nothing. EOF here stands in for "the test
            // is still running / hung when the pipe closes".
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert_eq!(still_running, vec!["foo".to_string()]);
    }

    #[test]
    fn watchdog_clears_when_test_prints_ok_literal() {
        // Trigger B fixture: the test calls `println!("ok")` then
        // completes normally. Both the test's `ok` and libtest's
        // own `ok\n` look like bare-status terminators with no
        // intermediate output - the deferred-observe path drops
        // both from display but does *not* clear pending. The
        // `test result:` summary line at the end is what actually
        // clears the watchdog tracker.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"ok\n",
            b"ok\n",
            b"\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_clears_when_test_prints_failed_literal() {
        // Symmetric to the `ok` case: `println!("FAILED")` followed
        // by libtest's own `FAILED\n`. Both bare-status lines are
        // dropped from display without observing a result; the
        // `test result:` summary at the end is the actual
        // pending-clear event.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"FAILED\n",
            b"FAILED\n",
            b"\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_clears_silent_test_via_bare_status() {
        // No test output: stream is `test foo ... ok\n`. The partial
        // start marker is stripped; the bare `ok` line is dropped from
        // display (deferred-observe: we can't tell it from a test's
        // own `println!("ok")`) without clearing pending. The
        // `test result:` summary is what clears the watchdog tracker.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"ok\n",
            b"\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_clears_after_panic_message() {
        // A failing test usually prints panic info on stdout/stderr
        // before libtest writes `FAILED\n`. The state machine must
        // eventually clear pending; the `test result:` summary is the
        // last-resort terminator after intermediate output suppressed
        // the bare-status shortcut.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"thread 'foo' panicked at tests/x.rs:1:1:\n",
            b"assertion failed\n",
            b"FAILED\n",
            b"\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (forwarded, still_running) = drive_drain(&stream);
        assert!(
            forwarded.iter().any(|l| l.contains("panicked")),
            "forwarded: {forwarded:?}"
        );
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_clears_with_report_time_suffix() {
        // libtest with `--report-time` emits `ok <0.001s>\n`. The
        // is_bare_status_line check accepts the `<X.Xs>` suffix, so
        // pending should clear normally even when timing is enabled.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"ok <0.001s>\n",
            b"\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_full_form_marker_clears_pending() {
        // When `--nocapture` is off, libtest writes the full
        // `test foo ... ok\n` line atomically. parse_result_marker
        // catches it; pending stays empty.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ok\n",
            b"\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn start_marker_allows_spaces_in_doctest_names() {
        let name = parse_start_marker("test src/lib.rs - module::Foo::bar (line 42) ... ")
            .expect("start marker");
        assert_eq!(name, "src/lib.rs - module::Foo::bar (line 42)");
    }

    #[test]
    fn result_marker_allows_report_time_suffix() {
        let name = parse_result_marker("test my_mod::slow ... ok (0.25s)")
            .expect("result marker");
        assert_eq!(name, "my_mod::slow");
    }

    #[test]
    fn result_marker_parses_failed() {
        let name = parse_result_marker("test my_mod::slow ... FAILED")
            .expect("result marker");
        assert_eq!(name, "my_mod::slow");
    }

    #[test]
    fn result_marker_ignores_non_test_lines() {
        assert!(parse_result_marker("hello test my_mod::slow ... ok").is_none());
    }

    #[test]
    fn effective_test_threads_uses_last_value() {
        let args = vec![
            "--test-threads=1".to_owned(),
            "--test-threads".to_owned(),
            "4".to_owned(),
        ];
        assert_eq!(effective_test_threads(&args).unwrap(), Some(4));
    }

    #[test]
    fn sanitize_test_name_for_path_component() {
        assert_eq!(
            sanitize_path_component("src/lib.rs - foo::bar (line 42)"),
            "src_lib.rs_-_foo_bar_line_42"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn watchdog_kills_process_group_and_writes_snapshot() {
        use std::os::unix::process::CommandExt;

        let root = test_root("watchdog_kill_snapshot");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 60 & wait")
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn shell");
        let cargo_pid = child.id();
        let child_pids = wait_for_direct_children(cargo_pid);
        assert!(!child_pids.is_empty(), "shell did not spawn sleep child");

        let started = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        let tracker = Arc::new(Mutex::new(TestTracker {
            current: HashMap::from([("watchdog::hangs".to_owned(), started)]),
            completed: Vec::new(),
        }));
        let done = Arc::new(AtomicBool::new(false));
        let hung = Arc::new(Mutex::new(None::<HungTest>));

        watchdog_loop_with_timing(
            root.clone(),
            cargo_pid,
            Arc::clone(&tracker),
            Arc::clone(&done),
            Arc::clone(&hung),
            Duration::from_millis(20),
            Duration::from_millis(5),
        );
        child.wait().ok();

        let hung = hung.lock().unwrap().clone().expect("hung result");
        assert_eq!(hung.test, "watchdog::hangs");
        assert!(hung.snapshot_dir.exists());
        assert!(hung.snapshot_dir.join("proc-status.txt").exists());
        assert!(hung.snapshot_pid.is_some());
        assert!(
            wait_for_process_group_exit(cargo_pid),
            "process group {cargo_pid} survived watchdog kill"
        );
    }

    fn test_root(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/test-runner")
            .join(name);
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn wait_for_direct_children(parent: u32) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let pids = direct_child_pids(parent);
            if !pids.is_empty() || Instant::now() >= deadline {
                return pids;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_process_group_exit(pgid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !process_group_exists(pgid) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        !process_group_exists(pgid)
    }

    fn process_group_exists(pgid: u32) -> bool {
        let Ok(pgid) = i32::try_from(pgid) else {
            return false;
        };
        let ret = unsafe { libc::kill(-pgid, 0) };
        if ret == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}
