//! The `[[script_check]]` phase: run a command and assert its output.
//!
//! Some pre-commit gates do not fit a built-in phase because their logic is
//! semantic or formatter-specific (e.g. a `# Panics`/`# Errors` doc analyser). A
//! `[[script_check]]` runs an arbitrary `command` via `sh -c` and passes iff
//! its captured output matches a configured `expect` sentinel per `match` and
//! `stream`. Asserting on a success sentinel - not the exit code - is the
//! point: it catches a check silently stubbed to exit 0, because the script
//! must prove it ran to completion by emitting the sentinel. The exit code is
//! therefore ignored; spawn failure and cooperative shutdown are errors.
//!
//! A script-check may run cargo under the lock brokkr already holds; it is
//! admitted by the inherited hold capability like every other child - see
//! [`run_one`].
//!
//! Each entry runs under a deadline the caller derives from the phase ceiling,
//! so one hung script fails as that entry - its captured output shown, the
//! other entries still run - instead of taking the whole run down through the
//! phase watchdog.
//! Each output stream retains at most [`CAPTURE_LIMIT`] bytes. If a selected
//! stream exceeds that cap, or its pipe remains open past the drain grace, the
//! entry fails because capture cannot prove the complete stream matched.
//!
//! This module is the logic (`evaluate` + `run_one`); orchestration and
//! failure formatting live in `check_cmd::phase::run_script_checks`, mirroring
//! how `textlint`/`manifest` split scan-logic from phase-plumbing.

use std::path::Path;
use std::time::Duration;

use crate::config::{MatchMode, ScriptCheck, Stream};
use crate::error::DevError;

/// Maximum retained bytes per stream for one script-check. The child keeps
/// running after this point, but the supervisor drains and discards the rest.
const CAPTURE_LIMIT: usize = 1024 * 1024;

/// The captured result of running one script-check.
pub struct Outcome {
    /// Whether the output matched the `expect` sentinel. Always false for a
    /// run killed on its deadline, whatever it printed before the kill.
    pub passed: bool,
    /// `Some(deadline)` when the command was killed for running past it.
    pub timed_out: Option<Duration>,
    /// The retained stdout prefix (shown on failure).
    pub stdout: Vec<u8>,
    /// The retained stderr prefix (shown on failure).
    pub stderr: Vec<u8>,
    /// True when stdout exceeded the retained prefix.
    pub stdout_truncated: bool,
    /// True when stderr exceeded the retained prefix.
    pub stderr_truncated: bool,
    /// True when a writer kept a pipe open beyond the supervisor's drain grace.
    pub capture_incomplete: bool,
}

/// Run one `[[script_check]]` and evaluate its output.
///
/// The command is run as `sh -c "<command>"` with `cwd` as the working
/// directory (the code tree), so pipes, redirects, and env expansion work.
/// Returns `Err` only when the process could not be spawned, or on a
/// cooperative shutdown (`Interrupted`).
///
/// `deadline` bounds the command's wall time. The shell runs in its own
/// process group, so a deadline kill takes the whole pipeline down, not just
/// `sh`; that is safe because every caller runs inside `check`'s
/// `SigtermGuard`, whose flag poll forwards an interrupt to the group.
///
/// A script-check runs *inside* a brokkr command that already holds the lock,
/// so a cargo it starts is brokkr's own work and must not be refused by the
/// rustc guard (`src/bin/rustc_guard.rs`). It gets that admission the same way
/// every other child of a hold does: the inherited capability, stamped by the
/// spawn choke point (`crate::hold`). Nothing special is needed here any more.
///
/// This used to export `BROKKR_CARGO=1` explicitly, because the guard's other
/// admission path was a `/proc` ancestor walk that arbitrary shell could break
/// by detaching, re-execing or reparenting - observed as a `cargo doc`
/// script-check refused at its `rustc -vV` probe. The capability survives all
/// three, so the hand-granted override is gone: `BROKKR_CARGO` is a *human*
/// escape hatch that bypasses the lease protocol entirely, and handing it to
/// brokkr's own children would let a script descendant keep compiling into
/// later holds it was never authorized for.
pub fn run_one(check: &ScriptCheck, cwd: &Path, deadline: Duration) -> Result<Outcome, DevError> {
    let signal_guard = crate::shutdown::SigtermGuard::install();
    let run = crate::output::child::run_captured_bounded(
        "sh",
        &["-c", &check.command],
        cwd,
        &[],
        deadline,
        None,
        crate::shutdown::ChildIsolation::ProcessGroup(&signal_guard),
        crate::output::child::CaptureCap {
            stdout: CAPTURE_LIMIT + 1,
            stderr: CAPTURE_LIMIT + 1,
        },
    )?;
    let timed_out = run.killed_on_deadline().then_some(deadline);
    let capture_incomplete = run.output_cut;
    let mut captured = run.captured;
    let stdout_truncated = captured.stdout.len() > CAPTURE_LIMIT;
    let stderr_truncated = captured.stderr.len() > CAPTURE_LIMIT;
    captured.stdout.truncate(CAPTURE_LIMIT);
    captured.stderr.truncate(CAPTURE_LIMIT);
    let relevant_output_truncated = match check.stream {
        Stream::Stdout => stdout_truncated,
        Stream::Stderr => stderr_truncated,
        Stream::Both => stdout_truncated || stderr_truncated,
    };
    let passed = timed_out.is_none()
        && !capture_incomplete
        && !relevant_output_truncated
        && evaluate(
            &check.expect,
            check.match_mode,
            check.stream,
            &captured.stdout,
            &captured.stderr,
        );
    Ok(Outcome {
        passed,
        timed_out,
        stdout: captured.stdout,
        stderr: captured.stderr,
        stdout_truncated,
        stderr_truncated,
        capture_incomplete,
    })
}

