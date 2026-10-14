//! `publish_cycle` phase.
//!
//! A workspace can happily build and test with a dependency cycle among
//! its own members, because `cargo build` resolves the whole graph at
//! once. `cargo publish` cannot: it uploads one crate at a time, and
//! every dependency named in the published manifest must already exist
//! on the registry. A cycle among publishable members therefore has no
//! valid publication order, and the failure only surfaces at release
//! time - long after the PR that introduced it merged.
//!
//! The subtlety is dev-dependencies. Cargo *strips* a dev-dependency
//! from the published manifest when it carries no version requirement
//! (the `{ path = "..." }`-only form), precisely so intra-workspace test
//! helpers can point back at their dependents. Such an edge exists for
//! the local build and vanishes on publish, so it must not count here.
//! A dev-dependency that also names a version does get published, and
//! does close the cycle - that is the one worth flagging, and deleting
//! the `version` key is usually enough, without restructuring crates.
//!
//! That last part is a claim about *cargo publish*, and the renderer says
//! so rather than stating it flat. A project whose release path is its own
//! planner may order the graph off local `path =` edges regardless of
//! dependency kind (nautilus_trader's `scripts/ci/publish-cargo-crates.sh`
//! does exactly this, deliberately), and such a planner still sees a
//! path-only dev-dep. The detection is unaffected either way - the edge
//! genuinely does vanish from the published manifest - but the advice is
//! consumer-specific, so it ships with its caveat attached.
//!
//! Members with `publish = false` are excluded entirely: they are never
//! uploaded, so no edge into or out of them can block a release.
//!
//! This phase reads the declared manifests (`packages[].dependencies`)
//! rather than the resolve graph, because publication is a property of
//! what the manifest says, not of what the resolver picked.
//!
//! The enumeration is **not exhaustive**, by choice - see
//! [`INVENTORY_CAVEAT`], which every report prints. Exhaustiveness would
//! cost either the `visited` pruning (exponential worst case) or a
//! Johnson's-algorithm implementation, and buys nothing for gating; the
//! caveat covers the scoping case that the missing cycles would mislead.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::CargoMetadata;

/// A dependency cycle among publishable workspace members. `members` is
/// the loop in walk order, rotated to start at its lexicographically
/// smallest member; `edges` has one entry per member, closing back to
/// `members[0]`.
#[derive(Serialize)]
pub(crate) struct PublishCycleEvent {
    pub(super) members: Vec<String>,
    pub(super) edges: Vec<CycleEdge>,
}

#[derive(Serialize)]
pub(super) struct CycleEdge {
    pub(super) from: String,
    pub(super) to: String,
    /// `normal`, `build`, or `dev` (a dev-dep carrying a version, the only
    /// kind that survives into the published manifest).
    pub(super) kind: PublishedEdge,
}

/// How a cycle edge got into the published manifest. `DevVersioned` is the
/// publish-specific refinement of an ordinary dev-dependency.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) enum PublishedEdge {
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "build")]
    Build,
    #[serde(rename = "dev")]
    DevVersioned,
}

impl PublishedEdge {
    fn rank(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Build => 1,
            Self::DevVersioned => 2,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Build => "build",
            Self::DevVersioned => "dev",
        }
    }
}

