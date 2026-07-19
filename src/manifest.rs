//! `[manifest]` phase: native structural `Cargo.toml` conventions for
//! `brokkr check`. Each manifest is parsed with `toml_edit`, which preserves
//! the ordering and the whitespace/comment *decorations* around keys - so a
//! check can reason about blank-line dependency groups and key order that a
//! value-only parse (`toml`) throws away.
//!
//! On the `[style]` model: the config is a set of named toggles, not a rule
//! DSL. The phase is inert unless a project opts a check in.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Table};

use crate::config::ManifestConfig;
use crate::error::DevError;
use crate::{globs, gremlins};

/// One structural convention violation in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestViolation {
    pub file: PathBuf,
    /// Stable kebab-case check id, e.g. `sort-dependencies`.
    pub rule: &'static str,
    pub message: String,
}

/// `file:line-less: [rule] message` - manifests are reported at file scope.
pub fn format_one(v: &ManifestViolation) -> String {
    format!("{}: [{}] {}", v.file.display(), v.rule, v.message)
}

/// Names of the dependency tables a manifest carries, at any nesting depth
/// (so `[target.'cfg(unix)'.dependencies]` is covered alongside the top-level
/// three and `[workspace.dependencies]`).
fn is_dependency_table_name(name: &str) -> bool {
    matches!(name, "dependencies" | "dev-dependencies" | "build-dependencies")
}

/// Whether the leading decoration of a key contains a blank line - i.e. the key
/// opens a new visual group. `toml_edit` hands us the raw prefix (the
/// whitespace and comments between the previous item and this key), with the
/// normal line-ending absorbed, so an adjacent key's prefix is empty and a
/// single blank line shows as `"\n"`. A comment line without a blank line
/// (`"# note\n"`) must NOT count, so we look for an actually-empty line: split
/// on `\n` and check whether any segment *before the trailing one* is blank.
fn starts_new_group(prefix: &str) -> bool {
    let mut segs = prefix.split('\n').peekable();
    while let Some(seg) = segs.next() {
        if segs.peek().is_none() {
            break; // text after the final newline is the next key's own line
        }
        if seg.trim().is_empty() {
            return true;
        }
    }
    false
}

/// Scan every tracked manifest matching the config's globs and run the enabled
/// checks, newest concern first in declaration order.
pub fn scan(
    project_root: &Path,
    cfg: &ManifestConfig,
) -> Result<Vec<ManifestViolation>, DevError> {
    let default_paths = [String::from("**/Cargo.toml")];
    let path_globs = if cfg.paths.is_empty() {
        &default_paths[..]
    } else {
        &cfg.paths[..]
    };
    let paths = globs::build_set(path_globs, "[manifest] paths")?;
    let exclude = globs::build_set(&cfg.exclude, "[manifest] exclude")?;

    let mut out = Vec::new();
    for rel in gremlins::tracked_files(project_root)? {
        if !globs::matches(&paths, &rel) || globs::matches(&exclude, &rel) {
            continue;
        }
        let abs = project_root.join(&rel);
        let Ok(text) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let doc: DocumentMut = text.parse().map_err(|e| {
            DevError::Config(format!("[manifest] {}: parse error: {e}", rel.display()))
        })?;
        check_document(&rel, &doc, cfg, &mut out);
    }
    Ok(out)
}

fn check_document(
    rel: &Path,
    doc: &DocumentMut,
    cfg: &ManifestConfig,
    out: &mut Vec<ManifestViolation>,
) {
    if cfg.sort_dependencies {
        check_sorted_dependencies(rel, doc.as_table(), out);
    }
}

/// Walk every dependency table (at any depth) and require each blank-line
/// group's keys to be in order. A key smaller than the previous key *in the
/// same group* is the violation; a new group resets the comparison.
fn check_sorted_dependencies(rel: &Path, table: &Table, out: &mut Vec<ManifestViolation>) {
    for (name, item) in table {
        let Some(child) = item.as_table() else {
            continue;
        };
        if is_dependency_table_name(name) {
            check_sorted_table(rel, name, child, out);
        } else {
            // Recurse into container tables (`workspace`, `target.'cfg(..)'`).
            check_sorted_dependencies(rel, child, out);
        }
    }
}

fn check_sorted_table(rel: &Path, section: &str, table: &Table, out: &mut Vec<ManifestViolation>) {
    let mut prev: Option<String> = None;
    for (key, _) in table {
        let prefix = table
            .key(key)
            .and_then(|k| k.leaf_decor().prefix())
            .and_then(|p| p.as_str())
            .unwrap_or("");
        if starts_new_group(prefix) {
            prev = None;
        }
        let lower = key.to_ascii_lowercase();
        if let Some(prev_key) = &prev
            && &lower < prev_key
        {
            out.push(ManifestViolation {
                file: rel.to_path_buf(),
                rule: "sort-dependencies",
                message: format!("[{section}] `{key}` is out of order (after `{prev_key}`)"),
            });
        }
        prev = Some(lower);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn violations(section_and_body: &str) -> Vec<String> {
        let doc: DocumentMut = section_and_body.parse().unwrap();
        let mut out = Vec::new();
        check_sorted_dependencies(Path::new("Cargo.toml"), doc.as_table(), &mut out);
        out.iter().map(|v| v.message.clone()).collect()
    }

    #[test]
    fn sorted_dependencies_pass() {
        let src = "[dependencies]\nanyhow = \"1\"\nserde = \"1\"\ntokio = \"1\"\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn unsorted_dependencies_flagged() {
        let src = "[dependencies]\ntokio = \"1\"\nanyhow = \"1\"\n";
        let v = violations(src);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("anyhow"), "{v:?}");
    }

    #[test]
    fn blank_line_starts_a_new_group() {
        // Two sorted groups separated by a blank line: the reset means `serde`
        // following `zzz` across the blank line is fine.
        let src = "[dependencies]\ntokio = \"1\"\nzzz = \"1\"\n\nanyhow = \"1\"\nserde = \"1\"\n";
        assert!(violations(src).is_empty());
    }

    #[test]
    fn unsorted_within_a_later_group_flagged() {
        let src = "[dependencies]\nanyhow = \"1\"\n\nzzz = \"1\"\nserde = \"1\"\n";
        let v = violations(src);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("serde"), "{v:?}");
    }

    #[test]
    fn dev_and_build_and_target_tables_are_checked() {
        let src = "[dev-dependencies]\nb = \"1\"\na = \"1\"\n\n\
                   [target.'cfg(unix)'.dependencies]\nd = \"1\"\nc = \"1\"\n";
        let v = violations(src);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn comment_line_without_blank_does_not_reset_group() {
        // Groups are blank-line-separated; a bare comment line is not a
        // separator, so `anyhow` after `zzz` across only a comment is flagged.
        let src = "[dependencies]\nzzz = \"1\"\n# a comment\nanyhow = \"1\"\n";
        let v = violations(src);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("anyhow"), "{v:?}");
    }

    #[test]
    fn workspace_dependencies_are_checked() {
        let src = "[workspace.dependencies]\nb = \"1\"\na = \"1\"\n";
        assert_eq!(violations(src).len(), 1);
    }
}
