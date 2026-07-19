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
