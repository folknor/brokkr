//! `[[textlint]]` check: declarative regex rules over selected files.
//!
//! The generic engine behind most of nautilus_trader's grep-style convention
//! hooks: by default, each rule checks a regex `pattern` on lines of files
//! matching `paths`; `join_wrapped_use` instead checks reconstructed Rust
//! `use` statements. Bounded selectors and gates cover rules that need more
//! than a bare grep:
//!
//! - **file globs** (`paths`, `exclude`) select the files to scan;
//! - **exception markers** (`allow_marker`, optionally widened with
//!   `allow_marker_above`) exempt authored lines;
//! - **line and section predicates** (`table_row_only`, `in_toml_section`) and
//!   **lexical regions** (`region`) scope where the pattern is considered;
//! - **file and line preconditions** (`only_if_file_matches`,
//!   `only_if_file_matches_above`) and **skip boundaries** (`skip_after`) gate
//!   matches by surrounding file content;
//! - **regex exceptions** (`except`) and bounded context windows further
//!   suppress or require nearby text.
//!
//! Patterns are compiled with the linear-time `regex` crate (no backtracking),
//! so an author-supplied pattern cannot hang the phase.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::Regex;

use std::borrow::Cow;

use crate::config::{TestCode, TextlintRule};
use crate::error::DevError;
use crate::{globs, lex};

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
/// it counts from live in the call site; every orientation shares this one
/// "is the pattern present in the window" test.
#[derive(Debug, Clone)]
struct Window {
    lines: usize,
    re: Regex,
}

/// A rule with its regexes and glob set compiled once, up front.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
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
    test_code: TestCode,
    must_scan: Vec<PathBuf>,
}

/// Maximum character count in one rendered source line. This is a presentation
/// bound shared by every rule, not a per-project matching option.
const DISPLAY_CAP: usize = 200;

