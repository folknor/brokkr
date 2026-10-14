//! `workspace_dep` phase: every `[workspace.dependencies]` entry should be
//! used by some workspace member (`dep = { workspace = true }`). An entry no
//! member inherits is dead weight in the root manifest.
//!
//! Workspace inheritance is a manifest concept `cargo metadata` does not
//! surface (the resolve graph shows a resolved dep, not that it came from the
//! workspace table), so this phase reads the TOML directly - the root for the
//! declarations, each member for its `workspace = true` uses. `cargo metadata`
//! still provides the workspace root and the member manifest paths.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use super::CargoMetadata;
use crate::error::DevError;
use crate::manifest::{ManifestDependencyScope, ManifestDeps, read_manifest};

#[derive(Serialize)]
pub struct UnusedWorkspaceDepEvent {
    #[serde(rename = "crate")]
    pub krate: String,
}

/// Find declared workspace deps that no member inherits (minus the ignore
/// list, whose entries may end in `*` for a prefix match).
///
/// A manifest that cannot be read or parsed is an error, never a skip: an
/// unreadable root would read as "declares nothing" (a silent all-clear),
/// and an unreadable member would drop its inherited deps from `used`,
/// turning every dep only it inherits into a false "unused" finding.
pub fn run(
    metadata: &CargoMetadata,
    ignore: &[String],
) -> Result<Vec<UnusedWorkspaceDepEvent>, DevError> {
    if metadata.workspace_root.is_empty() {
        return Err(DevError::Build(
            "cargo metadata returned an empty workspace_root; cannot audit [workspace.dependencies]"
                .into(),
        ));
    }
    let root_manifest = Path::new(&metadata.workspace_root).join("Cargo.toml");
    let root = parse(&root_manifest)?;
    let declared: BTreeSet<String> = ManifestDeps::new(&root)
        .entries()
        .filter(|dep| dep.scope == ManifestDependencyScope::Workspace)
        .map(|dep| dep.key.to_owned())
        .collect();
    validate_ignore(ignore, &declared)?;
    if declared.is_empty() {
        return Ok(Vec::new());
    }

    let mut used = BTreeSet::new();
    collect_from(&root, &mut used);
    for pkg in &metadata.packages {
        if metadata.is_member(&pkg.id) {
            collect_from(&parse(Path::new(&pkg.manifest_path))?, &mut used);
        }
    }

    Ok(declared
        .into_iter()
        .filter(|d| !used.contains(d.as_str()) && !ignored(d, ignore))
        .map(|krate| UnusedWorkspaceDepEvent { krate })
        .collect())
}

/// Whether `name` matches any ignore entry - exact, or a `*`-suffixed prefix.
///
/// A bare `"*"` is treated literally (exact match against a crate named `*`,
/// i.e. never), not as an empty prefix: `strip_suffix('*')` on it yields `""`,
/// and `starts_with("")` is always true, which would silently disable the whole
/// phase.
fn ignored(name: &str, ignore: &[String]) -> bool {
    ignore.iter().any(|ig| ignore_entry_matches(name, ig))
}

fn ignore_entry_matches(name: &str, entry: &str) -> bool {
    match entry.strip_suffix('*') {
        Some("") => entry == name,
        Some(prefix) => name.starts_with(prefix),
        None => entry == name,
    }
}

fn validate_ignore(ignore: &[String], declared: &BTreeSet<String>) -> Result<(), DevError> {
    for entry in ignore {
        if !declared
            .iter()
            .any(|name| ignore_entry_matches(name, entry))
        {
            return Err(DevError::Config(format!(
                "[deps].workspace_dep_ignore entry {entry:?} matches no [workspace.dependencies] key"
            )));
        }
    }
    Ok(())
}

/// Names of deps this manifest inherits from the workspace
/// (`dep = { workspace = true }`), across every dependency table.
fn collect_from(doc: &toml_edit::DocumentMut, out: &mut BTreeSet<String>) {
    for dep in ManifestDeps::new(doc).entries() {
        if dep.scope == ManifestDependencyScope::Package
            && dep
                .spec
                .as_table_like()
                .and_then(|spec| spec.get("workspace"))
                .and_then(toml_edit::Item::as_bool)
                == Some(true)
        {
            out.insert(dep.key.to_owned());
        }
    }
}

