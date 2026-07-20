//! The `[[script_check]]` phase: run a command and assert its output.
//!
//! Some pre-commit gates can't be expressed as brokkr's native phases
//! (textlint/manifest/style/header) - their logic is semantic or
//! formatter-specific (e.g. a `# Panics`/`# Errors` doc analyser). A
//! `[[script_check]]` runs an arbitrary `command` via `sh -c` and passes iff
//! its captured output matches a configured `expect` sentinel per `match` and
//! `stream`. Asserting on a success sentinel - not the exit code - is the
//! point: it catches a check silently stubbed to exit 0, because the script
//! must prove it ran to completion by emitting the sentinel. The exit code is
//! therefore ignored; only a spawn failure is a hard error.
//!
//! This module is the logic (`evaluate` + `run_one`); orchestration and
//! failure formatting live in `check_cmd::phase::run_script_checks`, mirroring
//! how `textlint`/`manifest` split scan-logic from phase-plumbing.

use std::path::Path;

use crate::config::{MatchMode, ScriptCheck, Stream};
use crate::error::DevError;
use crate::output;

/// The captured result of running one script-check.
pub struct Outcome {
    /// Whether the output matched the `expect` sentinel.
    pub passed: bool,
    /// The command's captured stdout (shown verbatim on failure).
    pub stdout: Vec<u8>,
    /// The command's captured stderr (shown verbatim on failure).
    pub stderr: Vec<u8>,
}

/// Run one `[[script_check]]` and evaluate its output.
///
/// The command is run as `sh -c "<command>"` with `cwd` as the working
/// directory (the code tree), so pipes, redirects, and env expansion work.
/// Returns `Err` only when the process could not be spawned.
pub fn run_one(check: &ScriptCheck, cwd: &Path) -> Result<Outcome, DevError> {
    let captured = output::run_captured("sh", &["-c", &check.command], cwd)?;
    let passed = evaluate(
        &check.expect,
        check.match_mode,
        check.stream,
        &captured.stdout,
        &captured.stderr,
    );
    Ok(Outcome {
        passed,
        stdout: captured.stdout,
        stderr: captured.stderr,
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
    use super::evaluate;
    use crate::config::{MatchMode, Stream};

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
        assert!(!evaluate("done", MatchMode::LastLine, Stream::Stdout, b"", b"done\n"));
        assert!(evaluate("done", MatchMode::LastLine, Stream::Stderr, b"", b"done\n"));
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
        assert!(!evaluate("done", MatchMode::LastLine, Stream::Stdout, b"", b""));
        assert!(!evaluate("done", MatchMode::Exact, Stream::Stdout, b"", b""));
        assert!(!evaluate("done", MatchMode::Contains, Stream::Stdout, b"", b""));
    }
}
