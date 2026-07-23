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
use crate::lex;

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
    // Strip a single leading UTF-8 BOM so line 1's text starts at the real
    // first token; `strip_prefix` returns a subslice of the same allocation, so
    // the pointer-offset bookkeeping below stays internally consistent. Line
    // numbers are unaffected - the BOM precedes line 1's text.
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let lines: Vec<&str> = content.lines().collect();
    // Lexical regions for the whole file, tokenized once, so a keyword that
    // lives inside a string literal or a comment is blanked before detection
    // and never read as a control-flow construct. Mirrors textlint's masking:
    // `offset` is each physical line's byte position within `content`, derived
    // by pointer arithmetic (which accounts for the `\n`/`\r\n` terminator that
    // `content.lines()` drops).
    let regions = lex::classify(content);
    let base = content.as_ptr() as usize;
    // Masked once for the whole file: detection reads the CODE-masked view
    // (string/comment bytes blanked) and so does the forward guard scan, so a
    // `=>` inside a literal cannot exempt an `if` that isn't a guard.
    let masked: Vec<String> = lines
        .iter()
        .map(|raw| {
            let offset = raw.as_ptr() as usize - base;
            lex::mask_line(raw, offset, &regions, lex::Region::Code)
        })
        .collect();

    for (i, _raw) in lines.iter().enumerate() {
        // Detect on the masked view; report the original text.
        let trimmed = masked[i].trim_start();
        let Some(kw) = control_flow_keyword(trimmed) else {
            continue;
        };
        // No line above: first line of the file is exempt (nothing to
        // separate from).
        if i == 0 {
            continue;
        }
        // A multi-line match guard: the `if` opens no block, it is the arm's
        // guard clause, terminated by `=>`. Checked before the line-above
        // exemptions because the construct simply isn't a statement.
        if kw == "if" && opens_match_guard(&masked, i) {
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
            content: cap(lines[i].trim_start()),
            prev: cap(prev.trim_start()),
        });
    }
}

