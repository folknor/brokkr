//! Gremlin detector for `brokkr check`.
//!
//! Scans tracked source, config, and doc files for invisible or visually
//! deceptive Unicode characters that tend to sneak in via copy-paste from
//! editors, chat logs, or LLM output and cause subtle bugs. The banned
//! set covers three families:
//!
//! * zero-width / invisible (ZWSP, BOM inside files, word joiner, soft hyphen)
//! * non-breaking spaces (NBSP, narrow NBSP)
//! * bidi marks / overrides / isolates (Trojan Source)
//! * line / paragraph separators
//! * em-dash, en-dash
//! * typographic single and double quotes

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::DevError;

/// One gremlin occurrence.
pub struct Gremlin {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub codepoint: u32,
    pub name: &'static str,
}

/// Format a gremlin occurrence as a one-liner matching cargo_filter style.
///
/// `src/foo.rs:10:5 U+200B ZERO WIDTH SPACE`
pub fn format_one(g: &Gremlin) -> String {
    format!(
        "{}:{}:{} U+{:04X} {}",
        g.path.display(),
        g.line,
        g.column,
        g.codepoint,
        g.name,
    )
}

/// Per-file summary of a `fix` operation.
pub struct FixSummary {
    pub path: PathBuf,
    pub count: usize,
}

/// Mechanical replacement for a gremlin char. Empty string means delete.
/// Returns `None` for non-gremlin chars.
fn replacement(c: char) -> Option<&'static str> {
    Some(match c {
        // Zero-width / invisible / bidi / control noise → delete
        '\u{0003}' | '\u{000B}' | '\u{200B}' | '\u{200C}' | '\u{200D}'
        | '\u{2060}' | '\u{FEFF}' | '\u{00AD}' | '\u{200E}' | '\u{200F}'
        | '\u{202A}' | '\u{202B}' | '\u{202C}' | '\u{202D}' | '\u{202E}'
        | '\u{2066}' | '\u{2067}' | '\u{2068}' | '\u{2069}' | '\u{FFFC}' => "",
        // Non-breaking spaces → regular space
        '\u{00A0}' | '\u{202F}' => " ",
        // Line / paragraph separators → newline
        '\u{2028}' | '\u{2029}' => "\n",
        // Em / en dash → hyphen-minus
        '\u{2013}' | '\u{2014}' => "-",
        // Typographic single quotes → ASCII apostrophe
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => "'",
        // Typographic double quotes → ASCII double quote
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => "\"",
        _ => return None,
    })
}

/// Rewrite every tracked scannable file, replacing gremlin chars with their
/// ASCII equivalents (or deleting zero-width / bidi noise). Returns one
/// summary entry per file that was actually modified.
pub fn fix(project_root: &Path) -> Result<Vec<FixSummary>, DevError> {
    let files = tracked_files(project_root)?;
    let mut out = Vec::new();
    for rel in &files {
        if !is_scannable(rel) {
            continue;
        }
        let abs = project_root.join(rel);
        let Ok(content) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let (fixed, count) = fix_content(&content);
        if count > 0 {
            std::fs::write(&abs, fixed).map_err(DevError::Io)?;
            out.push(FixSummary {
                path: rel.clone(),
                count,
            });
        }
    }
    Ok(out)
}

fn fix_content(content: &str) -> (String, usize) {
    let mut out = String::with_capacity(content.len());
    let mut count = 0usize;
    for c in content.chars() {
        // Fast path: printable ASCII and the usual whitespace never need fixing.
        let cp = c as u32;
        if (0x20..0x7F).contains(&cp) || cp == 0x09 || cp == 0x0A || cp == 0x0D {
            out.push(c);
            continue;
        }
        if let Some(r) = replacement(c) {
            out.push_str(r);
            count += 1;
        } else {
            out.push(c);
        }
    }
    (out, count)
}

