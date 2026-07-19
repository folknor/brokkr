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

use std::borrow::Cow;

use crate::config::TextlintRule;
use crate::error::DevError;
use crate::{globs, gremlins, lex};

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

/// A compiled context-window gate: scan `lines` raw physical lines above/below
/// a match for `re`; a hit suppresses the match. The direction and which anchor
/// it counts from live in the call site, not here - all four gate orientations
/// share this one "is the pattern present in the window" test.
struct Window {
    lines: usize,
    re: Regex,
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
    only_if_file_matches_above: bool,
    region: Option<lex::Region>,
    join_wrapped_use: bool,
    except_above: Option<Window>,
    except_below: Option<Window>,
    require_above: Option<Window>,
    require_below: Option<Window>,
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
        if rule.only_if_file_matches_above && rule.only_if_file_matches.is_none() {
            return Err(DevError::Config(format!(
                "[[textlint]] {:?}: only_if_file_matches_above needs only_if_file_matches to be set",
                rule.name
            )));
        }
        let region = match &rule.region {
            Some(r) => Some(lex::Region::parse(r).ok_or_else(|| {
                DevError::Config(format!(
                    "[[textlint]] {:?}: unknown region {r:?}; expected \"code\", \"string\", \
                     or \"comment\"",
                    rule.name
                ))
            })?),
            None => None,
        };
        let except_above = compile_window(&rule.except_above, &rule.name, "except_above")?;
        let except_below = compile_window(&rule.except_below, &rule.name, "except_below")?;
        let require_above = compile_window(&rule.require_above, &rule.name, "require_above")?;
        let require_below = compile_window(&rule.require_below, &rule.name, "require_below")?;
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
            only_if_file_matches_above: rule.only_if_file_matches_above,
            region,
            join_wrapped_use: rule.join_wrapped_use,
            except_above,
            except_below,
            require_above,
            require_below,
        });
    }
    Ok(out)
}

