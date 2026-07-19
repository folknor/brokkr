//! `[style]` native check: blank line above control-flow constructs.
//!
//! A native, opt-in Rust style rule ported from nautilus_trader's
//! `.pre-commit-hooks/check_formatting_rs.sh`. It is the one convention in
//! that battery that is *not* expressible as a regex rule: whether an `if` (or
//! `match`/`for`/`while`/`loop`/`spawn`) needs a blank line above it depends on
//! classifying the line directly above it - does it open a block, is it a
//! comment or attribute attached to the construct, does it share an identifier
//! with the condition, etc. That classification is this module.
//!
//! Unlike the bash original - which drives the check off `rg -B1` context lines
//! and can get confused when matches abut - this walks each file's physical
//! lines, so "the line above" is always the true previous line. We track the
//! *intent* of the upstream hook (its documented exception list, cribbed here
//! as the test corpus), not its line-by-line behaviour.
//!
//! Enabled by `[style] rust_blank_line_above_control_flow = true`; off by
//! default, so it is inert for every project that does not opt in.

use std::path::{Path, PathBuf};

use crate::config::GremlinsConfig;
use crate::error::DevError;
use crate::gremlins;

/// One missing-blank-line violation: a control-flow line that needs a blank
/// line above it and does not have one, and does not qualify for any exemption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleViolation {
    pub file: PathBuf,
    /// 1-based line of the control-flow construct.
    pub line: usize,
    /// The construct: `if`, `match`, `for`, `while`, `loop`, or `spawn`.
    pub keyword: &'static str,
    /// The offending line, trimmed and length-capped for display.
    pub content: String,
    /// The line directly above it, trimmed and length-capped for display.
    pub prev: String,
}

/// `file:line: missing blank line above `keyword`` plus the two context lines.
pub fn format_one(v: &StyleViolation) -> String {
    format!(
        "{}:{}: missing blank line above `{}`\n      {}\n      above: {}",
        v.file.display(),
        v.line,
        v.keyword,
        v.content,
        v.prev,
    )
}

/// Scan every tracked `.rs` file (minus `[gremlins].exclude` dirs) for
/// control-flow constructs missing a blank line above.
pub fn scan(
    project_root: &Path,
    gremlins_cfg: Option<&GremlinsConfig>,
) -> Result<Vec<StyleViolation>, DevError> {
    let files = gremlins::tracked_files(project_root)?;
    let mut out = Vec::new();
    for rel in &files {
        if rel.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if gremlins_cfg.is_some_and(|c| c.is_excluded(rel)) {
            continue;
        }
        let abs = project_root.join(rel);
        let Ok(content) = std::fs::read_to_string(&abs) else {
            continue;
        };
        scan_content(rel, &content, &mut out);
    }
    Ok(out)
}

/// Max characters of a context line echoed in a violation (matches the bash
/// hook's `:0:100` truncation).
const DISPLAY_CAP: usize = 100;

fn cap(s: &str) -> String {
    s.chars().take(DISPLAY_CAP).collect()
}

fn scan_content(rel: &Path, content: &str, out: &mut Vec<StyleViolation>) {
    let lines: Vec<&str> = content.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        let Some(kw) = control_flow_keyword(trimmed) else {
            continue;
        };
        // No line above: first line of the file is exempt (nothing to
        // separate from).
        if i == 0 {
            continue;
        }
        let prev = lines[i - 1];
        if is_exempt(kw, trimmed, prev, lines.get(i + 1).copied()) {
            continue;
        }
        out.push(StyleViolation {
            file: rel.to_path_buf(),
            line: i + 1,
            keyword: kw,
            content: cap(trimmed),
            prev: cap(prev.trim_start()),
        });
    }
}

/// Which control-flow construct, if any, this (already leading-trimmed) line
/// opens. Mirrors the six `rg` patterns in the bash hook.
fn control_flow_keyword(trimmed: &str) -> Option<&'static str> {
    if starts_kw(trimmed, "if") {
        return Some("if");
    }
    if starts_kw(trimmed, "match") {
        return Some("match");
    }
    // Loop keywords may carry a leading `'label:` prefix.
    let unlabelled = strip_label(trimmed);
    if starts_kw(unlabelled, "for") {
        return Some("for");
    }
    if starts_kw(unlabelled, "while") {
        return Some("while");
    }
    if starts_kw(unlabelled, "loop") {
        return Some("loop");
    }
    if is_spawn(trimmed) {
        return Some("spawn");
    }
    None
}

