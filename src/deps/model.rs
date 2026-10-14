//! Shared Cargo metadata model and its indexed dependency graph.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;

use serde::{Deserialize, Deserializer};

/// Cargo's opaque package identity. Names and versions are not unique in a
/// resolved graph, so phase code keys graph operations by this type.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(transparent)]
pub(crate) struct PackageId(String);

impl From<&str> for PackageId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for PackageId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Cargo dependency kind shared by metadata edges, declared dependencies,
/// manifest tables, and dependency-rule diagnostics.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum DependencyKind {
    Normal,
    Dev,
    Build,
    // Metadata may grow a new kind. Metadata fields use
    // `deserialize_cargo_kind`, while config fields use this enum's strict
    // derived deserializer and cannot spell this sentinel.
    #[serde(skip_deserializing)]
    Unknown,
}

impl DependencyKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Dev => "dev",
            Self::Build => "build",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn from_metadata(value: Option<&str>) -> Self {
        match value {
            None | Some("normal") => Self::Normal,
            Some("dev") => Self::Dev,
            Some("build") => Self::Build,
            Some(_) => Self::Unknown,
        }
    }

    pub(crate) fn parse_config(value: &str) -> Option<Self> {
        match value {
            "normal" => Some(Self::Normal),
            "dev" => Some(Self::Dev),
            "build" => Some(Self::Build),
            _ => None,
        }
    }

    pub(crate) fn is_normal(self) -> bool {
        self == Self::Normal
    }
}

fn deserialize_cargo_kind<'de, D>(deserializer: D) -> Result<DependencyKind, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(DependencyKind::from_metadata(value.as_deref()))
}

fn normal_kind() -> DependencyKind {
    DependencyKind::Normal
}

#[derive(Deserialize)]
struct RawCargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<PackageId>,
    /// Cargo writes null under `--no-deps`.
    #[serde(default, deserialize_with = "null_as_default")]
    resolve: CargoResolve,
    #[serde(default)]
    workspace_root: String,
}

/// The metadata subset consumed by both dependency audits and
/// `[[dependency_rule]]`, with an index built once as JSON is deserialized.
pub(crate) struct CargoMetadata {
    pub(crate) packages: Vec<CargoPackage>,
    pub(crate) workspace_root: String,
    graph: Graph,
}

impl<'de> Deserialize<'de> for CargoMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawCargoMetadata::deserialize(deserializer)?;
        let graph = Graph::build(&raw.packages, &raw.workspace_members, &raw.resolve);
        Ok(Self {
            packages: raw.packages,
            workspace_root: raw.workspace_root,
            graph,
        })
    }
}

impl CargoMetadata {
    #[cfg(test)]
    pub(crate) fn new(
        packages: Vec<CargoPackage>,
        workspace_members: &[PackageId],
        resolve: &CargoResolve,
        workspace_root: String,
    ) -> Self {
        let graph = Graph::build(&packages, workspace_members, resolve);
        Self {
            packages,
            workspace_root,
            graph,
        }
    }

    pub(crate) fn is_member(&self, id: &PackageId) -> bool {
        self.graph.members.contains(id)
    }

    pub(crate) fn name(&self, id: &PackageId) -> Option<&str> {
        self.graph
            .package_indexes
            .get(id)
            .and_then(|&index| self.packages.get(index))
            .map(|package| package.name.as_str())
    }

    pub(crate) fn version(&self, id: &PackageId) -> Option<&str> {
        self.graph
            .package_indexes
            .get(id)
            .and_then(|&index| self.packages.get(index))
            .map(|package| package.version.as_str())
    }

    pub(crate) fn label(&self, id: &PackageId) -> String {
        match (self.name(id), self.version(id)) {
            (Some(name), Some(version)) => format!("{name} {version}"),
            _ => id.to_string(),
        }
    }