fn cap(s: &str) -> String {
    let mut chars = s.chars();
    let prefix: String = chars.by_ref().take(DISPLAY_CAP - 1).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[allow(clippy::too_many_lines)] // one pass compiling every optional pattern of a rule
fn compile(rules: &[TextlintRule]) -> Result<Vec<Compiled>, DevError> {
    let mut out = Vec::with_capacity(rules.len());
    for rule in rules {
        let pattern = Regex::new(&rule.pattern).map_err(|e| {
            DevError::Config(format!(
                "[[textlint]] {:?}: invalid pattern: {e}",
                rule.name
            ))
        })?;
        let mut except = Vec::with_capacity(rule.except.len());
        for ex in &rule.except {
            except.push(Regex::new(ex).map_err(|e| {
                DevError::Config(format!(
                    "[[textlint]] {:?}: invalid except pattern: {e}",
                    rule.name
                ))
            })?);
        }
        // A `join_wrapped_use` rule matches only reconstructed `use ...;`
        // statements (see `scan_use_statements`). If an `except` pattern exempts
        // an ordinary `use` line, it exempts the very statements the rule scans,
        // so the rule can never fire - a silent no-op. That is the fingerprint of
        // a use-site rule wearing the import-rule flag by mistake (the bug that
        // let `no-fq-nautilus-types` pass a tree CI then rejected). A rule with a
        // *specific* except (e.g. `use tracing::info`) does not match the generic
        // probe and is left alone. Reject the broad form at compile time.
        if rule.join_wrapped_use
            && let Some(ex) = except.iter().find(|r| r.is_match("use a::b;"))
        {
            return Err(DevError::Config(format!(
                "[[textlint]] {:?}: join_wrapped_use scans only `use` statements, but the \
                 except pattern {:?} exempts ordinary `use` lines, so the rule can never fire. \
                 Drop join_wrapped_use if this is a use-site rule, or narrow the except.",
                rule.name,
                ex.as_str(),
            )));
        }
        // The join pass matches whole reconstructed `use` statements, which are
        // always Rust code - never a markdown table row nor inside a TOML
        // `[section]`, and region-masking to "code" is a no-op while "string"/
        // "comment" never matches one. So these line/region predicates cannot
        // *bound* a join rule; they can only silently make it a no-op. Same
        // fingerprint as the `except`-eats-`use` footgun above: reject at compile
        // time rather than drop them mutely (which is what the join pass would).
        if rule.join_wrapped_use
            && let Some(field) = join_incompatible_predicate(rule)
        {
            return Err(DevError::Config(format!(
                "[[textlint]] {:?}: {field} cannot combine with join_wrapped_use, which matches \
                 whole reconstructed `use` statements (always code, never a markdown table row \
                 or TOML section). The predicate would silently make the rule a no-op; drop it, \
                 or drop join_wrapped_use.",
                rule.name,
            )));
        }
        let paths_field = format!("[[textlint]] {:?} paths", rule.name);
        let paths = globs::build_set(&rule.paths, &paths_field)?;
        let exclude_field = format!("[[textlint]] {:?} exclude", rule.name);
        let exclude = globs::build_set(&rule.exclude, &exclude_field)?;
        let skip_after = match &rule.skip_after {
            Some(p) => Some(Regex::new(p).map_err(|e| {
                DevError::Config(format!(
                    "[[textlint]] {:?}: invalid skip_after pattern: {e}",
                    rule.name
                ))
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
            test_code: rule.test_code,
            must_scan: rule.must_scan.iter().map(PathBuf::from).collect(),
        });
    }
    Ok(out)
}

/// Compile and cache every rule while config is being parsed. This moves
/// malformed regexes, globs, and incoherent rule combinations out of the
/// convention phase. The cached values are skipped when config rules serialize.
pub(crate) fn compile_rules(rules: &mut [TextlintRule]) -> Result<(), DevError> {
    let compiled = compile(rules)?;
    for (rule, compiled) in rules.iter_mut().zip(compiled) {
        rule.compiled = Some(compiled);
    }
    Ok(())
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
    Ok(Some(Window {
        lines: win.lines,
        re,
    }))
}

/// The name of a line/region predicate that is incoherent with
/// `join_wrapped_use` (they scope the *per-line* pass, which the join pass does
/// not run), or `None` if the rule sets none of them. Used only for the
/// compile-time rejection above.
fn join_incompatible_predicate(rule: &TextlintRule) -> Option<&'static str> {
    if rule.region.is_some() {
        Some("region")
    } else if rule.test_code != TestCode::Include {
        // The join pass reconstructs imports lexically, so `use` text inside
        // macro input would be read as an import; no rule needs both.
        Some("test_code")
    } else if rule.table_row_only {
        Some("table_row_only")
    } else if rule.in_toml_section.is_some() {
        Some("in_toml_section")
    } else {
        None
    }
}

/// What a scan found, and how much it looked at.
///
/// A green phase reports the second half so "ok" is a statement about a known
/// corpus rather than an unfalsifiable one - a rule whose `paths` glob stopped
/// matching anything still passes, and `0 files` is the only thing that
/// distinguishes that from a rule doing its job.
pub struct TextlintScan {
    pub violations: Vec<TextlintViolation>,
    /// Files at least one rule applied to - not the tracked-file total, which
    /// would credit the phase with files no rule looks at.
    pub files: usize,
}

/// Scan one tree snapshot against every rule, one pass per file.
pub fn scan(
    tree: &crate::scope::TreeSnapshot,
    rules: &[TextlintRule],
) -> Result<TextlintScan, DevError> {
    if rules.is_empty() {
        return Ok(TextlintScan {
            violations: Vec::new(),
            files: 0,
        });
    }
    let files = tree.files();
    let compiled = if rules.iter().all(|rule| rule.compiled.is_some()) {
        rules
            .iter()
            .filter_map(|rule| rule.compiled.clone())
            .collect()
    } else {
        // Keep the scanner usable with rules constructed directly by callers
        // and tests; parsed config uses the cache compiled by compile_rules.
        compile(rules)?
    };
    // Dead scope (a glob matching nothing, an exclude excusing everything) is
    // a `brokkr check --audit` finding, never a refusal nor a warning here:
    // user-wide rules reach every tree brokkr checks, and most trees lack most
    // of the paths a personal rule names.
    if let Some(rule) = rules.iter().find(|rule| rule.paths.is_empty()) {
        return Err(DevError::Config(format!(
            "[[textlint]] {:?} has no `paths` - list the files this rule scans",
            rule.name
        )));
    }
    let classification = classify_tests(tree, &compiled, files)?;
    if let Some(c) = &classification {
        for note in &c.notes {
            crate::output::warn(&format!("[[textlint]] test_code: {note}"));
        }
        refuse_unresolved(c, &compiled, files)?;
    }
    let mut out = Vec::new();
    let mut scanned = 0usize;
    let mut admitted: Vec<HashSet<PathBuf>> = vec![HashSet::new(); compiled.len()];
    for rel in files {
        let (indices, applicable): (Vec<usize>, Vec<&Compiled>) = compiled
            .iter()
            .enumerate()
            .filter(|(_, c)| applies(c, rel))
            .unzip();
        if applicable.is_empty() {
            continue;
        }
        // An unreadable file is an error, not a skip (see
        // `TreeSnapshot::read_file`); only a path deleted from the working
        // tree is passed over.
        let Some(bytes) = tree.read_file(rel)? else {
            continue;
        };
        let Some(content) = text_of(bytes.as_ref()) else {
            continue;
        };
        scanned += 1;
        let class = classification.as_ref().filter(|_| is_rust(rel));
        let view = FileClass {
            mask: class.and_then(|c| c.mask(rel)),
            tests: class.and_then(|c| c.test_selection(rel)),
        };
        for k in scan_file(rel, &content, &applicable, view, &mut out) {
            admitted[indices[k]].insert(rel.clone());
        }
    }
    out.extend(must_scan_violations(&compiled, &admitted, files));
    Ok(TextlintScan {
        violations: out,
        files: scanned,
    })
}

fn is_rust(rel: &Path) -> bool {
    rel.extension().is_some_and(|e| e == "rs")
}

/// What the classifier says about one Rust file, for the rules that ask.
#[derive(Clone, Copy, Default)]
struct FileClass<'a> {
    /// What `test_code = "skip"` blanks.
    mask: Option<&'a crate::test_code::FileMask>,
    /// What `test_code = "only"` keeps.
    tests: Option<&'a [std::ops::Range<usize>]>,
}

/// The test-code classification, computed once when some rule with a
/// `test_code` scope applies to at least one tracked `.rs` file. Otherwise no
/// `cargo metadata` runs and nothing is parsed: a user-wide scoped rule must
/// not make a tree it has nothing to scan in fail for lacking a Cargo
/// workspace.
fn classify_tests(
    tree: &crate::scope::TreeSnapshot,
    compiled: &[Compiled],
    files: &[PathBuf],
) -> Result<Option<crate::test_code::Classification>, DevError> {
    let rust: Vec<PathBuf> = files.iter().filter(|f| is_rust(f)).cloned().collect();
    let wants = |scope: fn(TestCode) -> bool| {
        compiled
            .iter()
            .any(|c| scope(c.test_code) && rust.iter().any(|f| applies(c, f)))
    };
    if !wants(|t| t != TestCode::Include) {
        return Ok(None);
    }
    let select_tests = wants(|t| t == TestCode::Only);
    let roots = crate::test_code::workspace_roots(tree.root())?;
    // Masks must describe the bytes the scan matches against, so the
    // classifier reads through the same snapshot. The snapshot caches by
    // file identity, so `src/a/../b.rs`, a symlinked spelling and a file
    // outside the root all land on the one entry the scan also reads.
    let read = |abs: &Path| -> Result<String, String> {
        let bytes = match tree.read_file(abs) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err("does not exist".to_owned()),
            Err(error) => return Err(format!("cannot be read: {error}")),
        };
        String::from_utf8(bytes.to_vec()).map_err(|_| "is not valid UTF-8 source".to_owned())
    };
    crate::test_code::classify(tree.root(), &roots, &rust, &read, select_tests).map(Some)
}

