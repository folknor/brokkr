//! `duplicate_version` phase.
//!
//! Finds crate names with >=2 resolved versions in `cargo metadata` and
//! reports direct Normal-kind parents plus workspace routes that can be
//! changed to influence each pin. The indexed metadata graph owns package
//! identity, membership, and the shared breadth-first traversal.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::Serialize;

use super::{CargoMetadata, CargoPackage, PackageId};

#[derive(Serialize)]
pub(super) struct DuplicateVersionEvent {
    #[serde(rename = "crate")]
    pub(super) krate: String,
    pub(super) pins: Vec<VersionPin>,
}

#[derive(Serialize)]
pub(super) struct VersionPin {
    pub(super) version: String,
    /// Direct Normal-kind parents. `direct` identifies workspace members;
    /// `version` is absent for those members, so consumers do not need to
    /// parse the prose used by the text renderer.
    pub(super) picked_by: Vec<Picker>,
    /// Workspace-direct dep names that lead to this pin through a transitive
    /// immediate parent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) via_workspace: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct Picker {
    pub(super) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) version: Option<String>,
    pub(super) direct: bool,
}

impl Picker {
    pub(super) fn label(&self) -> String {
        if self.direct {
            format!("{} (direct)", self.name)
        } else if let Some(version) = &self.version {
            format!("{} {version}", self.name)
        } else {
            self.name.clone()
        }
    }
}

pub fn run(metadata: &CargoMetadata) -> Vec<DuplicateVersionEvent> {
    // Group package IDs by crate name.
    let mut by_name: BTreeMap<&str, Vec<&CargoPackage>> = BTreeMap::new();
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
                let picked_by = picked_by(metadata, &pkg.id);
                let via_workspace = via_workspace(metadata, &pkg.id, name, &picked_by);
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

/// One reverse step over Normal-kind edges, sorted and deduplicated by the
/// typed fields that the JSON event exposes.
fn picked_by(metadata: &CargoMetadata, target_id: &PackageId) -> Vec<Picker> {
    let mut blame: BTreeMap<(String, Option<String>, bool), Picker> = BTreeMap::new();
    for parent in metadata.normal_parents(target_id) {
        let direct = metadata.is_member(parent);
        let name = metadata
            .name(parent)
            .map(str::to_owned)
            .unwrap_or_else(|| parent.to_string());
        let version = (!direct)
            .then(|| metadata.version(parent).map(str::to_owned))
            .flatten();
        blame
            .entry((name.clone(), version.clone(), direct))
            .or_insert(Picker {
                name,
                version,
                direct,
            });
    }
    blame.into_values().collect()
}

/// Workspace-direct dep names that lead to the pin. The graph walker returns
/// tail-first paths from the pin to a member; the second-to-last package is
/// the workspace's direct anchor. Names already present in `picked_by` and
/// the duplicate's own name are omitted.
fn via_workspace(
    metadata: &CargoMetadata,
    target_id: &PackageId,
    target_name: &str,
    picked_by: &[Picker],
) -> Vec<String> {
    let picked_names: HashSet<&str> = picked_by
        .iter()
        .map(|picker| picker.name.as_str())
        .collect();
    let mut via = BTreeSet::new();
    for path in metadata.normal_paths_to_members(target_id, true) {
        if path.len() < 3 {
            continue;
        }
        let anchor = &path[path.len() - 2];
        let anchor_name = metadata.name(anchor).unwrap_or("");
        if anchor_name != target_name && !picked_names.contains(anchor_name) {
            via.insert(anchor_name.to_owned());
        }
    }
    via.into_iter().collect()
}
