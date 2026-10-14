use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// --- Quiet mode ---

static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

// --- Prefixed output ---
// Every prefixed printer goes through `emit_text`, so each line reaches the run
// log and the renderer (which keeps a drawn status line intact) - none writes
// to the console directly. Prefixed output defaults to stdout; `sidecar_msg`
// narration is stderr, and `warn_stderr` is the one deliberate exception that
// bypasses the renderer (shutdown and late-history paths must not wait on it).
// Other stderr users are free-standing output (a subcommand's own report, the
// man pager), not prefixed lines.
//
// The prefix is "[tag]" padded to 10 columns, so messages align for the usual
// short tags; a longer tag ("[ratatoskr]") is followed by one space and
// overflows the column rather than being cut.
//
// Quiet mode split: `run_msg`, `download_msg` and the lock, strays and guard
// printers always print because they represent user-facing actions that should
// be visible even in non-verbose mode. The others (`build_msg`, `bench_msg`,
// `result_msg`) are suppressed in quiet mode because they are
// internal progress messages. `verify_msg` has its own gate (the buffer
// below). Hotpath interpretation notes, errors and warnings are never suppressed.

// --- Verify detail buffer ---
// `verify_msg` carries the per-subcommand detail (section headers, inspect
// dumps, element diffs). By default each verify check runs "quiet on pass,
// loud on fail": the detail is captured into a buffer and only replayed if
// the check fails; on success it's discarded and just a one-line summary
// prints. `-v`/`--verbose` skips the buffer so detail streams live.
//
// `None` = live (print immediately). `Some(vec)` = capturing into the buffer,
// one rendered block (every line prefixed) per call.
// verify holds an exclusive process lock, so the Mutex is uncontended.
static VERIFY_BUFFER: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Start capturing `verify_msg` detail into the buffer (drops any prior).
pub fn verify_buffer_begin() {
    *VERIFY_BUFFER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Vec::new());
}

