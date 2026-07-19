//! `[manifest]` phase: native structural `Cargo.toml` conventions for
//! `brokkr check`. Each manifest is parsed with `toml_edit`, which preserves
//! the ordering and the whitespace/comment *decorations* around keys - so a
//! check can reason about blank-line dependency groups and key order that a
//! value-only parse (`toml`) throws away.
//!
//! On the `[style]` model: the config is a set of named toggles, not a rule
//! DSL. The phase is inert unless a project opts a check in.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table};

use crate::config::{AdapterGroup, ManifestConfig, VersionAlign};
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
    let shape_exclude = globs::build_set(&cfg.shape_exclude, "[manifest] shape_exclude")?;
    for g in &cfg.version_align {
        if !matches!(g.granularity.as_str(), "" | "major" | "minor") {
            return Err(DevError::Config(format!(
                "[[manifest.version_align]] granularity {:?} must be \"major\" or \"minor\"",
                g.granularity
            )));
        }
    }

    let mut manifests: Vec<(PathBuf, DocumentMut)> = Vec::new();
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
        manifests.push((rel, doc));
    }

    let mut out = Vec::new();
    for (rel, doc) in &manifests {
        check_document(rel, doc, cfg, globs::matches(&shape_exclude, rel), &mut out);
    }
    // A workspace-level check needs every manifest at once (the root defines the
    // group, members are checked against it).
    if let Some(ag) = &cfg.adapter_group {
        check_adapter_group(&manifests, ag, &mut out);
    }
    Ok(out)
}

/// Cargo-conv check 9: crates named in `forbidden_in` must not depend on any
/// member of the comment-labelled `[workspace.dependencies]` group.
fn check_adapter_group(
    manifests: &[(PathBuf, DocumentMut)],
    ag: &AdapterGroup,
    out: &mut Vec<ManifestViolation>,
) {
    let adapter_deps = manifests
        .iter()
        .find_map(|(_, doc)| {
            let ws = doc
                .get("workspace")
                .and_then(Item::as_table)
                .and_then(|w| w.get("dependencies"))
                .and_then(Item::as_table)?;
            Some(labelled_group_keys(ws, &ag.marker))
        })
        .unwrap_or_default();
    if adapter_deps.is_empty() {
        return;
    }
    for (rel, doc) in manifests {
        let Some(name) = doc
            .get("package")
            .and_then(Item::as_table)
            .and_then(|p| p.get("name"))
            .and_then(Item::as_str)
        else {
            continue;
        };
        if !ag.forbidden_in.iter().any(|f| f == name) {
            continue;
        }
        let mut declared = std::collections::BTreeSet::new();
        declared_deps(doc.as_table(), &mut declared);
        for dep in declared.intersection(&adapter_deps) {
            out.push(ManifestViolation {
                file: rel.clone(),
                rule: "adapter-group",
                message: format!("`{name}` must not depend on adapter-group crate `{dep}`"),
            });
        }
    }
}

/// Keys of the dependency-table group whose header comment contains `marker`.
fn labelled_group_keys(table: &Table, marker: &str) -> std::collections::BTreeSet<String> {
    dependency_groups(table)
        .into_iter()
        .find(|g| g.labels.iter().any(|l| l.contains(marker)))
        .map(|g| g.keys.into_iter().collect())
        .unwrap_or_default()
}

fn check_document(
    rel: &Path,
    doc: &DocumentMut,
    cfg: &ManifestConfig,
    shape_excluded: bool,
    out: &mut Vec<ManifestViolation>,
) {
    // Dependency-content checks (1, 10, 8) apply to every manifest.
    if cfg.sort_dependencies {
        check_sorted_dependencies(rel, doc.as_table(), out);
    }
    if cfg.cargo_machete_ignored_declared {
        check_cargo_machete(rel, doc, out);
    }
    if !cfg.version_align.is_empty() {
        check_version_align(rel, doc, &cfg.version_align, out);
    }
    // The section/target-shape checks (2-6) describe a crate's own structure,
    // which is moot for a cargo-fuzz stub (its own tiny standalone workspace) -
    // the hook exempts those, as does an explicit `shape_exclude` glob (a
    // placeholder crate whose layout is deliberately non-conventional but whose
    // deps still sort).
    if is_cargo_fuzz(doc) || shape_excluded {
        return;
    }
    if !cfg.section_order.is_empty() {
        for (name, before) in order_violations(top_level_keys(doc), &cfg.section_order) {
            out.push(ManifestViolation {
                file: rel.to_path_buf(),
                rule: "section-order",
                message: format!("[{name}] section should come before [{before}]"),
            });
        }
    }
    if !cfg.crate_type_order.is_empty() {
        check_crate_type_order(rel, doc, &cfg.crate_type_order, out);
    }
    if !cfg.package_field_order.is_empty() {
        for (name, before) in order_violations(package_keys(doc), &cfg.package_field_order) {
            out.push(ManifestViolation {
                file: rel.to_path_buf(),
                rule: "package-field-order",
                message: format!("[package] `{name}` should come before `{before}`"),
            });
        }
    }
    if cfg.lints_workspace_required {
        check_lints_workspace(rel, doc, out);
    }
    check_bin_example_flags(rel, doc, cfg, out);
}

