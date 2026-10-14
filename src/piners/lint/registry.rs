//! The lint corpus registry: `lints.toml` plus keyword grouping files.
//!
//! Layout under `[piners.lint] registry_dir`:
//!
//! - `lints.toml` - the canonical universe. One `[probes.<id>]` per snippet,
//!   each pinning its `pine` file by path + xxh128, the gated `expected`
//!   disposition, and an optional TV anchor (`tv_anchored_at` + a `tv`
//!   fingerprint) written only by `--reanchor`.
//! - `<keyword>.toml` - a pure selection grouping: `probes = ["id", ...]`.
//!
//! Snippet `path`s resolve under `[piners] corpus_root`, same as the trade
//! corpus. No feeds, no `[roots]` - lint needs no market data. See
//! `docs/commands/lint-corpus.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{DiagKey, DiagSet, PinnableDisposition, Severity};
use crate::error::DevError;
use crate::lockfile::LockGuard;
use crate::piners::registry::FilePin;
use crate::piners::registry_io;
use crate::preflight;

/// File name of the canonical lint pin file inside the registry directory.
pub(crate) const LINTS_FILE: &str = "lints.toml";

/// One pinned TV diagnostic: the re-anchor fingerprint. Its severity is typed
/// so an unknown value cannot silently disappear from the anchor. Only
/// `error`/`warning` are meaningful (the gated grain).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TvDiag {
    pub line: usize,
    #[serde(default)]
    pub col: Option<usize>,
    pub severity: Severity,
}

impl TvDiag {
    /// Convert to the normalized anchor key.
    fn to_key(&self) -> DiagKey {
        DiagKey {
            line: self.line,
            col: self.col,
            severity: self.severity,
        }
    }
}

/// A pinned lint probe: its input snippet, the disposition the gate holds it
/// to, and the optional TV anchor.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LintPin {
    /// The blessed `(piners <-> pine-lint)` disposition. `None` = never
    /// blessed: a hard "must bless" gate failure, not a silent pass.
    #[serde(default)]
    pub expected: Option<PinnableDisposition>,
    /// The diagnostic scope (`validators::Scope::label`) `expected` was
    /// blessed under. `None` = the default scope. A disposition is a function
    /// of the scope (`--warnings` can turn `agree_clean` into `divergent`), so
    /// the gate holds a run to `expected` only when the scopes match.
    #[serde(default)]
    pub expected_scope: Option<String>,
    /// When the TV anchor was last refreshed (absolute RFC3339). `--reanchor`
    /// writes it; informational on frequent runs.
    #[serde(default)]
    pub tv_anchored_at: Option<String>,
    /// The diagnostic scope the `tv` fingerprint was filtered to at
    /// `--reanchor` time. `None` = the default scope. A run under another
    /// scope does not compare against the anchor.
    #[serde(default)]
    pub tv_scope: Option<String>,
    /// TV's last-seen diagnostic fingerprint. Written by `--reanchor`.
    #[serde(default)]
    pub tv: Vec<TvDiag>,
    pub pine: FilePin,
}

impl LintPin {
    /// A content-only pin: just the snippet, unblessed, no anchor. Test
    /// constructor; production pins arrive via deserialization.
    #[cfg(test)]
    pub fn new(pine: FilePin) -> Self {
        Self {
            expected: None,
            expected_scope: None,
            tv_anchored_at: None,
            tv_scope: None,
            tv: Vec::new(),
            pine,
        }
    }

    /// The TV anchor fingerprint as a comparable [`DiagSet`], or `None` when
    /// the probe has never been anchored.
    pub fn tv_anchor(&self) -> Option<DiagSet> {
        if self.tv.is_empty() && self.tv_anchored_at.is_none() {
            return None;
        }
        Some(self.tv.iter().map(TvDiag::to_key).collect())
    }
}

/// The full `lints.toml` shape.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LintsData {
    #[serde(default)]
    pub probes: BTreeMap<String, LintPin>,
}

/// The loaded lint registry: the pinned universe plus the keyword index.
#[derive(Debug, Default)]
pub struct LintRegistry {
    pub pins: BTreeMap<String, LintPin>,
    pub keywords: BTreeMap<String, Vec<String>>,
    /// The exact `lints.toml` text `pins` was parsed from. The writers
    /// (bless, reanchor) edit this text, not a fresh read, and refuse to
    /// replace a file whose bytes no longer match it: the load and the write
    /// are both under the global lock, so a difference is a hand edit the
    /// write would revert. A successful write stores what it wrote here, so
    /// a later write in the same process compares against that.
    pub lints_text: String,
}

/// Parse `lints.toml` text in hand (the comment-preserving writer keeps the
/// raw text around).
pub fn parse_lints(text: &str, origin: &Path) -> Result<LintsData, DevError> {
    toml::from_str(text)
        .map_err(|e| DevError::Config(format!("piners lint: {}: {e}", origin.display())))
}

impl LintRegistry {
    /// Load `lints.toml` and every sibling `<keyword>.toml` from `registry_dir`.
    /// Requires the caller's lock so writers cannot replace files while this
    /// snapshot is being assembled.
    pub fn load(registry_dir: &Path, _lock: &LockGuard) -> Result<Self, DevError> {
        Self::load_unlocked(registry_dir)
    }

    fn load_unlocked(registry_dir: &Path) -> Result<Self, DevError> {
        let files = registry_io::load_files(registry_dir, LINTS_FILE, "piners lint")?;
        let lints_path = registry_dir.join(LINTS_FILE);
        let data = parse_lints(&files.canonical_text, &lints_path)?;

        Ok(Self {
            pins: data.probes,
            keywords: files.keywords,
            lints_text: files.canonical_text,
        })
    }