/// Whether the `if` at `start` is the guard clause of a match arm rather than
/// a statement. Ports the bash hook's awk prepass: scan forward from the `if`
/// and exempt it if a line ending in `=>` arrives before any line containing
/// `{` or `;`.
///
/// rustfmt splits an arm whose guard is too long to sit beside the pattern:
///
/// ```text
/// BybitOrderStatus::Canceled
///     if filled_qty.is_zero()
///         && due_post_only(reason) =>
/// {
/// ```
///
/// The `if` opens no block and `=>` terminates it, so demanding a blank line
/// above it is nonsense - and the line above is a bare pattern, which shares
/// no identifier with the guard, so the identifier escape hatch never fires.
/// Both same-line orderings the awk encodes are preserved: `=>` is tested
/// before `{`/`;`, so a line carrying both still reads as a guard.
fn opens_match_guard(masked: &[String], start: usize) -> bool {
    for line in &masked[start..] {
        let end = line.trim_end();
        if end.ends_with("=>") {
            return true;
        }
        if line.contains('{') || line.contains(';') {
            return false;
        }
    }
    false
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
        || prev_end == "*"
        || prev_trim.starts_with("*/")
        || prev_trim.starts_with("/*")
        || prev_trim.starts_with("#[")
        || prev_trim.starts_with("#![")
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
                || prev_end.ends_with("=>")
                || is_match_guard(prev_trim)
        }
        // `match` as an expression / argument, or a match-arm body: a previous
        // line ending in `=>` is a single-pattern arm whose arrow rustfmt left
        // on its own line, so this construct is the arm body (`is_match_guard`
        // only recognises the OR-pattern shape, which shares a `|`).
        "match" => {
            ends_with_assign(prev_end)
                || ends_with_any(prev_end, &[',', '(', '|'])
                || prev_end.ends_with("=>")
        }
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
/// but not a `||` boolean and not a bitwise-or expression/statement.
///
/// A match or-pattern arm ends with `=>` (or is a bare pattern continuation
/// involving `|`); a bitwise-or is an assignment/expression - it contains a
/// lone `=` (not `=>`) and/or ends with `;`. `let mask = a | b;` must NOT be
/// treated as a match arm, or a following construct is wrongly exempted.
fn is_match_guard(prev_trim: &str) -> bool {
    if prev_trim.contains("||") {
        return false;
    }
    let prev_end = prev_trim.trim_end();
    if !prev_end.ends_with("=>") && (prev_end.ends_with(';') || has_plain_assign(prev_end)) {
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

/// Whether `s` contains a plain assignment `=` - excluding `=>`, `==`, `!=`,
/// `<=`, `>=`. Used to tell a bitwise-or expression from a match or-pattern.
fn has_plain_assign(s: &str) -> bool {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] != b'=' {
            continue;
        }
        let next = b.get(i + 1).copied();
        let prev = if i > 0 { Some(b[i - 1]) } else { None };
        if next == Some(b'>') || next == Some(b'=') {
            continue;
        }
        // `Some(b'.')` skips the `=` in an inclusive-range pattern `..=` so a
        // wrapped range or-pattern (`'0'..='9' | 'a'..='f'`) isn't read as an
        // assignment and wrongly un-exempted.
        if matches!(prev, Some(b'=') | Some(b'!') | Some(b'<') | Some(b'>') | Some(b'.')) {
            continue;
        }
        return true;
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

    #[test]
    fn keyword_inside_string_literal_is_not_flagged() {
        // A multi-line string continuation line begins (after indent) with a
        // control-flow keyword; the lexer marks it as Str, so it is masked out.
        let src = "fn f() {\n    let s = \"\n    for x\n    \";\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn keyword_inside_block_comment_body_is_not_flagged() {
        // Plain-prose block-comment body lines starting with `for`/`if` are
        // Comment bytes and must be blanked before detection.
        let src = "fn f() {\n    let x = 1;\n    /*\n    for a\n    if this happens then\n    */\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn spawn_inside_string_literal_is_not_flagged() {
        let src = "fn f() {\n    let q = w;\n    let s = \"a.spawn(b)\";\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn genuine_control_flow_still_flags_after_masking() {
        // No strings/comments in play: a bare `for`/`if` with a non-blank,
        // non-comment, unrelated line above must still be flagged.
        let src = "fn f() {\n    let a = compute();\n    for x in items {\n        g();\n    }\n}\n";
        assert_eq!(violations(src), vec![("for", 3)]);
        let src2 = "fn f() {\n    let a = compute();\n    if y {\n        g();\n    }\n}\n";
        assert_eq!(violations(src2), vec![("if", 3)]);
    }

    // ---- Fix #1: is_match_guard must not over-match bitwise-or. ----

    #[test]
    fn rustfmt_split_match_guard_is_exempt() {
        // The nautilus bybit repro (parse.rs:1412): rustfmt pushed a long guard
        // onto its own line under a bare pattern. No blank line, no shared
        // identifier - exempt only because `=>` terminates the guard.
        let src = "fn f() {\n    match s {\n        Bybit::Canceled\n            if filled_qty.is_zero()\n                && due_post_only(r) =>\n        {\n            Rejected\n        }\n    }\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn guard_scan_stops_at_a_block_or_statement() {
        // A plain `if` statement must stay flagged: its own line carries `{`,
        // so the forward scan rejects it before any later `=>` is reached.
        let src = "fn f() {\n    let x = 1;\n    if y {\n        match z {\n            A =>\n        }\n    }\n}\n";
        assert_eq!(violations(src), vec![("if", 3)]);
        // Same, with the `{` on a following line and a `;` intervening.
        let src2 = "fn f() {\n    let x = 1;\n    if y\n    {\n        let a = q =>;\n    }\n}\n";
        assert_eq!(violations(src2), vec![("if", 3)]);
    }

    #[test]
    fn fat_arrow_in_a_string_does_not_exempt_a_guard() {
        // The scan reads the masked view, so a `=>` inside a literal cannot
        // launder a genuine statement `if` into a match guard.
        let src = "fn f() {\n    let x = 1;\n    if y\n        .cmp(\"a =>\")\n    {\n    }\n}\n";
        assert_eq!(violations(src), vec![("if", 3)]);
    }

    #[test]
    fn bitwise_or_above_if_is_flagged() {
        // `let mask = a | b;` is a bitwise-or statement, NOT a match arm, so the
        // following `if` still needs a blank line (false-negative regression).
        let src = "fn f() {\n    let mask = a | b;\n    if c {\n        g();\n    }\n}\n";
        assert_eq!(violations(src), vec![("if", 3)]);
    }

    #[test]
    fn match_or_pattern_arm_above_if_is_exempt() {
        // A genuine match or-pattern arm ending in `=>` above an indented `if`
        // stays exempt (no regression).
        let src = "fn f() {\n    match z {\n        Foo | Bar =>\n        if y {}\n    }\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn inclusive_range_or_pattern_arm_is_exempt() {
        // Regression (d026e96): the `=` in `..=` must not be read as a plain
        // assignment, or a wrapped numeric inclusive-range or-pattern above an
        // if-guard line gets falsely flagged.
        let src = "fn f() {\n    match n {\n        0..=9 | 20..=29\n        if odd => {}\n    }\n}\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn single_pattern_arm_body_construct_is_exempt() {
        // A single-pattern match arm whose arrow rustfmt left on its own line,
        // with the arm body a nested `match`: the body is the first thing in the
        // arm, so demanding a blank line above it is nonsense. The `=>` at the
        // end of the pattern line marks it - and a bare pattern shares no
        // identifier with the body, so the fallback never fired before the fix.
        let nested_match =
            "fn f() {\n    match v {\n        A =>\n            match w {\n                B => g(),\n            }\n    }\n}\n";
        assert!(violations(nested_match).is_empty());
        // Identical shape with an `if` arm body.
        let nested_if =
            "fn f() {\n    match v {\n        A =>\n            if cond {\n                g()\n            }\n    }\n}\n";
        assert!(violations(nested_if).is_empty());
    }

    #[test]
    fn fat_arrow_end_does_not_exempt_a_genuine_missing_blank_line() {
        // Guard against over-exemption: a plain statement `if` whose previous
        // line is ordinary code (no trailing `=>`) must still be flagged.
        let src = "fn f() {\n    let x = compute();\n    if y {\n        g();\n    }\n}\n";
        assert_eq!(violations(src), vec![("if", 3)]);
    }

    // ---- Fix #2: bare `*` comment line and `#![...]` inner attribute. ----

    #[test]
    fn bare_star_block_comment_line_above_is_ok() {
        // A block-comment continuation line that is a bare `*` (no trailing
        // space) exempts the construct below it.
        let src = "fn f() {\n    let x = 1;\n    /*\n     *\n     */\n    if y {}\n}\n";
        assert!(violations(src).is_empty());
        // Directly below a bare `*` line specifically.
        let src2 = "fn f() {\n    let x = 1;\n    *\n    if y {}\n}\n";
        assert!(violations(src2).is_empty());
    }

    #[test]
    fn inner_attribute_above_is_ok() {
        // `#![...]` inner attribute exempts the construct below, like `#[...]`.
        let src = "fn f() {\n    let x = 1;\n    #![allow(dead_code)]\n    if y {}\n}\n";
        assert!(violations(src).is_empty());
    }

    // ---- Fix #3: leading UTF-8 BOM must not mis-anchor line 1. ----

    #[test]
    fn leading_bom_does_not_hide_first_line_construct() {
        // With a BOM ahead of `if x {`, line 1 must still be classified as a
        // control-flow construct (first line of file -> exempt, but detected,
        // not silently skipped as non-matching text).
        let src = "\u{FEFF}if x {\n    y\n}\n";
        // First line is exempt (nothing above), so no violation - the point is
        // that detection is not derailed by the BOM.
        assert!(violations(src).is_empty());
        // And a BOM'd file whose first-line construct has a non-blank line
        // above it (a second construct) is still scanned correctly.
        let src2 = "\u{FEFF}let a = compute();\nif y {\n    g();\n}\n";
        assert_eq!(violations(src2), vec![("if", 2)]);
    }
}
