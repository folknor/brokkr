//! `duplicate_version` phase.
//!
//! Finds crate names with >=2 resolved versions in `cargo metadata` and
//! for each version reports two things:
//!
//! - `picked_by` - the direct parents of the pin (one reverse step
//!   over Normal-kind edges, host-target filtered). Workspace members
//!   appear with a `(direct)` tag. This is "what crate's resolver
//!   landed on this version".
//! - `via_workspace` - workspace-direct dep names that lead to the pin
//!   via chains whose immediate pinner is transitive. Empty when every
//!   pinner is already a workspace member or workspace-direct dep,
//!   since `picked_by` already names what to bump in that case. This
//!   is the "what should I bump in Cargo.toml" hint, computed once so
//!   the reader doesn't have to run `cargo tree -i` to figure it out.
//!
//! Target gating is handled upstream by loading metadata with
//! `--filter-platform=<host>`; this module only filters dep kinds.

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
            .map(|pkg| {
                let picked_by = picked_by(
                    pkg.id.as_str(),
                    &workspace_set,
                    &normal_parents,
                    &id_to_label,
                    &id_to_name,
                );
                let via_workspace = via_workspace(
                    pkg.id.as_str(),
                    name,
                    &workspace_set,
                    &normal_parents,
                    &id_to_name,
                    &picked_by,
                );
                VersionPin {
                    version: pkg.version.clone(),
                    picked_by,
                    via_workspace,
                }
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
fn picked_by(
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

/// Workspace-direct dep names that lead to the pin. Computed by
/// walking up every Normal-kind chain from `target_id` to a workspace
/// member and recording the workspace member's direct dep on each
/// chain (the second-to-last id in tail-first order). Filtered to
/// drop names already in `picked_by` (so they aren't repeated) and
/// the dup's own name (which appears when a workspace member depends
/// directly on the dup - already conveyed by the `(direct)` tag).
fn via_workspace(
    target_id: &str,
    target_name: &str,
    workspace_set: &HashSet<&str>,
    normal_parents: &HashMap<&str, Vec<&str>>,
    id_to_name: &HashMap<&str, &str>,
    picked_by_labels: &[String],
) -> Vec<String> {
    // Bare names from the picker list, with any `(direct)` suffix or
    // version suffix stripped. Used to dedup so we don't repeat names
    // already on the same line.
    let picked_names: HashSet<&str> = picked_by_labels
        .iter()
        .map(|s| s.split_whitespace().next().unwrap_or(""))
        .collect();

    let mut via: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<Vec<&str>> = VecDeque::new();
    queue.push_back(vec![target_id]);
    let mut visited: HashSet<&str> = HashSet::new();
    visited.insert(target_id);

    while let Some(path) = queue.pop_front() {
        let last = *path.last().expect("path is non-empty by construction");
        if workspace_set.contains(last) && path.len() > 1 {
            // Need at least one hop between workspace member and dup:
            // length-2 chains (ws -> dup directly) are already covered
            // by the `(direct)` tag on the picker.
            if path.len() >= 3 {
                let ws_anchor_id = path[path.len() - 2];
                let anchor_name = *id_to_name.get(ws_anchor_id).unwrap_or(&"");
                if anchor_name != target_name && !picked_names.contains(anchor_name) {
                    via.insert(anchor_name.to_string());
                }
            }
            continue;
        }
        let Some(ps) = normal_parents.get(last) else {
            continue;
        };
        for &p in ps {
            if !workspace_set.contains(p) && !visited.insert(p) {
                continue;
            }
            let mut next = path.clone();
            next.push(p);
            queue.push_back(next);
        }
    }
    via.into_iter().collect()
}