fn parse(manifest: &Path) -> Result<toml_edit::DocumentMut, DevError> {
    read_manifest(manifest).map_err(|why| DevError::Config(format!("workspace_dep: {why}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn inherited(src: &str) -> BTreeSet<String> {
        let value = crate::manifest::parse_manifest_bytes(src.as_bytes()).unwrap();
        let mut out = BTreeSet::new();
        collect_from(&value, &mut out);
        out
    }

    #[test]
    fn collects_only_workspace_inherited_deps() {
        let src = "\
[dependencies]\n\
serde = { workspace = true }\n\
tokio = \"1\"\n\
[dev-dependencies]\n\
rstest = { workspace = true }\n\
[target.'cfg(unix)'.dependencies]\n\
libc = { workspace = true }\n";
        let got = inherited(src);
        // `tokio` (own version, not inherited) is excluded; the three inherited
        // deps across normal/dev/target tables are collected.
        assert_eq!(
            got,
            ["libc", "rstest", "serde"]
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        );
    }

    #[test]
    fn workspace_dependencies_table_is_not_a_use() {
        // The `[workspace.dependencies]` declaration must never count as usage.
        let src = "[workspace.dependencies]\nserde = \"1\"\n";
        assert!(inherited(src).is_empty());
    }

    #[test]
    fn ignore_matches_exact_and_prefix_glob() {
        let ignore = vec!["lychee".to_string(), "cargo-*".to_string()];
        assert!(ignored("lychee", &ignore));
        assert!(ignored("cargo-machete", &ignore));
        assert!(ignored("cargo-nextest", &ignore));
        assert!(!ignored("serde", &ignore));
        // "cargo" lacks the trailing hyphen, so the `cargo-*` glob misses it.
        assert!(!ignored("cargo", &ignore));
    }

    #[test]
    fn bare_star_ignore_does_not_match_everything() {
        // A lone "*" must not silently disable the phase (empty-prefix match).
        // It is treated literally: only a crate actually named "*" would match.
        let ignore = vec!["*".to_string()];
        assert!(!ignored("serde", &ignore));
        assert!(!ignored("tokio", &ignore));
        assert!(ignored("*", &ignore));
    }

    #[test]
    fn unmatched_ignore_entries_are_rejected() {
        let declared = ["serde", "tokio"].into_iter().map(str::to_owned).collect();
        assert!(validate_ignore(&["ser*".into()], &declared).is_ok());
        let error = validate_ignore(&["missing".into()], &declared).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("matches no [workspace.dependencies] key")
        );
        assert!(validate_ignore(&["anything*".into()], &BTreeSet::new()).is_err());
    }

    fn metadata_for(root: &Path, member_manifest: &Path) -> CargoMetadata {
        let json = serde_json::json!({
            "packages": [{
                "name": "m", "version": "0.1.0", "id": "m-id",
                "manifest_path": member_manifest.to_string_lossy(),
            }],
            "workspace_members": ["m-id"],
            "workspace_root": root.to_string_lossy(),
        });
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn unparseable_member_is_an_error_not_a_false_finding() {
        // Before: the member's `serde = { workspace = true }` was lost with
        // the parse failure, and `serde` came back as "unused".
        let dir = crate::test_scratch::scratch("workspace_dep", "bad_member");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace.dependencies]\nserde = \"1\"\n",
        )
        .unwrap();
        let member = dir.join("member.toml");
        std::fs::write(&member, "[dependencies]\nserde = { workspace = true\n").unwrap();
        assert!(run(&metadata_for(&dir, &member), &[]).is_err());
        // The same member, well-formed, inherits it: no finding.
        std::fs::write(&member, "[dependencies]\nserde = { workspace = true }\n").unwrap();
        assert!(run(&metadata_for(&dir, &member), &[]).unwrap().is_empty());
    }

    #[test]
    fn unreadable_root_is_an_error_not_an_all_clear() {
        let dir = crate::test_scratch::scratch("workspace_dep", "missing_root");
        let member = dir.join("member.toml");
        std::fs::write(&member, "[dependencies]\n").unwrap();
        assert!(run(&metadata_for(&dir, &member), &[]).is_err());
    }

    #[test]
    fn collects_underscore_alias_dep_tables() {
        // cargo accepts the underscore spellings via serde alias; a member using
        // them must still register its inherited deps (else false "unused").
        let src = "\
[dev_dependencies]\n\
rstest = { workspace = true }\n\
[build_dependencies]\n\
cc = { workspace = true }\n";
        let got = inherited(src);
        assert_eq!(
            got,
            ["cc", "rstest"].iter().map(|s| (*s).to_string()).collect()
        );
    }
}
