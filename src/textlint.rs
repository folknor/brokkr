//! `[[textlint]]` check: declarative "forbid a pattern on a line" rules.
//!
//! The generic engine behind most of nautilus_trader's grep-style convention
//! hooks: each rule forbids a regex `pattern` on lines of files matching
//! `paths`, and a match is a violation. Four bounded capabilities carry the
//! rules that need more than a bare grep:
//!
//! - **per-rule file globs** (`paths`) - which files the rule scans;
//! - **inline exception markers** (`allow_marker`) - a line carrying the
//!   author's escape-hatch comment is skipped;
//! - **line/region predicates** - `table_row_only` (markdown table rows) and
//!   `in_toml_section` (only while the last-seen `[section]` matches). These are
//!   the *only* two predicates: no arbitrary multiline, no Rust block tracking;
//! - **regex exceptions** (`except`) - lines matching any are exempt.
//!
//! Patterns are compiled with the linear-time `regex` crate (no backtracking),
//! so an author-supplied pattern cannot hang the phase.

use std::path::{Path, PathBuf};

use regex::Regex;

use crate::config::TextlintRule;
use crate::error::DevError;
use crate::{globs, gremlins};

/// One forbidden-pattern hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextlintViolation {
    pub file: PathBuf,
    pub line: usize,
    pub rule: String,
    pub message: String,
    /// The offending line, trimmed and length-capped for display.
    pub content: String,
}

/// `file:line: [rule] message` plus the offending line.
pub fn format_one(v: &TextlintViolation) -> String {
    format!(
        "{}:{}: [{}] {}\n      {}",
        v.file.display(),
        v.line,
        v.rule,
        v.message,
        v.content,
    )
}

/// A rule with its regexes and glob set compiled once, up front.
struct Compiled {
    name: String,
    pattern: Regex,
    paths: globset::GlobSet,
    exclude: globset::GlobSet,
    message: String,
    allow_marker: Option<String>,
    allow_marker_above: usize,
    except: Vec<Regex>,
    in_toml_section: Option<String>,
    table_row_only: bool,
    skip_after: Option<Regex>,
    only_if_file_matches: Option<Regex>,
}

const DISPLAY_CAP: usize = 200;

fn cap(s: &str) -> String {
    s.chars().take(DISPLAY_CAP).collect()
}

fn compile(rules: &[TextlintRule]) -> Result<Vec<Compiled>, DevError> {
    let mut out = Vec::with_capacity(rules.len());
    for rule in rules {
        let pattern = Regex::new(&rule.pattern).map_err(|e| {
            DevError::Config(format!("[[textlint]] {:?}: invalid pattern: {e}", rule.name))
        })?;
        let mut except = Vec::with_capacity(rule.except.len());
        for ex in &rule.except {
            except.push(Regex::new(ex).map_err(|e| {
                DevError::Config(format!("[[textlint]] {:?}: invalid except pattern: {e}", rule.name))
            })?);
        }
        let paths = globs::build_set(&rule.paths, &format!("[[textlint]] {:?} paths", rule.name))?;
        let exclude =
            globs::build_set(&rule.exclude, &format!("[[textlint]] {:?} exclude", rule.name))?;
        let skip_after = match &rule.skip_after {
            Some(p) => Some(Regex::new(p).map_err(|e| {
                DevError::Config(format!("[[textlint]] {:?}: invalid skip_after pattern: {e}", rule.name))
            })?),
            None => None,
        };
        let only_if_file_matches = match &rule.only_if_file_matches {
            Some(p) => Some(Regex::new(p).map_err(|e| {
                DevError::Config(format!(
                    "[[textlint]] {:?}: invalid only_if_file_matches pattern: {e}",
                    rule.name
                ))
            })?),
            None => None,
        };
        if rule.allow_marker_above > 0 && rule.allow_marker.is_none() {
            return Err(DevError::Config(format!(
                "[[textlint]] {:?}: allow_marker_above needs allow_marker to be set",
                rule.name
            )));
        }
        out.push(Compiled {
            name: rule.name.clone(),
            pattern,
            paths,
            exclude,
            message: rule.message.clone(),
            allow_marker: rule.allow_marker.clone(),
            allow_marker_above: rule.allow_marker_above,
            except,
            in_toml_section: rule.in_toml_section.clone(),
            table_row_only: rule.table_row_only,
            skip_after,
            only_if_file_matches,
        });
    }
    Ok(out)
}

/// Scan tracked files against every rule, one pass per file.
pub fn scan(
    project_root: &Path,
    rules: &[TextlintRule],
) -> Result<Vec<TextlintViolation>, DevError> {
    if rules.is_empty() {
        return Ok(Vec::new());
    }
    let compiled = compile(rules)?;
    let files = gremlins::tracked_files(project_root)?;
    let mut out = Vec::new();
    for rel in &files {
        let applicable: Vec<&Compiled> = compiled.iter().filter(|c| applies(c, rel)).collect();
        if applicable.is_empty() {
            continue;
        }
        let abs = project_root.join(rel);
        let Ok(content) = std::fs::read_to_string(&abs) else {
            continue;
        };
        scan_file(rel, &content, &applicable, &mut out);
    }
    Ok(out)
}