    pub(crate) fn normal_parents(&self, id: &PackageId) -> &[PackageId] {
        self.graph
            .normal_parents
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(crate) fn resolved_dependencies(&self, id: &PackageId) -> &[ResolvedDependency] {
        self.graph
            .resolved_dependencies
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(crate) fn source_kind(&self, id: &PackageId) -> PackageSourceKind {
        self.graph
            .source_kinds
            .get(id)
            .copied()
            .unwrap_or(PackageSourceKind::Path)
    }

    /// Breadth-first paths from `target` to workspace members over Normal
    /// edges. The path is tail-first (`target` to member). Focus passes false
    /// to report a member target as the trivial chain; blame passes true to
    /// continue through a member target and find workspace routes above it.
    /// Non-member nodes are visited once, keeping converging and cyclic graphs
    /// bounded while giving both callers one traversal policy.
    pub(crate) fn normal_paths_to_members(
        &self,
        target: &PackageId,
        include_target_member: bool,
    ) -> Vec<Vec<PackageId>> {
        if self.is_member(target) && !include_target_member {
            return vec![vec![target.clone()]];
        }

        let mut queue = VecDeque::new();
        queue.push_back(vec![target.clone()]);
        let mut visited = HashSet::new();
        visited.insert(target.clone());
        let mut paths = Vec::new();
        while let Some(path) = queue.pop_front() {
            let Some(last) = path.last() else {
                continue;
            };
            for parent in self.normal_parents(last) {
                if self.is_member(parent) {
                    let mut found = path.clone();
                    found.push(parent.clone());
                    paths.push(found);
                } else if visited.insert(parent.clone()) {
                    let mut next = path.clone();
                    next.push(parent.clone());
                    queue.push_back(next);
                }
            }
        }
        paths
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PackageSourceKind {
    Registry,
    Git,
    Path,
    Member,
}

/// Indexed package identities, membership, source provenance, and reverse
/// Normal-kind edges for one Cargo metadata snapshot.
pub(crate) struct Graph {
    package_indexes: HashMap<PackageId, usize>,
    members: HashSet<PackageId>,
    normal_parents: HashMap<PackageId, Vec<PackageId>>,
    resolved_dependencies: HashMap<PackageId, Vec<ResolvedDependency>>,
    source_kinds: HashMap<PackageId, PackageSourceKind>,
}

pub(crate) struct ResolvedDependency {
    pub(crate) package: PackageId,
    pub(crate) kind: DependencyKind,
}

impl Graph {
    fn build(packages: &[CargoPackage], members: &[PackageId], resolve: &CargoResolve) -> Self {
        let members: HashSet<PackageId> = members.iter().cloned().collect();
        let package_indexes: HashMap<PackageId, usize> = packages
            .iter()
            .enumerate()
            .map(|(index, package)| (package.id.clone(), index))
            .collect();
        let source_kinds = packages
            .iter()
            .map(|package| {
                let kind = if members.contains(&package.id) {
                    PackageSourceKind::Member
                } else {
                    match package.source.as_deref() {
                        None => PackageSourceKind::Path,
                        Some(source) if source.starts_with("git+") => PackageSourceKind::Git,
                        Some(_) => PackageSourceKind::Registry,
                    }
                };
                (package.id.clone(), kind)
            })
            .collect();

        let mut parents: HashMap<PackageId, BTreeSet<PackageId>> = HashMap::new();
        let mut edges: HashMap<PackageId, BTreeSet<(PackageId, DependencyKind)>> = HashMap::new();
        for node in &resolve.nodes {
            for dep in &node.deps {
                for dep_kind in &dep.dep_kinds {
                    edges
                        .entry(node.id.clone())
                        .or_default()
                        .insert((dep.pkg.clone(), dep_kind.kind));
                    if dep_kind.kind.is_normal() {
                        parents
                            .entry(dep.pkg.clone())
                            .or_default()
                            .insert(node.id.clone());
                    }
                }
            }
        }
        let normal_parents = parents
            .into_iter()
            .map(|(id, parents)| (id, parents.into_iter().collect()))
            .collect();
        let resolved_dependencies = edges
            .into_iter()
            .map(|(id, edges)| {
                (
                    id,
                    edges
                        .into_iter()
                        .map(|(package, kind)| ResolvedDependency { package, kind })
                        .collect(),
                )
            })
            .collect();

        Self {
            package_indexes,
            members,
            normal_parents,
            resolved_dependencies,
            source_kinds,
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct CargoPackage {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) version: String,
    pub(crate) id: PackageId,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) manifest_path: String,
    #[serde(default)]
    pub(crate) links: Option<String>,
    #[serde(default)]
    pub(crate) publish: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) dependencies: Vec<PackageDep>,
}

/// One entry of `packages[].dependencies`.
#[derive(Deserialize)]
pub(crate) struct PackageDep {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) rename: Option<String>,
    #[serde(default = "normal_kind", deserialize_with = "deserialize_cargo_kind")]
    pub(crate) kind: DependencyKind,
    #[serde(default)]
    pub(crate) target: Option<String>,
    #[serde(default)]
    pub(crate) optional: bool,
    /// Cargo renders "none specified" - the path-only form that publish strips
    /// from a dev-dep - as `*`.
    #[serde(default)]
    pub(crate) req: String,
}

#[derive(Deserialize, Default)]
pub(crate) struct CargoResolve {
    pub(crate) nodes: Vec<ResolveNode>,
}

#[derive(Deserialize)]
pub(crate) struct ResolveNode {
    pub(crate) id: PackageId,
    #[serde(default)]
    pub(crate) deps: Vec<NodeDep>,
}

#[derive(Deserialize)]
pub(crate) struct NodeDep {
    pub(crate) pkg: PackageId,
    #[serde(default)]
    pub(crate) dep_kinds: Vec<DepKind>,
}

#[derive(Deserialize)]
pub(crate) struct DepKind {
    #[serde(default = "normal_kind", deserialize_with = "deserialize_cargo_kind")]
    pub(crate) kind: DependencyKind,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn metadata_deserialization_builds_one_typed_graph() {
        let json = serde_json::json!({
            "packages": [
                { "name": "app", "id": "member" },
                { "name": "transitive", "id": "transitive" },
                { "name": "leaf", "id": "leaf" },
                { "name": "dev-only", "id": "dev-only" },
                { "name": "registry-crate", "id": "registry", "source": "registry+https://example.com/index" },
                { "name": "git-crate", "id": "git", "source": "git+https://example.com/repo#abc" },
                { "name": "path-crate", "id": "path" }
            ],
            "workspace_members": ["member"],
            "resolve": {
                "nodes": [
                    { "id": "transitive", "deps": [
                        { "pkg": "leaf", "dep_kinds": [{ "kind": null }] }
                    ] },
                    { "id": "member", "deps": [
                        { "pkg": "transitive", "dep_kinds": [{ "kind": null }] },
                        { "pkg": "dev-only", "dep_kinds": [{ "kind": "dev" }] }
                    ] }
                ]
            },
            "workspace_root": "/workspace"
        });
        let metadata: CargoMetadata = serde_json::from_value(json).unwrap();

        assert!(metadata.is_member(&PackageId::from("member")));
        assert_eq!(
            metadata.normal_parents(&PackageId::from("leaf")),
            &[PackageId::from("transitive")]
        );
        assert!(
            metadata
                .normal_parents(&PackageId::from("dev-only"))
                .is_empty()
        );
        assert_eq!(
            metadata.source_kind(&PackageId::from("member")),
            PackageSourceKind::Member
        );
        assert_eq!(
            metadata.source_kind(&PackageId::from("registry")),
            PackageSourceKind::Registry
        );
        assert_eq!(
            metadata.source_kind(&PackageId::from("git")),
            PackageSourceKind::Git
        );
        assert_eq!(
            metadata.source_kind(&PackageId::from("path")),
            PackageSourceKind::Path
        );
        assert_eq!(
            metadata.normal_paths_to_members(&PackageId::from("leaf"), false),
            vec![vec![
                PackageId::from("leaf"),
                PackageId::from("transitive"),
                PackageId::from("member"),
            ]]
        );
    }
}