/// `^kw\s`: the line starts with the keyword followed by whitespace (so `iffy`,
/// `matches!`, `format!` do not match).
fn starts_kw(s: &str, kw: &str) -> bool {
    s.strip_prefix(kw)
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_whitespace()))
}

/// A `spawn` call: `^spawn(`, `.spawn(`, or `::spawn(` anywhere on the line.
fn is_spawn(trimmed: &str) -> bool {
    trimmed.starts_with("spawn(") || trimmed.contains(".spawn(") || trimmed.contains("::spawn(")
}

/// Strip a leading loop label (`'name: `) if present, else return unchanged.
fn strip_label(s: &str) -> &str {
    let Some(rest) = s.strip_prefix('\'') else {
        return s;
    };
    if !rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return s;
    }
    match rest.split_once(':') {
        Some((_label, after)) => after.trim_start(),
        None => s,
    }
}

/// Whether the construct on `trimmed` is exempt from needing a blank line above
/// it, given the previous physical line `prev` and the first body line `body`.
fn is_exempt(kw: &str, trimmed: &str, prev: &str, body: Option<&str>) -> bool {
    let prev_trim = prev.trim_start();
    let prev_end = prev_trim.trim_end();

    // (a) blank line above - the state we want.
    if prev_trim.is_empty() {
        return true;
    }
    // (b) first expression in a block: previous line opens one.
    if prev_end == "{" || prev_end.ends_with('{') {
        return true;
    }
    // (c) comment or attribute attached to the construct.
    if prev_trim.starts_with("//")
        || prev_trim.starts_with("* ")
        || prev_trim.starts_with("*/")
        || prev_trim.starts_with("/*")
        || prev_trim.starts_with("#[")
    {
        return true;
    }
    // (d) string continuation: previous line ends with a backslash.
    if prev_end.ends_with('\\') {
        return true;
    }

    // Per-keyword structural exemptions.
    if keyword_structural_exempt(kw, trimmed, prev_trim, prev_end) {
        return true;
    }

    // Shared identifier: an identifier from the construct (or its first body
    // line) appears on the line above.
    let target = target_text(kw, trimmed);
    let spawn = kw == "spawn";
    if shares_identifier(target, prev, spawn) {
        return true;
    }
    if let Some(body) = body
        && shares_identifier(body, prev, spawn)
    {
        return true;
    }
    false
}

/// The structural, per-keyword exemptions (else-if chains, expression position,
/// loop labels, `.spawn` method chains) - everything before the shared-
/// identifier fallback. Split out so `is_exempt` reads as a flat ladder.
fn keyword_structural_exempt(kw: &str, trimmed: &str, prev_trim: &str, prev_end: &str) -> bool {
    match kw {
        // else-if chain, or `if` as an expression (assignment RHS) / argument /
        // continuation / match-arm guard.
        "if" => {
            prev_end == "else"
                || (prev_end.ends_with("else") && prev_trim.contains('}'))
                || ends_with_assign(prev_end)
                || ends_with_any(prev_end, &[',', '(', ')', '|'])
                || prev_trim.starts_with('|')
                || is_match_guard(prev_trim)
        }
        // `match` as an expression / argument.
        "match" => ends_with_assign(prev_end) || ends_with_any(prev_end, &[',', '(', '|']),
        // Loop label on the previous line.
        "for" | "while" | "loop" => {
            prev_trim.starts_with('\'')
                && prev_trim[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        }
        // `.spawn(...)` method-chain continuation.
        "spawn" => trimmed.starts_with('.'),
        _ => false,
    }
}

/// Previous line ends with `=` (assignment), but not `==`/`!=`/`<=`/`>=`.
fn ends_with_assign(prev_end: &str) -> bool {
    prev_end.ends_with('=')
        && !prev_end.ends_with("==")
        && !prev_end.ends_with("!=")
        && !prev_end.ends_with("<=")
        && !prev_end.ends_with(">=")
}

fn ends_with_any(s: &str, chars: &[char]) -> bool {
    s.chars().next_back().is_some_and(|c| chars.contains(&c))
}

/// A multi-alternative match pattern: `alnum | alnum` (with optional spaces),
/// but not a `||` boolean.
fn is_match_guard(prev_trim: &str) -> bool {
    if prev_trim.contains("||") {
        return false;
    }
    let chars: Vec<char> = prev_trim.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != '|' {
            continue;
        }
        let before = chars[..i].iter().rev().find(|c| !c.is_whitespace());
        let after = chars[i + 1..].iter().find(|c| !c.is_whitespace());
        if let (Some(b), Some(a)) = (before, after)
            && b.is_alphanumeric()
            && a.is_alphanumeric()
        {
            return true;
        }
    }
    false
}