/// Refuse when a `test_code = "only"` rule applies to a file whose test
/// selection depends on what a shadowable bare `#[test]` names: scanning it
/// either way would be a guess, and treating the function as production
/// would silently drop it from a rule written to police it.
fn refuse_unresolved(
    c: &crate::test_code::Classification,
    compiled: &[Compiled],
    files: &[PathBuf],
) -> Result<(), DevError> {
    let mut sites = Vec::new();
    for rule in compiled.iter().filter(|r| r.test_code == TestCode::Only) {
        for rel in files {
            if is_rust(rel) && applies(rule, rel) && c.is_unresolved(rel) {
                sites.push(format!("{} (rule {:?})", rel.display(), rule.name));
            }
        }
    }
    if sites.is_empty() {
        return Ok(());
    }
    Err(DevError::Refused(format!(
        "[[textlint]] test_code = \"only\" cannot tell whether bare #[test] functions are test code in:\n  {}\nbecause something could shadow the builtin attribute:\n  {}",
        sites.join("\n  "),
        c.shadows.join("\n  ")
    )))
}

/// One violation per `must_scan` path its rule did not actually scan.
fn must_scan_violations(
    compiled: &[Compiled],
    admitted: &[HashSet<PathBuf>],
    files: &[PathBuf],
) -> Vec<TextlintViolation> {
    let mut out = Vec::new();
    for (rule, seen) in compiled.iter().zip(admitted) {
        for path in &rule.must_scan {
            if seen.contains(path) {
                continue;
            }
            let why = if !files.contains(path) {
                "is not a tracked file of this tree"
            } else if !applies(rule, path) {
                "is not selected by this rule's paths/exclude"
            } else if rule.test_code != TestCode::Include {
                "was not scanned: deleted, binary, refused by only_if_file_matches, or no code left under test_code"
            } else {
                "was not scanned: deleted, binary, or refused by only_if_file_matches"
            };
            out.push(TextlintViolation {
                file: path.clone(),
                line: 0,
                rule: rule.name.clone(),
                message: format!("must_scan: {} {why}", path.display()),
                content: String::new(),
            });
        }
    }
    out
}

/// The text a rule is matched against, or `None` for a binary file.
///
/// A file that merely is not valid UTF-8 (a Latin-1 `.md`, say) used to fail
/// `read_to_string` and be skipped, so no rule ever saw it - a silent pass.
/// It is now decoded lossily: every ASCII byte, which is what patterns are
/// written against, survives verbatim, and an invalid byte becomes U+FFFD
/// (gremlins reports the byte itself). Only a *binary* file is skipped, by
/// git's own heuristic - a NUL in the first 8000 bytes - because rule globs
/// are author-supplied and a broad one (`**/*`) legitimately sweeps in
/// images and archives, where a line pattern means nothing. Reporting those
/// as violations would fail every tree with a binary asset in it.
fn text_of(bytes: &[u8]) -> Option<Cow<'_, str>> {
    const BINARY_PROBE: usize = 8000;
    if bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(bytes))
}

/// Whether a compiled rule scans `rel`: matched by `paths` and not excused by
/// `exclude`.
fn applies(c: &Compiled, rel: &Path) -> bool {
    globs::matches(&c.paths, rel) && !globs::matches(&c.exclude, rel)
}

/// Per-byte membership of `spans` in `content` (already BOM-stripped;
/// `shift` is the stripped length, since the classifier counts file bytes).
fn span_bytes(spans: &[std::ops::Range<usize>], content: &str, shift: usize) -> Vec<bool> {
    let mut bytes = vec![false; content.len()];
    for span in spans {
        let start = span.start.saturating_sub(shift).min(bytes.len());
        let end = span.end.saturating_sub(shift).min(bytes.len());
        bytes[start..end].fill(true);
    }
    bytes
}

/// Per-byte test mask of `content`: what `test_code = "skip"` blanks.
fn test_bytes(mask: &crate::test_code::FileMask, content: &str, shift: usize) -> Vec<bool> {
    match mask {
        crate::test_code::FileMask::Test => vec![true; content.len()],
        crate::test_code::FileMask::Production(spans) => span_bytes(spans, content, shift),
    }
}

/// The bytes of one file a rule may not match, per its `test_code`.
#[derive(Clone, Copy)]
enum Hidden<'a> {
    /// Every byte is in scope.
    Nothing,
    /// The flagged bytes are out of scope.
    Bytes(&'a [bool]),
    /// No byte is in scope: a `test_code = "only"` rule in a file with no
    /// test selection (not Rust, or not classified).
    All,
}

impl Hidden<'_> {
    fn hides(self, b: usize) -> bool {
        match self {
            Self::Nothing => false,
            Self::Bytes(t) => t.get(b).copied().unwrap_or(false),
            Self::All => true,
        }
    }
}

/// The per-byte views of one file the rules need, computed once.
struct Views {
    /// Test bytes, for `skip`; `None` leaves the file whole.
    skip: Option<Vec<bool>>,
    /// Bytes outside the test selection, for `only`; `None` hides it all.
    only: Option<Vec<bool>>,
}

impl Views {
    /// The views `rules` need of `content` (BOM-stripped by `shift` bytes).
    fn of(class: FileClass<'_>, rules: &[&Compiled], content: &str, shift: usize) -> Self {
        let wants = |scope: TestCode| rules.iter().any(|r| r.test_code == scope);
        Self {
            skip: class
                .mask
                .filter(|_| wants(TestCode::Skip))
                .map(|m| test_bytes(m, content, shift)),
            only: class.tests.filter(|_| wants(TestCode::Only)).map(|spans| {
                let mut bytes = span_bytes(spans, content, shift);
                bytes.iter_mut().for_each(|b| *b = !*b);
                bytes
            }),
        }
    }

