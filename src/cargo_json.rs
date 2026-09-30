//! Parser for cargo's `--message-format=json` output.
//!
//! Turns cargo/clippy/rustc JSON diagnostics into structured [`DiagnosticEvent`]
//! values that the `check` text renderer formats. This is how brokkr *reads*
//! cargo - there is no machine-readable output mode of brokkr's own.

// --- Diagnostic model ---

/// A parsed compiler/clippy diagnostic. Field set mirrors the parts of cargo's
/// JSON the text renderer consumes.
#[derive(Clone)]
pub struct DiagnosticEvent {
    pub level: String,
    pub code: Option<String>,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
    /// Inline label on the primary span, e.g. "expected `i32`, found `&str`".
    pub primary_label: Option<String>,
    pub children: Vec<ChildDiagnostic>,
    /// The package cargo compiled when rustc emitted this: the
    /// `compiler-message`'s top-level `package_id`, in the same form
    /// `cargo metadata` lists `workspace_members`.
    pub package_id: Option<String>,
}

#[derive(Clone)]
pub struct ChildDiagnostic {
    pub message: String,
}

// --- Parsing ---

/// Parse cargo `--message-format=json` stdout into diagnostics.
///
/// Each line is a JSON object from cargo. Only lines with
/// `"reason": "compiler-message"` are extracted; everything else is skipped.
///
#[allow(clippy::too_many_lines)] // JSON walk - splitting just shuffles match arms
pub fn parse_cargo_diagnostics(stdout: &str) -> Vec<DiagnosticEvent> {
    let mut events = Vec::new();

    for line in stdout.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("reason").and_then(|v| v.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = val.get("message") else {
            continue;
        };

        let level = msg
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Only the levels a finding is reported at. rustc also emits top-level
        // `failure-note` messages ("For more information about this error,
        // try `rustc --explain E0027`") and could emit bare `note`/`help`;
        // `check` counts every diagnostic it keeps as an error, so those would
        // surface as phantom errors.
        if !matches!(level.as_str(), "error" | "warning" | "error: internal compiler error") {
            continue;
        }

        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string);

        let message = msg
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Whether this compiler-message carries any spans. rustc's summary
        // and meta-noise messages ("N warnings emitted", "generated N
        // warnings", "aborting due to N previous errors") always arrive with
        // an empty `spans` array and a null `code`.
        let spans_empty = msg
            .get("spans")
            .and_then(|s| s.as_array())
            .is_none_or(Vec::is_empty);

        // Skip rustc/cargo summary + meta-noise diagnostics. The old text
        // scraper filtered these (`cargo_filter::is_meta_noise`); the JSON
        // path must do the same or the phantom, location-less messages inflate
        // the lint count (3 lints reported as "4 errors"). A real lint/error
        // always carries a primary span or a diagnostic code, so gating on
        // `spans_empty && code.is_none()` cannot drop a genuine finding.
        if is_summary_noise(&message, spans_empty, code.is_some()) {
            continue;
        }

        // Primary span for file/line/column.
        let primary_span = msg
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|spans| spans.iter().find(|s| s.get("is_primary") == Some(&serde_json::Value::Bool(true))));

        let (file, line_start, col_start, primary_label) = match primary_span {
            Some(span) => (
                span.get("file_name").and_then(|v| v.as_str()).map(std::string::ToString::to_string),
                span.get("line_start").and_then(serde_json::Value::as_u64),
                span.get("column_start").and_then(serde_json::Value::as_u64),
                span.get("label")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(std::string::ToString::to_string),
            ),
            None => (None, None, None, None),
        };

        // Child diagnostics - only the message is consumed (the text renderer
        // scrapes the "expected/found" detail line from it).
        let children = msg
            .get("children")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|child| {
                        let child_msg = child.get("message")?.as_str()?.to_string();
                        if child_msg.is_empty() {
                            return None;
                        }
                        Some(ChildDiagnostic { message: child_msg })
                    })
                    .collect()
            })
            .unwrap_or_default();

        events.push(DiagnosticEvent {
            level,
            code,
            message,
            file,
            line: line_start,
            column: col_start,
            primary_label,
            children,
            package_id: val
                .get("package_id")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
        });
    }

    events
}

