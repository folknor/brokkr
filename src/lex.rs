//! Rust source tokenization for `[[textlint]]` region scoping (and, later,
//! logical-line joins). Backed by `rustc_lexer` - the rust-lang tokenizer -
//! so the string/char/comment boundaries are the *real* ones: raw strings
//! (`r#"..."#`), byte strings, the lifetime-vs-char trap (`'a` vs `'x'`), and
//! nested block comments are all classified correctly, which a hand-rolled
//! scanner reliably gets wrong.
//!
//! The engine consumes this as a masking layer: [`classify`] labels every byte
//! of a file, and [`mask_line`] blanks out the bytes of a line that fall
//! outside a target region, so a textlint `pattern` can be scoped to match only
//! in code, only in string literals, or only in comments.

use rustc_lexer::{tokenize, LiteralKind, TokenKind};

/// Which lexical region a byte belongs to. Numeric literals, identifiers,
/// punctuation and whitespace are all [`Region::Code`]; only *textual* literals
/// (string/char/byte, raw or not) count as [`Region::Str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Code,
    Str,
    Comment,
}

impl Region {
    /// Parse a `region = "..."` config value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "code" => Some(Self::Code),
            "string" => Some(Self::Str),
            "comment" => Some(Self::Comment),
            _ => None,
        }
    }
}

/// Label every byte of `src` with its lexical region. The returned vector has
/// exactly `src.len()` entries (rustc_lexer's token lengths tile the input).
pub fn classify(src: &str) -> Vec<Region> {
    let mut regions = Vec::with_capacity(src.len());
    for token in tokenize(src) {
        let region = match token.kind {
            TokenKind::LineComment | TokenKind::BlockComment { .. } => Region::Comment,
            TokenKind::Literal { kind, .. } => match kind {
                LiteralKind::Str { .. }
                | LiteralKind::ByteStr { .. }
                | LiteralKind::RawStr { .. }
                | LiteralKind::RawByteStr { .. }
                | LiteralKind::Char { .. }
                | LiteralKind::Byte { .. } => Region::Str,
                LiteralKind::Int { .. } | LiteralKind::Float { .. } => Region::Code,
            },
            _ => Region::Code,
        };
        for _ in 0..token.len {
            regions.push(region);
        }
    }
    regions
}

/// A copy of `line` with every char outside `target` replaced by a single
/// space (kept chars are verbatim, so a pattern still matches real text).
/// `offset` is the byte position of `line` within the file `regions` came from.
/// Masked-out multi-byte chars collapse to one space - columns drift only
/// inside the ignored region, which never carries a reported match.
pub fn mask_line(line: &str, offset: usize, regions: &[Region], target: Region) -> String {
    let mut out = String::with_capacity(line.len());
    for (j, ch) in line.char_indices() {
        if regions.get(offset + j).copied() == Some(target) {
            out.push(ch);
        } else {
            out.push(' ');
        }
    }
    out
}

/// One `use ...;` statement, its physical-line span, and a single-line
/// reconstruction with comments stripped and whitespace collapsed - so a
/// pattern can match a rustfmt-wrapped import that no single physical line
/// carries in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseStmt {
    /// 0-based first physical line (the `use` keyword).
    pub start_line: usize,
    /// 0-based last physical line (the terminating `;`).
    pub end_line: usize,
    /// The statement from `use` to `;`, comments removed, whitespace runs
    /// (including the line wraps) collapsed to single spaces.
    pub joined: String,
}

