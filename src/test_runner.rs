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
}

pub(crate) enum LibtestOutcome {
    Completed,
    HungTest(HungTest),
}

#[derive(Clone, Debug)]
pub(crate) struct HungTest {
    pub(crate) test: String,
    pub(crate) elapsed: Duration,
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
}

impl TestTracker {
    fn observe_start(&mut self, name: String) {
        self.current.entry(name).or_insert_with(Instant::now);
    }

    fn observe_result(&mut self, name: &str) {
        self.current.remove(name);
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
pub(crate) fn streaming_run_libtest<Out, Err>(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    forward_stdout_line: Out,
    forward_stderr_line: Err,
) -> Result<LibtestRun, DevError>
where
    Out: FnMut(&str) + Send + 'static,
    Err: FnMut(&str) + Send + 'static,
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
    let stderr_thread = thread::spawn(move || {
        drain_stderr(stderr_pipe, &stderr_buf_t, forward_stderr_line);
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
                handle_stdout_line(&line, tracker, &mut forward_line, true);
                line.clear();
            } else {
                line.push(byte);
                if byte == b' ' && line.len() >= "test x ... ".len() {
                    observe_partial_start(&line, tracker);
                }
            }
        }
    }

    if !line.is_empty() {
        handle_stdout_line(&line, tracker, &mut forward_line, false);
    }
}

fn drain_stderr<F>(mut pipe: ChildStderr, buf: &Mutex<Vec<u8>>, mut forward_line: F)
where
    F: FnMut(&str),
{
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

fn observe_partial_start(line: &[u8], tracker: &Mutex<TestTracker>) {
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    let Some(name) = parse_start_marker(text) else {
        return;
    };
    if let Ok(mut t) = tracker.lock() {
        t.observe_start(name);
    }
}

fn handle_stdout_line<F>(
    line: &[u8],
    tracker: &Mutex<TestTracker>,
    forward_line: &mut F,
    terminated: bool,
) where
    F: FnMut(&str),
{
    let text = String::from_utf8_lossy(line).into_owned();
    if let Some(name) = parse_result_marker(&text)
        && let Ok(mut t) = tracker.lock()
    {
        t.observe_result(&name);
    }

    if terminated || parse_start_marker(&text).is_none() {
        forward_line(&text);
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

        let hung_test = capture_hung_test(&cwd, cargo_pid, &test, elapsed);
        if let Ok(mut slot) = hung.lock() {
            *slot = Some(hung_test);
        }
        kill_process_group(cargo_pid).ok();
        return;
    }
}

fn capture_hung_test(cwd: &Path, cargo_pid: u32, test: &str, elapsed: Duration) -> HungTest {
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
        "test {} exceeded {}s ceiling ({:.1}s elapsed)\n  killed cargo process group (pgid {}) and {}\n  /proc/{}/wchan: {}\n  /proc/{}/stack: {}\n  {}",
        hung.test,
        TEST_TIMEOUT.as_secs(),
        hung.elapsed.as_secs_f64(),
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
    if effective_test_threads_from(args)? == Some(1) {
        return Ok(());
    }
    Err(DevError::Config(
        "libtest watchdog requires --test-threads=1".into(),
    ))
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
