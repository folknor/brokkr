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

#[derive(Serialize)]
pub struct UnusedWorkspaceDepEvent {
    #[serde(rename = "crate")]
    pub krate: String,
}

/// Find declared workspace deps that no member inherits (minus the ignore
/// list, whose entries may end in `*` for a prefix match).
pub fn run(metadata: &CargoMetadata, ignore: &[String]) -> Vec<UnusedWorkspaceDepEvent> {
    if metadata.workspace_root.is_empty() {
        return Vec::new();
    }
    let root_manifest = Path::new(&metadata.workspace_root).join("Cargo.toml");
    let declared = read_workspace_deps(&root_manifest);
    if declared.is_empty() {
        return Vec::new();
    }

    let members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let mut used = BTreeSet::new();
    collect_inherited(&root_manifest, &mut used);
    for pkg in &metadata.packages {
        if members.contains(pkg.id.as_str()) {
            collect_inherited(Path::new(&pkg.manifest_path), &mut used);
        }
    }

    declared
        .into_iter()
        .filter(|d| !used.contains(d.as_str()) && !ignored(d, ignore))
        .map(|krate| UnusedWorkspaceDepEvent { krate })
        .collect()
}

/// Whether `name` matches any ignore entry - exact, or a `*`-suffixed prefix.
///
/// A bare `"*"` is treated literally (exact match against a crate named `*`,
/// i.e. never), not as an empty prefix: `strip_suffix('*')` on it yields `""`,
/// and `starts_with("")` is always true, which would silently disable the whole
/// phase.
fn ignored(name: &str, ignore: &[String]) -> bool {
    ignore.iter().any(|ig| match ig.strip_suffix('*') {
        Some("") => ig == name,
        Some(prefix) => name.starts_with(prefix),
        None => ig == name,
    })
}

/// Keys of the root manifest's `[workspace.dependencies]`.
fn read_workspace_deps(manifest: &Path) -> BTreeSet<String> {
    parse(manifest)
        .as_ref()
        .and_then(|v| v.get("workspace"))
        .and_then(|w| w.get("dependencies"))
        .and_then(toml::Value::as_table)
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default()
}

/// Names of deps this manifest inherits from the workspace
/// (`dep = { workspace = true }`), across every dependency table.
fn collect_inherited(manifest: &Path, out: &mut BTreeSet<String>) {
    if let Some(v) = parse(manifest) {
        collect_from(&v, out);
    }
}

fn collect_from(value: &toml::Value, out: &mut BTreeSet<String>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, val) in table {
        match key.as_str() {
            "dependencies"
            | "dev-dependencies"
            | "build-dependencies"
            | "dev_dependencies"
            | "build_dependencies" => {
                if let Some(deps) = val.as_table() {
                    for (name, spec) in deps {
                        if spec.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                            out.insert(name.clone());
                        }
                    }
                }
            }
            // Descend through each `[target.'cfg(..)']` to its dependency
            // tables; never into `[workspace.dependencies]` (a declaration).
            "target" => {
                if let Some(cfgs) = val.as_table() {
                    for cfg_val in cfgs.values() {
                        collect_from(cfg_val, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn parse(manifest: &Path) -> Option<toml::Value> {
    let text = std::fs::read_to_string(manifest).ok()?;
    toml::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn inherited(src: &str) -> BTreeSet<String> {
        let value: toml::Value = toml::from_str(src).unwrap();
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