    /// Structural lint: every keyword id must be pinned. Deserialization
    /// restricts `expected` to the pinnable disposition enum.
    pub fn lint(&self) -> Result<(), DevError> {
        let dangling =
            registry_io::dangling_keywords(&self.keywords, |id| self.pins.contains_key(id));
        let mut errs = Vec::new();
        if !dangling.is_empty() {
            errs.push(format!(
                "keyword file(s) reference ids absent from {LINTS_FILE}:\n  {}",
                dangling.join("\n  ")
            ));
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(DevError::Config(format!(
                "piners lint: {}",
                errs.join("\n")
            )))
        }
    }
}

/// Resolve and hard-verify a single pinned snippet against `corpus_root`.
/// A missing file or hash mismatch is a hard error (registry lying or tree
/// drifted). Returns the absolute snippet path for the validator runners.
pub fn verify_probe(
    id: &str,
    pin: &LintPin,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let abs = corpus_root.join(&pin.pine.path);
    if !abs.exists() {
        return Err(DevError::Preflight(vec![format!(
            "piners lint: probe '{id}' pins a snippet path that is missing from the corpus:\n  {}\n  (registry is lying or the corpus drifted)",
            abs.display()
        )]));
    }
    let origin = format!("probe '{id}' ({})", pin.pine.path.display());
    preflight::verify_file_hash(&abs, &pin.pine.xxh128, project_root, Some(&origin))?;
    Ok(abs)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn registry_with(keyword_ids: &[(&str, &[&str])], pin_ids: &[&str]) -> LintRegistry {
        let mut pins = BTreeMap::new();
        for id in pin_ids {
            pins.insert(
                (*id).to_owned(),
                LintPin::new(FilePin {
                    path: PathBuf::from(format!("lint/{id}.pine")),
                    xxh128: "00".into(),
                }),
            );
        }
        let mut keywords = BTreeMap::new();
        for (k, ids) in keyword_ids {
            keywords.insert(
                (*k).to_owned(),
                ids.iter().map(|s| (*s).to_owned()).collect(),
            );
        }
        LintRegistry {
            pins,
            keywords,
            lints_text: String::new(),
        }
    }

    #[test]
    fn lint_passes_when_keyword_ids_are_pinned() {
        let r = registry_with(&[("k", &["a", "b"])], &["a", "b", "c"]);
        assert!(r.lint().is_ok());
    }

    #[test]
    fn lint_fails_on_dangling_keyword_id() {
        let r = registry_with(&[("k", &["a", "ghost"])], &["a"]);
        assert!(format!("{:?}", r.lint().unwrap_err()).contains("ghost"));
    }

    #[test]
    fn registry_rejects_validator_errors_as_expected_dispositions() {
        for label in ["piners_error", "lint_error"] {
            let text = format!(
                "[probes.a]\nexpected = \"{label}\"\npine = {{ path = \"lint/a.pine\", xxh128 = \"00\" }}\n"
            );
            let error = parse_lints(&text, Path::new("lints.toml")).unwrap_err();
            assert!(format!("{error:?}").contains(label));
        }
    }

    #[test]
    fn tv_anchor_none_when_never_anchored() {
        let pin = LintPin::new(FilePin {
            path: PathBuf::from("lint/a.pine"),
            xxh128: "00".into(),
        });
        assert!(pin.tv_anchor().is_none());
    }

    #[test]
    fn tv_anchor_some_when_anchored() {
        let mut pin = LintPin::new(FilePin {
            path: PathBuf::from("lint/a.pine"),
            xxh128: "00".into(),
        });
        pin.tv_anchored_at = Some("2026-06-22T00:00:00Z".to_owned());
        pin.tv = vec![TvDiag {
            line: 3,
            col: Some(5),
            severity: Severity::Error,
        }];
        let anchor = pin.tv_anchor().unwrap();
        assert_eq!(anchor.len(), 1);
    }

    #[test]
    fn load_parses_lints_and_keyword_files() {
        let dir = std::env::temp_dir().join(format!("brokkr_lint_reg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("lints.toml"),
            r#"
[probes.unterminated-01]
expected = "agree_flagged"
tv_anchored_at = "2026-06-22T14:03:00Z"
tv = [ { line = 4, col = 8, severity = "error" } ]
pine = { path = "lint/unterminated-01.pine", xxh128 = "aaa" }

[probes.clean-01]
expected = "agree_clean"
pine = { path = "lint/clean-01.pine", xxh128 = "bbb" }
"#,
        )
        .unwrap();
        std::fs::write(dir.join("lex.toml"), "probes = [\"unterminated-01\"]\n").unwrap();

        let r = LintRegistry::load_unlocked(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert!(r.lints_text.contains("[probes.clean-01]"));
        assert_eq!(r.pins.len(), 2);
        assert_eq!(r.pins["unterminated-01"].pine.xxh128, "aaa");
        assert_eq!(
            r.pins["unterminated-01"].expected,
            Some(PinnableDisposition::AgreeFlagged)
        );
        assert!(r.pins["unterminated-01"].tv_anchor().is_some());
        assert!(r.pins["clean-01"].tv_anchor().is_none());
        assert_eq!(r.keywords["lex"], vec!["unterminated-01".to_owned()]);
        assert!(r.lint().is_ok());
    }
}