/// Whether a compiled rule scans `rel`: matched by `paths` and not excused by
/// `exclude`.
fn applies(c: &Compiled, rel: &Path) -> bool {
    globs::matches(&c.paths, rel) && !globs::matches(&c.exclude, rel)
}

fn scan_file(rel: &Path, content: &str, rules: &[&Compiled], out: &mut Vec<TextlintViolation>) {
    let lines: Vec<&str> = content.lines().collect();
    // File-scope precondition: a rule with `only_if_file_matches` fires only in
    // files where some line matches it. Computed once per file per rule.
    let file_ok: Vec<bool> = rules
        .iter()
        .map(|r| match &r.only_if_file_matches {
            Some(re) => lines.iter().any(|l| re.is_match(l)),
            None => true,
        })
        .collect();
    let mut section: Option<String> = None;
    // Per-rule "past the skip_after boundary" latch, indexed alongside `rules`.
    let mut skipping = vec![false; rules.len()];
    for (i, raw) in lines.iter().copied().enumerate() {
        let trimmed = raw.trim();
        if let Some(s) = toml_section(trimmed) {
            section = Some(s);
        }
        for (ri, rule) in rules.iter().enumerate() {
            if !file_ok[ri] || skipping[ri] {
                continue;
            }
            // Arm the latch on the boundary line so every *following* line is
            // exempt; the boundary line itself is still evaluated below (it is
            // not the offending pattern in practice, e.g. `#[cfg(test)]`).
            if rule.skip_after.as_ref().is_some_and(|r| r.is_match(raw)) {
                skipping[ri] = true;
            }
            if rule.table_row_only && !trimmed.starts_with('|') {
                continue;
            }
            if let Some(want) = &rule.in_toml_section
                && section.as_deref() != Some(want.as_str())
            {
                continue;
            }
            if !rule.pattern.is_match(raw) {
                continue;
            }
            if marker_suppresses(rule, &lines, i) {
                continue;
            }
            if rule.except.iter().any(|r| r.is_match(raw)) {
                continue;
            }
            out.push(TextlintViolation {
                file: rel.to_path_buf(),
                line: i + 1,
                rule: rule.name.clone(),
                message: rule.message.clone(),
                content: cap(trimmed),
            });
        }
    }
}

/// Whether `rule`'s allow-marker suppresses the match at line index `i`: on the
/// line itself, or within `allow_marker_above` lines above it.
fn marker_suppresses(rule: &Compiled, lines: &[&str], i: usize) -> bool {
    let Some(marker) = rule.allow_marker.as_deref() else {
        return false;
    };
    if lines[i].contains(marker) {
        return true;
    }
    if rule.allow_marker_above > 0 {
        let start = i.saturating_sub(rule.allow_marker_above);
        return lines[start..i].iter().any(|l| l.contains(marker));
    }
    false
}