    fn hidden(&self, rule: &Compiled) -> Hidden<'_> {
        match rule.test_code {
            TestCode::Include => Hidden::Nothing,
            TestCode::Skip => self.skip.as_deref().map_or(Hidden::Nothing, Hidden::Bytes),
            TestCode::Only => self.only.as_deref().map_or(Hidden::All, Hidden::Bytes),
        }
    }
}

/// The text `rule.pattern` is matched against for one physical line: the
/// line itself, or a copy with bytes outside the rule's lexical region and
/// its `test_code` scope blanked.
fn haystack<'a>(
    rule: &Compiled,
    raw: &'a str,
    offset: usize,
    regions: Option<&[lex::Region]>,
    hidden: Hidden<'_>,
) -> Cow<'a, str> {
    if rule.region.is_none() && matches!(hidden, Hidden::Nothing) {
        return Cow::Borrowed(raw);
    }
    Cow::Owned(lex::mask_line_by(raw, offset, |b| {
        rule.region
            .is_none_or(|target| regions.and_then(|r| r.get(b)).copied() == Some(target))
            && !hidden.hides(b)
    }))
}

/// Whether `rule` matches per physical line at all, and this line's shape
/// and TOML section are ones it scans.
fn line_in_scope(rule: &Compiled, trimmed: &str, section: Option<&str>) -> bool {
    // Whole-`use`-statement rules are handled in the join pass, not per
    // physical line (a single-line `use` is one 1-line join).
    if rule.join_wrapped_use {
        return false;
    }
    if rule.table_row_only && !trimmed.starts_with('|') {
        return false;
    }
    rule.in_toml_section
        .as_deref()
        .is_none_or(|want| section == Some(want))
}

