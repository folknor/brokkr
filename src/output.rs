use std::path::Path;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use crate::error::DevError;

// --- Quiet mode ---

static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

// --- Prefixed output ---
// All output goes to stdout (stderr reserved for panics only).
// Prefix column is 10 chars wide: "[tag]" + padding to align the message.
//
// Quiet mode split: `run_msg` and `download_msg` always print because they
// represent user-facing actions that should be visible even in non-verbose
// mode. The others (`build_msg`, `bench_msg`, `result_msg`, `hotpath_msg`)
// are suppressed in quiet mode because they are internal progress messages.
// `verify_msg` has its own gate (see `VERIFY_DETAIL`). Errors are never
// suppressed.

// --- Verify detail buffer ---
// `verify_msg` carries the per-subcommand detail (section headers, inspect
// dumps, element diffs). By default each verify check runs "quiet on pass,
// loud on fail": the detail is captured into a buffer and only replayed if
// the check fails; on success it's discarded and just a one-line summary
// prints. `-v`/`--verbose` skips the buffer so detail streams live.
//
// `None` = live (print immediately). `Some(vec)` = capturing into the buffer.
// verify holds an exclusive process lock, so the Mutex is uncontended.
static VERIFY_BUFFER: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Start capturing `verify_msg` detail into the buffer (drops any prior).
pub fn verify_buffer_begin() {
    *VERIFY_BUFFER.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Vec::new());
}