/// The section name inside a TOML section header (`[deps]` -> `deps`,
/// `[[bin]]` -> `bin`), or `None` for a non-header line.
fn toml_section(trimmed: &str) -> Option<String> {
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return None;
    }
    let inner = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
    if inner.is_empty() {
        return None;
    }
    Some(inner.to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Compile one rule and scan a source string; return (line, rule) hits.
    fn run(rule: TextlintRule, src: &str) -> Vec<(usize, String)> {
        let compiled = compile(&[rule]).unwrap();
        let refs: Vec<&Compiled> = compiled.iter().collect();
        let mut out = Vec::new();
        scan_file(Path::new("t.rs"), src, &refs, &mut out);
        out.iter().map(|v| (v.line, v.rule.clone())).collect()
    }

    fn rule(pattern: &str) -> TextlintRule {
        TextlintRule {
            name: "r".into(),
            pattern: pattern.into(),
            paths: vec!["**/*".into()],
            exclude: Vec::new(),
            message: "m".into(),
            allow_marker: None,
            allow_marker_above: 0,
            except: Vec::new(),
            in_toml_section: None,
            table_row_only: false,
            skip_after: None,
            only_if_file_matches: None,
        }
    }

    #[test]
    fn forbids_a_pattern() {
        assert_eq!(run(rule("todo!"), "let x = 1;\ntodo!();\n"), vec![(2, "r".into())]);
    }

    #[test]
    fn allow_marker_skips_line() {
        let mut r = rule("unwrap\\(\\)");
        r.allow_marker = Some("allow-unwrap".into());
        assert!(run(r, "a.unwrap(); // allow-unwrap\n").is_empty());
    }

    #[test]
    fn except_regex_skips_line() {
        let mut r = rule("panic!");
        r.except = vec!["// in test".into()];
        assert!(run(r, "panic!(); // in test\n").is_empty());
    }

    #[test]
    fn table_row_only_gates_to_markdown_rows() {
        let mut r = rule("foo-bar");
        r.table_row_only = true;
        // Only the table-row occurrence is flagged; the prose line is not.
        let src = "a foo-bar in prose\n| cell | foo-bar |\n";
        assert_eq!(run(r, src), vec![(2, "r".into())]);
    }

    #[test]
    fn in_toml_section_gates_to_a_section() {
        let mut r = rule("^tokio");
        r.in_toml_section = Some("dependencies".into());
        let src = "[dependencies]\ntokio = \"1\"\n\n[dev-dependencies]\ntokio = \"1\"\n";
        // Only the `tokio` under [dependencies] (line 2) fires; the one under
        // [dev-dependencies] (line 5) is out of section.
        assert_eq!(run(r, src), vec![(2, "r".into())]);
    }

    #[test]
    fn exclude_glob_excuses_matching_files() {
        let mut r = rule("panic!");
        r.paths = vec!["crates/**/*.rs".into()];
        r.exclude = vec!["**/*ANYHOW*".into(), "**/anyhow_style_guide*".into()];
        let compiled = compile(&[r]).unwrap();
        let c = &compiled[0];
        // A normal source file is scanned; the style-guide docs are excused.
        assert!(applies(c, Path::new("crates/core/src/lib.rs")));
        assert!(!applies(c, Path::new("crates/core/src/ANYHOW.rs")));
        assert!(!applies(c, Path::new("crates/core/docs/anyhow_style_guide.rs")));
        // Outside `paths` entirely -> not applicable regardless of exclude.
        assert!(!applies(c, Path::new("examples/demo.rs")));
    }

    #[test]
    fn skip_after_exempts_lines_past_the_boundary() {
        let mut r = rule("tokio::spawn\\(");
        r.skip_after = Some("^#\\[cfg\\(test\\)\\]".into());
        // The boundary line itself is still checked; only lines *after* it are
        // exempt. Line 2 fires, line 5 (inside the test module) does not.
        let src = "\
tokio::spawn(a);
let x = 1;
#[cfg(test)]
mod tests {
    tokio::spawn(b);
}
";
        assert_eq!(run(r, src), vec![(1, "r".into())]);
    }

    #[test]
    fn allow_marker_above_suppresses_within_window() {
        let mut r = rule("panic!");
        r.allow_marker = Some("allow-panic".into());
        r.allow_marker_above = 2;
        // Marker two lines above the match -> suppressed.
        assert!(run(r, "// allow-panic\nlet x = 1;\npanic!();\n").is_empty());

        let mut r2 = rule("panic!");
        r2.allow_marker = Some("allow-panic".into());
        r2.allow_marker_above = 1;
        // Marker three lines above but window is only 1 -> still flagged.
        assert_eq!(
            run(r2, "// allow-panic\na;\nb;\npanic!();\n"),
            vec![(4, "r".into())]
        );
    }

    #[test]
    fn allow_marker_above_zero_is_same_line_only() {
        let mut r = rule("panic!");
        r.allow_marker = Some("allow-panic".into());
        // Default window 0: a marker on the line above does not suppress.
        assert_eq!(run(r, "// allow-panic\npanic!();\n"), vec![(2, "r".into())]);
    }

    #[test]
    fn only_if_file_matches_gates_the_whole_file() {
        let mut r = rule("Instant::now\\(\\)");
        r.only_if_file_matches = Some("use std::time::Instant".into());
        // Condition present -> the bare call is flagged.
        let with = "use std::time::Instant;\nlet t = Instant::now();\n";
        assert_eq!(run(r.clone(), with), vec![(2, "r".into())]);
        // Condition absent -> the rule does not fire at all.
        let without = "let t = Instant::now();\n";
        assert!(run(r, without).is_empty());
    }

    #[test]
    fn allow_marker_above_without_marker_is_a_config_error() {
        let mut r = rule("panic!");
        r.allow_marker_above = 3;
        assert!(compile(&[r]).is_err());
    }

    #[test]
    fn skip_after_boundary_line_is_still_checked() {
        // A match on the very boundary line is reported before the latch arms.
        let mut r = rule("MARK");
        r.skip_after = Some("MARK".into());
        let src = "ok\nMARK here\nMARK after\n";
        assert_eq!(run(r, src), vec![(2, "r".into())]);
    }

    #[test]
    fn toml_section_parses_single_and_array_headers() {
        assert_eq!(toml_section("[dependencies]").as_deref(), Some("dependencies"));
        assert_eq!(toml_section("[[bin]]").as_deref(), Some("bin"));
        assert_eq!(toml_section("tokio = \"1\""), None);
    }
}
