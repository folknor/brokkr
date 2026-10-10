// Feature routing: which of a sweep's configured `--features` tokens one cargo
// run, over one concrete package set, can carry.
//
// A sweep's `features` are written against the sweep's OWN selection: a sweep
// over `vm`, `strategy` and `harness` may list `vm/opcode-counts` and
// `strategy/hotpath`, and the full run is fine because some selected package
// routes every token. Narrow that run - `brokkr check -p strategy`, or a
// package-mode resolution of one package - and cargo rejects every token no
// selected package routes ("the package does not contain these features").
// Projection removes exactly those tokens, and only those: a token that NO
// package of the sweep's own domain routes either is kept, so cargo rejects it
// with its own message, exactly as the full run would.
//
// The routing rule is cargo's own (resolver 2, `collect_matching_features` in
// cargo's `workspace.rs`), for a selected set P and a token:
//
// - bare `f`: some p in P has a feature `f`, or an OPTIONAL declared
//   dependency (any kind, any target) whose name - `rename` or else `name`,
//   manifest spelling, hyphens kept - is `f` (its implicit feature).
// - `x/f` or `x?/f`: some p in P declares a dependency (any kind, any target,
//   optional or not) named `x`, or p IS `x` and recognises `f` as its own
//   feature by the bare rule.
// - anything else (`dep:x`, more than one slash, an empty half) is malformed:
//   it routes nowhere, and is never dropped - cargo owns that error.
//
// What this module does NOT do: decide which runs exist, or which packages a
// run selects. That is `selection.rs`, which calls `project` once per planned
// request and keeps the answer on the request.

mod features {
    use std::cell::OnceCell;
    use std::collections::{BTreeSet, HashMap};
    use std::path::Path;

    use crate::error::DevError;

