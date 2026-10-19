//! The fixture expectation manifest and the per-variant verdict.
//!
//! ```toml
//! [files."src/submit_order_method.rs"]
//! expect = [{ lint = "clippy::disallowed_methods", path = "nautilus_trading::Strategy::submit_order" }]
//!
//! [files."src/lib.rs"]
//! clean = true
//! ```
//!
//! Matching is exact on both halves. The lint is the diagnostic's code; the
//! path must equal one backticked token of the message - never a substring,
//! because `submit_order` is a prefix of `submit_order_list` and a fixture
//! for one sits next to a fixture for the other. Reading the path out of the
//! message is a toolchain-dependent adapter, not structured identity, so a
//! diagnostic whose wording no longer carries the path fails visibly as
//! unexpected rather than matching loosely.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    files: BTreeMap<String, RawFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    expect: Vec<Expectation>,
    #[serde(default)]
    clean: bool,
}

/// One expected diagnostic: a lint and the item path its message names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Expectation {
    pub(super) lint: String,
    pub(super) path: String,
}

/// A parsed manifest: fixture-relative file (forward slashes) to its
/// expectations; an empty list is a `clean` file.
#[derive(Debug, Clone)]
pub(super) struct Manifest {
    pub(super) files: BTreeMap<String, Vec<Expectation>>,
}

impl Manifest {
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        let raw: RawManifest = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut files = BTreeMap::new();
        for (file, entry) in raw.files {
            match (entry.clean, entry.expect.is_empty()) {
                (true, false) => {
                    return Err(format!(
                        "{file}: `clean = true` and `expect` together - pick one"
                    ));
                }
                (false, true) => {
                    return Err(format!(
                        "{file}: neither `expect` nor `clean = true` - say what the file must produce"
                    ));
                }
                _ => {}
            }
            for e in &entry.expect {
                if e.lint.trim().is_empty() || e.path.trim().is_empty() {
                    return Err(format!("{file}: an expectation has a blank lint or path"));
                }
            }
            files.insert(file, entry.expect);
        }
        if files.values().all(Vec::is_empty) {
            // Every file clean proves only that nothing fired - including
            // the case where the lint never runs at all.
            return Err(
                "no file expects a diagnostic - a manifest of clean files proves nothing \
                        fires, not that anything is sealed"
                    .into(),
            );
        }
        Ok(Self { files })
    }
}

/// One fixture warning, reduced to what the verdict reads.
#[derive(Debug, Clone)]
pub(super) struct Observed {
    /// Fixture-relative file, when the primary span is in a listed file.
    pub(super) file: Option<String>,
    pub(super) lint: Option<String>,
    pub(super) message: String,
    /// `file:line` for the report.
    pub(super) at: String,
}

/// Lint spellings compare with `-` and `_` unified (`unfulfilled-lint-expectations`
/// on the command line is `unfulfilled_lint_expectations` in the JSON).
fn same_lint(a: &str, b: &str) -> bool {
    a.replace('-', "_") == b.replace('-', "_")
}

/// The backticked tokens of a diagnostic message.
fn backticked(message: &str) -> impl Iterator<Item = &str> {
    message.split('`').skip(1).step_by(2)
}

fn matches(e: &Expectation, o: &Observed) -> bool {
    o.lint.as_deref().is_some_and(|l| same_lint(l, &e.lint))
        && backticked(&o.message).any(|t| t == e.path)
}