/// Stop capturing and discard the buffered detail (the pass path).
pub fn verify_buffer_discard() {
    *VERIFY_BUFFER.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Stop capturing and print the buffered detail (the fail path).
pub fn verify_buffer_flush() {
    let buffered = VERIFY_BUFFER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(lines) = buffered {
        for line in lines {
            println!("{line}");
        }
    }
}

// --- Run log and status line ---
// `brokkr check` tees every line it prints - plus the narration it no longer
// prints (`detail`) - into a per-run log file, and on a terminal keeps one
// transient status line under the output. Both live here because every
// prefixed printer has to go through them: a persistent line written past a
// drawn status line without clearing it first garbles both.
//
// Two locks, never nested: the log writer and the renderer. The watchdog must
// reach `exit(124)` whatever state either is in, so its path (`error_forced`)
// only ever try-locks, and writes around a lock it cannot get.

struct Renderer {
    status: Option<String>,
    drawn: bool,
}

static RENDER: Mutex<Renderer> = Mutex::new(Renderer { status: None, drawn: false });
static RUN_LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
static STATUS_ENABLED: AtomicBool = AtomicBool::new(false);

/// The longest status line drawn, in chars. The line is transient and is
/// never read back, so a fixed cap is enough to keep it from wrapping on an
/// ordinary terminal (a wrapped line cannot be cleared with one `\r`).
const STATUS_WIDTH: usize = 100;

/// Start teeing output into `file`. Every line printed from here on, and every
/// [`detail`] line, is appended and flushed as it happens, so a run the
/// watchdog kills still leaves everything up to the kill on disk.
pub fn open_run_log(file: std::fs::File) {
    *RUN_LOG.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
}

pub fn close_run_log() {
    RUN_LOG.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
}

/// Turn the status line on when stdout is a terminal. Off otherwise: a pipe or
/// a file (the LLM case) gets persistent lines only.
pub fn enable_status_line() {
    use std::io::IsTerminal;
    STATUS_ENABLED.store(std::io::stdout().is_terminal(), Ordering::Relaxed);
}

/// Clear any drawn status line and stop drawing one.
pub fn disable_status_line() {
    let mut r = RENDER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    r.status = None;
    render(&mut r, "");
    STATUS_ENABLED.store(false, Ordering::Relaxed);
}

/// Show `msg` as the transient status line (a no-op off a terminal). Replaced
/// by the next status, and cleared from under every persistent line.
pub fn status(msg: &str) {
    if !STATUS_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let line: String = format!("[....]    {msg}").chars().take(STATUS_WIDTH).collect();
    let mut r = RENDER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    r.status = Some(line);
    render(&mut r, "");
}

/// Narration for the run log only: what a passing run no longer prints but a
/// reader investigating one (a slow build, a cross-host comparison) needs.
/// Dropped when no log is open.
pub fn detail(msg: &str) {
    log_write(&prefixed("[run]     ", msg), false);
}

/// `prefix` on every line of `msg`, newline-terminated.
fn prefixed(prefix: &str, msg: &str) -> String {
    let mut out = String::new();
    for line in msg.lines() {
        out.push_str(prefix);
        out.push_str(line);
        out.push('\n');
    }
    if msg.is_empty() {
        out.push_str(prefix.trim_end());
        out.push('\n');
    }
    out
}

/// Append to the run log, if one is open. `forced` try-locks rather than
/// waiting (the watchdog path).
fn log_write(text: &str, forced: bool) {
    use std::io::Write;
    let guard = if forced {
        lock_patiently(&RUN_LOG)
    } else {
        Some(RUN_LOG.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    };
    if let Some(mut guard) = guard
        && let Some(file) = guard.as_mut()
    {
        file.write_all(text.as_bytes()).ok();
    }
}

/// Try a lock for about a second, then give up. For the watchdog only.
fn lock_patiently<T>(m: &Mutex<T>) -> Option<std::sync::MutexGuard<'_, T>> {
    for _ in 0..100 {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    None
}

/// Write `text` (whole lines) to stdout under the renderer: clear a drawn
/// status line, write, redraw it. Write errors are ignored - a reader that
/// closed the pipe must not turn into a panic that stops the run log.
fn render(r: &mut Renderer, text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    if r.drawn {
        out.write_all(b"\r\x1b[2K").ok();
        r.drawn = false;
    }
    out.write_all(text.as_bytes()).ok();
    if STATUS_ENABLED.load(Ordering::Relaxed)
        && let Some(s) = &r.status
    {
        out.write_all(s.as_bytes()).ok();
        r.drawn = true;
    }
    out.flush().ok();
}

/// Print a persistent block (every line prefixed) and tee it to the run log.
/// The whole block goes out under one renderer hold, so nothing can split it.
fn emit(prefix: &str, msg: &str) {
    let text = prefixed(prefix, msg);
    log_write(&text, false);
    let mut r = RENDER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    render(&mut r, &text);
}

/// [`error`] for the watchdog: never waits on either lock for long, so the
/// caller always reaches its `exit`. A renderer held for that long is a
/// thread blocked writing to a stdout nobody is reading - and that thread
/// also holds std's own stdout lock, so writing around it would block just
/// the same. The message is skipped on stdout then, and survives in the log.
pub fn error_forced(msg: &str) {
    let text = prefixed("[error]   ", msg);
    log_write(&text, true);
    if let Some(mut r) = lock_patiently(&RENDER) {
        render(&mut r, &text);
    }
}

/// Print `msg` with no tag, through the renderer and into the run log - for
/// machine-readable lines such as `check --json`'s trailer.
pub fn plain(msg: &str) {
    emit("", msg);
}

pub fn build_msg(msg: &str) {
    if !is_quiet() {
        emit("[build]   ", msg);
    }
}

pub fn run_msg(msg: &str) {
    emit("[run]     ", msg);
}

pub fn result_msg(msg: &str) {
    if !is_quiet() {
        emit("[result]  ", msg);
    }
}

pub fn bench_msg(msg: &str) {
    if !is_quiet() {
        println!("[bench]   {msg}");
    }
}

pub fn verify_msg(msg: &str) {
    let line = format!("[verify]  {msg}");
    let mut guard = VERIFY_BUFFER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match guard.as_mut() {
        Some(buf) => buf.push(line),
        None => println!("{line}"),
    }
}

/// Verify summary line - always printed immediately, bypassing the detail
/// buffer (used for each check's one-line PASS/FAIL result and the final tally).
pub fn verify_summary(msg: &str) {
    println!("[verify]  {msg}");
}

pub fn hotpath_msg(msg: &str) {
    if !is_quiet() {
        println!("[hotpath] {msg}");
    }
}

pub fn download_msg(msg: &str) {
    println!("[download] {msg}");
}

pub fn lock_msg(msg: &str) {
    println!("[lock]    {msg}");
}

#[allow(dead_code)]
pub fn history_msg(msg: &str) {
    println!("[history] {msg}");
}

pub fn sidecar_msg(msg: &str) {
    if !is_quiet() {
        // Always stderr - every [sidecar] line is narration (run provenance,
        // "attached to pid X", "showing run N/M"), never the data the caller
        // is asking for. Keeping them off stdout lets `brokkr sidecar …
        // --samples | jq` Just Work.
        eprintln!("[sidecar] {msg}");
    }
}

pub fn litehtml_msg(msg: &str) {
    println!("[litehtml] {msg}");
}

pub fn sluggrs_msg(msg: &str) {
    println!("[sluggrs] {msg}");
}

pub fn ratatoskr_msg(msg: &str) {
    println!("[ratatoskr] {msg}");
}

pub fn corpus_msg(msg: &str) {
    println!("[corpus]  {msg}");
}

pub fn lint_msg(msg: &str) {
    println!("[lint]    {msg}");
}

pub fn harness_msg(msg: &str) {
    println!("[harness] {msg}");
}

pub fn deps_msg(msg: &str) {
    println!("[deps]    {msg}");
}

pub fn wc_msg(msg: &str) {
    println!("[wc]      {msg}");
}

thread_local! {
    /// The errors this thread is holding back instead of printing, while a
    /// [`capture_errors`] call is in progress.
    static HELD_ERRORS: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Run `f`, holding back every [`error`] it prints on this thread, and return
/// them beside its result. For work whose failure is not yet a failure of the
/// command: a preparation that may degrade to "no inventory" and then be
/// retried by the step that actually needs it, which prints its own error once.
/// Calls nest: an inner call's errors are returned to it and the outer call's
/// holding resumes where it was.
pub fn capture_errors<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let outer = HELD_ERRORS.with(|h| h.borrow_mut().replace(Vec::new()));
    let out = f();
    let held = HELD_ERRORS.with(|h| std::mem::replace(&mut *h.borrow_mut(), outer)).unwrap_or_default();
    (out, held)
}

/// Print an error message. Multi-line messages get each line prefixed.
/// Errors are NEVER suppressed by quiet mode.
pub fn error(msg: &str) {
    if msg.is_empty() {
        return;
    }
    let held = HELD_ERRORS.with(|h| {
        h.borrow_mut().as_mut().map(|v| v.push(msg.to_owned())).is_some()
    });
    if !held {
        emit("[error]   ", msg);
    }
}

/// Print a warning message. Multi-line messages get each line prefixed.
/// Warnings are NEVER suppressed by quiet mode.
pub fn warn(msg: &str) {
    if !msg.is_empty() {
        emit("[warn]    ", msg);
    }
}

/// `"1 rule"` / `"5 rules"` - a count with its noun, pluralized by the
/// count instead of the `(s)` hedge. For the regular `+s` nouns brokkr's
/// messages use; a noun with an irregular plural wants its own format
/// string, not a smarter helper.
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else if noun.ends_with('s') {
        format!("{n} {noun}es")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod count_tests {
    // The whole point is the `1` case: `1 rules` (and the old `1 rule(s)`
    // hedge) is what the helper exists to remove.
    #[test]
    fn one_is_singular_everything_else_plural() {
        assert_eq!(super::count(1, "rule"), "1 rule");
        assert_eq!(super::count(0, "rule"), "0 rules");
        assert_eq!(super::count(5, "workspace package"), "5 workspace packages");
    }
}

// --- Subprocess types ---

/// Captured output from a subprocess.
pub struct CapturedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
}

/// Exit code and elapsed time from a passthrough subprocess.
pub struct PassthroughOutput {
    pub code: i32,
    pub elapsed: Duration,
}

impl CapturedOutput {
    /// Return `Ok(())` if the process exited successfully, or a `DevError::Subprocess`
    /// with the captured stderr if it failed.
    pub fn check_success(&self, program: &str) -> Result<(), DevError> {
        self.check_success_or(program, &[])
    }

    /// Like `check_success`, but also treats the given exit codes as success.
    /// For example, `diff` uses exit 1 to mean "differences found" (not an error).
    pub fn check_success_or(&self, program: &str, ok_codes: &[i32]) -> Result<(), DevError> {
        use std::os::unix::process::ExitStatusExt;

        if self.status.success() {
            return Ok(());
        }
        if let Some(code) = self.status.code()
            && ok_codes.contains(&code)
        {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&self.stderr).into_owned();
        // `DevError::Subprocess` carries no signal, so name it here: with no
        // exit code the variant can only say the child "ended without" one.
        let stderr = match (self.status.code(), self.status.signal()) {
            (None, Some(sig)) if stderr.trim().is_empty() => format!("killed by signal {sig}"),
            (None, Some(sig)) => format!("killed by signal {sig}\n{stderr}"),
            _ => stderr,
        };
        Err(DevError::Subprocess {
            program: program.to_owned(),
            code: self.status.code(),
            stderr,
        })
    }
}

/// Run a subprocess, capturing stdout and stderr.
///
/// Returns `CapturedOutput` on success (even if the process exited non-zero).
/// Returns `DevError::Spawn` only if the process could not be spawned.
pub fn run_captured(program: &str, args: &[&str], cwd: &Path) -> Result<CapturedOutput, DevError> {
    run_captured_with_env(program, args, cwd, &[])
}

/// As [`run_captured`], but invokes `on_spawn` with the child's PID
/// immediately after `Command::spawn` returns. Lets callers (notably
/// `cargo_build_observed`) publish the live PID into the lockfile so
/// `brokkr kill --hard` during a long cargo build can SIGKILL cargo too.
/// `env` adds variables on top of the inherited environment - the worktree
/// build path uses it to pin `CARGO_TARGET_DIR`.
pub fn run_captured_observed(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    on_spawn: Option<&dyn Fn(u32)>,
    isolate_pg: bool,
) -> Result<CapturedOutput, DevError> {
    let dc = run_captured_with_env_and_deadline(
        program,
        args,
        cwd,
        env,
        Duration::MAX,
        on_spawn,
        isolate_pg,
    )?;
    Ok(dc.captured)
}

/// Run a subprocess with extra environment variables, capturing stdout and stderr.
///
/// Variables are added on top of the inherited environment.
pub fn run_captured_with_env(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
) -> Result<CapturedOutput, DevError> {
    // Route through the deadline+observed runner so the shutdown-flag
    // poll covers cargo build / cargo metadata too. `Duration::MAX`
    // disables the deadline branch in practice (kernel pid lifetime is
    // measured in hours, this in eons).
    let dc = run_captured_with_env_and_deadline(
        program,
        args,
        cwd,
        env,
        Duration::MAX,
        None,
        false,
    )?;
    Ok(dc.captured)
}

/// Captured output plus a flag indicating whether the deadline fired.
///
/// Returned by [`run_captured_with_env_and_deadline`] only - the regular
/// captured-output paths cannot trigger a deadline kill, so they keep
/// their plain [`CapturedOutput`] return type.
pub struct DeadlineCapture {
    pub captured: CapturedOutput,
    /// `true` when the child was SIGKILL'd because `deadline` elapsed
    /// before it exited on its own. The captured `status` will reflect
    /// the SIGKILL (signal=9 on Linux); this flag is what callers should
    /// branch on to surface "ceiling exceeded" in user output.
    pub killed_on_deadline: bool,
    /// `true` when the child exited but its output did not close in time and
    /// capture was stopped (see `settle_drains`): `stdout`/`stderr` may be
    /// truncated, whatever `status` says. A caller whose verdict reads the
    /// output must not trust it when this is set.
    pub output_cut: bool,
}

/// How often to poll `Child::try_wait` while waiting for a deadline-bounded
/// run. Matches `ServiceClient::observe_child_exit`'s 50 ms cadence so the
/// brokkr-side and runtime-side loops have the same scheduling granularity.
const DEADLINE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Spawn a subprocess with captured stdio and a wall-clock deadline.
///
/// Drains stdout and stderr in background threads (otherwise a child
/// that prints more than the pipe buffer holds - ~64 KiB - would block
/// while we're polling for exit). Polls `Child::try_wait` at the
/// [`DEADLINE_POLL_INTERVAL`] cadence and SIGKILLs the child if `deadline`
/// elapses first.
///
/// The captured `status` reflects whatever the kernel actually reaped:
/// the child's own exit code/signal if it finished within the deadline,
/// or signal=9 if brokkr killed it. Callers should branch on
/// `killed_on_deadline` (not the status alone) to distinguish a child
/// that died on its own from one we killed.
/// `on_spawn` is invoked with the child's PID immediately after
/// `Command::spawn` returns. Callers can use it to publish the live PID
/// into the lockfile so concurrent `brokkr lock` invocations can see what
/// is currently running.
///
/// `isolate_pg` puts the child in its own process group via
/// `process_group(0)` and switches the deadline / cooperative-SIGTERM
/// kill paths to `kill(-pgid, ...)` so descendants (rustc, sæhrimnir
/// listeners, harness helpers, etc.) go down with the leader. Only set
/// to `true` when the caller has a `SigtermGuard` (or equivalent)
/// active for the lifetime of the spawn - otherwise terminal Ctrl-C
/// kills brokkr but leaves the PG-detached child orphaned.
pub fn run_captured_with_env_and_deadline(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    deadline: Duration,
    on_spawn: Option<&dyn Fn(u32)>,
    isolate_pg: bool,
) -> Result<DeadlineCapture, DevError> {
    run_captured_limited(program, args, cwd, env, Limit::Wall(deadline), on_spawn, isolate_pg)
}

/// As [`run_captured_with_env_and_deadline`], but the limit is on
/// *inactivity*: the child is killed (`killed_on_deadline`) only once `idle`
/// has passed with no byte arriving on either stdout or stderr. Every chunk
/// of output restarts the clock, so a long build that keeps reporting
/// progress runs as long as it needs, while one parked with nothing to say
/// (cargo "Blocking waiting for file lock" behind another cargo) is killed.
/// The wall-time bound on such a run is the caller's phase ceiling.
pub fn run_captured_with_idle_deadline(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    idle: Duration,
    isolate_pg: bool,
) -> Result<DeadlineCapture, DevError> {
    run_captured_limited(program, args, cwd, env, Limit::Idle(idle), None, isolate_pg)
}

/// What bounds a captured run.
#[derive(Clone, Copy)]
enum Limit {
    /// Total wall time since spawn.
    Wall(Duration),
    /// Time since the last output byte (or since spawn, before the first).
    Idle(Duration),
}

/// Read `pipe` on a new thread until EOF or until the drain is cancelled,
/// stamping `last_output_ms` (millis since `start`) on every chunk - the
/// progress signal [`Limit::Idle`] reads.
///
/// The bytes land in a shared buffer rather than the thread's return value,
/// and the read polls rather than blocks, so [`settle_drains`] can stop a drain
/// that will never reach EOF, join it, and keep what it already read.
fn drain_stamped<R>(pipe: R, start: Instant, last_output_ms: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Drain
where
    R: std::io::Read + std::os::fd::AsRawFd + Send + 'static,
{
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (buf_t, cancel_t) = (std::sync::Arc::clone(&buf), std::sync::Arc::clone(&cancel));
    let handle = std::thread::spawn(move || {
        let mut reader = pipe;
        let mut chunk = [0u8; 8192];
        while let Some(n) = crate::test_runner::read_unless_cancelled(&mut reader, &mut chunk, &cancel_t)
        {
            if let Ok(mut b) = buf_t.lock() {
                b.extend_from_slice(chunk.get(..n).unwrap_or_default());
            }
            let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            last_output_ms.store(now, Ordering::Relaxed);
        }
        // `reader` drops here, closing brokkr's end of the pipe: a process
        // still holding the other end gets EPIPE rather than an unbounded
        // reader behind it.
    });
    Drain { buf, cancel, handle }
}

/// One output drain: the buffer it fills, its stop switch, and its thread.
struct Drain {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

/// How long a captured child's output gets to close once the child exited.
const DRAIN_SETTLE: Duration = Duration::from_secs(2);

/// Wait up to [`DRAIN_SETTLE`] for the drains to reach EOF, then stop any
/// still open, join them all, and return what they read plus whether capture
/// was cut short.
///
/// A process the child started (a daemon, a build-script helper) that
/// inherited a pipe keeps it open forever, and the unbounded join that used to
/// stand here hung brokkr with the lock held. A stopped drain closes its end,
/// so nothing keeps reading - or buffering - after the run is over.
fn settle_drains(drains: Vec<Drain>, program: &str) -> (Vec<Vec<u8>>, bool) {
    let until = Instant::now() + DRAIN_SETTLE;
    while Instant::now() < until && !drains.iter().all(|d| d.handle.is_finished()) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let cut = !drains.iter().all(|d| d.handle.is_finished());
    if cut {
        warn(&format!(
            "{program} exited but its output did not close within {}s - something it started \
             still holds the pipe; capture stopped there",
            DRAIN_SETTLE.as_secs()
        ));
    }
    let bufs = drains
        .into_iter()
        .map(|d| {
            d.cancel.store(true, Ordering::Release);
            d.handle.join().ok();
            d.buf.lock().map(|b| b.clone()).unwrap_or_default()
        })
        .collect();
    (bufs, cut)
}

/// Shared body of the captured runners; see
/// [`run_captured_with_env_and_deadline`] for the spawn, kill and interrupt
/// contract.
fn run_captured_limited(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    limit: Limit,
    on_spawn: Option<&dyn Fn(u32)>,
    isolate_pg: bool,
) -> Result<DeadlineCapture, DevError> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for &(key, value) in env {
        cmd.env(key, value);
    }
    crate::oom::protect_child(&mut cmd);
    // Last, after every caller-supplied env var, so a caller cannot displace
    // the capability that lets this child compile under the hold. See
    // `crate::hold` for why the mark is inherited rather than inferred, and why
    // it is stamped on every child rather than only the ones named cargo.
    crate::hold::stamp(&mut cmd);
    // PG isolation is opt-in: the caller asserts a SigtermGuard (or
    // equivalent) is active so terminal Ctrl-C bridges to the PG via
    // the wait-loop's flag-poll. Without that bridge, isolating the
    // child would orphan it on ctrl-C; without a SigtermGuard tracking
    // children alone (PID published, no PG) keeps them in brokkr's PG
    // so ctrl-C reaches them naturally.
    use std::os::unix::process::CommandExt;
    if isolate_pg {
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|error| DevError::Spawn {
        program: program.to_owned(),
        error,
    })?;
    if let Some(cb) = on_spawn {
        cb(child.id());
    }

    // Milliseconds since `start` at which the last output chunk arrived - the
    // idle clock's reference. Written by both drain threads.
    let last_output_ms = Arc::new(AtomicU64::new(0));
    let stdout_thread = child.stdout.take().map(|p| drain_stamped(p, start, Arc::clone(&last_output_ms)));
    let stderr_thread = child.stderr.take().map(|p| drain_stamped(p, start, Arc::clone(&last_output_ms)));
    let expired = || match limit {
        Limit::Wall(deadline) => start.elapsed() >= deadline,
        Limit::Idle(idle) => {
            let last = Duration::from_millis(last_output_ms.load(Ordering::Relaxed));
            start.elapsed().saturating_sub(last) >= idle
        }
    };

    let mut killed_on_deadline = false;
    let mut interrupted = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if crate::shutdown::is_shutdown_requested() {
                    // `brokkr kill` (SIGTERM) reached us. Forward SIGTERM
                    // to the child, give it a brief budget to clean up,
                    // then SIGKILL. The Err propagates as Interrupted so
                    // the orchestrator can run its mock-teardown path
                    // before main's scratch-cleanup.
                    forward_sigterm_then_kill(&mut child, isolate_pg);
                    interrupted = true;
                    break child.wait().map_err(|error| DevError::Spawn {
                        program: program.to_owned(),
                        error,
                    })?;
                }
                if expired() {
                    // SIGKILL: PG-isolated children get a `kill(-pgid,
                    // ...)` sweep so descendants don't outlive the
                    // deadline; non-isolated children share brokkr's
                    // PG, so `child.kill()` (single-PID SIGKILL) is
                    // the right hammer - sending to -pid would also
                    // signal brokkr itself.
                    if isolate_pg {
                        crate::ratatoskr::process::send_signal_pgrp(child.id(), libc::SIGKILL).ok();
                    }
                    drop(child.kill());
                    killed_on_deadline = true;
                    break child.wait().map_err(|error| DevError::Spawn {
                        program: program.to_owned(),
                        error,
                    })?;
                }
                std::thread::sleep(DEADLINE_POLL_INTERVAL);
            }
            Err(error) => {
                return Err(DevError::Spawn {
                    program: program.to_owned(),
                    error,
                });
            }
        }
    };
    let drains: Vec<_> = stdout_thread.into_iter().chain(stderr_thread).collect();
    if interrupted {
        // Bounded like the ordinary path, so an interrupt cannot hang either.
        settle_drains(drains, program);
        return Err(DevError::Interrupted);
    }

    let (bufs, output_cut) = settle_drains(drains, program);
    let mut bufs = bufs.into_iter();
    let stdout = bufs.next().unwrap_or_default();
    let stderr = bufs.next().unwrap_or_default();
    let elapsed = start.elapsed();

    Ok(DeadlineCapture {
        captured: CapturedOutput {
            status,
            stdout,
            stderr,
            elapsed,
        },
        killed_on_deadline,
        output_cut,
    })
}