/// The alignment key of a version requirement at `granularity`: `"^54.1.2"` is
/// `"54.1"` at minor, `"54"` at major. `None` if it does not start with a
/// number (a git/path dep, say).
fn version_key(req: &str, granularity: &str) -> Option<String> {
    let digits = req.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = digits.split('.');
    let major = parts.next().filter(|s| !s.is_empty())?;
    if !major.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if granularity == "major" {
        return Some(major.to_string());
    }
    let minor: String = parts
        .next()
        .unwrap_or("0")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some(format!("{major}.{}", if minor.is_empty() { "0" } else { &minor }))
}

/// The version requirement of a dependency item - a bare `"1.2"` string or the
/// `version = "..."` of an inline/full table dep.
fn dep_version(item: &Item) -> Option<String> {
    if let Some(s) = item.as_str() {
        return Some(s.to_string());
    }
    item.as_table_like()?
        .get("version")
        .and_then(Item::as_str)
        .map(str::to_string)
}

/// Find `name`'s version requirement in any dependency table (any depth).
fn find_dep_version(table: &Table, name: &str) -> Option<String> {
    for (k, item) in table {
        let Some(child) = item.as_table() else {
            continue;
        };
        if is_dependency_table_name(k) {
            if let Some(v) = child.get(name).and_then(dep_version) {
                return Some(v);
            }
        } else if let Some(v) = find_dep_version(child, name) {
            return Some(v);
        }
    }
    None
}

/// Every dependency name declared in any dependency table (any depth).
fn declared_deps(table: &Table, out: &mut std::collections::BTreeSet<String>) {
    for (k, item) in table {
        let Some(child) = item.as_table() else {
            continue;
        };
        if is_dependency_table_name(k) {
            for (dep, _) in child {
                out.insert(dep.to_string());
            }
        } else {
            declared_deps(child, out);
        }
    }
}

/// `[package.metadata.cargo-machete] ignored` must reference declared deps.
fn check_cargo_machete(rel: &Path, doc: &DocumentMut, out: &mut Vec<ManifestViolation>) {
    let Some(ignored) = doc
        .get("package")
        .and_then(Item::as_table)
        .and_then(|p| p.get("metadata"))
        .and_then(Item::as_table)
        .and_then(|m| m.get("cargo-machete"))
        .and_then(Item::as_table)
        .and_then(|c| c.get("ignored"))
        .and_then(Item::as_array)
    else {
        return;
    };
    let mut declared = std::collections::BTreeSet::new();
    declared_deps(doc.as_table(), &mut declared);
    for entry in ignored.iter().filter_map(|v| v.as_str()) {
        if !declared.contains(entry) {
            out.push(ManifestViolation {
                file: rel.to_path_buf(),
                rule: "cargo-machete-ignored",
                message: format!("cargo-machete `ignored` names `{entry}`, not a declared dependency"),
            });
        }
    }
}