/// Scan one file against its applicable rules; returns the indices (into
/// `rules`) of the rules that admitted the file to matching, for `must_scan`.
fn scan_file(
    rel: &Path,
    content: &str,
    rules: &[&Compiled],
    class: FileClass<'_>,
    out: &mut Vec<TextlintViolation>,
) -> Vec<usize> {
    // Strip a single leading UTF-8 BOM so a rule anchored to the start of line 1
    // (e.g. `^use `) matches its first physical line. `strip_prefix` returns a
    // subslice of the same allocation, so classify/line-iteration/offset all stay
    // internally consistent as long as they read this one slice. Line numbers are
    // unaffected: the BOM precedes line 1's text.
    let original_len = content.len();
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let views = Views::of(class, rules, content, original_len - content.len());
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
    let skip_after: Vec<Vec<bool>> = rules
        .iter()
        .map(|rule| skip_after_mask(rule, &lines))
        .collect();
    // Lexical regions, tokenized once per file, only when a rule needs
    // scoping or `must_scan` admission needs to tell code from comments.
    let base = content.as_ptr() as usize;
    let regions: Option<Vec<lex::Region>> = rules
        .iter()
        .any(|r| r.region.is_some() || (r.test_code != TestCode::Include && is_rust(rel)))
        .then(|| lex::classify(content));
    let first_precondition: Vec<Option<usize>> = rules
        .iter()
        .map(|r| first_precondition_line(r, &lines))
        .collect();
    let mut eligible = vec![false; rules.len()];
    let mut section: Option<String> = None;
    for (i, raw) in lines.iter().copied().enumerate() {
        let trimmed = raw.trim();
        if let Some(s) = toml_section(trimmed) {
            section = Some(s);
        }
        for (ri, rule) in rules.iter().enumerate() {
            if !file_ok[ri]
                || skip_after[ri][i]
                || !line_in_scope(rule, trimmed, section.as_deref())
            {
                continue;
            }
            let offset = raw.as_ptr() as usize - base;
            let hidden = views.hidden(rule);
            if wholly_hidden(hidden, raw, offset) {
                continue;
            }
            // `must_scan` admission: an eligible position that holds real
            // content (under `test_code`, selected code), whether or not the
            // pattern then matches.
            if !eligible[ri]
                && first_precondition[ri].is_none_or(|first| i >= first)
                && holds_content(is_rust(rel), raw, offset, regions.as_deref(), hidden)
            {
                eligible[ri] = true;
            }
            // Scope the pattern to a lexical region and to the rule's
            // `test_code` when asked; markers, `except`, and the reported
            // line all stay the physical line.
            let hay = haystack(rule, raw, offset, regions.as_deref(), hidden);
            if !rule.pattern.is_match(&hay) {
                continue;
            }
            // Position-sensitive precondition: the import must appear at or
            // above this match line, not merely somewhere in the file.
            if !precondition_above_ok(rule, &lines, i) {
                continue;
            }
            if marker_suppresses(rule, &lines, i, i) {
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
        let stmts = lex::use_statements(content);
        for (ri, rule) in rules.iter().enumerate() {
            if rule.join_wrapped_use && file_ok[ri] {
                eligible[ri] = stmts.iter().any(|st| {
                    !skip_after[ri].get(st.start_line).copied().unwrap_or(false)
                        && first_precondition[ri].is_none_or(|first| st.start_line >= first)
                });
            }
        }
        scan_use_statements(rel, &stmts, &lines, rules, &file_ok, &skip_after, out);
    }
    (0..rules.len())
        .filter(|ri| file_ok[*ri] && eligible[*ri])
        .collect()
}

/// Whether a scoped rule must pass over this line entirely, because none of
/// it is in the rule's `test_code` scope. Matching its blanked copy could
/// still fire a whitespace-shaped pattern, so such a line is not a position
/// at all.
/// - A line with content is out of scope when every non-whitespace byte is
///   hidden (syn spans leave indentation outside a masked statement).
/// - A blank line is out of scope when it lies inside a hidden span: all of
///   its bytes and its line terminator are hidden. A blank line between
///   in-scope items stays a position.
fn wholly_hidden(hidden: Hidden<'_>, raw: &str, offset: usize) -> bool {
    let t = match hidden {
        Hidden::Nothing => return false,
        Hidden::All => return true,
        Hidden::Bytes(t) => t,
    };
    let Some(span) = t.get(offset..offset + raw.len()) else {
        return false;
    };
    let mut content = raw
        .bytes()
        .zip(span)
        .filter(|(b, _)| !b.is_ascii_whitespace())
        .peekable();
    if content.peek().is_some() {
        return content.all(|(_, masked)| *masked);
    }
    // Blank: the terminator is the byte after the line (absent at the end of
    // a file without a trailing newline, which then judges by the line alone).
    let end = (offset + raw.len() + 1).min(t.len());
    t.get(offset..end)
        .is_some_and(|s| !s.is_empty() && s.iter().all(|m| *m))
}

/// The first line satisfying a position-sensitive `only_if_file_matches`
/// (`Some(usize::MAX)` when none does), or `None` when the rule has no such
/// precondition. A line at or below it passes `precondition_above_ok`.
fn first_precondition_line(rule: &Compiled, lines: &[&str]) -> Option<usize> {
    if !rule.only_if_file_matches_above {
        return None;
    }
    let re = rule.only_if_file_matches.as_ref()?;
    Some(
        lines
            .iter()
            .position(|l| re.is_match(l))
            .unwrap_or(usize::MAX),
    )
}

/// Whether a physical line holds something a rule really scans: any
/// non-blank text, or under a `test_code` scope in a `.rs` file a
/// non-whitespace code byte in scope. A comment beside an in-scope item is
/// not code. Other files are not Rust, so their lexical regions mean nothing;
/// a `skip` rule scans them whole and an `only` rule not at all.
fn holds_content(
    rust: bool,
    raw: &str,
    offset: usize,
    regions: Option<&[lex::Region]>,
    hidden: Hidden<'_>,
) -> bool {
    match hidden {
        Hidden::All => false,
        Hidden::Bytes(_) if rust => raw.bytes().enumerate().any(|(j, b)| {
            let at = offset + j;
            !b.is_ascii_whitespace()
                && regions.and_then(|r| r.get(at)).copied() == Some(lex::Region::Code)
                && !hidden.hides(at)
        }),
        Hidden::Nothing | Hidden::Bytes(_) => !raw.trim().is_empty(),
    }
}

/// The whole-`use`-statement pass of `scan_file`: reconstruct each wrapped
/// import onto one line (via the lexer) and match every `join_wrapped_use` rule
/// against the joined text, applying the same predicate/gate ladder as the
/// per-line pass but anchored at the statement's physical span.
fn scan_use_statements(
    rel: &Path,
    stmts: &[lex::UseStmt],
    lines: &[&str],
    rules: &[&Compiled],
    file_ok: &[bool],
    skip_after: &[Vec<bool>],
    out: &mut Vec<TextlintViolation>,
) {
    for (ri, rule) in rules.iter().enumerate() {
        if !rule.join_wrapped_use || !file_ok[ri] {
            continue;
        }
        for st in stmts {
            // The same precomputed boundary mask drives physical lines and
            // joined statements: the boundary itself is checked and everything
            // after it is skipped.
            if skip_after[ri].get(st.start_line).copied().unwrap_or(false) {
                continue;
            }
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
            if marker_suppresses(rule, lines, st.start_line, st.end_line) {
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

/// A per-line mask for `skip_after`. The boundary line itself is not skipped;
/// all following lines are. Joined statements index the same mask at their
/// first physical line, so the two passes share one boundary rule.
fn skip_after_mask(rule: &Compiled, lines: &[&str]) -> Vec<bool> {
    let Some(re) = &rule.skip_after else {
        return vec![false; lines.len()];
    };
    let mut boundary_seen = false;
    lines
        .iter()
        .map(|line| {
            let skip = boundary_seen;
            boundary_seen |= re.is_match(line);
            skip
        })
        .collect()
}

/// Whether `allow_marker` suppresses a physical line or joined statement.
/// The span is inclusive; the configured preceding window is anchored at its
/// first line, and the marker may also appear anywhere inside the span.
fn marker_suppresses(rule: &Compiled, lines: &[&str], start: usize, end: usize) -> bool {
    let Some(marker) = rule.allow_marker.as_deref() else {
        return false;
    };
    let start = start.saturating_sub(rule.allow_marker_above);
    lines
        .get(start..=end.min(lines.len().saturating_sub(1)))
        .is_some_and(|window| window.iter().any(|l| l.contains(marker)))
}

/// Whether any configured context-window gate suppresses this match. The gates
/// share one test - the match is suppressed iff the gate's pattern is
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
    let above = |w: &Option<Window>| {
        w.as_ref()
            .is_some_and(|w| window_hit(w, lines, above_anchor, true))
    };
    let below = |w: &Option<Window>| {
        w.as_ref()
            .is_some_and(|w| window_hit(w, lines, below_anchor, false))
    };
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
        // Saturating throughout: `w.lines` is unbounded from config, so a
        // pathological value must not overflow (debug panic / release wrap into
        // a slice-panic). Mirrors the above-branch's `saturating_sub`.
        let start = anchor.saturating_add(1).min(lines.len());
        let end = anchor
            .saturating_add(1)
            .saturating_add(w.lines)
            .min(lines.len());
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

/// The section name inside a TOML section header (`[deps]` -> `deps`,
/// `[[bin]]` -> `bin`), or `None` for a non-header line.
fn toml_section(trimmed: &str) -> Option<String> {
    if !trimmed.starts_with('[') {
        return None;
    }
    // Strip a trailing `# comment` (and surrounding whitespace) so a legal
    // `[section] # note` header is recognized. Cut at the first `#` (a `#` in a
    // header name is not valid TOML), then the closing `]` in what remains ends
    // the header; anything after it is ignored.
    let before_comment = trimmed.split('#').next().unwrap_or(trimmed);
    let end = before_comment.rfind(']')?;
    // A real header has nothing but optional whitespace after the closing `]`
    // (the comment was already stripped above). Reject lines that merely start
    // with `[` and contain a `]`: `[foo] bar`, a markdown link `[label]: url`,
    // or a slice-pattern arm `[a, b] => ...` are not TOML section headers.
    if !before_comment[end + 1..].trim().is_empty() {
        return None;
    }
    let head = &before_comment[..=end];
    let inner = head.trim_start_matches('[').trim_end_matches(']').trim();
    if inner.is_empty() {
        return None;
    }
    Some(inner.to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn no_rules_scans_nothing_and_says_so() {
        // The early return must not report a file count it never earned - the
        // green line's whole value is that the number is falsifiable.
        let root = crate::scope::test_git_tree("textlint", "no_rules");
        let tree = crate::scope::TreeSnapshot::load(&root).unwrap();
        let scan = scan(&tree, &[]).unwrap();
        assert!(scan.violations.is_empty());
        assert_eq!(scan.files, 0);
    }

    #[test]
    fn scan_walks_git_files_and_applies_rule_globs() {
        let root = crate::scope::test_git_tree("textlint", "scan_tree");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "unsafe { work(); }\n").unwrap();

        let tree = crate::scope::TreeSnapshot::load(&root).unwrap();
        let result = scan(&tree, &[rule("unsafe")]).unwrap();
        assert_eq!(result.files, 1);
        assert_eq!(result.violations.len(), 1);
        assert_eq!(result.violations[0].file, PathBuf::from("src/lib.rs"));
    }

    /// Scan `src` with one rule under a test-code mask; return the hit lines
    /// and whether the rule admitted the file (`must_scan`'s notion).
    fn run_masked(
        rule: TextlintRule,
        src: &str,
        mask: &crate::test_code::FileMask,
    ) -> (Vec<usize>, bool) {
        let class = FileClass {
            mask: Some(mask),
            tests: None,
        };
        run_classed(rule, "t.rs", src, class)
    }

    fn run_classed(
        rule: TextlintRule,
        rel: &str,
        src: &str,
        class: FileClass<'_>,
    ) -> (Vec<usize>, bool) {
        let compiled = compile(&[rule]).unwrap();
        let refs: Vec<&Compiled> = compiled.iter().collect();
        let mut out = Vec::new();
        let admitted = scan_file(Path::new(rel), src, &refs, class, &mut out);
        (out.iter().map(|v| v.line).collect(), admitted == vec![0])
    }

    /// Scan `src` with one rule given a test selection of the spans of
    /// `needles`.
    fn run_selected(rule: TextlintRule, src: &str, needles: &[&str]) -> (Vec<usize>, bool) {
        let spans: Vec<std::ops::Range<usize>> = needles
            .iter()
            .map(|n| {
                let start = src.find(n).unwrap();
                start..start + n.len()
            })
            .collect();
        let class = FileClass {
            mask: None,
            tests: Some(&spans),
        };
        run_classed(rule, "t.rs", src, class)
    }

    #[test]
    fn only_test_matches_inside_the_selection_alone() {
        let src = "fn p() { sleep(); }\n#[cfg(test)]\nmod tests {\n    fn t() { sleep(); }\n}\n";
        let mut r = rule("sleep");
        r.test_code = TestCode::Only;
        r.region = Some("code".into());
        let test_mod = "#[cfg(test)]\nmod tests {\n    fn t() { sleep(); }\n}";
        assert_eq!(run_selected(r.clone(), src, &[test_mod]), (vec![4], true));
        // Nothing selected: nothing matches and the file is not admitted.
        assert_eq!(run_selected(r.clone(), src, &[]), (vec![], false));
        // A whitespace-shaped pattern never fires on hidden production lines.
        let mut blank = rule(r"^\s*$");
        blank.test_code = TestCode::Only;
        assert_eq!(run_selected(blank, "fn p() {}\n\n", &[]).0, Vec::<usize>::new());
    }

    #[test]
    fn only_test_scans_no_file_without_a_selection() {
        // Not Rust, or not classified: an `only` rule has nothing in scope,
        // while a `skip` rule scans the file whole.
        let mut only = rule("sleep");
        only.test_code = TestCode::Only;
        assert_eq!(
            run_classed(only, "notes.md", "sleep\n", FileClass::default()),
            (vec![], false)
        );
        let mut skip = rule("sleep");
        skip.test_code = TestCode::Skip;
        assert_eq!(
            run_classed(skip, "notes.md", "sleep\n", FileClass::default()),
            (vec![1], true)
        );
    }

    fn masked_span(src: &str, needle: &str) -> crate::test_code::FileMask {
        let start = src.find(needle).unwrap();
        let span = start..start + needle.len();
        crate::test_code::FileMask::Production(std::iter::once(span).collect())
    }

    #[test]
    fn skip_test_blanks_test_bytes_and_composes_with_region() {
        let src = "fn p() { forbidden(); } fn q() { \"forbidden\"; }\n#[cfg(test)]\nfn t() { forbidden(); }\n";
        let mask = masked_span(src, "#[cfg(test)]\nfn t() { forbidden(); }");
        let mut r = rule("forbidden");
        r.test_code = TestCode::Skip;
        r.region = Some("code".into());
        assert_eq!(run_masked(r.clone(), src, &mask), (vec![1], true));
        // Without skip_test the mask is ignored.
        r.test_code = TestCode::Include;
        assert_eq!(run_masked(r, src, &mask).0, vec![1, 3]);
    }

    #[test]
    fn a_wholly_test_file_is_not_admitted() {
        let mut r = rule("forbidden");
        r.test_code = TestCode::Skip;
        let src = "fn t() { forbidden(); }\n";
        assert_eq!(
            run_masked(r, src, &crate::test_code::FileMask::Test),
            (vec![], false)
        );
    }

    #[test]
    fn blanked_test_lines_never_reach_the_pattern() {
        let mut r = rule(r"^\s+$");
        r.test_code = TestCode::Skip;
        let src = "fn t() { forbidden(); }\n";
        assert_eq!(
            run_masked(r.clone(), src, &crate::test_code::FileMask::Test).0,
            Vec::<usize>::new()
        );
        let mixed = "fn p() {}\n#[cfg(test)]\nfn t() {}\n";
        let mask = masked_span(mixed, "#[cfg(test)]\nfn t() {}");
        assert_eq!(run_masked(r.clone(), mixed, &mask).0, Vec::<usize>::new());
        // An indented masked statement leaves its indentation unmasked.
        let indented = "fn p() {\n    #[cfg(test)] let fixture = 1;\n}\n";
        let mask = masked_span(indented, "#[cfg(test)] let fixture = 1;");
        assert_eq!(
            run_masked(r.clone(), indented, &mask).0,
            Vec::<usize>::new()
        );
        // Blank lines inside a masked module are test code; blank lines
        // between production items are not.
        let blanks = "fn p() {}\n    \n#[cfg(test)]\nmod tests {\n    \n\n}\n";
        let mask = masked_span(blanks, "#[cfg(test)]\nmod tests {\n    \n\n}");
        assert_eq!(run_masked(r.clone(), blanks, &mask).0, vec![2]);
        let mut empty = rule("^$");
        empty.test_code = TestCode::Skip;
        assert_eq!(
            run_masked(empty.clone(), blanks, &mask).0,
            Vec::<usize>::new()
        );
        assert_eq!(
            run_masked(empty, "a\n\nb\n", &crate::test_code::FileMask::Test).0,
            Vec::<usize>::new()
        );
    }

    #[test]
    fn a_comment_beside_masked_code_does_not_admit_the_file() {
        let src = "// explanation\n#[cfg(test)]\nmod fixtures {}\n";
        let mask = masked_span(src, "#[cfg(test)]\nmod fixtures {}");
        let mut r = rule("never");
        r.test_code = TestCode::Skip;
        assert!(!run_masked(r, src, &mask).1);
    }

    #[test]
    fn an_unsatisfied_positional_precondition_does_not_admit_the_file() {
        let mut r = rule("never");
        r.only_if_file_matches = Some("^use x".into());
        r.only_if_file_matches_above = true;
        let none = crate::test_code::FileMask::Production(Vec::new());
        assert!(!run_masked(r.clone(), "fn p() {}\n", &none).1);
        assert!(run_masked(r, "use x;\nfn p() {}\n", &none).1);
    }

    #[test]
    fn skip_test_with_join_wrapped_use_is_a_config_error() {
        let mut r = rule("use a");
        r.test_code = TestCode::Skip;
        r.join_wrapped_use = true;
        assert!(compile(&[r]).is_err());
    }

    #[test]
    fn must_scan_reports_each_path_the_rule_did_not_scan() {
        let mut r = rule("never");
        r.paths = vec!["src/**".into()];
        r.must_scan = vec!["src/a.rs".into(), "src/b.rs".into(), "docs/c.md".into()];
        let compiled = compile(&[r]).unwrap();
        let files = vec![
            PathBuf::from("src/a.rs"),
            PathBuf::from("src/b.rs"),
            PathBuf::from("docs/c.md"),
        ];
        let admitted = vec![HashSet::from([PathBuf::from("src/a.rs")])];
        let missing: Vec<PathBuf> = must_scan_violations(&compiled, &admitted, &files)
            .into_iter()
            .map(|v| v.file)
            .collect();
        assert_eq!(
            missing,
            vec![PathBuf::from("src/b.rs"), PathBuf::from("docs/c.md")]
        );
    }

    /// Compile one rule and scan a source string; return (line, rule) hits.
    fn run(rule: TextlintRule, src: &str) -> Vec<(usize, String)> {
        let compiled = compile(&[rule]).unwrap();
        let refs: Vec<&Compiled> = compiled.iter().collect();
        let mut out = Vec::new();
        scan_file(Path::new("t.rs"), src, &refs, FileClass::default(), &mut out);
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
            test_code: TestCode::Include,
            must_scan: Vec::new(),
            compiled: None,
        }
    }

    #[test]
    fn displayed_content_marks_truncation() {
        let short = "x".repeat(DISPLAY_CAP - 1);
        assert_eq!(cap(&short), short);

        let long = "x".repeat(DISPLAY_CAP + 1);
        let displayed = cap(&long);
        assert_eq!(displayed.chars().count(), DISPLAY_CAP);
        assert!(displayed.ends_with('…'));
    }

    fn win(lines: usize, pattern: &str) -> crate::config::ContextWindow {
        crate::config::ContextWindow {
            lines,
            pattern: pattern.into(),
        }
    }

    #[test]
    fn forbids_a_pattern() {
        assert_eq!(
            run(rule("todo!"), "let x = 1;\ntodo!();\n"),
            vec![(2, "r".into())]
        );
    }

    #[test]
    fn leading_bom_does_not_mis_anchor_line_one() {
        // A file starting with U+FEFF must not shift line 1's first character:
        // a `^use ` rule fires on line 1. Before the BOM strip it silently missed.
        assert_eq!(
            run(rule("^use "), "\u{FEFF}use foo;\n"),
            vec![(1, "r".into())]
        );
    }

    #[test]
    fn non_utf8_text_is_scanned_and_binary_is_skipped() {
        // Latin-1 text: decoded lossily and scanned, not skipped as a pass.
        let latin1 = b"caf\xE9 todo!()\n";
        let text = text_of(latin1).unwrap();
        assert_eq!(run(rule("todo!"), &text), vec![(1, "r".into())]);
        // A NUL in the probe window marks the file binary.
        assert!(text_of(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").is_none());
        assert!(text_of(b"").is_some());
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
    fn window_hit_below_does_not_overflow_on_huge_lines() {
        // A pathological `w.lines` must saturate, not overflow (debug panic) or
        // wrap into an out-of-bounds slice (release panic). Anchor is line 0 of
        // a 3-line file, so the below window would run off the end.
        let w = Window {
            lines: usize::MAX,
            re: Regex::new("x").unwrap(),
        };
        let lines = ["a", "b", "c"];
        // No match below -> false, but crucially: no panic.
        assert!(!window_hit(&w, &lines, 0, false));
        let hit = Window {
            lines: usize::MAX,
            re: Regex::new("c").unwrap(),
        };
        assert!(window_hit(&hit, &lines, 0, false));
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
    fn in_toml_section_recognizes_header_with_trailing_comment() {
        let mut r = rule("^bar");
        r.in_toml_section = Some("dev-dependencies".into());
        let src = "[dependencies]\nbar = \"1\"\n\n[dev-dependencies] # workspace test deps\nbar = \"1\"\n";
        // The `bar` under the commented [dev-dependencies] header (line 5) fires;
        // the one under [dependencies] (line 2) is out of section.
        assert_eq!(run(r, src), vec![(5, "r".into())]);
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
        assert!(!applies(
            c,
            Path::new("crates/core/docs/anyhow_style_guide.rs")
        ));
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
        let src =
            "let s = TcpStream::connect(addr);\nmod tests {\n    use tokio::net::TcpListener;\n}\n";
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
        assert_eq!(
            toml_section("[dependencies]").as_deref(),
            Some("dependencies")
        );
        assert_eq!(toml_section("[[bin]]").as_deref(), Some("bin"));
        assert_eq!(toml_section("tokio = \"1\""), None);
        // A trailing `# comment` after the closing `]` is ignored.
        assert_eq!(
            toml_section("[dev-dependencies] # workspace test deps").as_deref(),
            Some("dev-dependencies")
        );
        // A line that merely starts with `[` and contains a `]` is NOT a header:
        // arbitrary trailing text, a markdown reference link, or a slice-pattern
        // match arm must leave the `in_toml_section` latch untouched.
        assert_eq!(toml_section("[foo] bar"), None);
        assert_eq!(toml_section("[label]: http://example.com"), None);
        assert_eq!(toml_section("[a, b] => process(),"), None);
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
    fn any_matching_context_window_suppresses() {
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
    fn join_wrapped_use_excepting_use_lines_is_a_config_error() {
        // The `no-fq-nautilus-types` footgun: join_wrapped_use scans only `use`
        // statements, so a broad `^\s*use\s` except suppresses every one of them
        // - the rule becomes a silent no-op. Reject it at compile time.
        let mut r = rule("nautilus_[a-z_]+::.*::[A-Z]");
        r.join_wrapped_use = true;
        r.except = vec![r"^\s*(pub(\([^)]*\))?\s+)?use\s".into()];
        assert!(compile(&[r]).is_err());
    }

    #[test]
    fn join_wrapped_use_with_specific_except_is_allowed() {
        // A genuine import rule may except one *specific* import without tripping
        // the guard - the probe `use a::b;` does not match a narrow pattern.
        let mut r = rule("use tracing::.*warn");
        r.join_wrapped_use = true;
        r.except = vec!["use tracing::info".into()];
        assert!(compile(&[r]).is_ok());
    }

    #[test]
    fn join_wrapped_use_skip_after_exempts_past_boundary() {
        // The join pass must honour `skip_after`: the wrapped import above the
        // `#[cfg(test)]` boundary fires (line 1); the one inside the test module
        // is past the boundary and exempt. Before the fix the latch never armed
        // for a join rule, so both fired.
        let mut r = rule("use tracing::.*warn");
        r.join_wrapped_use = true;
        r.skip_after = Some("^#\\[cfg\\(test\\)\\]".into());
        let src = "\
use tracing::{
    warn,
};
#[cfg(test)]
mod tests {
    use tracing::{
        warn,
    };
}
";
        assert_eq!(run(r, src), vec![(1, "r".into())]);
    }

    #[test]
    fn join_wrapped_use_with_region_is_a_config_error() {
        // `region` scopes the per-line pass; the join pass matches joined `use`
        // statements (always code), so the predicate can only no-op the rule.
        let mut r = rule("use tracing::.*warn");
        r.join_wrapped_use = true;
        r.region = Some("code".into());
        assert!(compile(&[r]).is_err());
    }

    #[test]
    fn join_wrapped_use_with_table_row_only_is_a_config_error() {
        // A `use` statement is never a markdown table row.
        let mut r = rule("use tracing::.*warn");
        r.join_wrapped_use = true;
        r.table_row_only = true;
        assert!(compile(&[r]).is_err());
    }

    #[test]
    fn join_wrapped_use_with_in_toml_section_is_a_config_error() {
        // A `use` statement is never inside a TOML `[section]`.
        let mut r = rule("use tracing::.*warn");
        r.join_wrapped_use = true;
        r.in_toml_section = Some("dependencies".into());
        assert!(compile(&[r]).is_err());
    }

    #[test]
    fn except_use_lines_without_join_is_fine() {
        // Without join_wrapped_use the rule scans every physical line, so
        // excepting `use` lines is the ordinary, valid way to skip imports.
        let mut r = rule("nautilus_[a-z_]+::.*::[A-Z]");
        r.except = vec![r"^\s*use\s".into()];
        assert!(compile(&[r]).is_ok());
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