/// The substring whose identifiers are checked against the line above: the
/// condition/expression for keyword constructs, the whole line for `spawn`.
fn target_text<'a>(kw: &str, trimmed: &'a str) -> &'a str {
    match kw {
        "if" => &trimmed[2..],
        "match" => &trimmed[5..],
        "for" | "while" | "loop" => {
            let s = strip_label(trimmed);
            let len = kw.len();
            &s[len..]
        }
        _ => trimmed, // spawn
    }
}

/// Whether any non-keyword identifier in `target` appears as a whole word on
/// `prev`. `spawn` additionally ignores `spawn`/`tokio` as noise words.
fn shares_identifier(target: &str, prev: &str, spawn: bool) -> bool {
    for ident in identifiers(target) {
        if is_noise_ident(ident, spawn) {
            continue;
        }
        if contains_word(prev, ident) {
            return true;
        }
    }
    false
}

/// Rust keywords (and, for `spawn`, `spawn`/`tokio`) that don't count as a
/// shared identifier - matching the bash hook's `case` denylist.
fn is_noise_ident(ident: &str, spawn: bool) -> bool {
    const KEYWORDS: &[&str] = &[
        "if", "else", "let", "mut", "ref", "true", "false", "return", "break", "continue", "match",
        "as", "in", "for", "while", "loop", "fn", "struct", "enum", "impl", "trait", "pub", "use",
        "mod", "const", "static", "type", "where", "async", "await", "move", "unsafe", "extern",
        "crate", "super", "dyn", "self", "Self",
    ];
    if KEYWORDS.contains(&ident) {
        return true;
    }
    spawn && (ident == "spawn" || ident == "tokio")
}

/// Maximal `[A-Za-z_][A-Za-z0-9_]*` runs.
fn identifiers(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'_' || b.is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            out.push(&s[start..i]);
        } else {
            i += 1;
        }
    }
    out
}