/// Forward SIGTERM to the child, give it [`SIGTERM_FORWARD_BUDGET`] to
/// honour it, then escalate to SIGKILL. Used by
/// [`run_captured_with_env_and_deadline`] when `brokkr kill` reaches us
/// mid-orchestration.
fn forward_sigterm_then_kill(child: &mut std::process::Child, isolate_pg: bool) {
    let pid = child.id();
    // PG-isolated children: SIGTERM the group so sæhrimnir / cargo /
    // rustc helpers get the cooperative shutdown too. Non-isolated
    // children share brokkr's PG (so the same `kill -<pgid>` would also
    // signal brokkr) - SIGTERM only the leader; if the user is doing
    // ctrl-C from the terminal, the child already received SIGINT via
    // the foreground PG anyway.
    if isolate_pg {
        crate::ratatoskr::process::send_signal_pgrp(pid, libc::SIGTERM).ok();
    } else {
        // SAFETY: sending SIGTERM to our own child by PID; ESRCH is
        // benign (handled by the wait loop below).
        unsafe { libc::kill(pid.cast_signed(), libc::SIGTERM) };
    }
    let term_sent = Instant::now();
    while term_sent.elapsed() < SIGTERM_FORWARD_BUDGET {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(DEADLINE_POLL_INTERVAL),
            Err(_) => break,
        }
    }
    // SIGKILL escalation: PG for isolated, single-PID for non-isolated.
    if isolate_pg {
        crate::ratatoskr::process::send_signal_pgrp(pid, libc::SIGKILL).ok();
    }
    drop(child.kill());
}