/// Stop capturing and discard the buffered detail (the pass path).
pub fn verify_buffer_discard() {
    *VERIFY_BUFFER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Stop capturing and print the buffered detail (the fail path).
pub fn verify_buffer_flush() {
    let buffered = VERIFY_BUFFER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(blocks) = buffered {
        for block in blocks {
            emit_text(Stream::Out, &block);
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

static RENDER: Mutex<Renderer> = Mutex::new(Renderer {
    status: None,
    drawn: false,
});
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
    *RUN_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
}

pub fn close_run_log() {
    RUN_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
}

/// Turn the status line on when stdout is a terminal. Off otherwise: a pipe or
/// a file (the LLM case) gets persistent lines only.
pub fn enable_status_line() {
    use std::io::IsTerminal;
    STATUS_ENABLED.store(std::io::stdout().is_terminal(), Ordering::Relaxed);
}

/// Clear any drawn status line and stop drawing one.
pub fn disable_status_line() {
    let mut r = RENDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    r.status = None;
    render(&mut r, Stream::Out, "");
    STATUS_ENABLED.store(false, Ordering::Relaxed);
}

/// Show `msg` as the transient status line (a no-op off a terminal). Replaced
/// by the next status, and cleared from under every persistent line.
pub fn status(msg: &str) {
    if !STATUS_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let line: String = format!("[....]    {msg}")
        .chars()
        .take(STATUS_WIDTH)
        .collect();
    let mut r = RENDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    r.status = Some(line);
    render(&mut r, Stream::Out, "");
}

/// Narration for the run log only: what a passing run no longer prints but a
/// reader investigating one (a slow build, a cross-host comparison) needs.
/// Dropped when no log is open - so call it only from code that runs under a
/// command that opens one (`check`, `test`, measured runs, `bench`); code
/// shared with commands that do not must use [`detail_or_console`].
pub fn detail(msg: &str) {
    log_write(&prefixed("[run]     ", msg), false);
}

/// [`detail`] when a run log is open; otherwise a quiet-gated `[run]` console
/// line. For narration from shared code (the lock's toolchain move, `--commit`
/// worktrees) that also runs under commands with no run log, where a
/// log-only line would vanish.
pub fn detail_or_console(msg: &str) {
    let open = RUN_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some();
    if open {
        detail(msg);
    } else if !is_quiet() {
        emit("[run]     ", msg);
    }
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
        Some(
            RUN_LOG
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
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

/// Which console stream a persistent line goes to. The status line lives on
/// stdout; a stderr line still clears and redraws it, since both share the
/// terminal.
#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// Write `text` (whole lines) to `stream` under the renderer: clear a drawn
/// status line, write, redraw it. Write errors are ignored - a reader that
/// closed the pipe must not turn into a panic that stops the run log.
fn render(r: &mut Renderer, stream: Stream, text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    if r.drawn {
        out.write_all(b"\r\x1b[2K").ok();
        r.drawn = false;
    }
    match stream {
        Stream::Out => {
            out.write_all(text.as_bytes()).ok();
        }
        Stream::Err => {
            // Order the clear before the stderr write on a shared terminal.
            out.flush().ok();
            let mut err = std::io::stderr().lock();
            err.write_all(text.as_bytes()).ok();
            err.flush().ok();
        }
    }
    if STATUS_ENABLED.load(Ordering::Relaxed)
        && let Some(s) = &r.status
    {
        out.write_all(s.as_bytes()).ok();
        r.drawn = true;
    }
    out.flush().ok();
}

/// Print an already-prefixed block to `stream` and tee it to the run log.
/// The whole block goes out under one renderer hold, so nothing can split it.
fn emit_text(stream: Stream, text: &str) {
    log_write(text, false);
    let mut r = RENDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    render(&mut r, stream, text);
}

/// Print a persistent block (every line prefixed) and tee it to the run log.
fn emit(prefix: &str, msg: &str) {
    emit_text(Stream::Out, &prefixed(prefix, msg));
}

/// The prefix for `tag` (brackets included): padded to the 10-column prefix
/// field, and always followed by a space even when the tag is wider.
fn tag_prefix(tag: &str) -> String {
    format!("{tag:<9} ")
}

/// [`emit`] for a bare tag such as `"[lock]"`.
fn emit_tag(tag: &str, msg: &str) {
    emit(&tag_prefix(tag), msg);
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
        render(&mut r, Stream::Out, &text);
    }
}

/// Print `msg` with no tag, through the renderer and into the run log - for
/// machine-readable lines such as `check --json`'s trailer.
pub fn plain(msg: &str) {
    emit("", msg);
}

/// Print a block on stderr through the renderer and run log.
pub fn plain_stderr(msg: &str) {
    emit_text(Stream::Err, &prefixed("", msg));
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
        emit_tag("[bench]", msg);
    }
}

pub fn verify_msg(msg: &str) {
    let text = prefixed(&tag_prefix("[verify]"), msg);
    let mut guard = VERIFY_BUFFER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match guard.as_mut() {
        Some(buf) => buf.push(text),
        None => {
            // Release the buffer lock before taking the renderer's.
            drop(guard);
            emit_text(Stream::Out, &text);
        }
    }
}

/// Verify summary line - always printed immediately, bypassing the detail
/// buffer (used for each check's one-line PASS/FAIL result and the final tally).
pub fn verify_summary(msg: &str) {
    emit_tag("[verify]", msg);
}

/// Interpretation notes must remain visible even on a quiet measured run.
pub fn hotpath_msg(msg: &str) {
    emit_tag("[hotpath]", msg);
}

pub fn download_msg(msg: &str) {
    emit_tag("[download]", msg);
}

/// A planned action under `--dry-run`, tagged `[dry-run]` rather than `[run]`
/// (it is not a run). Not quiet-gated, like [`run_msg`].
pub fn dry_run_msg(msg: &str) {
    emit_tag("[dry-run]", msg);
}

/// Lock events: acquisition, the wait, `brokkr lock`, `brokkr kill`.
pub fn lock_msg(msg: &str) {
    emit_tag("[lock]", msg);
}

/// The stray-process reaps and `brokkr strays`.
pub fn strays_msg(msg: &str) {
    emit_tag("[strays]", msg);
}

/// `brokkr guard`: the rustc-wrapper fence.
pub fn guard_msg(msg: &str) {
    emit_tag("[guard]", msg);
}

pub fn sidecar_msg(msg: &str) {
    if !is_quiet() {
        // Always stderr - every [sidecar] line is narration (run provenance,
        // the stored-profile summary, "showing run N/M"), never the data the caller
        // is asking for. Keeping them off stdout lets `brokkr sidecar ...
        // --samples | jq` Just Work.
        emit_text(Stream::Err, &prefixed(&tag_prefix("[sidecar]"), msg));
    }
}

pub fn litehtml_msg(msg: &str) {
    emit_tag("[litehtml]", msg);
}

pub fn sluggrs_msg(msg: &str) {
    emit_tag("[sluggrs]", msg);
}

pub fn ratatoskr_msg(msg: &str) {
    emit_tag("[ratatoskr]", msg);
}

pub fn corpus_msg(msg: &str) {
    emit_tag("[corpus]", msg);
}

pub fn lint_msg(msg: &str) {
    emit_tag("[lint]", msg);
}

pub fn deps_msg(msg: &str) {
    emit_tag("[deps]", msg);
}

pub fn wc_msg(msg: &str) {
    emit_tag("[wc]", msg);
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
///
/// Only errors are held. A [`warn`] prints at once: the held vector replays as
/// errors, so holding a warning would either turn it into one or need a typed
/// replay at every caller, and a warning is true whether or not the held work
/// later fails.
pub fn capture_errors<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let outer = HELD_ERRORS.with(|h| h.borrow_mut().replace(Vec::new()));
    let out = f();
    let held = HELD_ERRORS
        .with(|h| std::mem::replace(&mut *h.borrow_mut(), outer))
        .unwrap_or_default();
    (out, held)
}

/// Print an error message. Multi-line messages get each line prefixed.
/// Errors are NEVER suppressed by quiet mode.
pub fn error(msg: &str) {
    if msg.is_empty() {
        return;
    }
    let held = HELD_ERRORS.with(|h| {
        h.borrow_mut()
            .as_mut()
            .map(|v| v.push(msg.to_owned()))
            .is_some()
    });
    if !held {
        emit("[error]   ", msg);
    }
}

/// Print a warning message. Multi-line messages get each line prefixed.
/// Warnings are NEVER suppressed by quiet mode. They stay on stdout during
/// `check --json` too: that flag appends its JSON summary as the last line; it
/// does not make stdout JSON-only.
pub fn warn(msg: &str) {
    if !msg.is_empty() {
        emit("[warn]    ", msg);
    }
}

/// A warning on stderr, without the console renderer or run-log locks.
/// For lock metadata and late history writes: shutdown must not wait on a
/// renderer held by another thread, and stdout may already end in JSON.
/// This is ordinary thread code, never an async signal handler.
pub fn warn_stderr(msg: &str) {
    if !msg.is_empty() {
        eprint!("{}", prefixed("[warn]    ", msg));
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

/// A byte count in binary units, `df`-style: `512B`, `3K`, `14G`, `1.8T`.
/// Shared by `brokkr env`'s storage rows and the free-space gate.
#[allow(clippy::cast_precision_loss)] // a display figure
pub fn format_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if b >= TB {
        format!("{:.1}T", b as f64 / TB as f64)
    } else if b >= GB {
        format!("{}G", b / GB)
    } else if b >= MB {
        format!("{}M", b / MB)
    } else if b >= KB {
        format!("{}K", b / KB)
    } else {
        format!("{b}B")
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

    /// The prefix field is 10 columns; a wider tag overflows by one space
    /// instead of being cut or glued to its message.
    #[test]
    fn tag_prefix_pads_to_ten_and_never_truncates() {
        assert_eq!(super::tag_prefix("[build]"), "[build]   ");
        assert_eq!(super::tag_prefix("[download]"), "[download] ");
        assert_eq!(super::tag_prefix("[ratatoskr]"), "[ratatoskr] ");
        assert_eq!(super::tag_prefix("[dry-run]"), "[dry-run] ");
        assert_eq!(
            super::prefixed(&super::tag_prefix("[lock]"), "a\nb"),
            "[lock]    a\n[lock]    b\n"
        );
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