/// Crates in a `version_align` group must share a version key.
fn check_version_align(
    rel: &Path,
    doc: &DocumentMut,
    groups: &[VersionAlign],
    out: &mut Vec<ManifestViolation>,
) {
    for g in groups {
        let gran = if g.granularity.is_empty() { "minor" } else { &g.granularity };
        let found: Vec<(String, String)> = g
            .crates
            .iter()
            .filter_map(|c| {
                let ver = find_dep_version(doc.as_table(), c)?;
                Some((c.clone(), version_key(&ver, gran)?))
            })
            .collect();
        let Some((_, first)) = found.first() else {
            continue;
        };
        if found.iter().any(|(_, k)| k != first) {
            let desc = found
                .iter()
                .map(|(c, k)| format!("{c}={k}"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(ManifestViolation {
                file: rel.to_path_buf(),
                rule: "version-align",
                message: format!("versions must align at {gran}: {desc}"),
            });
        }
    }
}

fn top_level_keys(doc: &DocumentMut) -> impl Iterator<Item = &str> {
    doc.as_table().iter().map(|(k, _)| k)
}

fn package_keys(doc: &DocumentMut) -> impl Iterator<Item = &str> {
    doc.get("package")
        .and_then(Item::as_table)
        .into_iter()
        .flat_map(|t| t.iter().map(|(k, _)| k))
}

/// Each `(name, earlier_name)` where `name` (present in `order`) appears in the
/// stream before an item ranked earlier in `order`. Unlisted items are skipped.
fn order_violations<'a>(
    items: impl Iterator<Item = &'a str>,
    order: &[String],
) -> Vec<(String, String)> {
    let rank = |name: &str| order.iter().position(|s| s == name);
    let mut max_rank = 0usize;
    let mut max_name = String::new();
    let mut out = Vec::new();
    for name in items {
        let Some(r) = rank(name) else {
            continue;
        };
        if r < max_rank {
            out.push((name.to_string(), max_name.clone()));
        } else {
            max_rank = r;
            max_name = name.to_string();
        }
    }
    out
}

/// Require `[lints] workspace = true` whenever the crate ships a `[lib]` or
/// `[[bin]]` target.
fn check_lints_workspace(rel: &Path, doc: &DocumentMut, out: &mut Vec<ManifestViolation>) {
    let has_lib = doc.get("lib").is_some();
    let has_bin = doc
        .get("bin")
        .and_then(Item::as_array_of_tables)
        .is_some_and(|a| !a.is_empty());
    if !has_lib && !has_bin {
        return;
    }
    let ok = doc
        .get("lints")
        .and_then(Item::as_table)
        .and_then(|t| t.get("workspace"))
        .and_then(Item::as_bool)
        == Some(true);
    if !ok {
        out.push(ManifestViolation {
            file: rel.to_path_buf(),
            rule: "lints-workspace",
            message: "a crate with [lib]/[[bin]] must set `[lints] workspace = true`".into(),
        });
    }
}

/// `[[bin]]` must set `doc`/`test` false and `[[example]]` must set `doc` false,
/// per the enabled toggles. A missing or `true` flag is a violation.
fn check_bin_example_flags(
    rel: &Path,
    doc: &DocumentMut,
    cfg: &ManifestConfig,
    out: &mut Vec<ManifestViolation>,
) {
    let flag_false = |t: &Table, key: &str| t.get(key).and_then(Item::as_bool) == Some(false);
    let name_of = |t: &Table| {
        t.get("name")
            .and_then(Item::as_str)
            .unwrap_or("?")
            .to_string()
    };
    let mut require = |kind: &str, key: &'static str, rule: &'static str| {
        let Some(arr) = doc.get(kind).and_then(Item::as_array_of_tables) else {
            return;
        };
        for t in arr {
            if !flag_false(t, key) {
                out.push(ManifestViolation {
                    file: rel.to_path_buf(),
                    rule,
                    message: format!("[[{kind}]] `{}` must set `{key} = false`", name_of(t)),
                });
            }
        }
    };
    if cfg.bin_doc_false {
        require("bin", "doc", "bin-doc-false");
    }
    if cfg.bin_test_false {
        require("bin", "test", "bin-test-false");
    }
    if cfg.example_doc_false {
        require("example", "doc", "example-doc-false");
    }
}

/// A `cargo-fuzz` crate declares `[package.metadata] cargo-fuzz = true`.
fn is_cargo_fuzz(doc: &DocumentMut) -> bool {
    doc.get("package")
        .and_then(Item::as_table)
        .and_then(|p| p.get("metadata"))
        .and_then(Item::as_table)
        .and_then(|m| m.get("cargo-fuzz"))
        .and_then(Item::as_bool)
        == Some(true)
}

/// Flag `[lib] crate-type` entries that are out of the required relative order.
fn check_crate_type_order(
    rel: &Path,
    doc: &DocumentMut,
    order: &[String],
    out: &mut Vec<ManifestViolation>,
) {
    let Some(arr) = doc
        .get("lib")
        .and_then(Item::as_table)
        .and_then(|t| t.get("crate-type"))
        .and_then(Item::as_array)
    else {
        return;
    };
    for (name, before) in order_violations(arr.iter().filter_map(|v| v.as_str()), order) {
        out.push(ManifestViolation {
            file: rel.to_path_buf(),
            rule: "crate-type-order",
            message: format!("crate-type `{name}` should come before `{before}`"),
        });
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
    for group in dependency_groups(table) {
        let mut prev: Option<String> = None;
        for key in &group.keys {
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
}

/// A blank-line-separated group of a dependency table, with any comment lines
/// that head it (from the first key's leading decoration).
#[derive(Debug, Default)]
struct DepGroup {
    /// Comment lines heading the group, `#` and whitespace trimmed.
    labels: Vec<String>,
    keys: Vec<String>,
}

/// Segment a dependency table into blank-line-separated groups, reading key
/// order and the comment/whitespace decorations `toml_edit` preserves. The one
/// reader behind both `sort_dependencies` (order within a group) and
/// `adapter_group` (find a group by its header comment).
fn dependency_groups(table: &Table) -> Vec<DepGroup> {
    let mut groups: Vec<DepGroup> = Vec::new();
    let mut cur = DepGroup::default();
    for (key, item) in table {
        // A `[dependencies.<name>]` dotted sub-section is an `Item::Table`, not
        // an inline entry. TOML grammar forces it physically after the whole
        // inline table, so it can never sort into the inline sequence - it is
        // its own single-entry group, never compared. Skip it (an inline-table
        // value like `foo = { version = "1" }` is `Item::Value`, not a table,
        // and stays in the sequence).
        if item.is_table() {
            continue;
        }
        let prefix = table
            .key(key)
            .and_then(|k| k.leaf_decor().prefix())
            .and_then(|p| p.as_str())
            .unwrap_or("");
        if starts_new_group(prefix) && !cur.keys.is_empty() {
            groups.push(std::mem::take(&mut cur));
        }
        if cur.keys.is_empty() {
            cur.labels = comment_lines(prefix);
        }
        cur.keys.push(key.to_string());
    }
    if !cur.keys.is_empty() {
        groups.push(cur);
    }
    groups
}

/// The `# ...` comment lines in a decoration prefix, `#` and surrounding
/// whitespace stripped.
fn comment_lines(prefix: &str) -> Vec<String> {
    prefix
        .lines()
        .filter_map(|l| l.trim().strip_prefix('#').map(|c| c.trim().to_string()))
        .collect()
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

    fn run_doc(cfg: &ManifestConfig, src: &str) -> Vec<(&'static str, String)> {
        run_doc_shape(cfg, src, false)
    }

    fn run_doc_shape(
        cfg: &ManifestConfig,
        src: &str,
        shape_excluded: bool,
    ) -> Vec<(&'static str, String)> {
        let doc: DocumentMut = src.parse().unwrap();
        let mut out = Vec::new();
        check_document(Path::new("Cargo.toml"), &doc, cfg, shape_excluded, &mut out);
        out.iter().map(|v| (v.rule, v.message.clone())).collect()
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

    #[test]
    fn dotted_section_deps_are_not_ordered_against_the_inline_table() {
        // TOML grammar forces every `[dependencies.<name>]` section physically
        // after the inline table, so `nautilus-common` (which sorts before
        // `tokio`) cannot precede it - no edit satisfies an ordering check, so
        // dotted sections must not participate in the inline group's order.
        let src = "[dependencies]\ndotenvy = \"1\"\nlog = \"1\"\ntokio = \"1\"\n\n\
                   [dependencies.nautilus-common]\nversion = \"1\"\n";
        assert!(violations(src).is_empty(), "{:?}", violations(src));
    }

    #[test]
    fn inline_table_dep_values_are_still_ordered() {
        // An inline-table *value* (`tokio = { ... }`) is an `Item::Value`, not a
        // dotted section, so it stays in the ordered sequence.
        let src = "[dependencies]\ntokio = { version = \"1\" }\nanyhow = \"1\"\n";
        assert_eq!(violations(src).len(), 1);
    }

    fn order_cfg() -> ManifestConfig {
        ManifestConfig {
            section_order: ["package", "lib", "features", "dependencies"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            crate_type_order: ["rlib", "staticlib", "cdylib"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn shape_exclude_skips_structural_checks_but_keeps_sort() {
        let mut cfg = order_cfg();
        cfg.sort_dependencies = true;
        // `features` before `lib` trips section-order; `tokio` before `anyhow`
        // trips sort-dependencies.
        let src = "[package]\nname = \"x\"\n[features]\n[lib]\n\n\
                   [dependencies]\ntokio = \"1\"\nanyhow = \"1\"\n";
        // Shape-excluded: the structural violation is gone, the sort one stays.
        let excluded = run_doc_shape(&cfg, src, true);
        assert_eq!(excluded.len(), 1, "{excluded:?}");
        assert_eq!(excluded[0].0, "sort-dependencies");
        // Not excluded: both fire.
        let full = run_doc_shape(&cfg, src, false);
        assert!(full.iter().any(|(r, _)| *r == "section-order"), "{full:?}");
        assert!(full.iter().any(|(r, _)| *r == "sort-dependencies"), "{full:?}");
    }

    #[test]
    fn section_order_pass_and_fail() {
        let good = "[package]\nname = \"x\"\n[features]\n[dependencies]\n";
        assert!(run_doc(&order_cfg(), good).is_empty());
        // features declared before lib -> lib is out of order.
        let bad = "[package]\nname = \"x\"\n[features]\n[lib]\n";
        let v = run_doc(&order_cfg(), bad);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "section-order");
        assert!(v[0].1.contains("[lib]"), "{v:?}");
    }

    #[test]
    fn crate_type_order_flags_reversed() {
        let bad = "[package]\nname = \"x\"\n[lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n";
        let v = run_doc(&order_cfg(), bad);
        assert!(v.iter().any(|(r, _)| *r == "crate-type-order"), "{v:?}");
        let good = "[package]\nname = \"x\"\n[lib]\ncrate-type = [\"rlib\", \"cdylib\"]\n";
        assert!(
            !run_doc(&order_cfg(), good)
                .iter()
                .any(|(r, _)| *r == "crate-type-order")
        );
    }

    #[test]
    fn package_field_order_checked() {
        let cfg = ManifestConfig {
            package_field_order: ["name", "version", "edition"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            ..Default::default()
        };
        let bad = "[package]\nname = \"x\"\nedition = \"2021\"\nversion = \"1\"\n";
        let v = run_doc(&cfg, bad);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "package-field-order");
        assert!(v[0].1.contains("version"), "{v:?}");
        let good = "[package]\nname = \"x\"\nversion = \"1\"\nedition = \"2021\"\n";
        assert!(run_doc(&cfg, good).is_empty());
    }

    #[test]
    fn lints_workspace_required_when_lib_present() {
        let cfg = ManifestConfig {
            lints_workspace_required: true,
            ..Default::default()
        };
        let bad = "[package]\nname = \"x\"\n\n[lib]\n";
        assert_eq!(run_doc(&cfg, bad).len(), 1);
        let good = "[package]\nname = \"x\"\n\n[lints]\nworkspace = true\n\n[lib]\n";
        assert!(run_doc(&cfg, good).is_empty());
        // No lib/bin -> not required.
        let none = "[package]\nname = \"x\"\n";
        assert!(run_doc(&cfg, none).is_empty());
    }

    #[test]
    fn bin_and_example_flags_required() {
        let cfg = ManifestConfig {
            bin_doc_false: true,
            bin_test_false: true,
            example_doc_false: true,
            ..Default::default()
        };
        // A bin missing both flags -> two violations; example missing doc -> one.
        let bad = "[package]\nname = \"x\"\n\n[[bin]]\nname = \"a\"\n\n\
                   [[example]]\nname = \"e\"\n";
        let rules: Vec<&str> = run_doc(&cfg, bad).iter().map(|(r, _)| *r).collect();
        assert!(rules.contains(&"bin-doc-false"), "{rules:?}");
        assert!(rules.contains(&"bin-test-false"), "{rules:?}");
        assert!(rules.contains(&"example-doc-false"), "{rules:?}");
        let good = "[package]\nname = \"x\"\n\n[[bin]]\nname = \"a\"\ndoc = false\ntest = false\n\n\
                    [[example]]\nname = \"e\"\ndoc = false\n";
        assert!(run_doc(&cfg, good).is_empty());
    }

    #[test]
    fn version_key_extracts_at_granularity() {
        assert_eq!(version_key("^54.1.2", "minor").as_deref(), Some("54.1"));
        assert_eq!(version_key("54.1", "major").as_deref(), Some("54"));
        assert_eq!(version_key("=7", "minor").as_deref(), Some("7.0"));
        assert_eq!(version_key(">=1.2, <2", "minor").as_deref(), Some("1.2"));
        assert_eq!(version_key("git-ref", "minor"), None);
    }

    #[test]
    fn version_align_flags_mismatch() {
        let cfg = ManifestConfig {
            version_align: vec![VersionAlign {
                crates: vec!["arrow".into(), "parquet".into()],
                granularity: "minor".into(),
            }],
            ..Default::default()
        };
        // Same major, different minor -> flagged at minor granularity.
        let bad = "[dependencies]\narrow = \"54.1\"\nparquet = \"54.2\"\n";
        let v = run_doc(&cfg, bad);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "version-align");
        // Aligned (inline-table version form also read) -> pass.
        let good = "[dependencies]\narrow = \"54.1\"\nparquet = { version = \"54.1.9\" }\n";
        assert!(run_doc(&cfg, good).is_empty());
        // Only one present -> group does not fire.
        let one = "[dependencies]\narrow = \"54.1\"\n";
        assert!(run_doc(&cfg, one).is_empty());
    }

    #[test]
    fn cargo_machete_ignored_must_be_declared() {
        let cfg = ManifestConfig {
            cargo_machete_ignored_declared: true,
            ..Default::default()
        };
        let bad = "[package]\nname = \"x\"\n\n[package.metadata.cargo-machete]\n\
                   ignored = [\"serde\", \"ghost\"]\n\n[dependencies]\nserde = \"1\"\n";
        let v = run_doc(&cfg, bad);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "cargo-machete-ignored");
        assert!(v[0].1.contains("ghost"), "{v:?}");
    }

    #[test]
    fn dependency_groups_capture_header_comments() {
        let src = "[workspace.dependencies]\nserde = \"1\"\n\n\
                   # Adapter dependencies\ndydx-proto = \"1\"\nfoo = \"1\"\n";
        let doc: DocumentMut = src.parse().unwrap();
        let t = doc
            .get("workspace")
            .unwrap()
            .as_table()
            .unwrap()
            .get("dependencies")
            .unwrap()
            .as_table()
            .unwrap();
        let groups = dependency_groups(t);
        assert_eq!(groups.len(), 2);
        assert!(groups[1].labels.iter().any(|l| l == "Adapter dependencies"));
        assert_eq!(groups[1].keys, vec!["dydx-proto", "foo"]);
    }

    #[test]
    fn adapter_group_forbids_core_crate_usage() {
        let cfg = ManifestConfig {
            adapter_group: Some(AdapterGroup {
                marker: "Adapter dependencies".into(),
                forbidden_in: vec!["nautilus-core".into()],
            }),
            ..Default::default()
        };
        let root = "[workspace]\nmembers = [\"a\"]\n\n[workspace.dependencies]\n\
                    serde = \"1\"\n\n# Adapter dependencies\ndydx-proto = \"1\"\n";
        // A core crate depending on an adapter-group crate is flagged.
        let core_bad = "[package]\nname = \"nautilus-core\"\n\n\
                        [dependencies]\ndydx-proto = { workspace = true }\n";
        let manifests = vec![
            (PathBuf::from("Cargo.toml"), root.parse::<DocumentMut>().unwrap()),
            (PathBuf::from("a/Cargo.toml"), core_bad.parse::<DocumentMut>().unwrap()),
        ];
        let mut out = Vec::new();
        if let Some(ag) = &cfg.adapter_group {
            check_adapter_group(&manifests, ag, &mut out);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rule, "adapter-group");
        assert!(out[0].message.contains("dydx-proto"), "{out:?}");

        // A non-core crate using the same dep is fine.
        let other = "[package]\nname = \"nautilus-adapter-x\"\n\n\
                     [dependencies]\ndydx-proto = { workspace = true }\n";
        let manifests2 = vec![
            (PathBuf::from("Cargo.toml"), root.parse::<DocumentMut>().unwrap()),
            (PathBuf::from("x/Cargo.toml"), other.parse::<DocumentMut>().unwrap()),
        ];
        let mut out2 = Vec::new();
        check_adapter_group(&manifests2, cfg.adapter_group.as_ref().unwrap(), &mut out2);
        assert!(out2.is_empty(), "{out2:?}");
    }

    #[test]
    fn cargo_fuzz_crate_is_exempt_from_structure_checks() {
        // A reversed crate-type would fail crate-type-order, but a cargo-fuzz
        // stub is exempt from the structural checks.
        let src = "[package]\nname = \"x\"\n\n[package.metadata]\ncargo-fuzz = true\n\n\
                   [lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n";
        assert!(run_doc(&order_cfg(), src).is_empty());
        // Sanity: without the cargo-fuzz marker the same manifest is flagged.
        let plain = "[package]\nname = \"x\"\n\n[lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n";
        assert!(!run_doc(&order_cfg(), plain).is_empty());
    }
}