const GREMLINS: &[(char, &str)] = &[
    // Control chars that should never appear in source
    ('\u{0003}', "END OF TEXT"),
    ('\u{000B}', "LINE TABULATION"),
    // Zero-width / invisible
    ('\u{200B}', "ZERO WIDTH SPACE"),
    ('\u{200C}', "ZERO WIDTH NON-JOINER"),
    ('\u{200D}', "ZERO WIDTH JOINER"),
    ('\u{2060}', "WORD JOINER"),
    ('\u{FEFF}', "ZERO WIDTH NO-BREAK SPACE (BOM)"),
    // Non-breaking spaces
    ('\u{00A0}', "NO-BREAK SPACE"),
    ('\u{202F}', "NARROW NO-BREAK SPACE"),
    // Soft hyphen
    ('\u{00AD}', "SOFT HYPHEN"),
    // Line / paragraph separators
    ('\u{2028}', "LINE SEPARATOR"),
    ('\u{2029}', "PARAGRAPH SEPARATOR"),
    // Bidi marks / overrides / isolates
    ('\u{200E}', "LEFT-TO-RIGHT MARK"),
    ('\u{200F}', "RIGHT-TO-LEFT MARK"),
    ('\u{202A}', "LEFT-TO-RIGHT EMBEDDING"),
    ('\u{202B}', "RIGHT-TO-LEFT EMBEDDING"),
    ('\u{202C}', "POP DIRECTIONAL FORMATTING"),
    ('\u{202D}', "LEFT-TO-RIGHT OVERRIDE"),
    ('\u{202E}', "RIGHT-TO-LEFT OVERRIDE"),
    ('\u{2066}', "LEFT-TO-RIGHT ISOLATE"),
    ('\u{2067}', "RIGHT-TO-LEFT ISOLATE"),
    ('\u{2068}', "FIRST STRONG ISOLATE"),
    ('\u{2069}', "POP DIRECTIONAL ISOLATE"),
    // Em-dash / en-dash
    ('\u{2013}', "EN DASH"),
    ('\u{2014}', "EM DASH"),
    // Typographic single quotes
    ('\u{2018}', "LEFT SINGLE QUOTATION MARK"),
    ('\u{2019}', "RIGHT SINGLE QUOTATION MARK"),
    ('\u{201A}', "SINGLE LOW-9 QUOTATION MARK"),
    ('\u{201B}', "SINGLE HIGH-REVERSED-9 QUOTATION MARK"),
    // Typographic double quotes
    ('\u{201C}', "LEFT DOUBLE QUOTATION MARK"),
    ('\u{201D}', "RIGHT DOUBLE QUOTATION MARK"),
    ('\u{201E}', "DOUBLE LOW-9 QUOTATION MARK"),
    ('\u{201F}', "DOUBLE HIGH-REVERSED-9 QUOTATION MARK"),
    // Placeholder for removed embedded objects
    ('\u{FFFC}', "OBJECT REPLACEMENT CHARACTER"),
];

/// File extensions scanned by default.
const SCANNED_EXTENSIONS: &[&str] = &["rs", "toml", "md", "js", "sh"];

