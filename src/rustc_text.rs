//! Shared parsing and classification for rustc-shaped text.
//!
//! The text splitter is used for script-check failure excerpts, manifest-lint
//! discovery, and failed cargo reports. Keeping header recognition and
//! summary classification here gives those readers one definition of a block.

/// The severity of a rustc-shaped diagnostic block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    /// An `error:` / `error[CODE]:` block - the actionable class.
    Error,
    /// A `warning:` / `warning[CODE]:` block.
    Warning,
    /// Everything above the first header: cargo progress, a banner, or a
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

/// What a rustc-shaped block represents to cargo-output readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// An actionable rustc diagnostic.
    Finding,
    /// A compiler/cargo summary of a failure already reported elsewhere.
    FailureSummary,
    /// Compiler/cargo warning tally or cargo's failed-build status.
    CargoStatus,
    /// Cargo's compile/download/lock progress chatter.
    Progress,
    /// Text that does not fit another known block kind.
    Other,
}

/// Split rustc-shaped output into diagnostic blocks.
///
/// Only `error` and `warning` at **column zero** open a block. A column-zero
/// `note:` or `help:` stays attached to the diagnostic it explains, and an
/// indented occurrence cannot forge a header from source context.
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
                None => blocks.push(Block {
                    level: Level::Other,
                    text: line.to_owned(),
                }),
            },
        }
    }
    blocks
}

/// Classify one rustc-shaped block using its header and level.
pub fn classify_block(block: &Block) -> BlockKind {
    let header = block.text.lines().next().unwrap_or("");
    if is_failure_summary(block.level, header) {
        return BlockKind::FailureSummary;
    }
    if is_cargo_status(block.level, header) {
        return BlockKind::CargoStatus;
    }
    if block.level == Level::Other && is_progress(header) {
        return BlockKind::Progress;
    }
    match block.level {
        Level::Error | Level::Warning => BlockKind::Finding,
        Level::Other => BlockKind::Other,
    }
}

/// Classify one ungrouped line through the same block rules.
pub fn classify_line(line: &str) -> BlockKind {
    let level = header_level(line).unwrap_or(Level::Other);
    classify_block(&Block {
        level,
        text: line.to_owned(),
    })
}

/// Classify a cargo JSON diagnostic message after its textual level has been
/// parsed once.
///
/// Cargo omits the rendered `error:` / `warning:` prefix from the JSON message,
/// so this restores it before asking the shared block classifier.
pub fn classify_diagnostic(level: Level, message: &str) -> BlockKind {
    let prefix = match level {
        Level::Warning => "warning",
        Level::Error => "error",
        Level::Other => "",
    };
    let text = if prefix.is_empty() {
        message.to_owned()
    } else {
        format!("{prefix}: {message}")
    };
    classify_block(&Block { level, text })
}

/// The level `line` opens a block at, or `None` when it is a continuation.
///
/// Accepts bare (`error: unused imports`) and coded
/// (`error[E0432]: unresolved import`) headers, requiring the closing bracket
/// and colon for coded headers.
fn header_level(line: &str) -> Option<Level> {
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
        if let Some(after) = rest.strip_prefix('[')
            && let Some(close) = after.find(']')
            && after[close + 1..].starts_with(':')
        {
            return Some(level);
        }
    }
    None
}

fn is_failure_summary(level: Level, header: &str) -> bool {
    if level != Level::Error {
        return false;
    }
    let Some(message) = header_message(header, "error") else {
        return false;
    };
    message == "build failed"
        || message.starts_with("aborting due to")
        || message.starts_with("could not compile ")
}

fn is_cargo_status(level: Level, header: &str) -> bool {
    if level != Level::Warning {
        return false;
    }
    let Some(message) = header_message(header, "warning") else {
        return false;
    };
    if message.starts_with("build failed, waiting for other jobs") {
        return true;
    }
    if has_generated_warning_tally(message) {
        return true;
    }
    has_emitted_warning_tally(message)
}

fn header_message<'a>(header: &'a str, level: &str) -> Option<&'a str> {
    let rest = header.strip_prefix(level)?;
    if let Some(message) = rest.strip_prefix(':') {
        return Some(message.trim_start());
    }
    let code = rest.strip_prefix('[')?;
    let close = code.find(']')?;
    code[close + 1..].strip_prefix(':').map(str::trim_start)
}

fn has_generated_warning_tally(message: &str) -> bool {
    message.match_indices("generated ").any(|(index, _)| {
        let starts_word = index == 0
            || message[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        starts_word && has_warning_count(&message[index + "generated ".len()..])
    })
}

fn has_emitted_warning_tally(message: &str) -> bool {
    let digit_count = leading_ascii_digits(message);
    if digit_count == 0 {
        return false;
    }
    let rest = &message[digit_count..];
    warning_count_tail(rest).is_some_and(|tail| {
        tail.strip_prefix(" emitted").is_some_and(|end| {
            end.is_empty() || end.chars().next().is_some_and(|c| !c.is_ascii_alphabetic())
        })
    })
}

fn has_warning_count(text: &str) -> bool {
    let digit_count = leading_ascii_digits(text);
    if digit_count == 0 {
        return false;
    }
    warning_count_tail(&text[digit_count..]).is_some()
}

fn warning_count_tail(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix(" warnings")
        .or_else(|| text.strip_prefix(" warning"))?;
    (rest.is_empty()
        || rest
            .chars()
            .next()
            .is_some_and(|c| !c.is_ascii_alphabetic()))
    .then_some(rest)
}

fn leading_ascii_digits(text: &str) -> usize {
    text.bytes().take_while(u8::is_ascii_digit).count()
}

fn is_progress(header: &str) -> bool {
    let text = header.trim_start();
    [
        "Compiling",
        "Checking",
        "Downloading",
        "Downloaded",
        "Finished",
        "Locking",
        "Updating",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}