/// How long to give a captured child to honour SIGTERM after `brokkr kill`
/// before we escalate to SIGKILL. Matches sæhrimnir's
/// [`crate::ratatoskr::saehrimnir::SHUTDOWN_BUDGET`] in spirit: long enough
/// for cooperative cleanup, short enough that the user doesn't think
/// `brokkr kill` hung.
const SIGTERM_FORWARD_BUDGET: Duration = Duration::from_millis(1500);

/// Spawn a subprocess with captured stdio, returning the `Child` handle.
///
/// The caller is responsible for waiting on the child and collecting output.
/// Used by the sidecar to run sampling alongside the child process.
pub fn spawn_captured(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    isolate_pg: bool,
) -> Result<std::process::Child, DevError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for &(key, value) in env {
        cmd.env(key, value);
    }
    crate::oom::protect_child(&mut cmd);
    // Last, after every caller-supplied env var, so a caller cannot displace
    // the capability that lets this child compile under the hold. See
    // `crate::hold` for why the mark is inherited rather than inferred, and why
    // it is stamped on every child rather than only the ones named cargo.
    crate::hold::stamp(&mut cmd);
    // PG isolation is opt-in for the same reason as the deadline
    // runner: the caller asserts a SigtermGuard is active so terminal
    // signals bridge to the PG. The sidecar's own SigtermGuard (around
    // `run_sidecar`) is the typical pairing.
    use std::os::unix::process::CommandExt;
    if isolate_pg {
        cmd.process_group(0);
    }

    cmd.spawn().map_err(|error| DevError::Spawn {
        program: program.to_owned(),
        error,
    })
}

