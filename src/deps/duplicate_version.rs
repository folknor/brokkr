//! `duplicate_version` phase.
//!
//! Finds crate names with >=2 resolved versions in `cargo metadata`. For
//! each duplicate, computes blame: the workspace-direct dep(s) that anchor
//! each version, plus the chain(s) from a workspace member to that
//! `(name, version)` instance.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

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

    // Reverse adjacency: for each id, who depends on it.
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in &metadata.resolve.nodes {
        for dep_id in &node.dependencies {
            parents
                .entry(dep_id.as_str())
                .or_default()
                .push(node.id.as_str());
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
            .map(|pkg| {
                let (blame, paths) = blame_for(
                    pkg.id.as_str(),
                    &workspace_set,
                    &parents,
                    &id_to_label,
                    &id_to_name,
                );
                VersionPin {
                    version: pkg.version.clone(),
                    direct_blame: blame,
                    paths,
                }
            })
            .collect();
        // Stable order: by version string. Good enough for v1; full semver
        // sort can come later.
        pins.sort_by(|a, b| a.version.cmp(&b.version));
        events.push(DuplicateVersionEvent {
            krate: name.to_string(),
            pins,
        });
    }
    events
}

/// Backwards BFS from `target_id` to every workspace member, returning
/// (direct-blame anchors, labeled paths). Each path is `[ws, ..., target]`
/// using `"name version"` labels.
fn blame_for(
    target_id: &str,
    workspace_set: &HashSet<&str>,
    parents: &HashMap<&str, Vec<&str>>,
    id_to_label: &HashMap<&str, String>,
    id_to_name: &HashMap<&str, &str>,
) -> (Vec<String>, Vec<Vec<String>>) {
    // Paths are built tail-first: each queue entry is target -> ... -> ws.
    // We reverse for output. Workspace members terminate a branch but are
    // *not* added to `visited`, so multiple paths can reach the same
    // workspace member through different intermediaries.
    let mut queue: VecDeque<Vec<&str>> = VecDeque::new();
    queue.push_back(vec![target_id]);
    let mut visited: HashSet<&str> = HashSet::new();
    visited.insert(target_id);
    let mut found: Vec<Vec<&str>> = Vec::new();

    while let Some(path) = queue.pop_front() {
        let last = *path.last().expect("path is non-empty by construction");
        if workspace_set.contains(last) && path.len() > 1 {
            found.push(path);
            continue;
        }
        let Some(ps) = parents.get(last) else {
            continue;
        };
        for &p in ps {
            // Cycle prevention for non-workspace nodes only. Workspace
            // members aren't marked visited so multiple paths can land
            // on the same one.
            if !workspace_set.contains(p) && !visited.insert(p) {
                continue;
            }
            let mut next = path.clone();
            next.push(p);
            queue.push_back(next);
        }
    }

    let mut blame: BTreeSet<String> = BTreeSet::new();
    let mut labeled_paths: Vec<Vec<String>> = Vec::new();
    for path in &found {
        let labeled: Vec<String> = path
            .iter()
            .rev()
            .map(|&id| id_to_label.get(id).cloned().unwrap_or_else(|| id.to_string()))
            .collect();

        let anchor = if labeled.len() == 2 {
            // [ws, target] - workspace depends directly on target.
            let ws_name = id_to_name.get(path[path.len() - 1]).copied().unwrap_or("");
            format!("{ws_name} (direct)")
        } else {
            // [ws, mid, ..., target] - first non-workspace hop is the
            // blame anchor. After reversal that's labeled[1].
            labeled[1].clone()
        };
        blame.insert(anchor);
        labeled_paths.push(labeled);
    }

    (blame.into_iter().collect(), labeled_paths)
}