/// Cargo's own manifest lints (`[lints.cargo]`: `unused_dependencies`,
/// `unused_workspace_dependencies`, ...) among a run's stderr `blocks`
/// (split by `script_check::rustc_blocks`), each with its block index - so a
/// renderer can tell which blocks a diagnostic list already carries.
///
/// Cargo renders these as text on stderr even under `--message-format=json` -
/// they never reach the JSON stream [`parse_cargo_diagnostics`] reads, which is
/// why a green `check` used to be silent about them. A manifest lint is a
/// rustc-shaped block whose `-->` arrow points at a `Cargo.toml` (rustc's own
/// diagnostics travel as JSON on stdout, so its arrows never appear here).
///
/// A block is a manifest lint only with a lint identity, not by its arrow
/// alone: cargo's ordinary manifest errors (a bad TOML string, an invalid
/// version) carry the same `--> Cargo.toml:L:C` arrow, and swallowing one as a
/// one-line diagnostic would hide the excerpt that explains it. The identity
/// is the lint cargo names (`` `cargo::unused_dependencies` is set to `warn` ``),
/// which it does only on the first hit per lint per manifest. Any other hit
/// takes the code of a named block in the *same* manifest with the same
/// message shape (the message with its backticked spans emptied), and only
/// when that shape names exactly one lint there; an unnamed block with no such
/// sibling, or an ambiguous one, stays in the stream.
///
/// Every event has `package_id: None` - stderr text names no package. The
/// `check` clippy phase attributes each one by its manifest path
/// (`attribute_manifest_lints`), since cargo's parsing lints also cover
/// path-sourced packages that are not members. The path is as cargo printed
/// it: relative to the workspace root, whatever directory cargo ran in.
pub fn manifest_lints_by_block(blocks: &[crate::script_check::Block]) -> Vec<(usize, DiagnosticEvent)> {
    struct Candidate<'a> {
        index: usize,
        level: &'static str,
        message: &'a str,
        file: &'a str,
        line: Option<u64>,
        column: Option<u64>,
        shape: String,
        code: Option<String>,
    }
    let candidates: Vec<Candidate<'_>> = blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            let (level, message) = manifest_lint_header(block)?;
            let (file, line, column) = manifest_lint_location(&block.text)?;
            Some(Candidate {
                index,
                level,
                message,
                file,
                line,
                column,
                shape: message_shape(message),
                code: block.text.lines().find_map(named_cargo_lint),
            })
        })
        .collect();

    // Every lint each (manifest, shape) pair names, in either direction of the
    // stream: a named hit may follow the unnamed one it vouches for.
    let mut named: std::collections::HashMap<(&str, &str), std::collections::BTreeSet<&str>> =
        std::collections::HashMap::new();
    for c in &candidates {
        if let Some(code) = &c.code {
            named.entry((c.file, c.shape.as_str())).or_default().insert(code.as_str());
        }
    }

    candidates
        .iter()
        .filter_map(|c| {
            let code = match &c.code {
                Some(code) => code.clone(),
                None => {
                    let codes = named.get(&(c.file, c.shape.as_str()))?;
                    if codes.len() != 1 {
                        return None;
                    }
                    (*codes.first()?).to_owned()
                }
            };
            Some((
                c.index,
                DiagnosticEvent {
                    level: c.level.to_owned(),
                    code: Some(code),
                    message: c.message.to_owned(),
                    file: Some(c.file.to_owned()),
                    line: c.line,
                    column: c.column,
                    primary_label: None,
                    children: Vec::new(),
                    package_id: None,
                },
            ))
        })
        .collect()
}

/// The level and message of a block's header line (`warning: unused
/// dependency `x``, `error[code]: ...`).
fn manifest_lint_header(block: &crate::script_check::Block) -> Option<(&'static str, &str)> {
    use crate::script_check::Level;
    let level = match block.level {
        Level::Error => "error",
        Level::Warning => "warning",
        Level::Other => return None,
    };
    let header = block.text.lines().next()?;
    let (_, message) = header.split_once(": ")?;
    Some((level, message))
}