/// Decide whether captured output matches `expect`. Pure - no process spawn -
/// so the match matrix is unit-testable in isolation.
pub fn evaluate(
    expect: &str,
    mode: MatchMode,
    stream: Stream,
    stdout: &[u8],
    stderr: &[u8],
) -> bool {
    let text = match stream {
        Stream::Stdout => String::from_utf8_lossy(stdout).into_owned(),
        Stream::Stderr => String::from_utf8_lossy(stderr).into_owned(),
        Stream::Both => {
            // The streams are captured separately, so their original write
            // order is unavailable. Define `both` as stdout, then a newline,
            // then stderr; exact can match either stream alone (after trim)
            // or the combined text, while last-line uses stderr's final
            // non-empty line when stderr has one.
            let mut s = String::from_utf8_lossy(stdout).into_owned();
            s.push('\n');
            s.push_str(&String::from_utf8_lossy(stderr));
            s
        }
    };
    match mode {
        MatchMode::Exact => text.trim() == expect.trim(),
        MatchMode::LastLine => last_non_empty_line(&text).map(str::trim) == Some(expect.trim()),
        MatchMode::Contains => text.contains(expect),
    }
}

/// The last line of `text` that is not blank (after trimming), or `None` when
/// every line is blank. Lets `last-line` tolerate a trailing newline and any
/// blank progress lines above the final verdict.
fn last_non_empty_line(text: &str) -> Option<&str> {
    text.lines().rev().find(|l| !l.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::{evaluate, run_one};
    use crate::config::{Diagnostics, MatchMode, ScriptCheck, Stage, Stream};
    use crate::rustc_text::{Level, rustc_blocks};

    /// A hung script fails as its own entry at its deadline - pipeline and all,
    /// since the shell runs in its own process group - instead of holding the
    /// phase until the watchdog ends the whole run.
    #[test]
    fn a_hung_script_fails_at_its_deadline() {
        let check = ScriptCheck {
            name: "hangs".into(),
            // The sentinel is printed before the hang: a killed run must not
            // pass on output it produced before its deadline.
            command: "echo DONE; sleep 30 | cat".into(),
            expect: "DONE".into(),
            match_mode: MatchMode::Contains,
            stream: Stream::Stdout,
            stage: Stage::PreClippy,
            diagnostics: Diagnostics::Opaque,
        };
        let started = std::time::Instant::now();
        let deadline = std::time::Duration::from_millis(300);
        let outcome = run_one(&check, std::path::Path::new("."), deadline).expect("spawned");
        assert_eq!(outcome.timed_out, Some(deadline));
        assert!(!outcome.passed);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn blocks_split_on_column_zero_headers_only() {
        // The shape the feature was built for: a handful of errors buried in
        // warnings, each diagnostic several lines deep.
        let text = "\
warning: public documentation for `Foo` links to private item `Bar`
  --> crates/a/src/lib.rs:3:5
   |
   = note: this link resolves only for private docs
error: unused imports: `Mutex` and `cell::RefCell`
  --> crates/daemon/src/worker_handle/binary.rs:10:5
   |
   = note: `-D unused-imports` implied by `-D warnings`
error[E0432]: unresolved import `crate::nope`
  --> crates/a/src/lib.rs:1:5
warning: unused variable: `x`
";
        let blocks = rustc_blocks(text);
        let levels: Vec<Level> = blocks.iter().map(|b| b.level).collect();
        assert_eq!(
            levels,
            vec![Level::Warning, Level::Error, Level::Error, Level::Warning]
        );
        // The continuation lines ride with their header, not as blocks of
        // their own - that grouping is what makes the error count meaningful.
        assert!(blocks[1].text.contains("binary.rs:10:5"));
        assert!(blocks[1].text.contains("-D unused-imports"));
        assert_eq!(blocks[3].text, "warning: unused variable: `x`");
    }

    #[test]
    fn indented_and_uncolonned_occurrences_are_not_headers() {
        // Source context quoting the word, a path containing it, and a
        // column-zero note: none of these may open a block, or the error
        // count stops meaning "errors".
        let text = "\
error: something broke
  --> src/error_handling.rs:1:1
   |
 1 | let error: u8 = 0;
   |
note: the lint level is defined here
errors are bad prose
";
        let blocks = rustc_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].level, Level::Error);
        assert!(blocks[0].text.contains("errors are bad prose"));
    }

    #[test]
    fn preamble_before_any_header_is_kept_as_other() {
        let text = "Documenting broadarrow v0.1.0\nerror: boom\n";
        let blocks = rustc_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].level, Level::Other);
        assert_eq!(blocks[1].level, Level::Error);
        // Empty input has no blocks at all, so a caller counting errors sees
        // zero rather than one empty `Other`.
        assert!(rustc_blocks("").is_empty());
    }

    #[test]
    fn exact_matches_trimmed_full_stream() {
        assert!(evaluate(
            "all good",
            MatchMode::Exact,
            Stream::Stdout,
            b"  all good\n",
            b"",
        ));
        // Extra output above the sentinel breaks exact.
        assert!(!evaluate(
            "all good",
            MatchMode::Exact,
            Stream::Stdout,
            b"working...\nall good\n",
            b"",
        ));
    }

    #[test]
    fn last_line_ignores_progress_above() {
        assert!(evaluate(
            "all good",
            MatchMode::LastLine,
            Stream::Stdout,
            b"checking a\nchecking b\nall good\n",
            b"",
        ));
        // Trailing blank lines are skipped to the real last line.
        assert!(evaluate(
            "all good",
            MatchMode::LastLine,
            Stream::Stdout,
            b"all good\n\n\n",
            b"",
        ));
        assert!(!evaluate(
            "all good",
            MatchMode::LastLine,
            Stream::Stdout,
            b"all good\nbut then a warning\n",
            b"",
        ));
    }

    #[test]
    fn contains_finds_substring_anywhere() {
        assert!(evaluate(
            "conventions are valid",
            MatchMode::Contains,
            Stream::Stdout,
            b"lots\nof\nnoise conventions are valid more noise\n",
            b"",
        ));
        assert!(!evaluate(
            "conventions are valid",
            MatchMode::Contains,
            Stream::Stdout,
            b"nothing relevant here\n",
            b"",
        ));
    }

    #[test]
    fn stream_selects_the_right_source() {
        // Sentinel only on stderr: stdout matching fails, stderr succeeds.
        assert!(!evaluate(
            "done",
            MatchMode::LastLine,
            Stream::Stdout,
            b"",
            b"done\n"
        ));
        assert!(evaluate(
            "done",
            MatchMode::LastLine,
            Stream::Stderr,
            b"",
            b"done\n"
        ));
        // `both` concatenates stdout then stderr, so a stderr verdict is the
        // combined last line.
        assert!(evaluate(
            "done",
            MatchMode::LastLine,
            Stream::Both,
            b"progress on stdout\n",
            b"done\n",
        ));
    }

    #[test]
    fn empty_output_never_matches_a_sentinel() {
        // A check stubbed to exit 0 with no output must not pass.
        assert!(!evaluate(
            "done",
            MatchMode::LastLine,
            Stream::Stdout,
            b"",
            b""
        ));
        assert!(!evaluate(
            "done",
            MatchMode::Exact,
            Stream::Stdout,
            b"",
            b""
        ));
        assert!(!evaluate(
            "done",
            MatchMode::Contains,
            Stream::Stdout,
            b"",
            b""
        ));
    }
}
