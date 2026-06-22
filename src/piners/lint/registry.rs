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

use super::{is_disposition, DiagKey, DiagSet, Severity, DISPOSITION_LABELS};
use crate::error::DevError;
use crate::piners::registry::FilePin;
use crate::preflight;

/// File name of the canonical lint pin file inside the registry directory.
const LINTS_FILE: &str = "lints.toml";

/// One pinned TV diagnostic: the re-anchor fingerprint. Stored as a string
/// severity so the file reads naturally; converted to a [`DiagKey`] for
/// comparison. Only `error`/`warning` are meaningful (the gated grain).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TvDiag {
    pub line: usize,
    #[serde(default)]
    pub col: Option<usize>,
    pub severity: String,
}

impl TvDiag {
    /// Convert to a [`DiagKey`], or `None` for an unrecognized severity.
    fn to_key(&self) -> Option<DiagKey> {
        let severity = match self.severity.as_str() {
            "error" => Severity::Error,
            "warning" => Severity::Warning,
            _ => return None,
        };
        Some(DiagKey {
            line: self.line,
            col: self.col,
            severity,
        })
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
    pub expected: Option<String>,
    /// When the TV anchor was last refreshed (absolute RFC3339). `--reanchor`
    /// writes it; informational on frequent runs.
    #[serde(default)]
    pub tv_anchored_at: Option<String>,
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
            tv_anchored_at: None,
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
        Some(self.tv.iter().filter_map(TvDiag::to_key).collect())
    }
}

/// The full `lints.toml` shape.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LintsData {
    #[serde(default)]
    pub probes: BTreeMap<String, LintPin>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeywordFile {
    #[serde(default)]
    probes: Vec<String>,
}

/// The loaded lint registry: the pinned universe plus the keyword index.
#[derive(Debug, Default)]
pub struct LintRegistry {
    pub pins: BTreeMap<String, LintPin>,
    pub keywords: BTreeMap<String, Vec<String>>,
}

/// Parse `lints.toml` text in hand (the comment-preserving writer keeps the
/// raw text around).
pub fn parse_lints(text: &str, origin: &Path) -> Result<LintsData, DevError> {
    toml::from_str(text).map_err(|e| DevError::Config(format!("piners lint: {}: {e}", origin.display())))
}

impl LintRegistry {
    /// Load `lints.toml` and every sibling `<keyword>.toml` from `registry_dir`.
    pub fn load(registry_dir: &Path) -> Result<Self, DevError> {
        if !registry_dir.is_dir() {
            return Err(DevError::Config(format!(
                "piners lint: registry directory not found: {}",
                registry_dir.display()
            )));
        }
        let lints_path = registry_dir.join(LINTS_FILE);
        let text = std::fs::read_to_string(&lints_path).map_err(|e| {
            DevError::Config(format!(
                "piners lint: failed to read {}: {e}",
                lints_path.display()
            ))
        })?;
        let data = parse_lints(&text, &lints_path)?;

        let mut keywords = BTreeMap::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(registry_dir)
            .map_err(DevError::Io)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for path in entries {
            if path.file_name().and_then(|n| n.to_str()) == Some(LINTS_FILE) {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let text = std::fs::read_to_string(&path).map_err(|e| {
                DevError::Config(format!(
                    "piners lint: failed to read keyword file {}: {e}",
                    path.display()
                ))
            })?;
            let kw: KeywordFile = toml::from_str(&text)
                .map_err(|e| DevError::Config(format!("piners lint: {}: {e}", path.display())))?;
            keywords.insert(stem.to_owned(), kw.probes);
        }

        Ok(Self {
            pins: data.probes,
            keywords,
        })
    }

    /// Structural lint: every keyword id must be pinned, every `expected`
    /// must be a known disposition label.
    pub fn lint(&self) -> Result<(), DevError> {
        let mut dangling = Vec::new();
        for (keyword, ids) in &self.keywords {
            for id in ids {
                if !self.pins.contains_key(id) {
                    dangling.push(format!("{keyword}.toml -> {id}"));
                }
            }
        }
        let mut bad_expected = Vec::new();
        for (id, pin) in &self.pins {
            if let Some(exp) = &pin.expected
                && !is_disposition(exp)
            {
                bad_expected.push(format!("{id} -> expected = \"{exp}\""));
            }
        }
        let mut errs = Vec::new();
        if !dangling.is_empty() {
            errs.push(format!(
                "keyword file(s) reference ids absent from {LINTS_FILE}:\n  {}",
                dangling.join("\n  ")
            ));
        }
        if !bad_expected.is_empty() {
            errs.push(format!(
                "pin(s) carry an unknown `expected` label (must be one of {}):\n  {}",
                DISPOSITION_LABELS.join(", "),
                bad_expected.join("\n  ")
            ));
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(DevError::Config(format!("piners lint: {}", errs.join("\n"))))
        }
    }

    /// Sorted keyword names for error/help messages.
    pub fn keyword_names(&self) -> Vec<&str> {
        self.keywords.keys().map(String::as_str).collect()
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
    let origin = format!("probe '{id}' (strategy.pine)");
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
            keywords.insert((*k).to_owned(), ids.iter().map(|s| (*s).to_owned()).collect());
        }
        LintRegistry { pins, keywords }
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
    fn lint_fails_on_unknown_expected() {
        let mut r = registry_with(&[], &["a"]);
        r.pins.get_mut("a").unwrap().expected = Some("bogus".to_owned());
        assert!(format!("{:?}", r.lint().unwrap_err()).contains("bogus"));
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
            severity: "error".to_owned(),
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

        let r = LintRegistry::load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(r.pins.len(), 2);
        assert_eq!(r.pins["unterminated-01"].pine.xxh128, "aaa");
        assert_eq!(r.pins["unterminated-01"].expected.as_deref(), Some("agree_flagged"));
        assert!(r.pins["unterminated-01"].tv_anchor().is_some());
        assert!(r.pins["clean-01"].tv_anchor().is_none());
        assert_eq!(r.keywords["lex"], vec!["unterminated-01".to_owned()]);
        assert!(r.lint().is_ok());
    }
}
