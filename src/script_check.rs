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
//! The child is given `BROKKR_CARGO=1`: a script-check may run cargo, and it
//! runs under a lock brokkr already holds - see [`run_one`] for why the
//! rustc guard's ancestor path cannot carry that here.
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
pub fn run_one(check: &ScriptCheck, cwd: &Path) -> Result<Outcome, DevError> {
    let captured = output::run_captured_with_env("sh", &["-c", &check.command], cwd, &[])?;
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

/// The severity of a rustc-shaped diagnostic block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// An `error:` / `error[CODE]:` block - the actionable class.
    Error,
    /// A `warning:` / `warning[CODE]:` block.
    Warning,
    /// Everything above the first header: cargo's progress lines, a banner, a
    /// tool's own preamble. Kept as a block so no captured byte is silently
    /// dropped from the counts.
    Other,
}

/// One rustc-shaped diagnostic: a header line at column zero plus every line
/// under it up to the next header. `text` retains the original line breaks and
/// carries no trailing newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub level: Level,
    pub text: String,
}

/// Split rustc-shaped output into diagnostic blocks.
///
/// Only `error` and `warning` at **column zero** open a block. Two consequences,
/// both deliberate:
///
/// - A column-zero `note:` or `help:` does not open one, so rustc's trailing
///   notes stay attached to the diagnostic they explain rather than becoming
///   orphan blocks that outnumber the errors.
/// - An indented occurrence of the word never opens one, so source context
///   quoting `error` (or a `--> path/error.rs` arrow) cannot forge a block.
///
/// This is a *renderer's* parse, reached only on a failure of an entry that
/// declared `diagnostics = "rustc"`. It never feeds the pass/fail decision -
/// that stays [`evaluate`]'s sentinel match - so a misparse costs display
/// quality and nothing else.
pub fn rustc_blocks(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for line in text.lines() {
        match header_level(line) {
            Some(level) => blocks.push(Block {
                level,
                text: line.to_owned(),
            }),
            None => match blocks.last_mut() {
                Some(block) => {
                    block.text.push('\n');
                    block.text.push_str(line);
                }
                // Preamble before any header.
                None => blocks.push(Block {
                    level: Level::Other,
                    text: line.to_owned(),
                }),
            },
        }
    }
    blocks
}

/// The level `line` opens a block at, or `None` when it is a continuation.
///
/// Accepts both rustc header spellings - bare (`error: unused imports`) and
/// coded (`error[E0432]: unresolved import`) - and requires the colon, so a
/// prose line beginning with the word is not mistaken for a header.
fn header_level(line: &str) -> Option<Level> {
    // Column zero is the whole discriminator: rustc indents every continuation.
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    for (word, level) in [("error", Level::Error), ("warning", Level::Warning)] {
        let Some(rest) = line.strip_prefix(word) else {
            continue;
        };
        if rest.starts_with(':') {
            return Some(level);
        }
        // `error[E0432]:` - the code is bracketed, then the colon.
        if let Some(after) = rest.strip_prefix('[')
            && let Some(close) = after.find(']')
            && after[close + 1..].starts_with(':')
        {
            return Some(level);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{evaluate, rustc_blocks, Level};
    use crate::config::{MatchMode, Stream};

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