/// Compile one optional context-window gate, rejecting a zero-line window (the
/// window would be empty and the gate a no-op) and an invalid regex.
fn compile_window(
    win: &Option<crate::config::ContextWindow>,
    rule_name: &str,
    field: &str,
) -> Result<Option<Window>, DevError> {
    let Some(win) = win else {
        return Ok(None);
    };
    if win.lines < 1 {
        return Err(DevError::Config(format!(
            "[[textlint]] {rule_name:?}: {field}.lines must be >= 1"
        )));
    }
    let re = Regex::new(&win.pattern).map_err(|e| {
        DevError::Config(format!(
            "[[textlint]] {rule_name:?}: invalid {field} pattern: {e}"
        ))
    })?;
    Ok(Some(Window { lines: win.lines, re }))
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
    // files where some line matches it. Computed once per file per rule. When
    // `only_if_file_matches_above` is set the precondition is position-sensitive
    // (at-or-above each candidate) so it cannot be precomputed - it passes the
    // file gate here and is re-checked per match below.
    let file_ok: Vec<bool> = rules
        .iter()
        .map(|r| match &r.only_if_file_matches {
            Some(re) if !r.only_if_file_matches_above => lines.iter().any(|l| re.is_match(l)),
            _ => true,
        })
        .collect();
    // Lexical regions, tokenized once per file, only when a rule needs scoping.
    let base = content.as_ptr() as usize;
    let regions: Option<Vec<lex::Region>> = rules
        .iter()
        .any(|r| r.region.is_some())
        .then(|| lex::classify(content));
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
            // Whole-`use`-statement rules are handled in the join pass below,
            // not per physical line (a single-line `use` is one 1-line join).
            if rule.join_wrapped_use {
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
            // Scope the pattern to a lexical region when asked; markers,
            // `except`, and the reported line all stay the physical line.
            let hay: Cow<'_, str> = match (rule.region, &regions) {
                (Some(target), Some(regs)) => {
                    let offset = raw.as_ptr() as usize - base;
                    Cow::Owned(lex::mask_line(raw, offset, regs, target))
                }
                _ => Cow::Borrowed(raw),
            };
            if !rule.pattern.is_match(&hay) {
                continue;
            }
            // Position-sensitive precondition: the import must appear at or
            // above this match line, not merely somewhere in the file.
            if !precondition_above_ok(rule, &lines, i) {
                continue;
            }
            if marker_suppresses(rule, &lines, i) {
                continue;
            }
            if rule.except.iter().any(|r| r.is_match(raw)) {
                continue;
            }
            // Context-window gates: for a physical-line match both anchors are
            // the match line itself.
            if context_suppresses(rule, &lines, i, i) {
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

    // Whole-`use`-statement pass: reconstruct wrapped imports once, then match
    // each join rule against the joined text.
    if rules.iter().any(|r| r.join_wrapped_use) {
        scan_use_statements(rel, content, &lines, rules, &file_ok, out);
    }
}

/// The whole-`use`-statement pass of `scan_file`: reconstruct each wrapped
/// import onto one line (via the lexer) and match every `join_wrapped_use` rule
/// against the joined text, applying the same predicate/gate ladder as the
/// per-line pass but anchored at the statement's physical span.
fn scan_use_statements(
    rel: &Path,
    content: &str,
    lines: &[&str],
    rules: &[&Compiled],
    file_ok: &[bool],
    out: &mut Vec<TextlintViolation>,
) {
    let stmts = lex::use_statements(content);
    for (ri, rule) in rules.iter().enumerate() {
        if !rule.join_wrapped_use || !file_ok[ri] {
            continue;
        }
        for st in &stmts {
            if !rule.pattern.is_match(&st.joined) {
                continue;
            }
            // Position-sensitive precondition anchors at the statement's first
            // physical line.
            if !precondition_above_ok(rule, lines, st.start_line) {
                continue;
            }
            // `allow_marker` matches on any physical line of the statement
            // (plus the `allow_marker_above` window at its start).
            if use_marker_suppresses(rule, lines, st) {
                continue;
            }
            if rule.except.iter().any(|r| r.is_match(&st.joined)) {
                continue;
            }
            // Context-window gates anchor the above-window to the statement's
            // first physical line, the below-window to its last.
            if context_suppresses(rule, lines, st.start_line, st.end_line) {
                continue;
            }
            out.push(TextlintViolation {
                file: rel.to_path_buf(),
                line: st.start_line + 1,
                rule: rule.name.clone(),
                message: rule.message.clone(),
                content: cap(&st.joined),
            });
        }
    }
}

/// `allow_marker` for a joined `use` statement: the marker on any physical line
/// of the statement suppresses it, plus the `allow_marker_above` window above.
fn use_marker_suppresses(rule: &Compiled, lines: &[&str], st: &lex::UseStmt) -> bool {
    let Some(marker) = rule.allow_marker.as_deref() else {
        return false;
    };
    let start = st.start_line.saturating_sub(rule.allow_marker_above);
    lines
        .get(start..=st.end_line.min(lines.len().saturating_sub(1)))
        .is_some_and(|window| window.iter().any(|l| l.contains(marker)))
}

/// Whether any configured context-window gate suppresses this match. All four
/// gates share one test - the match is suppressed iff the gate's pattern is
/// found in its window - so any hit suppresses (the gates AND together for the
/// reporter: the violation stands only when every window is clear). The
/// above-windows count from `above_anchor`, the below-windows from
/// `below_anchor`; for a physical-line match the two coincide, while a joined
/// `use` statement anchors above to its first line and below to its last.
/// Windows read raw physical line text - no region masking, no `use`-joining.
fn context_suppresses(
    rule: &Compiled,
    lines: &[&str],
    above_anchor: usize,
    below_anchor: usize,
) -> bool {
    let above = |w: &Option<Window>| w.as_ref().is_some_and(|w| window_hit(w, lines, above_anchor, true));
    let below = |w: &Option<Window>| w.as_ref().is_some_and(|w| window_hit(w, lines, below_anchor, false));
    above(&rule.except_above)
        || above(&rule.require_above)
        || below(&rule.except_below)
        || below(&rule.require_below)
}

/// Whether `w.re` matches any raw line in the window of `w.lines` lines on one
/// side of `anchor`, excluding the anchor line itself and clamped at the file
/// boundaries. `above` scans `[anchor-N, anchor-1]`; otherwise `[anchor+1,
/// anchor+N]`.
fn window_hit(w: &Window, lines: &[&str], anchor: usize, above: bool) -> bool {
    let range = if above {
        anchor.saturating_sub(w.lines)..anchor
    } else {
        let start = (anchor + 1).min(lines.len());
        let end = (anchor + 1 + w.lines).min(lines.len());
        start..end
    };
    lines[range].iter().any(|l| w.re.is_match(l))
}

/// Whether `rule`'s position-sensitive precondition holds for a match anchored
/// at line index `anchor`. Only meaningful when `only_if_file_matches_above` is
/// set (otherwise the whole-file gate already ran in `file_ok`); it requires the
/// `only_if_file_matches` regex to hit at or above the anchor - lines
/// `[0, anchor]`, inclusive, mirroring the upstream hook's `sed -n "1,<line>p"`.
/// Returns `true` (does not gate) when the modifier is off.
fn precondition_above_ok(rule: &Compiled, lines: &[&str], anchor: usize) -> bool {
    if !rule.only_if_file_matches_above {
        return true;
    }
    let Some(re) = &rule.only_if_file_matches else {
        return true;
    };
    lines
        .get(..=anchor)
        .is_some_and(|window| window.iter().any(|l| re.is_match(l)))
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
            only_if_file_matches_above: false,
            region: None,
            join_wrapped_use: false,
            except_above: None,
            except_below: None,
            require_above: None,
            require_below: None,
        }
    }

    fn win(lines: usize, pattern: &str) -> crate::config::ContextWindow {
        crate::config::ContextWindow { lines, pattern: pattern.into() }
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
    fn only_if_file_matches_above_ignores_imports_below_the_match() {
        let mut r = rule("TcpStream::connect");
        r.only_if_file_matches = Some("use\\s+tokio::[^;]*\\bnet\\b".into());
        r.only_if_file_matches_above = true;
        // The seam-correct call on line 1 with the tokio::net import only *below*
        // it (inside a test module) is not armed -> no violation. The whole-file
        // variant would wrongly flag it.
        let src = "let s = TcpStream::connect(addr);\nmod tests {\n    use tokio::net::TcpListener;\n}\n";
        assert!(run(r.clone(), src).is_empty());
        // The whole-file (position-blind) variant does flag it.
        let mut blind = r.clone();
        blind.only_if_file_matches_above = false;
        assert_eq!(run(blind, src), vec![(1, "r".into())]);
    }

    #[test]
    fn only_if_file_matches_above_arms_when_import_precedes_the_match() {
        let mut r = rule("TcpStream::connect");
        r.only_if_file_matches = Some("use\\s+tokio::[^;]*\\bnet\\b".into());
        r.only_if_file_matches_above = true;
        // Import above the call -> armed, and the call is flagged.
        let src = "use tokio::net::TcpStream;\nlet s = TcpStream::connect(addr);\n";
        assert_eq!(run(r, src), vec![(2, "r".into())]);
    }

    #[test]
    fn only_if_file_matches_above_without_base_is_a_config_error() {
        let mut r = rule("x");
        r.only_if_file_matches_above = true;
        assert!(compile(&[r]).is_err());
    }

    #[test]
    fn region_code_ignores_matches_in_strings_and_comments() {
        let mut r = rule("todo");
        r.region = Some("code".into());
        // Only the bare `todo` identifier (line 3) is code; the quoted and
        // commented occurrences are masked out.
        let src = "let s = \"todo later\";\n// todo: nope\nlet todo = 1;\n";
        assert_eq!(run(r, src), vec![(3, "r".into())]);
    }

    #[test]
    fn region_string_targets_message_text_only() {
        let mut r = rule(", got");
        r.region = Some("string".into());
        // The `, got` inside the message string fires; the one in a comment
        // and the one in a code expression do not.
        let src = "// x, got y\nlet e = \"expected a, got b\";\nfoo(a, got);\n";
        assert_eq!(run(r, src), vec![(2, "r".into())]);
    }

    #[test]
    fn region_comment_targets_comments_only() {
        let mut r = rule("FIXME");
        r.region = Some("comment".into());
        let src = "let FIXME = 1; // FIXME real\n";
        // Only the comment occurrence is flagged, not the identifier.
        assert_eq!(run(r, src), vec![(1, "r".into())]);
    }

    #[test]
    fn join_wrapped_use_matches_across_lines() {
        let mut r = rule("use tracing::.*warn");
        r.join_wrapped_use = true;
        // The wrapped import is caught and reported at the `use` line (1).
        let src = "use tracing::{\n    info,\n    warn,\n};\n";
        assert_eq!(run(r, src), vec![(1, "r".into())]);
    }

    #[test]
    fn join_wrapped_use_single_line_still_matches() {
        let mut r = rule("use tracing::");
        r.join_wrapped_use = true;
        assert_eq!(run(r, "use tracing::info;\n"), vec![(1, "r".into())]);
    }

    #[test]
    fn join_wrapped_use_marker_on_any_statement_line_suppresses() {
        let mut r = rule("use tracing::.*warn");
        r.join_wrapped_use = true;
        r.allow_marker = Some("import-ok".into());
        // Marker on the closing line of the wrapped statement still suppresses.
        let src = "use tracing::{\n    info,\n    warn,\n}; // import-ok\n";
        assert!(run(r, src).is_empty());
    }

    #[test]
    fn unknown_region_is_a_config_error() {
        let mut r = rule("x");
        r.region = Some("doc".into());
        assert!(compile(&[r]).is_err());
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

    #[test]
    fn except_above_suppresses_when_pattern_in_window() {
        let mut r = rule("std::thread::spawn");
        r.except_above = Some(win(15, "cfg\\(test\\)"));
        // The `#[cfg(test)]` two lines above is within the 15-line window.
        let src = "#[cfg(test)]\nmod tests {\n    std::thread::spawn(f);\n}\n";
        assert!(run(r, src).is_empty());
    }

    #[test]
    fn except_above_outside_window_still_flags() {
        let mut r = rule("std::thread::spawn");
        // Window of 1: the cfg attribute two lines up is out of reach.
        r.except_above = Some(win(1, "cfg\\(test\\)"));
        let src = "#[cfg(test)]\nlet x = 1;\nstd::thread::spawn(f);\n";
        assert_eq!(run(r, src), vec![(3, "r".into())]);
    }

    #[test]
    fn require_below_flags_unless_token_follows() {
        let mut r = rule("tokio::select!\\s*\\{");
        r.require_below = Some(win(3, "biased;"));
        // `biased;` within 3 lines below -> the select! is disciplined, no hit.
        let ok = "tokio::select! {\n    biased;\n    a = f() => {}\n}\n";
        assert!(run(r.clone(), ok).is_empty());
        // No `biased;` below -> the opener stands as a violation at its line.
        let bad = "tokio::select! {\n    a = f() => {}\n    b = g() => {}\n}\n";
        assert_eq!(run(r, bad), vec![(1, "r".into())]);
    }

    #[test]
    fn window_excludes_the_match_line() {
        let mut r = rule("select");
        // The required token sits only on the match line; the below-window
        // (which excludes it) is clear, so the violation must still stand.
        r.require_below = Some(win(2, "select"));
        let src = "select x\ny;\nz;\n";
        assert_eq!(run(r, src), vec![(1, "r".into())]);
    }

    #[test]
    fn windows_clamp_at_file_boundaries() {
        // A match on the first line with an above-window, and a match on the
        // last line with a below-window: neither over-runs the buffer.
        let mut top = rule("top");
        top.except_above = Some(win(5, "nope"));
        assert_eq!(run(top, "top here\n"), vec![(1, "r".into())]);

        let mut bottom = rule("bottom");
        bottom.require_below = Some(win(5, "nope"));
        assert_eq!(run(bottom, "a;\nbottom here\n"), vec![(2, "r".into())]);
    }

    #[test]
    fn gates_or_together_any_hit_suppresses() {
        let mut base = rule("select");
        base.except_above = Some(win(1, "cfg"));
        base.require_below = Some(win(1, "biased"));
        // cfg above -> suppressed.
        assert!(run(base.clone(), "cfg here\nselect {\n").is_empty());
        // biased below -> suppressed.
        assert!(run(base.clone(), "select {\nbiased;\n").is_empty());
        // Neither window hits -> the violation stands.
        assert_eq!(run(base, "x;\nselect {\ny;\n"), vec![(2, "r".into())]);
    }

    #[test]
    fn join_wrapped_use_anchors_above_to_first_line() {
        let mut r = rule("use tracing::.*warn");
        r.join_wrapped_use = true;
        // The marker one line above the `use` opener is inside a 1-line
        // above-window anchored to the statement's first physical line.
        r.except_above = Some(win(1, "allow-import"));
        let src = "// allow-import\nuse tracing::{\n    warn,\n};\n";
        assert!(run(r, src).is_empty());
    }

    #[test]
    fn join_wrapped_use_anchors_below_to_last_line() {
        let mut r = rule("use tracing::.*warn");
        r.join_wrapped_use = true;
        // The token one line below the closing `};` is inside a 1-line
        // below-window anchored to the statement's last physical line - proving
        // the below-anchor is the end, not the start (a start-anchored window
        // would land inside the block and miss it).
        r.require_below = Some(win(1, "import-ok"));
        let src = "use tracing::{\n    warn,\n};\n// import-ok\n";
        assert!(run(r, src).is_empty());
    }

    #[test]
    fn zero_line_window_is_a_config_error() {
        let mut r = rule("x");
        r.require_below = Some(win(0, "y"));
        assert!(compile(&[r]).is_err());
    }

    #[test]
    fn invalid_window_pattern_is_a_config_error() {
        let mut r = rule("x");
        r.except_above = Some(win(3, "["));
        assert!(compile(&[r]).is_err());
    }
}