/// The location of the block's first `-->` arrow, when it names a
/// `Cargo.toml`: `path:line:col`, or a bare `path` - cargo falls back to the
/// manifest alone when it has no span for the offending entry.
fn manifest_lint_location(text: &str) -> Option<(&str, Option<u64>, Option<u64>)> {
    let arrow = text.lines().find_map(|l| l.trim_start().strip_prefix("--> "))?.trim_end();
    let mut parts = arrow.rsplitn(3, ':');
    let spanned = match (parts.next(), parts.next(), parts.next()) {
        (Some(col), Some(line), Some(file)) => match (line.parse().ok(), col.parse().ok()) {
            (Some(line), Some(col)) => Some((file, Some(line), Some(col))),
            _ => None,
        },
        _ => None,
    };
    let (file, line, column) = spanned.unwrap_or((arrow, None, None));
    (std::path::Path::new(file).file_name()? == "Cargo.toml").then_some((file, line, column))
}

/// The lint a `= note: `cargo::NAME` is set to ...` line names.
fn named_cargo_lint(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("= note: `")?;
    let (name, tail) = rest.split_once('`')?;
    (name.starts_with("cargo::") && tail.starts_with(" is set to")).then(|| name.to_owned())
}

/// A message with every backticked span emptied: `unused dependency `toml``
/// and `unused dependency `anyhow`` share the shape `unused dependency ```.
fn message_shape(message: &str) -> String {
    message
        .split('`')
        .enumerate()
        .filter(|(i, _)| i % 2 == 0)
        .map(|(_, s)| s)
        .collect::<Vec<_>>()
        .join("``")
}