pub fn run(metadata: &CargoMetadata) -> Vec<PublishCycleEvent> {
    // Publishable workspace members, by package name. `publish` is
    // `Some([])` for `publish = false`, `Some([registry, ...])` for an
    // allow-list, `None` for the unrestricted default.
    let mut publishable: HashMap<&str, &super::CargoPackage> = HashMap::new();
    for pkg in &metadata.packages {
        if !metadata.is_member(&pkg.id) {
            continue;
        }
        if pkg.publish.as_ref().is_some_and(Vec::is_empty) {
            continue;
        }
        publishable.insert(pkg.name.as_str(), pkg);
    }

    // Adjacency over publishable members only, keyed by name. Parallel
    // edges (the same pair declared as both normal and dev, say) collapse
    // to the first one seen in kind order, so the report names the edge
    // that would actually have to change.
    let mut edges: HashMap<&str, Vec<(&str, PublishedEdge)>> = HashMap::new();
    for (name, pkg) in &publishable {
        let mut out: Vec<(&str, PublishedEdge)> = Vec::new();
        for dep in &pkg.dependencies {
            let kind = match dep.kind {
                super::DependencyKind::Normal => PublishedEdge::Normal,
                super::DependencyKind::Build => PublishedEdge::Build,
                // A version-less dev-dep is stripped on publish and so
                // cannot participate in a publication cycle. Cargo
                // renders "no version requirement" as `*`.
                super::DependencyKind::Dev if dep.req == "*" => continue,
                super::DependencyKind::Dev => PublishedEdge::DevVersioned,
                super::DependencyKind::Unknown => continue,
            };
            let Some((target, _)) = publishable.get_key_value(dep.name.as_str()) else {
                continue;
            };
            if *target == *name {
                continue;
            }
            if let Some(slot) = out.iter_mut().find(|(t, _)| t == target) {
                // Prefer the structural kind in the report: a normal edge
                // can't be fixed by deleting a version key.
                if kind.rank() < slot.1.rank() {
                    slot.1 = kind;
                }
            } else {
                out.push((*target, kind));
            }
        }
        out.sort_by(|a, b| a.0.cmp(b.0));
        edges.insert(name, out);
    }

    let mut roots: Vec<&str> = publishable.keys().copied().collect();
    roots.sort_unstable();

    let mut seen: HashSet<Vec<&str>> = HashSet::new();
    let mut events = Vec::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    let mut visited: HashSet<&str> = HashSet::new();
    for root in roots {
        walk(
            root,
            &edges,
            &mut stack,
            &mut on_stack,
            &mut visited,
            &mut seen,
            &mut events,
        );
    }
    events.sort_by(|a, b| a.members.cmp(&b.members));
    events
}

/// Lower binds tighter when two edges join the same pair.
/// Depth-first walk that reports one cycle per back edge. `visited`
/// keeps the walk linear in the common acyclic case; a node is only
/// marked visited once its whole subtree is explored, so a cycle
/// reachable from two roots is still found from the first.
fn walk<'a>(
    node: &'a str,
    edges: &HashMap<&'a str, Vec<(&'a str, PublishedEdge)>>,
    stack: &mut Vec<&'a str>,
    on_stack: &mut HashSet<&'a str>,
    visited: &mut HashSet<&'a str>,
    seen: &mut HashSet<Vec<&'a str>>,
    events: &mut Vec<PublishCycleEvent>,
) {
    if visited.contains(node) {
        return;
    }
    stack.push(node);
    on_stack.insert(node);
    for (next, kind) in edges.get(node).map(Vec::as_slice).unwrap_or_default() {
        if on_stack.contains(next) {
            let start = stack.iter().position(|n| n == next).unwrap_or(0);
            let cycle: Vec<&str> = stack[start..].to_vec();
            let mut kinds: Vec<PublishedEdge> = cycle
                .windows(2)
                .map(|pair| edge_kind(edges, pair[0], pair[1]))
                .collect();
            kinds.push(*kind);
            record(&cycle, &kinds, seen, events);
        } else {
            walk(next, edges, stack, on_stack, visited, seen, events);
        }
    }
    on_stack.remove(node);
    stack.pop();
    visited.insert(node);
}

fn edge_kind<'a>(
    edges: &HashMap<&'a str, Vec<(&'a str, PublishedEdge)>>,
    from: &str,
    to: &str,
) -> PublishedEdge {
    edges
        .get(from)
        .and_then(|outs| outs.iter().find(|(t, _)| *t == to))
        .map(|(_, k)| *k)
        .unwrap_or(PublishedEdge::Normal)
}

/// Rotate the cycle to start at its lexicographically smallest member so
/// the same loop discovered from different roots dedups to one event.
fn record<'a>(
    cycle: &[&'a str],
    kinds: &[PublishedEdge],
    seen: &mut HashSet<Vec<&'a str>>,
    events: &mut Vec<PublishCycleEvent>,
) {
    let Some(offset) = (0..cycle.len()).min_by_key(|i| cycle[*i]) else {
        return;
    };
    let members: Vec<&str> = cycle[offset..]
        .iter()
        .chain(cycle[..offset].iter())
        .copied()
        .collect();
    if !seen.insert(members.clone()) {
        return;
    }
    let rotated_kinds: Vec<PublishedEdge> = kinds[offset..]
        .iter()
        .chain(kinds[..offset].iter())
        .copied()
        .collect();
    let edge_list = members
        .iter()
        .enumerate()
        .map(|(i, from)| CycleEdge {
            from: (*from).to_string(),
            to: members[(i + 1) % members.len()].to_string(),
            kind: rotated_kinds[i],
        })
        .collect();
    events.push(PublishCycleEvent {
        members: members.into_iter().map(str::to_string).collect(),
        edges: edge_list,
    });
}