/// Whole-word (`[A-Za-z0-9_]`-bounded) occurrence of `word` in `haystack`.
fn contains_word(haystack: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let hb = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(word) {
        let idx = start + pos;
        let before_ok = idx == 0 || !is_word_byte(hb[idx - 1]);
        let after = idx + word.len();
        let after_ok = after >= hb.len() || !is_word_byte(hb[after]);
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    /// Run the scanner over a one-file snippet; return the keyword+line of each
    /// violation.
    fn violations(src: &str) -> Vec<(&'static str, usize)> {
        let mut out = Vec::new();
        scan_content(Path::new("t.rs"), src, &mut out);
        out.iter().map(|v| (v.keyword, v.line)).collect()
    }

    #[test]
    fn flags_if_without_blank_line() {
        let src = "fn f() {\n    let x = 1;\n    if y {\n        g();\n    }\n}\n";
        assert_eq!(violations(src), vec![("if", 3)]);
    }

    #[test]
    fn blank_line_above_is_ok() {
        let src = "fn f() {\n    let x = 1;\n\n    if y {\n    }\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn first_expression_in_block_is_ok() {
        // Prev line opens the block.
        let src = "fn f() {\n    if y {\n    }\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn comment_or_attribute_above_is_ok() {
        let src = "fn f() {\n    let x = 1;\n    // comment\n    if y {}\n}\n";
        assert!(violations(src).is_empty());
        let attr = "fn f() {\n    let x = 1;\n    #[cfg(test)]\n    match y {}\n}\n";
        assert!(violations(attr).is_empty());
    }

    #[test]
    fn shared_identifier_above_is_ok() {
        // `x` from the condition appears on the line above.
        let src = "fn f() {\n    let x = compute();\n    if x > 0 {\n    }\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn shared_identifier_in_first_body_line_is_ok() {
        // Nothing shared in the condition, but the body's first line references
        // `v` from the line above.
        let src = "fn f() {\n    let v = make();\n    for item in list {\n        v.push(item);\n    }\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn unrelated_identifier_still_flags() {
        let src = "fn f() {\n    let x = compute();\n    if y > 0 {\n        z();\n    }\n}\n";
        assert_eq!(violations(src), vec![("if", 3)]);
    }

    #[test]
    fn else_if_chain_is_ok() {
        let src = "fn f() {\n    let z = 1;\n    if a {\n    } else\n    if b {\n    }\n}\n";
        // Line 3 `if a` shares nothing with `let z` -> flagged; line 5 `if b`
        // follows `} else` -> exempt.
        assert_eq!(violations(src), vec![("if", 3)]);
    }

    #[test]
    fn if_as_assignment_rhs_is_ok() {
        let src = "fn f() {\n    let q = w;\n    let r =\n    if cond { 1 } else { 2 };\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn if_as_argument_is_ok() {
        let src = "fn f() {\n    let q = w;\n    call(\n    if cond { 1 } else { 2 });\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn assignment_not_confused_with_comparison() {
        // Prev ends with `==`, which is NOT an assignment exemption.
        let src = "fn f() {\n    let flag = a ==\n    if b {}\n}\n";
        assert_eq!(violations(src), vec![("if", 3)]);
    }

    #[test]
    fn match_guard_pattern_above_is_ok() {
        let src = "fn f() {\n    Foo | Bar\n    if guard {}\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn loop_label_above_is_ok() {
        let src = "fn f() {\n    let z = 1;\n    'outer:\n    for i in v {\n    }\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn labelled_loop_is_detected_and_checked() {
        // `'outer: for` shares nothing with `let z` -> flagged.
        let src = "fn f() {\n    let z = 1;\n    'outer: for i in items {\n        g();\n    }\n}\n";
        assert_eq!(violations(src), vec![("for", 3)]);
    }

    #[test]
    fn spawn_method_chain_continuation_is_ok() {
        let src = "fn f() {\n    let q = builder\n        .spawn(task);\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn spawn_without_blank_line_flags() {
        let src = "fn f() {\n    let q = w;\n    tokio::spawn(async move { go() });\n}\n";
        assert_eq!(violations(src), vec![("spawn", 3)]);
    }

    #[test]
    fn spawn_ignores_spawn_and_tokio_as_shared_words() {
        // Prev line mentions `tokio` and `spawn`, which are noise words, so
        // they must NOT count as a shared identifier.
        let src = "fn f() {\n    use tokio::task::spawn;\n    spawn(go());\n}\n";
        assert_eq!(violations(src), vec![("spawn", 3)]);
    }

    #[test]
    fn keyword_lookalikes_do_not_match() {
        // `iffy`, `matches!`, `format!` are not control-flow constructs.
        let src = "fn f() {\n    let a = 1;\n    let iffy = 2;\n    let m = matches!(a, 1);\n    let s = format!(\"{a}\");\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn all_keywords_detected() {
        assert_eq!(control_flow_keyword("if x {"), Some("if"));
        assert_eq!(control_flow_keyword("match x {"), Some("match"));
        assert_eq!(control_flow_keyword("for x in y {"), Some("for"));
        assert_eq!(control_flow_keyword("while x {"), Some("while"));
        assert_eq!(control_flow_keyword("loop {"), Some("loop"));
        assert_eq!(control_flow_keyword("'a: loop {"), Some("loop"));
        assert_eq!(control_flow_keyword("handle.spawn(x)"), Some("spawn"));
        assert_eq!(control_flow_keyword("let x = 1;"), None);
    }

    #[test]
    fn first_line_of_file_is_exempt() {
        let src = "if cfg!(test) {}\n";
        assert!(violations(src).is_empty());
    }
}