/// Reconstruct every `use ...;` statement in `src`. `use` is reserved, so an
/// `Ident` token reading `use` is always the keyword (never in a string or
/// comment - those tokenize as other kinds), which makes this robust without a
/// parser. Bracket depth is tracked so the terminating `;` is the statement's
/// own, not one inside its `{...}` group.
pub fn use_statements(src: &str) -> Vec<UseStmt> {
    let regions = classify(src);
    let mut line_starts = vec![0usize];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let line_of = |off: usize| line_starts.partition_point(|&s| s <= off).saturating_sub(1);

    // (start, len, kind) for every token, in order.
    let mut toks: Vec<(usize, usize, TokenKind)> = Vec::new();
    let mut off = 0usize;
    for t in tokenize(src) {
        toks.push((off, t.len, t.kind));
        off += t.len;
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let (start, len, kind) = toks[i];
        if kind == TokenKind::Ident && &src[start..start + len] == "use" {
            // Scan to the depth-0 `;`.
            let mut depth: i32 = 0;
            let mut j = i + 1;
            let mut end: Option<usize> = None;
            while j < toks.len() {
                let (ts, tl, tk) = toks[j];
                match tk {
                    TokenKind::OpenBrace | TokenKind::OpenParen | TokenKind::OpenBracket => {
                        depth += 1;
                    }
                    TokenKind::CloseBrace | TokenKind::CloseParen | TokenKind::CloseBracket => {
                        depth -= 1;
                    }
                    TokenKind::Semi if depth == 0 => {
                        end = Some(ts + tl);
                        break;
                    }
                    _ => {}
                }
                j += 1;
            }
            if let Some(end) = end {
                out.push(UseStmt {
                    start_line: line_of(start),
                    end_line: line_of(end - 1),
                    joined: join_range(src, &regions, start, end),
                });
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Flatten `src[start..end]` to one line: comment bytes and whitespace runs
/// become a single space, code is verbatim (so `foo::bar` stays adjacent).
fn join_range(src: &str, regions: &[Region], start: usize, end: usize) -> String {
    let mut joined = String::new();
    let mut prev_space = false;
    for (k, ch) in src[start..end].char_indices() {
        let drop = ch.is_whitespace() || regions.get(start + k).copied() == Some(Region::Comment);
        if drop {
            if !prev_space {
                joined.push(' ');
                prev_space = true;
            }
        } else {
            joined.push(ch);
            prev_space = false;
        }
    }
    joined.trim().to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn regions_of(src: &str) -> Vec<Region> {
        classify(src)
    }

    #[test]
    fn classify_tiles_the_whole_input() {
        let src = "let x = \"hi\"; // c\n";
        assert_eq!(classify(src).len(), src.len());
    }

    #[test]
    fn string_and_comment_regions_are_labelled() {
        let src = r#"let m = "a, got b"; // note"#;
        let regs = regions_of(src);
        // The `,` inside the string is Str, not Code.
        let comma = src.find(',').unwrap();
        assert_eq!(regs[comma], Region::Str);
        // The `note` in the trailing comment is Comment.
        let note = src.find("note").unwrap();
        assert_eq!(regs[note], Region::Comment);
        // The leading `let` is Code.
        assert_eq!(regs[0], Region::Code);
    }

    #[test]
    fn lifetime_is_not_a_char_literal() {
        // The classic trap: `'a` is a lifetime (Code), `'x'` is a char (Str).
        let src = "fn f<'a>(c: char) { let _ = 'x'; }";
        let regs = regions_of(src);
        let life = src.find("'a").unwrap();
        assert_eq!(regs[life], Region::Code);
        let ch = src.find("'x'").unwrap();
        assert_eq!(regs[ch], Region::Str);
    }

    #[test]
    fn raw_string_hashes_and_content_are_str() {
        let src = "r#\"a // not a comment\"#";
        let regs = regions_of(src);
        // The `//` inside a raw string must NOT be seen as a comment.
        let slashes = src.find("//").unwrap();
        assert_eq!(regs[slashes], Region::Str);
    }

    #[test]
    fn use_statement_single_line() {
        let s = use_statements("use tracing::info;\n");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].start_line, 0);
        assert_eq!(s[0].end_line, 0);
        assert_eq!(s[0].joined, "use tracing::info;");
    }

    #[test]
    fn use_statement_wrapped_across_lines_is_joined() {
        // rustfmt-style wrap: the full path is on no single physical line.
        let src = "use tracing::{\n    info,\n    warn,\n};\nfn f() {}\n";
        let s = use_statements(src);
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].start_line, s[0].end_line), (0, 3));
        assert_eq!(s[0].joined, "use tracing::{ info, warn, };");
        // `use tracing::` and the imported names are matchable on one line.
        assert!(s[0].joined.contains("use tracing::"));
        assert!(s[0].joined.contains("warn"));
    }

    #[test]
    fn use_statement_semicolon_inside_braces_is_not_the_end() {
        // A `;`-free brace group; the terminating `;` is the statement's own.
        let src = "use a::{b, c};\nlet x = 1;\n";
        let s = use_statements(src);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].joined, "use a::{b, c};");
        assert_eq!(s[0].end_line, 0);
    }

    #[test]
    fn use_keyword_in_string_or_comment_is_not_a_statement() {
        let src = "let s = \"use x::y;\";\n// use a::b;\n";
        assert!(use_statements(src).is_empty());
    }

    #[test]
    fn mask_line_keeps_only_target_region() {
        let line = r#"foo("bar"); // baz"#;
        let regs = classify(line);
        let code = mask_line(line, 0, &regs, Region::Code);
        // Code view keeps foo(...); and blanks the string body + comment.
        assert!(code.contains("foo("));
        assert!(!code.contains("bar"));
        assert!(!code.contains("baz"));
        let strs = mask_line(line, 0, &regs, Region::Str);
        assert!(strs.contains("bar"));
        assert!(!strs.contains("foo"));
    }
}
