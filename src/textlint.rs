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
    message: String,
    allow_marker: Option<String>,
    except: Vec<Regex>,
    in_toml_section: Option<String>,
    table_row_only: bool,
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
        out.push(Compiled {
            name: rule.name.clone(),
            pattern,
            paths,
            message: rule.message.clone(),
            allow_marker: rule.allow_marker.clone(),
            except,
            in_toml_section: rule.in_toml_section.clone(),
            table_row_only: rule.table_row_only,
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
        let applicable: Vec<&Compiled> = compiled
            .iter()
            .filter(|c| globs::matches(&c.paths, rel))
            .collect();
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

fn scan_file(rel: &Path, content: &str, rules: &[&Compiled], out: &mut Vec<TextlintViolation>) {
    let mut section: Option<String> = None;
    for (i, raw) in content.lines().enumerate() {
        let trimmed = raw.trim();
        if let Some(s) = toml_section(trimmed) {
            section = Some(s);
        }
        for rule in rules {
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
            if rule.allow_marker.as_deref().is_some_and(|m| raw.contains(m)) {
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
            message: "m".into(),
            allow_marker: None,
            except: Vec::new(),
            in_toml_section: None,
            table_row_only: false,
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
    fn toml_section_parses_single_and_array_headers() {
        assert_eq!(toml_section("[dependencies]").as_deref(), Some("dependencies"));
        assert_eq!(toml_section("[[bin]]").as_deref(), Some("bin"));
        assert_eq!(toml_section("tokio = \"1\""), None);
    }
}