/// Judge one variant's fixture warnings against the manifest. Returns one
/// line per failure; empty is a pass.
pub(super) fn verdict(manifest: &Manifest, observed: &[Observed]) -> Vec<String> {
    let mut failures = Vec::new();
    for o in observed {
        let Some(file) = &o.file else {
            failures.push(format!(
                "unattributed: {} ({}) - its primary span is not in a listed fixture file",
                o.message, o.at
            ));
            continue;
        };
        let expected = manifest
            .files
            .get(file)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if !expected.iter().any(|e| matches(e, o)) {
            let lint = o.lint.as_deref().unwrap_or("uncoded");
            failures.push(format!(
                "{file}: unexpected {lint}: {} ({})",
                o.message, o.at
            ));
        }
    }
    for (file, expected) in &manifest.files {
        for e in expected {
            let seen = observed
                .iter()
                .any(|o| o.file.as_deref() == Some(file.as_str()) && matches(e, o));
            if !seen {
                failures.push(format!("{file}: missing {} naming `{}`", e.lint, e.path));
            }
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const MANIFEST: &str = r#"
[files."src/submit_order_method.rs"]
expect = [{ lint = "clippy::disallowed_methods", path = "nautilus_trading::Strategy::submit_order" }]

[files."src/submit_order_list.rs"]
expect = [{ lint = "clippy::disallowed_methods", path = "nautilus_trading::Strategy::submit_order_list" }]

[files."src/lib.rs"]
clean = true
"#;

    fn seen(file: &str, lint: &str, path: &str) -> Observed {
        Observed {
            file: Some(file.into()),
            lint: Some(lint.into()),
            message: format!("use of a disallowed method `{path}`"),
            at: format!("{file}:1"),
        }
    }

    #[test]
    fn exact_matches_pass() {
        let m = Manifest::parse(MANIFEST).unwrap();
        let observed = [
            seen(
                "src/submit_order_method.rs",
                "clippy::disallowed_methods",
                "nautilus_trading::Strategy::submit_order",
            ),
            seen(
                "src/submit_order_list.rs",
                "clippy::disallowed_methods",
                "nautilus_trading::Strategy::submit_order_list",
            ),
        ];
        assert!(verdict(&m, &observed).is_empty());
    }

    // The substring trap: `submit_order` is a prefix of `submit_order_list`.
    // A list call in the method fixture is unexpected there, and the method
    // expectation stays missing.
    #[test]
    fn a_prefix_path_does_not_satisfy_an_expectation() {
        let m = Manifest::parse(MANIFEST).unwrap();
        let observed = [
            seen(
                "src/submit_order_method.rs",
                "clippy::disallowed_methods",
                "nautilus_trading::Strategy::submit_order_list",
            ),
            seen(
                "src/submit_order_list.rs",
                "clippy::disallowed_methods",
                "nautilus_trading::Strategy::submit_order_list",
            ),
        ];
        let failures = verdict(&m, &observed);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures[0].contains("unexpected"));
        assert!(failures[1].contains("missing"));
    }

    #[test]
    fn a_clean_file_with_any_diagnostic_and_an_unlisted_span_fail() {
        let m = Manifest::parse(MANIFEST).unwrap();
        let mut stray = seen("src/lib.rs", "unfulfilled_lint_expectations", "x");
        stray.message = "this lint expectation is unfulfilled".into();
        let unattributed = Observed {
            file: None,
            ..seen("x", "dead_code", "y")
        };
        let observed = [
            seen(
                "src/submit_order_method.rs",
                "clippy::disallowed_methods",
                "nautilus_trading::Strategy::submit_order",
            ),
            seen(
                "src/submit_order_list.rs",
                "clippy::disallowed_methods",
                "nautilus_trading::Strategy::submit_order_list",
            ),
            stray,
            unattributed,
        ];
        let failures = verdict(&m, &observed);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures[0].starts_with("src/lib.rs: unexpected unfulfilled_lint_expectations"));
        assert!(failures[1].starts_with("unattributed"));
    }

    #[test]
    fn lint_spellings_unify_dashes() {
        assert!(same_lint(
            "unfulfilled-lint-expectations",
            "unfulfilled_lint_expectations"
        ));
    }

    #[test]
    fn malformed_manifests_are_refused() {
        assert!(Manifest::parse("[files.\"a.rs\"]\nclean = true\n").is_err());
        assert!(Manifest::parse("[files.\"a.rs\"]\n").is_err());
        assert!(
            Manifest::parse(
                "[files.\"a.rs\"]\nclean = true\nexpect = [{ lint = \"x\", path = \"y\" }]\n"
            )
            .is_err()
        );
    }
}