/// Run a subprocess with inherited stdio (passthrough mode), returning timing.
///
/// If the process is killed by a signal (e.g. OOM killer SIGKILL), returns a
/// `DevError::Subprocess` with the signal number instead of silently mapping
/// to exit code 1.
pub fn run_passthrough_timed(
    program: &str,
    args: &[&str],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<PassthroughOutput, DevError> {
    run_passthrough_in(program, args, None, &[], lock)
}

/// [`run_passthrough_timed`] with a working directory and environment.
///
/// Same lifecycle guarantees - that is the point of having it. A caller needing
/// to set `cwd` or an env var previously had to reach for a bare
/// `Command::status()`, which silently gave up the `SigtermGuard`, the OOM
/// marking and the lockfile PID publication. `brokkr bench` did exactly that.
pub fn run_passthrough_in(
    program: &str,
    args: &[&str],
    dir: Option<&std::path::Path>,
    env: &[(&str, &str)],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<PassthroughOutput, DevError> {
    use std::os::unix::process::ExitStatusExt;

    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    crate::oom::protect_child(&mut cmd);
    // Last, after every caller-supplied env var, so a caller cannot displace
    // the capability that lets this child compile under the hold. See
    // `crate::hold` for why the mark is inherited rather than inferred, and why
    // it is stamped on every child rather than only the ones named cargo.
    crate::hold::stamp(&mut cmd);

    // A passthrough child (elivagar `regress`, elivagar run-mode dispatch) is a
    // real, long-running workload with no sidecar window around it. Historically
    // this used a bare `cmd.status()`, which left brokkr with the default
    // SIGTERM disposition: a `brokkr kill` (SIGTERM to brokkr's PID only) then
    // terminated brokkr and orphaned the child, which kept running. Install a
    // `SigtermGuard` and spawn+poll instead so a `brokkr kill` (or terminal
    // ctrl-C) reaches the child. The child stays in brokkr's process group -
    // not isolated - so a terminal SIGINT still hits it directly, and the guard
    // covers the `brokkr kill` case where only brokkr's PID is signalled.
    let _guard = crate::shutdown::SigtermGuard::install();
    let mut child = cmd.spawn().map_err(|error| DevError::Spawn {
        program: program.to_owned(),
        error,
    })?;
    // Publish the child PID into the lockfile so `brokkr kill --hard` (which
    // reads child_pid from the lock file rather than holding the Child handle)
    // can SIGKILL it, and `brokkr lock` can show it. Cleared on every exit path
    // below so a recycled PID can't be killed after this returns.
    if let Some(lock) = lock {
        lock.set_child_pid(child.id());
    }

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if crate::shutdown::is_shutdown_requested() {
                    // `brokkr kill` (SIGTERM) or ctrl-C reached us. Forward
                    // SIGTERM to the child (single-PID: it shares brokkr's PG),
                    // give it a brief budget, then SIGKILL, and surface the run
                    // as interrupted so main's cleanup path handles scratch.
                    forward_sigterm_then_kill(&mut child, false);
                    let _reaped = child.wait();
                    if let Some(lock) = lock {
                        lock.clear_child_pid();
                    }
                    return Err(DevError::Interrupted);
                }
                std::thread::sleep(DEADLINE_POLL_INTERVAL);
            }
            Err(error) => {
                if let Some(lock) = lock {
                    lock.clear_child_pid();
                }
                return Err(DevError::Spawn {
                    program: program.to_owned(),
                    error,
                });
            }
        }
    };
    if let Some(lock) = lock {
        lock.clear_child_pid();
    }

    // A stop may arrive as the child exits, before the polling loop sees it.
    // It still owns the verdict, including when cargo handled SIGINT itself.
    if crate::shutdown::is_shutdown_requested() {
        return Err(DevError::Interrupted);
    }

    let elapsed = start.elapsed();

    match status.code() {
        Some(code) => Ok(PassthroughOutput { code, elapsed }),
        None => {
            let signal = status.signal().unwrap_or(0);
            let signal_name = match signal {
                9 => " (SIGKILL - possible OOM kill)",
                15 => " (SIGTERM)",
                11 => " (SIGSEGV)",
                _ => "",
            };
            Err(DevError::Subprocess {
                program: program.to_owned(),
                code: None,
                stderr: format!("killed by signal {signal}{signal_name}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    fn cwd() -> &'static Path {
        Path::new(".")
    }

    /// A child that keeps printing outlives a wall time several idle windows
    /// long; one that goes quiet is killed one window after its last output.
    #[test]
    fn idle_deadline_resets_on_output_and_fires_on_silence() {
        let idle = Duration::from_millis(600);
        let chatty = run_captured_with_idle_deadline(
            "sh",
            &["-c", "for i in 1 2 3 4 5 6 7 8; do echo $i; sleep 0.2; done"],
            cwd(),
            &[],
            idle,
            true,
        )
        .unwrap();
        assert!(!chatty.killed_on_deadline, "progress must keep resetting the clock");
        assert!(chatty.captured.status.success());
        assert!(chatty.captured.elapsed > idle);

        let quiet = run_captured_with_idle_deadline(
            "sh",
            &["-c", "echo start; sleep 30"],
            cwd(),
            &[],
            idle,
            true,
        )
        .unwrap();
        assert!(quiet.killed_on_deadline);
        assert!(quiet.captured.elapsed < Duration::from_secs(10));
        assert_eq!(String::from_utf8_lossy(&quiet.captured.stdout).trim(), "start");
    }

    #[test]
    fn deadline_lets_short_runs_finish_normally() {
        let result =
            run_captured_with_env_and_deadline("/bin/true", &[], cwd(), &[], Duration::from_secs(5), None, false)
                .unwrap();
        assert!(!result.killed_on_deadline);
        assert!(result.captured.status.success());
        assert_eq!(result.captured.status.code(), Some(0));
    }

    #[test]
    fn deadline_kills_runaway_child() {
        // /bin/sleep 30 will outlive a 250 ms deadline by orders of
        // magnitude; brokkr should reap it quickly.
        let start = std::time::Instant::now();
        let result = run_captured_with_env_and_deadline(
            "/bin/sleep",
            &["30"],
            cwd(),
            &[],
            Duration::from_millis(250),
            None,
            false,
        )
        .unwrap();
        let elapsed = start.elapsed();
        assert!(result.killed_on_deadline);
        // SIGKILL on Linux is signal 9; status.code() is None for
        // signal-killed children.
        assert_eq!(result.captured.status.signal(), Some(9));
        assert!(result.captured.status.code().is_none());
        // Should finish well inside one poll interval after the deadline,
        // plus a healthy slack budget for slow CI hardware.
        assert!(
            elapsed < Duration::from_secs(5),
            "deadline kill took too long: {elapsed:?}"
        );
    }

    #[test]
    fn deadline_captures_stdout_from_short_run() {
        let result = run_captured_with_env_and_deadline(
            "/bin/echo",
            &["hello", "world"],
            cwd(),
            &[],
            Duration::from_secs(5),
            None,
            false,
        )
        .unwrap();
        assert!(!result.killed_on_deadline);
        assert_eq!(result.captured.stdout, b"hello world\n");
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    /// Errors printed inside a capture are returned, not printed; the capture
    /// nests (an inner call gets its own, the outer's resumes); and nothing is
    /// held after it ends.
    #[test]
    fn capture_errors_holds_back_and_nests() {
        let (value, held) = capture_errors(|| {
            error("outer one");
            let (inner, inner_held) = capture_errors(|| {
                error("inner");
                7
            });
            assert_eq!((inner, inner_held), (7, vec!["inner".to_owned()]));
            error("outer two");
            "done"
        });
        assert_eq!(value, "done");
        assert_eq!(held, vec!["outer one".to_owned(), "outer two".to_owned()]);
        // Outside any capture the thread holds nothing back.
        assert!(HELD_ERRORS.with(|h| h.borrow().is_none()));
        let (_, nothing) = capture_errors(|| ());
        assert!(nothing.is_empty());
    }
}

