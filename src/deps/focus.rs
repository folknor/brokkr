//! `brokkr deps <pkg>` - chain trace for one package.
//!
//! Given a spec like `"indexmap"` or `"hashbrown@0.17.1"`, walks the
//! resolve graph backwards over Normal-kind edges and prints every
//! distinct chain from a workspace member down to the target. Uses
//! host-filtered metadata by default so we don't surface chains that
//! only exist for inactive targets. If the spec doesn't resolve in the
//! host-filtered graph at all, falls back to the unfiltered graph and
//! says so explicitly - silent empty output is the worst case.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

use super::{
    load_metadata, load_metadata_host_filtered, CargoMetadata, CargoPackage,
};
use crate::error::DevError;
use crate::output;

pub(super) fn run_focus(
    project_root: &Path,
    spec: &str,
    json: bool,
) -> Result<(), DevError> {
    let (name, version) = parse_spec(spec);
    let host_md = load_metadata_host_filtered(project_root)?;

    let (md, fell_back) = if has_match(&host_md, name, version) {
        (host_md, false)
    } else {
        let unfiltered = load_metadata(project_root)?;
        if !has_match(&unfiltered, name, version) {
            return Err(DevError::Build(format!(
                "no package named {spec:?} in this workspace's resolve graph"
            )));
        }
        (unfiltered, true)
    };

    let traces = trace(&md, name, version);

    if json {
        for t in &traces {
            let line = serde_json::to_string(t)?;
            println!("{line}");
        }
        return Ok(());
    }

    if fell_back {
        output::deps_msg(&format!(
            "{spec}: not in host-filtered graph; showing all-target chains"
        ));
    }
    for t in &traces {
        output::deps_msg(&format!("{} {} ({} chains)", t.krate, t.version, t.chains.len()));
        if t.chains.is_empty() {
            output::deps_msg("  (no parents - package is a workspace member or unreachable)");
            continue;
        }
        for chain in &t.chains {
            output::deps_msg(&format!("  {}", chain.join(" -> ")));
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub struct ChainTrace {
    #[serde(rename = "crate")]
    pub krate: String,
    pub version: String,
    /// Chains ordered workspace-root first, target last.
    pub chains: Vec<Vec<String>>,
}

fn parse_spec(spec: &str) -> (&str, Option<&str>) {
    match spec.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (spec, None),
    }
}

fn has_match(md: &CargoMetadata, name: &str, version: Option<&str>) -> bool {
    md.packages.iter().any(|p| pkg_matches(p, name, version))
}

fn pkg_matches(p: &CargoPackage, name: &str, version: Option<&str>) -> bool {
    p.name == name && version.is_none_or(|v| p.version == v)
}

fn trace(md: &CargoMetadata, name: &str, version: Option<&str>) -> Vec<ChainTrace> {
    let workspace_set: HashSet<&str> = md
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let id_to_label: HashMap<&str, String> = md
        .packages
        .iter()
        .map(|p| (p.id.as_str(), format!("{} {}", p.name, p.version)))
        .collect();

    // Reverse adjacency over Normal-kind edges only - matches the
    // duplicate_version blame filter so the chains shown here line up
    // with the blame anchors shown by the full report.
    let mut normal_parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in &md.resolve.nodes {
        for d in &node.deps {
            if d.dep_kinds.iter().any(|dk| dk.kind.is_none()) {
                normal_parents
                    .entry(d.pkg.as_str())
                    .or_default()
                    .push(node.id.as_str());
            }
        }
    }

    let mut out = Vec::new();
    let mut targets: Vec<&CargoPackage> = md
        .packages
        .iter()
        .filter(|p| pkg_matches(p, name, version))
        .collect();
    targets.sort_by(|a, b| a.version.cmp(&b.version));

    for pkg in targets {
        let chains = chains_to_workspace(
            pkg.id.as_str(),
            &workspace_set,
            &normal_parents,
            &id_to_label,
        );
        out.push(ChainTrace {
            krate: pkg.name.clone(),
            version: pkg.version.clone(),
            chains,
        });
    }
    out
}

/// BFS upward from `target_id`. Each branch ends when it reaches a
/// workspace member. Cycle prevention only applies to non-workspace
/// nodes so distinct paths can converge on the same workspace root.
fn chains_to_workspace(
    target_id: &str,
    workspace_set: &HashSet<&str>,
    normal_parents: &HashMap<&str, Vec<&str>>,
    id_to_label: &HashMap<&str, String>,
) -> Vec<Vec<String>> {
    // Special case: the target itself is a workspace member. No upward
    // walk to do; report the trivial chain.
    if workspace_set.contains(target_id) {
        return vec![vec![label(target_id, id_to_label)]];
    }

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

    // Convert tail-first paths into root-first labelled chains, dedup.
    let mut chains: BTreeSet<Vec<String>> = BTreeSet::new();
    for path in found {
        let labelled: Vec<String> = path
            .iter()
            .rev()
            .map(|&id| label(id, id_to_label))
            .collect();
        chains.insert(labelled);
    }
    chains.into_iter().collect()
}

fn label(id: &str, id_to_label: &HashMap<&str, String>) -> String {
    id_to_label.get(id).cloned().unwrap_or_else(|| id.to_string())
}