/// Classify a compiler-message as a rustc/cargo summary or meta-noise line
/// that should not become a real diagnostic.
///
/// Ported from the text-scraper's `cargo_filter::is_meta_noise`. These
/// messages arrive with no primary span and no diagnostic `code`, so the
/// caller gates on `spans_empty && !has_code` before matching message shape -
/// a genuine lint/error always carries a span or a code and is never dropped.
/// Matching is on message *shape* (substring patterns), never exact strings,
/// so `1 warning emitted` / `12 warnings emitted` / localized crate names all
/// fall through.
fn is_summary_noise(message: &str, spans_empty: bool, has_code: bool) -> bool {
    if !spans_empty || has_code {
        return false;
    }
    // "N warning(s) emitted"
    if message.contains("emitted") && message.contains("warning") {
        return true;
    }
    // "`crate` (lib) generated N warning(s)"
    if message.contains("generated") && message.contains("warning") {
        return true;
    }
    // "aborting due to N previous error(s)"
    if message.contains("aborting due to") {
        return true;
    }
    // "could not compile `crate` ..."
    if message.contains("could not compile") {
        return true;
    }
    // "build failed, waiting for other jobs to finish..."
    if message.contains("build failed") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )]
    use super::*;

    /// [`manifest_lints_by_block`] over a whole stderr, block indices dropped.
    fn parse_manifest_lints(stderr: &str) -> Vec<DiagnosticEvent> {
        let blocks = crate::script_check::rustc_blocks(stderr);
        manifest_lints_by_block(&blocks).into_iter().map(|(_, e)| e).collect()
    }

    fn sample_compiler_message(level: &str, code: &str, message: &str, file: &str, line: u64) -> String {
        format!(
            r#"{{"reason":"compiler-message","message":{{"rendered":"rendered text","level":"{level}","code":{{"code":"{code}"}},"message":"{message}","spans":[{{"file_name":"{file}","line_start":{line},"column_start":5,"line_end":{line},"column_end":10,"is_primary":true}}],"children":[{{"level":"help","message":"try this","spans":[]}}]}}}}"#
        )
    }

    #[test]
    fn parse_captures_primary_span_label() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/foo.rs","line_start":20,"column_start":5,"line_end":20,"column_end":10,"is_primary":true,"label":"expected `i32`, found `&str`"}],"children":[],"rendered":"rendered"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].primary_label.as_deref(), Some("expected `i32`, found `&str`"));
    }

    #[test]
    fn parse_omits_empty_primary_label() {
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":{"code":"unused_variables"},"message":"unused","spans":[{"file_name":"src/a.rs","line_start":1,"column_start":1,"line_end":1,"column_end":2,"is_primary":true,"label":""}],"children":[],"rendered":"rendered"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events[0].primary_label.is_none());
    }

    /// Seen on a failed clippy run: rustc's spanless `--explain` pointer arrives
    /// as its own compiler-message and was reported as an error.
    #[test]
    fn a_failure_note_is_not_a_diagnostic() {
        let input = r#"{"reason":"compiler-message","message":{"level":"failure-note","code":null,"message":"For more information about this error, try `rustc --explain E0027`.","spans":[],"children":[],"rendered":"For more information about this error, try `rustc --explain E0027`.\n"}}"#;
        assert!(parse_cargo_diagnostics(input).is_empty());
    }

    #[test]
    fn parse_single_error() {
        let input = sample_compiler_message("error", "E0425", "cannot find value", "src/main.rs", 10);
        let events = parse_cargo_diagnostics(&input);
        assert_eq!(events.len(), 1);
        let d = &events[0];
        assert_eq!(d.level, "error");
        assert_eq!(d.code.as_deref(), Some("E0425"));
        assert_eq!(d.message, "cannot find value");
        assert_eq!(d.file.as_deref(), Some("src/main.rs"));
        assert_eq!(d.line, Some(10));
        assert_eq!(d.column, Some(5));
        assert_eq!(d.children.len(), 1);
        assert_eq!(d.children[0].message, "try this");
    }

    #[test]
    fn skips_non_compiler_messages() {
        let input = r#"{"reason":"compiler-artifact","target":{"name":"foo"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events.is_empty());
    }

    #[test]
    fn skips_aborting_errors() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":null,"message":"aborting due to 3 previous errors","spans":[],"children":[],"rendered":"error: aborting"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events.is_empty());
    }

    #[test]
    fn real_clippy_warning_produces_one_diagnostic() {
        // A genuine lint: has both spans and a code -> must survive.
        let input = sample_compiler_message("warning", "clippy::needless_return", "unneeded return statement", "src/a.rs", 7);
        let events = parse_cargo_diagnostics(&input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code.as_deref(), Some("clippy::needless_return"));
        assert_eq!(events[0].file.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn skips_warnings_emitted_summary() {
        // rustc's "N warnings emitted": empty spans, null code -> zero diagnostics.
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":null,"message":"2 warnings emitted","spans":[],"children":[],"rendered":"warning: 2 warnings emitted"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events.is_empty(), "expected the summary line to be filtered, got {} event(s)", events.len());
    }

    #[test]
    fn skips_generated_warnings_summary() {
        // cargo's per-crate roll-up: "`crate` (lib) generated N warnings".
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":null,"message":"`brokkr` (lib) generated 3 warnings","spans":[],"children":[],"rendered":"warning: `brokkr` (lib) generated 3 warnings"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events.is_empty(), "expected the generated-warnings roll-up to be filtered, got {} event(s)", events.len());
    }

    #[test]
    fn multiple_diagnostics() {
        let mut input = sample_compiler_message("error", "E0308", "mismatched types", "src/a.rs", 1);
        input.push('\n');
        input.push_str(&sample_compiler_message("warning", "unused_variables", "unused var", "src/b.rs", 2));
        let events = parse_cargo_diagnostics(&input);
        assert_eq!(events.len(), 2);
    }

    /// Cargo's stderr from a real failing run: two manifest lints on one
    /// manifest (only the first names its lint), a workspace-level one, the
    /// per-manifest tallies, and a build-script failure that is not a manifest
    /// lint at all.
    const MANIFEST_STDERR: &str = "\
warning: unused workspace dependency `config`
   --> Cargo.toml:150:1
    |
150 | config = \"0.15.27\"
    | ^^^^^^
    |
    = note: `cargo::unused_workspace_dependencies` is set to `warn` by default
help: consider removing the workspace dependency `config`
warning: workspace (manifest) generated 1 warning
   Compiling yeslogic-fontconfig-sys v6.0.1
error: failed to run custom build command for `yeslogic-fontconfig-sys v6.0.1`

Caused by:
  process didn't exit successfully: `target/debug/build/x/build_script_build` (exit status: 101)
warning: unused dependency `anyhow`
  --> crates/shared/jaerlogg-test/Cargo.toml:25:1
   |
25 | anyhow = { workspace = true }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: `cargo::unused_dependencies` is set to `warn` by default
help: consider removing the dependency on `anyhow`
warning: unused dependency `toml`
  --> crates/shared/jaerlogg-test/Cargo.toml:19:1
   |
19 | toml = { workspace = true }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: consider removing the dependency on `toml`
warning: `jaerlogg-test` (manifest) generated 2 warnings
";

    #[test]
    fn manifest_lints_parse_from_stderr() {
        let events = parse_manifest_lints(MANIFEST_STDERR);
        let got: Vec<String> = events
            .iter()
            .map(|e| {
                format!(
                    "{} {} {}:{}:{} {}",
                    e.level,
                    e.code.as_deref().unwrap_or("-"),
                    e.file.as_deref().unwrap_or("-"),
                    e.line.unwrap_or(0),
                    e.column.unwrap_or(0),
                    e.message
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                "warning cargo::unused_workspace_dependencies Cargo.toml:150:1 unused workspace dependency `config`",
                "warning cargo::unused_dependencies crates/shared/jaerlogg-test/Cargo.toml:25:1 unused dependency `anyhow`",
                // No note of its own: the code comes from `anyhow`'s block.
                "warning cargo::unused_dependencies crates/shared/jaerlogg-test/Cargo.toml:19:1 unused dependency `toml`",
            ]
        );
    }

    #[test]
    fn a_non_manifest_arrow_is_not_a_manifest_lint() {
        let stderr = "warning: something\n  --> src/lib.rs:3:1\n   |\n";
        assert!(parse_manifest_lints(stderr).is_empty());
    }

    /// Cargo's ordinary manifest errors carry the same arrow, but name no
    /// lint: left in the stream, where their excerpt explains them.
    #[test]
    fn a_manifest_parse_error_is_not_a_manifest_lint() {
        let stderr = "\
error: invalid string
expected `\"`, `'`
 --> Cargo.toml:3:8
  |
3 | name = foo
  |        ^
";
        assert!(parse_manifest_lints(stderr).is_empty());
    }

    fn codes(stderr: &str) -> Vec<(String, Option<String>)> {
        parse_manifest_lints(stderr)
            .into_iter()
            .map(|e| (e.message, e.code))
            .collect()
    }

    /// The named hit may come after the unnamed one it vouches for.
    #[test]
    fn a_later_named_hit_names_an_earlier_one() {
        let stderr = "\
warning: unused dependency `toml`
  --> a/Cargo.toml:19:1
warning: unused dependency `anyhow`
  --> a/Cargo.toml:25:1
   = note: `cargo::unused_dependencies` is set to `warn` by default
";
        let unused = Some("cargo::unused_dependencies".to_owned());
        assert_eq!(
            codes(stderr),
            [
                ("unused dependency `toml`".to_owned(), unused.clone()),
                ("unused dependency `anyhow`".to_owned(), unused),
            ]
        );
    }

    /// Borrowing stays within one manifest, and a shape naming two lints
    /// there names neither.
    #[test]
    fn borrowing_is_per_manifest_and_unambiguous() {
        let stderr = "\
warning: unused dependency `anyhow`
  --> a/Cargo.toml:25:1
   = note: `cargo::unused_dependencies` is set to `warn` by default
warning: unused dependency `toml`
  --> b/Cargo.toml:19:1
warning: odd entry `x`
  --> c/Cargo.toml:1:1
   = note: `cargo::lint_one` is set to `warn` by default
warning: odd entry `y`
  --> c/Cargo.toml:2:1
   = note: `cargo::lint_two` is set to `warn` by default
warning: odd entry `z`
  --> c/Cargo.toml:3:1
";
        let got: Vec<String> = codes(stderr).into_iter().map(|(m, _)| m).collect();
        assert_eq!(got, ["unused dependency `anyhow`", "odd entry `x`", "odd entry `y`"]);
    }

    /// Cargo falls back to the manifest alone when it has no span.
    #[test]
    fn a_path_only_arrow_is_a_location() {
        let stderr = "\
warning: unused dependency `toml`
  --> a/Cargo.toml
   = note: `cargo::unused_dependencies` is set to `warn` by default
";
        let events = parse_manifest_lints(stderr);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].file.as_deref(), Some("a/Cargo.toml"));
        assert_eq!((events[0].line, events[0].column), (None, None));
    }
}