    /// A dependency a member declares, as feature routing sees it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct DeclaredDep {
        /// `rename` when set, else `name`: the manifest spelling a feature
        /// token uses.
        name: String,
        optional: bool,
    }

    /// One workspace member's routing surface.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MemberFeatures {
        name: String,
        features: BTreeSet<String>,
        deps: Vec<DeclaredDep>,
    }

    /// The routing surface of every workspace member, from one
    /// `cargo metadata --no-deps` snapshot.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub(crate) struct WorkspaceFeatures {
        members: Vec<MemberFeatures>,
        default_members: Vec<String>,
    }

    impl WorkspaceFeatures {
        pub(crate) fn from_metadata(meta: &serde_json::Value) -> Self {
            let str_of = |v: &serde_json::Value, key: &str| v.get(key).and_then(serde_json::Value::as_str).map(str::to_owned);
            let ids = |key: &str| -> Vec<String> {
                meta.get(key)
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            };
            let member_ids = ids("workspace_members");
            let mut by_id: HashMap<String, MemberFeatures> = HashMap::new();
            for pkg in meta.get("packages").and_then(serde_json::Value::as_array).into_iter().flatten() {
                let (Some(id), Some(name)) = (str_of(pkg, "id"), str_of(pkg, "name")) else { continue };
                if !member_ids.contains(&id) {
                    continue;
                }
                let features = pkg
                    .get("features")
                    .and_then(serde_json::Value::as_object)
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                let deps = pkg
                    .get("dependencies")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|d| {
                        let name = str_of(d, "rename").or_else(|| str_of(d, "name"))?;
                        let optional = d.get("optional").and_then(serde_json::Value::as_bool).unwrap_or(false);
                        Some(DeclaredDep { name, optional })
                    })
                    .collect();
                by_id.insert(id, MemberFeatures { name, features, deps });
            }
            let names_of = |list: &[String]| -> Vec<String> {
                list.iter().filter_map(|id| by_id.get(id).map(|m| m.name.clone())).collect()
            };
            // An older cargo omits the default set; the whole workspace is
            // then cargo's own default for a virtual manifest.
            let default_members = if meta.get("workspace_default_members").is_some() {
                names_of(&ids("workspace_default_members"))
            } else {
                names_of(&member_ids)
            };
            let members = member_ids.iter().filter_map(|id| by_id.remove(id)).collect();
            Self { members, default_members }
        }

        /// Every member's name, in metadata order.
        pub(crate) fn member_names(&self) -> Vec<String> {
            self.members.iter().map(|m| m.name.clone()).collect()
        }

        /// The members a bare (no package flags) cargo selection selects.
        pub(crate) fn default_members(&self) -> &[String] {
            &self.default_members
        }

        fn member(&self, name: &str) -> Option<&MemberFeatures> {
            self.members.iter().find(|m| m.name == name)
        }
    }

    /// One `--features` token, parsed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Token<'a> {
        Bare(&'a str),
        Qualified { dep: &'a str, feature: &'a str },
        Malformed,
    }

    fn parse(token: &str) -> Token<'_> {
        if token.is_empty() || token.starts_with("dep:") {
            return Token::Malformed;
        }
        match token.split_once('/') {
            None => Token::Bare(token),
            Some((dep, feature)) => {
                let dep = dep.strip_suffix('?').unwrap_or(dep);
                if dep.is_empty() || feature.is_empty() || feature.contains('/') || dep.contains('?') {
                    Token::Malformed
                } else {
                    Token::Qualified { dep, feature }
                }
            }
        }
    }

    /// The bare rule: a feature of the member's own, or an optional
    /// dependency's implicit feature.
    fn recognizes(m: &MemberFeatures, feature: &str) -> bool {
        m.features.contains(feature) || m.deps.iter().any(|d| d.optional && d.name == feature)
    }

    fn routes(m: &MemberFeatures, token: Token<'_>) -> bool {
        match token {
            Token::Bare(f) => recognizes(m, f),
            Token::Qualified { dep, feature } => {
                m.deps.iter().any(|d| d.name == dep) || (m.name == dep && recognizes(m, feature))
            }
            Token::Malformed => false,
        }
    }

    /// A token a run does not carry, and the domain members that route it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct DroppedToken {
        pub(crate) token: String,
        pub(crate) routed_by: Vec<String>,
    }

    /// What one run carries of a sweep's tokens.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub(crate) struct Projected {
        pub(crate) kept: Vec<String>,
        pub(crate) dropped: Vec<DroppedToken>,
    }

    impl Projected {
        /// Every token kept: no projection was needed.
        pub(crate) fn identity(tokens: &[String]) -> Self {
            Self { kept: tokens.to_vec(), dropped: Vec::new() }
        }
    }

    /// Project `tokens` onto the concrete package set `selected`, with
    /// `domain` the packages the tokens were written against.
    ///
    /// Kept: a token some selected package routes, a malformed token, and a
    /// token no domain member routes either (cargo's error, unchanged).
    /// Dropped: a token only domain members outside the selection route. A
    /// selected package the snapshot does not know (not a member) makes the
    /// whole projection the identity - nothing about it can be judged, and
    /// cargo will reject the selection on its own.
    pub(crate) fn project(
        tokens: &[String],
        selected: &[String],
        domain: &[String],
        ws: &WorkspaceFeatures,
    ) -> Projected {
        let Some(selected) = selected.iter().map(|n| ws.member(n)).collect::<Option<Vec<_>>>() else {
            return Projected::identity(tokens);
        };
        let mut out = Projected::default();
        for raw in tokens {
            let token = parse(raw);
            if token == Token::Malformed || selected.iter().any(|m| routes(m, token)) {
                out.kept.push(raw.clone());
                continue;
            }
            let routed_by: Vec<String> = domain
                .iter()
                .filter_map(|n| ws.member(n))
                .filter(|m| routes(m, token))
                .map(|m| m.name.clone())
                .collect();
            if routed_by.is_empty() {
                out.kept.push(raw.clone());
            } else {
                out.dropped.push(DroppedToken { token: raw.clone(), routed_by });
            }
        }
        out
    }

    /// The invocation's one metadata snapshot, fetched on first need and
    /// cached - a run whose projections are all the identity never pays the
    /// subprocess.
    pub(crate) struct FeatureOracle<'a> {
        root: Option<&'a Path>,
        snapshot: OnceCell<Result<WorkspaceFeatures, String>>,
    }

    impl<'a> FeatureOracle<'a> {
        /// Reads `cargo metadata` at the project root (where brokkr runs
        /// cargo) when first consulted.
        pub(crate) fn at(root: &'a Path) -> Self {
            Self { root: Some(root), snapshot: OnceCell::new() }
        }

        /// No workspace to read: consulting it is an error. For selections
        /// whose sweeps carry no feature tokens.
        #[cfg(test)]
        pub(crate) fn unavailable() -> Self {
            Self { root: None, snapshot: OnceCell::new() }
        }

        /// A snapshot of a workspace with no members: every selected package
        /// is unknown to it, so every projection is the identity. For tests
        /// whose sweeps carry tokens but are not about routing.
        #[cfg(test)]
        pub(crate) fn empty() -> Self {
            Self::fixed(WorkspaceFeatures::default())
        }

        #[cfg(test)]
        pub(crate) fn fixed(ws: WorkspaceFeatures) -> Self {
            Self { root: None, snapshot: OnceCell::from(Ok(ws)) }
        }

        pub(crate) fn get(&self) -> Result<&WorkspaceFeatures, DevError> {
            let snap = self.snapshot.get_or_init(|| match self.root {
                Some(root) => crate::build::metadata_no_deps(root)
                    .map(|v| WorkspaceFeatures::from_metadata(&v))
                    .map_err(|e| e.to_string()),
                None => Err("no workspace metadata is available".to_owned()),
            });
            snap.as_ref()
                .map_err(|e| DevError::Build(format!("routing a sweep's features needs cargo metadata: {e}")))
        }
    }

    #[cfg(test)]
    pub(crate) mod fixture {
        use super::WorkspaceFeatures;

        /// One member's metadata `packages` entry: its features, and its
        /// declared dependencies as `(name, rename, optional)`.
        pub(crate) fn member(name: &str, features: &[&str], deps: &[(&str, Option<&str>, bool)]) -> serde_json::Value {
            let features: serde_json::Map<String, serde_json::Value> =
                features.iter().map(|f| ((*f).to_owned(), serde_json::json!([]))).collect();
            let deps: Vec<serde_json::Value> = deps
                .iter()
                .map(|(dep, rename, optional)| {
                    serde_json::json!({ "name": dep, "rename": rename, "optional": optional, "kind": null })
                })
                .collect();
            serde_json::json!({
                "id": format!("path+file:///ws/{name}#0.1.0"),
                "name": name,
                "features": features,
                "dependencies": deps,
            })
        }

        /// A metadata document over `members`, every member a default member.
        pub(crate) fn workspace(members: &[serde_json::Value]) -> WorkspaceFeatures {
            let ids: Vec<serde_json::Value> = members.iter().filter_map(|m| m.get("id").cloned()).collect();
            WorkspaceFeatures::from_metadata(&serde_json::json!({
                "packages": members,
                "workspace_members": ids,
                "workspace_default_members": ids,
            }))
        }

        /// The piners workspace of the bug report: four members, the
        /// harness depending on the other three, each with its own feature.
        pub(crate) fn piners() -> WorkspaceFeatures {
            workspace(&[
                member("piners-vm", &["opcode-counts"], &[]),
                member("piners-strategy", &["hotpath"], &[("piners-vm", None, false)]),
                member("piners-runner", &["trace"], &[("piners-vm", None, false)]),
                member(
                    "piners-harness",
                    &["bench"],
                    &[("piners-vm", None, false), ("piners-strategy", None, false), ("piners-runner", None, false)],
                ),
            ])
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use super::fixture::{member, workspace};
        use super::*;

        fn s(list: &[&str]) -> Vec<String> {
            list.iter().map(|p| (*p).to_owned()).collect()
        }

        #[test]
        fn a_qualified_token_is_kept_dropped_or_left_for_cargo() {
            let ws = workspace(&[
                member("a", &["fa"], &[]),
                member("b", &["fb"], &[]),
                member("c", &[], &[("a", None, false)]),
            ]);
            let domain = s(&["a", "b", "c"]);
            // Kept: the selected package is `a` and knows `fa`; `c` declares
            // a dependency named `a`.
            assert_eq!(project(&s(&["a/fa"]), &s(&["a"]), &domain, &ws).kept, s(&["a/fa"]));
            assert_eq!(project(&s(&["a/fa"]), &s(&["c"]), &domain, &ws).kept, s(&["a/fa"]));
            // Dropped: only `a` (itself) and `c` (its dependency on `a`),
            // both outside the selection, route it.
            let p = project(&s(&["a/fa"]), &s(&["b"]), &domain, &ws);
            assert!(p.kept.is_empty());
            assert_eq!(p.dropped, vec![DroppedToken { token: "a/fa".into(), routed_by: s(&["a", "c"]) }]);
            // Kept for cargo: nothing in the domain routes it.
            assert_eq!(project(&s(&["zz/f"]), &s(&["b"]), &domain, &ws).kept, s(&["zz/f"]));
            // `a/nope`: `a` itself does not know the feature, but `c`'s
            // dependency on `a` routes it (whether `a` has it is cargo's to
            // check, later) - so a run over `b` alone drops it.
            let p = project(&s(&["a/nope"]), &s(&["b"]), &domain, &ws);
            assert_eq!(p.dropped, vec![DroppedToken { token: "a/nope".into(), routed_by: s(&["c"]) }]);
            // Over `a` alone nothing routes it: kept, cargo's error.
            assert_eq!(project(&s(&["a/nope"]), &s(&["a"]), &s(&["a", "b"]), &ws).kept, s(&["a/nope"]));
        }

        #[test]
        fn a_bare_token_routes_by_feature_or_optional_dependency() {
            let ws = workspace(&[
                member("a", &["fast"], &[("serde", None, true), ("log", None, false)]),
                member("b", &[], &[]),
            ]);
            let domain = s(&["a", "b"]);
            // Own feature, and an optional dependency's implicit feature.
            for t in ["fast", "serde"] {
                assert_eq!(project(&s(&[t]), &s(&["a"]), &domain, &ws).kept, s(&[t]));
                assert_eq!(project(&s(&[t]), &s(&["b"]), &domain, &ws).dropped.len(), 1, "{t}");
            }
            // A non-optional dependency has no implicit feature: nobody routes
            // `log` bare, so it is kept for cargo to reject.
            assert_eq!(project(&s(&["log"]), &s(&["b"]), &domain, &ws).kept, s(&["log"]));
        }

        #[test]
        fn weak_and_renamed_dependencies_route_by_manifest_spelling() {
            let ws = workspace(&[
                member("a", &[], &[("serde", None, true), ("tokio", Some("rt-tokio"), false)]),
                member("b", &[], &[]),
            ]);
            let domain = s(&["a", "b"]);
            assert_eq!(project(&s(&["serde?/derive"]), &s(&["a"]), &domain, &ws).kept, s(&["serde?/derive"]));
            assert_eq!(project(&s(&["serde?/derive"]), &s(&["b"]), &domain, &ws).dropped.len(), 1);
            // The rename is the spelling; the package name is not.
            assert_eq!(project(&s(&["rt-tokio/full"]), &s(&["a"]), &domain, &ws).kept, s(&["rt-tokio/full"]));
            assert_eq!(project(&s(&["rt-tokio/full"]), &s(&["b"]), &domain, &ws).dropped.len(), 1);
            assert_eq!(project(&s(&["tokio/full"]), &s(&["b"]), &domain, &ws).kept, s(&["tokio/full"]));
        }

        #[test]
        fn malformed_tokens_are_never_dropped() {
            let ws = workspace(&[member("a", &["f"], &[("x", None, true)]), member("b", &[], &[])]);
            let domain = s(&["a", "b"]);
            for t in ["dep:x", "a/f/g", "/f", "a/", "a??/f"] {
                assert_eq!(project(&s(&[t]), &s(&["b"]), &domain, &ws).kept, s(&[t]), "{t}");
            }
        }

        #[test]
        fn an_unknown_selected_package_leaves_the_tokens_alone() {
            let ws = workspace(&[member("a", &["f"], &[])]);
            let p = project(&s(&["a/f"]), &s(&["not-a-member"]), &s(&["a"]), &ws);
            assert_eq!(p, Projected::identity(&s(&["a/f"])));
        }

        #[test]
        fn metadata_names_members_and_default_members() {
            let meta = serde_json::json!({
                "packages": [
                    { "id": "a-id", "name": "a", "features": { "f": [] }, "dependencies": [] },
                    { "id": "b-id", "name": "b", "features": {}, "dependencies": [
                        { "name": "a", "rename": null, "optional": false, "kind": "dev" }
                    ] },
                ],
                "workspace_members": ["a-id", "b-id"],
                "workspace_default_members": ["b-id"],
            });
            let ws = WorkspaceFeatures::from_metadata(&meta);
            assert_eq!(ws.member_names(), s(&["a", "b"]));
            assert_eq!(ws.default_members(), s(&["b"]).as_slice());
            // A dev-dependency routes `a/f` too: any kind counts.
            assert_eq!(project(&s(&["a/f"]), &s(&["b"]), &s(&["a", "b"]), &ws).kept, s(&["a/f"]));
        }

        #[test]
        fn the_oracle_is_consulted_lazily_and_fails_without_a_workspace() {
            assert!(FeatureOracle::unavailable().get().is_err());
            let ws = workspace(&[member("a", &[], &[])]);
            assert_eq!(FeatureOracle::fixed(ws.clone()).get().unwrap(), &ws);
        }
    }
}

pub(crate) use features::FeatureOracle;
#[cfg(test)]
pub(crate) use features::fixture as feature_fixture;