/// Printed once per report whenever any cycle is found, by both `deps`
/// and the `check` phase.
///
/// The walk reports one cycle per back edge and prunes on `visited`, so a
/// back edge into an already-finished node is never examined: several
/// loops through one member can surface one at a time. That is harmless
/// for gating - any cycle fails the run - but actively misleading for
/// *scoping*, which is the decision this output actually feeds. Reading a
/// lone finding as the complete inventory is how you pick "delete this
/// edge" when the answer was "restructure the crate". So the report says
/// what it is rather than implying an exhaustiveness it doesn't have.
///
/// Deliberately command-neutral: `deps` prints it while the reader is
/// already in `deps`, so naming that command here would be noise. The
/// `check` phase appends its own pointer to `deps` instead - that is the
/// investigative command, and a `check` reader is one step further away.
pub const INVENTORY_CAVEAT: &str = "this inventory may be partial - cycles sharing a member can surface one at a time, so re-run after each fix rather than scoping the fix to what is listed here";

/// Render one cycle as the chain line, followed by the one-line fix for
/// each versioned dev-dep edge - that edge is removable without
/// restructuring anything, so it is the actionable part of the report.
///
/// Returned unindented, so each caller supplies its own: `deps` prefixes
/// two spaces per section convention, the `check` phase folds the lines
/// into one error block.
pub fn render_lines(c: &PublishCycleEvent) -> Vec<String> {
    let mut chain = String::new();
    for edge in &c.edges {
        chain.push_str(&edge.from);
        if edge.kind == PublishedEdge::Normal {
            chain.push_str(" -> ");
        } else {
            chain.push_str(&format!(" -[{}]-> ", edge.kind.as_str()));
        }
    }
    // Close the loop back onto its first member: `edges` has one entry per
    // member, so the last edge's `to` is `edges[0].from`.
    chain.push_str(&c.edges[0].from);

    let mut lines = vec![chain];
    for edge in c
        .edges
        .iter()
        .filter(|e| e.kind == PublishedEdge::DevVersioned)
    {
        lines.push(format!(
            "  dev-dependency {} -> {} names a version, so cargo publish keeps it; dropping the version key leaves a path-only dev-dep that publish strips",
            edge.from, edge.to,
        ));
        lines.push(
            "  that fix only helps a consumer that keys on the published manifest - a release planner ordering every `path =` edge regardless of kind still sees this one, and needs the dependency removed outright"
                .to_string(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::model::{CargoResolve, PackageDep};
    use crate::deps::{CargoMetadata, CargoPackage};

    fn pkg(name: &str, deps: Vec<PackageDep>) -> CargoPackage {
        CargoPackage {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            id: format!("path+file:///{name}#0.1.0").into(),
            source: None,
            manifest_path: format!("/ws/{name}/Cargo.toml"),
            links: None,
            publish: None,
            dependencies: deps,
        }
    }

    fn dep(name: &str, kind: Option<&str>, req: &str) -> PackageDep {
        use crate::deps::DependencyKind;
        let kind = kind.map_or(DependencyKind::Normal, |value| {
            DependencyKind::from_metadata(Some(value))
        });
        PackageDep {
            name: name.to_string(),
            rename: None,
            kind,
            target: None,
            optional: false,
            req: req.to_string(),
        }
    }

    fn meta(packages: Vec<CargoPackage>) -> CargoMetadata {
        let members: Vec<_> = packages.iter().map(|p| p.id.clone()).collect();
        CargoMetadata::new(
            packages,
            &members,
            &CargoResolve { nodes: Vec::new() },
            "/ws".to_string(),
        )
    }

    #[test]
    fn acyclic_workspace_is_clean() {
        let m = meta(vec![
            pkg("a", vec![dep("b", None, "0.1")]),
            pkg("b", vec![]),
        ]);
        assert!(run(&m).is_empty());
    }

    #[test]
    fn versionless_dev_dep_does_not_close_a_cycle() {
        let m = meta(vec![
            pkg("exec", vec![dep("testkit", Some("dev"), "*")]),
            pkg("testkit", vec![dep("trading", None, "0.1")]),
            pkg("trading", vec![dep("exec", None, "0.1")]),
        ]);
        assert!(run(&m).is_empty());
    }

    #[test]
    fn versioned_dev_dep_closes_the_cycle() {
        let m = meta(vec![
            pkg("exec", vec![dep("testkit", Some("dev"), "0.1")]),
            pkg("testkit", vec![dep("trading", None, "0.1")]),
            pkg("trading", vec![dep("exec", None, "0.1")]),
        ]);
        let events = run(&m);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].members, vec!["exec", "testkit", "trading"]);
        let kinds: Vec<&str> = events[0]
            .edges
            .iter()
            .map(|edge| edge.kind.as_str())
            .collect();
        assert_eq!(kinds, vec!["dev", "normal", "normal"]);
    }

    #[test]
    fn unpublished_member_cannot_be_in_a_cycle() {
        let mut packages = vec![
            pkg("exec", vec![dep("testkit", Some("dev"), "0.1")]),
            pkg("testkit", vec![dep("trading", None, "0.1")]),
            pkg("trading", vec![dep("exec", None, "0.1")]),
        ];
        packages[1].publish = Some(Vec::new());
        assert!(run(&meta(packages)).is_empty());
    }

    #[test]
    fn overlapping_cycles_are_why_the_caveat_exists() {
        // `a -> b -> a` and `a -> c -> b -> a` both exist. The walk
        // finishes `b` while exploring the first, so the second's back
        // edge is never examined and only one cycle is reported. Pinned
        // as the documented shape of the limitation, not as desired
        // behaviour - if this ever reports two, the caveat can go.
        let m = meta(vec![
            pkg("a", vec![dep("b", None, "0.1"), dep("c", None, "0.1")]),
            pkg("b", vec![dep("a", None, "0.1")]),
            pkg("c", vec![dep("b", None, "0.1")]),
        ]);
        let events = run(&m);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].members, vec!["a", "b"]);
        assert!(INVENTORY_CAVEAT.contains("re-run after each fix"));
    }

    #[test]
    fn cycle_dedups_across_roots() {
        let m = meta(vec![
            pkg("z", vec![dep("a", None, "0.1")]),
            pkg("a", vec![dep("b", None, "0.1")]),
            pkg("b", vec![dep("a", None, "0.1")]),
        ]);
        let events = run(&m);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].members, vec!["a", "b"]);
    }

    #[test]
    fn self_dev_dep_is_not_a_cycle() {
        let m = meta(vec![pkg("a", vec![dep("a", Some("dev"), "0.1")])]);
        assert!(run(&m).is_empty());
    }

    #[test]
    fn render_names_the_chain_and_the_removable_edge() {
        let m = meta(vec![
            pkg("exec", vec![dep("testkit", Some("dev"), "0.1")]),
            pkg("testkit", vec![dep("trading", None, "0.1")]),
            pkg("trading", vec![dep("exec", None, "0.1")]),
        ]);
        let events = run(&m);
        let lines = render_lines(&events[0]);
        assert_eq!(lines[0], "exec -[dev]-> testkit -> trading -> exec");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("dev-dependency exec -> testkit names a version"));
        // The caveat travels with the fix, never alone: a planner keying on
        // local `path =` edges is unmoved by deleting a version key.
        assert!(lines[2].contains("keys on the published manifest"));
    }

    #[test]
    fn render_of_an_all_normal_cycle_is_the_chain_alone() {
        let m = meta(vec![
            pkg("a", vec![dep("b", None, "0.1")]),
            pkg("b", vec![dep("a", None, "0.1")]),
        ]);
        let lines = render_lines(&run(&m)[0]);
        assert_eq!(lines, vec!["a -> b -> a"]);
    }

    #[test]
    fn build_dep_closes_a_cycle() {
        let m = meta(vec![
            pkg("a", vec![dep("b", Some("build"), "0.1")]),
            pkg("b", vec![dep("a", None, "0.1")]),
        ]);
        let events = run(&m);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].edges[0].kind.as_str(), "build");
    }
}