/// Scan every tracked file with a scannable extension.
pub fn scan(project_root: &Path) -> Result<Vec<Gremlin>, DevError> {
    let files = tracked_files(project_root)?;
    let mut out = Vec::new();
    for rel in &files {
        if !is_scannable(rel) {
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

fn scan_content(rel: &Path, content: &str, out: &mut Vec<Gremlin>) {
    let mut line = 1usize;
    let mut col = 1usize;
    for c in content.chars() {
        if c == '\n' {
            line += 1;
            col = 1;
            continue;
        }
        if let Some(name) = gremlin_name(c) {
            out.push(Gremlin {
                path: rel.to_path_buf(),
                line,
                column: col,
                codepoint: c as u32,
                name,
            });
        }
        col += 1;
    }
}

fn gremlin_name(c: char) -> Option<&'static str> {
    // Fast path: printable ASCII plus tab/LF/CR are the overwhelmingly common
    // case and never gremlins. Everything else falls through to the table,
    // including low-range control chars (U+0003, U+000B).
    let cp = c as u32;
    if (0x20..0x7F).contains(&cp) || cp == 0x09 || cp == 0x0A || cp == 0x0D {
        return None;
    }
    for (g, name) in GREMLINS {
        if *g == c {
            return Some(name);
        }
    }
    None
}

fn is_scannable(rel: &Path) -> bool {
    let Some(ext) = rel.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    SCANNED_EXTENSIONS.contains(&ext)
}

fn tracked_files(project_root: &Path) -> Result<Vec<PathBuf>, DevError> {
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(project_root)
        .output()
        .map_err(DevError::Io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(DevError::Subprocess {
            program: "git ls-files".into(),
            code: output.status.code(),
            stderr,
        });
    }
    let mut files = Vec::new();
    for raw in output.stdout.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(raw) {
            files.push(PathBuf::from(s));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic
    )]
    use super::*;

    fn scan_str(s: &str) -> Vec<Gremlin> {
        let mut out = Vec::new();
        scan_content(Path::new("t.rs"), s, &mut out);
        out
    }

    #[test]
    fn clean_ascii_finds_nothing() {
        let out = scan_str("fn main() {\n    println!(\"ok\");\n}\n");
        assert!(out.is_empty());
    }

    #[test]
    fn detects_em_dash() {
        let out = scan_str("// foo \u{2014} bar\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].codepoint, 0x2014);
        assert_eq!(out[0].name, "EM DASH");
        assert_eq!(out[0].line, 1);
    }

    #[test]
    fn detects_smart_quote() {
        let out = scan_str("let s = \u{201C}hi\u{201D};\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "LEFT DOUBLE QUOTATION MARK");
        assert_eq!(out[1].name, "RIGHT DOUBLE QUOTATION MARK");
    }

    #[test]
    fn detects_zero_width_space() {
        let out = scan_str("abc\u{200B}def\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].codepoint, 0x200B);
        assert_eq!(out[0].column, 4);
    }

    #[test]
    fn detects_nbsp() {
        let out = scan_str("foo\u{00A0}bar\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].codepoint, 0x00A0);
    }

    #[test]
    fn line_and_column_tracking() {
        let out = scan_str("ok\n\u{2014}second\n  \u{2014}third\n");
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].line, out[0].column), (2, 1));
        assert_eq!((out[1].line, out[1].column), (3, 3));
    }

    #[test]
    fn detects_end_of_text_control() {
        let out = scan_str("abc\u{0003}def\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].codepoint, 0x0003);
        assert_eq!(out[0].name, "END OF TEXT");
    }

    #[test]
    fn detects_line_tabulation() {
        let out = scan_str("abc\u{000B}def\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].codepoint, 0x000B);
        assert_eq!(out[0].name, "LINE TABULATION");
    }

    #[test]
    fn detects_object_replacement() {
        let out = scan_str("stub \u{FFFC} here\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].codepoint, 0xFFFC);
        assert_eq!(out[0].name, "OBJECT REPLACEMENT CHARACTER");
    }

    #[test]
    fn bidi_override_detected() {
        let out = scan_str("let evil = \u{202E}reversed\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "RIGHT-TO-LEFT OVERRIDE");
    }

    #[test]
    fn format_one_matches_expected_shape() {
        let g = Gremlin {
            path: PathBuf::from("src/foo.rs"),
            line: 10,
            column: 5,
            codepoint: 0x200B,
            name: "ZERO WIDTH SPACE",
        };
        assert_eq!(format_one(&g), "src/foo.rs:10:5 U+200B ZERO WIDTH SPACE");
    }

    #[test]
    fn fix_content_rewrites_known_gremlins() {
        let (fixed, count) = fix_content(
            "x\u{2014}y \u{201C}hi\u{201D} \u{00A0}end\u{200B}\n",
        );
        assert_eq!(count, 5);
        assert_eq!(fixed, "x-y \"hi\"  end\n");
    }

    #[test]
    fn fix_content_is_noop_when_clean() {
        let (fixed, count) = fix_content("fn main() {\n    println!(\"ok\");\n}\n");
        assert_eq!(count, 0);
        assert_eq!(fixed, "fn main() {\n    println!(\"ok\");\n}\n");
    }

    #[test]
    fn fix_content_preserves_unrelated_unicode() {
        let (fixed, count) = fix_content("// café\n");
        assert_eq!(count, 0);
        assert_eq!(fixed, "// café\n");
    }

    #[test]
    fn is_scannable_matches_expected_extensions() {
        assert!(is_scannable(Path::new("src/foo.rs")));
        assert!(is_scannable(Path::new("Cargo.toml")));
        assert!(is_scannable(Path::new("README.md")));
        assert!(is_scannable(Path::new("x.js")));
        assert!(is_scannable(Path::new("y.sh")));
        assert!(!is_scannable(Path::new("foo.html")));
        assert!(!is_scannable(Path::new("Cargo.lock")));
        assert!(!is_scannable(Path::new("no_ext")));
    }
}
