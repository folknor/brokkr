//! `duplicate_version` phase.
//!
//! Finds crate names with >=2 resolved versions in `cargo metadata`. For
//! each duplicate, computes blame: the set of direct parents (one
//! reverse step in the resolve graph) that pinned that version. Edges
//! are filtered to Normal kind only (Dev/Build dropped, mirroring
//! `cargo tree -d`'s default). Target gating is handled upstream by
//! loading metadata with `--filter-platform=<host>`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::{CargoMetadata, DuplicateVersionEvent, VersionPin};

pub fn run(metadata: &CargoMetadata) -> Vec<DuplicateVersionEvent> {
    let workspace_set: HashSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();

    let id_to_label: HashMap<&str, String> = metadata
        .packages
        .iter()
        .map(|p| (p.id.as_str(), format!("{} {}", p.name, p.version)))
        .collect();

    let id_to_name: HashMap<&str, &str> = metadata
        .packages
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();

    // Reverse adjacency over Normal-kind edges only. A package may appear
    // as both Normal and Dev/Build from the same parent; the parent counts
    // if at least one of its dep_kinds is Normal.
    let mut normal_parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in &metadata.resolve.nodes {
        for d in &node.deps {
            if d.dep_kinds.iter().any(|dk| dk.kind.is_none()) {
                normal_parents
                    .entry(d.pkg.as_str())
                    .or_default()
                    .push(node.id.as_str());
            }
        }
    }

    // Group package IDs by crate name.
    let mut by_name: BTreeMap<&str, Vec<&super::CargoPackage>> = BTreeMap::new();
    for pkg in &metadata.packages {
        by_name.entry(pkg.name.as_str()).or_default().push(pkg);
    }

    let mut events = Vec::new();
    for (name, pkgs) in by_name {
        if pkgs.len() < 2 {
            continue;
        }
        let mut pins: Vec<VersionPin> = pkgs
            .iter()
            .map(|pkg| VersionPin {
                version: pkg.version.clone(),
                direct_blame: direct_blame(
                    pkg.id.as_str(),
                    &workspace_set,
                    &normal_parents,
                    &id_to_label,
                    &id_to_name,
                ),
            })
            .collect();
        pins.sort_by(|a, b| a.version.cmp(&b.version));
        events.push(DuplicateVersionEvent {
            krate: name.to_string(),
            pins,
        });
    }
    events
}

/// One reverse step over Normal-kind edges. Workspace-direct parents
/// get a `(direct)` suffix on the bare crate name; non-workspace
/// parents are labelled `"name version"`. Sorted, deduplicated.
fn direct_blame(
    target_id: &str,
    workspace_set: &HashSet<&str>,
    normal_parents: &HashMap<&str, Vec<&str>>,
    id_to_label: &HashMap<&str, String>,
    id_to_name: &HashMap<&str, &str>,
) -> Vec<String> {
    let Some(parents) = normal_parents.get(target_id) else {
        return Vec::new();
    };
    let mut blame: BTreeSet<String> = BTreeSet::new();
    for &pid in parents {
        let label = if workspace_set.contains(pid) {
            let name = id_to_name.get(pid).copied().unwrap_or("");
            format!("{name} (direct)")
        } else {
            id_to_label.get(pid).cloned().unwrap_or_else(|| pid.to_string())
        };
        blame.insert(label);
    }
    blame.into_iter().collect()
}
