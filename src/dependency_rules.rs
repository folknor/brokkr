//! Static Cargo dependency-boundary checks for `brokkr check`.
//!
//! Rules come from `[[dependency_rule]]` entries in `brokkr.toml`.
//! Each rule judges the direct dependencies of one or more workspace
//! packages, either against a `forbid` list (listed names are violations) or
//! an `allow` list (unlisted names are violations).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::cargo::metadata::{CargoMetadata, CargoPackage};
use crate::config::DependencyRule;
use crate::error::DevError;

pub use crate::deps::DependencyKind;

#[derive(Debug, Clone)]
pub struct DependencyViolation {
    pub rule: Option<String>,
    pub from: String,
    pub to: String,
    pub alias: Option<String>,
    pub kind: DependencyKind,
    pub target: Option<String>,
    pub optional: bool,
}

enum RulePolarity<'a> {
    Forbid(BTreeSet<&'a str>),
    Allow(BTreeSet<&'a str>),
}

impl<'a> RulePolarity<'a> {
    fn violates(&self, name: &str) -> bool {
        match self {
            Self::Forbid(names) => names.contains(name),
            Self::Allow(names) => !names.contains(name),
        }
    }
}

fn rule_polarity(rule: &DependencyRule) -> Result<RulePolarity<'_>, DevError> {
    match (&rule.forbid, &rule.allow) {
        (Some(forbidden), None) => Ok(RulePolarity::Forbid(
            forbidden.iter().map(String::as_str).collect(),
        )),
        (None, Some(allowed)) => Ok(RulePolarity::Allow(
            allowed.iter().map(String::as_str).collect(),
        )),
        (Some(_), Some(_)) => Err(DevError::Config(
            "[[dependency_rule]] sets both `forbid` and `allow`; a rule has one polarity".into(),
        )),
        (None, None) => Err(DevError::Config(
            "[[dependency_rule]] needs `forbid` or `allow`".into(),
        )),
    }
}

#[derive(Debug)]
pub struct DependencyReport {
    pub rules: usize,
    pub packages: usize,
    pub violations: Vec<DependencyViolation>,
}

pub fn check(project_root: &Path, rules: &[DependencyRule]) -> Result<DependencyReport, DevError> {
    let metadata = crate::cargo::metadata::load(
        project_root,
        crate::cargo::metadata::Query::workspace(),
        &[],
        None,
    )?;
    check_metadata(&metadata, rules)
}

pub fn check_metadata(
    metadata: &CargoMetadata,
    rules: &[DependencyRule],
) -> Result<DependencyReport, DevError> {
    let workspace_packages: Vec<&CargoPackage> = metadata
        .packages
        .iter()
        .filter(|pkg| metadata.is_member(&pkg.id))
        .collect();
    let by_name: BTreeMap<&str, &CargoPackage> = workspace_packages
        .iter()
        .map(|pkg| (pkg.name.as_str(), *pkg))
        .collect();

    let mut violations = Vec::new();
    for rule in rules {
        // The parser enforces one polarity; materialize it as an enum here too
        // so malformed direct input cannot become a silent no-op.
        // An edge is judged by `dep.name`, the real package name - never the
        // rename key - so `foo = { package = "bar" }` can't dodge either list.
        let polarity = rule_polarity(rule)?;

        // The kind filter, parsed at config load. Empty = every kind.
        let kinds = &rule.kinds;

        // The first selector error is the refusal, as it always was.
        let from_pkgs = resolve_selectors(rule, &workspace_packages, &by_name)
            .map_err(|errors| DevError::Config(errors.into_iter().next().unwrap_or_default()))?;

        for pkg in from_pkgs {
            for dep in &pkg.dependencies {
                if !polarity.violates(dep.name.as_str()) {
                    continue;
                }
                let dep_kind = dep.kind;
                // `kinds` scopes to specific kinds (empty = all); `optional`
                // scopes to a specific optional flag (unset = either).
                if let Some(want) = rule.optional
                    && dep.optional != want
                {
                    continue;
                }
                if !kinds.is_empty() {
                    if dep_kind == DependencyKind::Unknown {
                        return Err(DevError::ContractViolation(format!(
                            "cargo metadata returned an unrecognized dependency kind for {} -> {} \
                             while applying a `kinds` filter",
                            pkg.name, dep.name,
                        )));
                    }
                    // An unknown metadata kind is an error under an explicit
                    // filter; otherwise it could silently evade the rule.
                    if !kinds.contains(&dep_kind) {
                        continue;
                    }
                }
                violations.push(DependencyViolation {
                    rule: rule.name.clone(),
                    from: pkg.name.clone(),
                    to: dep.name.clone(),
                    alias: dep.rename.clone(),
                    kind: dep_kind,
                    target: dep.target.clone(),
                    optional: dep.optional,
                });
            }
        }
    }

    Ok(DependencyReport {
        rules: rules.len(),
        packages: workspace_packages.len(),
        violations,
    })
}

/// One rule's `from` set, `except` applied, or every selector error in it.
///
/// `"*"` anywhere in `from` expands to every workspace package and the other
/// names are not looked up; otherwise each name must be a workspace package.
/// Every `except` must name a package `from` selected, so a typo cannot
/// silently leave the intended package in scope. It is judged against the
/// `from` names that did resolve, so a missing `from` name cannot hide a bad
/// `except`; missing `from` names come first, as the phase's refusal.
fn resolve_selectors<'m>(
    rule: &DependencyRule,
    workspace: &[&'m CargoPackage],
    by_name: &BTreeMap<&str, &'m CargoPackage>,
) -> Result<Vec<&'m CargoPackage>, Vec<String>> {
    let mut errors = Vec::new();
    let candidates: Vec<&CargoPackage> = if rule.from.iter().any(|f| f == "*") {
        workspace.to_vec()
    } else {
        let mut pkgs = Vec::new();
        for from in &rule.from {
            match by_name.get(from.as_str()) {
                Some(pkg) => pkgs.push(*pkg),
                None => errors.push(format!(
                    "[[dependency_rule]] references package '{from}' in `from`, \
                     but cargo metadata has no workspace package with that name"
                )),
            }
        }
        pkgs
    };
    errors.extend(
        rule.except
            .iter()
            .filter(|name| !candidates.iter().any(|pkg| pkg.name == **name))
            .map(|name| {
                format!(
                    "[[dependency_rule]] except entry {name:?} matches no package selected by `from`"
                )
            }),
    );
    if !errors.is_empty() {
        return Err(errors);
    }
    let except: BTreeSet<&str> = rule.except.iter().map(String::as_str).collect();
    Ok(candidates
        .into_iter()
        .filter(|pkg| !except.contains(pkg.name.as_str()))
        .collect())
}

/// Every selector error across `rules`, for `brokkr check --audit`: the
/// refusals the dependency-rules phase would raise, all of them rather than
/// the first. Names in `forbid`/`allow` matching no dependency are not
/// findings - absence is never a violation.
pub fn selector_findings(metadata: &CargoMetadata, rules: &[DependencyRule]) -> Vec<String> {
    let workspace: Vec<&CargoPackage> = metadata
        .packages
        .iter()
        .filter(|pkg| metadata.is_member(&pkg.id))
        .collect();
    let by_name: BTreeMap<&str, &CargoPackage> = workspace
        .iter()
        .map(|pkg| (pkg.name.as_str(), *pkg))
        .collect();
    rules
        .iter()
        .filter_map(|rule| resolve_selectors(rule, &workspace, &by_name).err())
        .flatten()
        .collect()
}

pub fn format_violation(v: &DependencyViolation) -> String {
    let mut out = String::new();
    if let Some(rule) = &v.rule {
        out.push_str(&format!("[{rule}] "));
    }
    out.push_str(&format!("{} -> {}", v.from, v.to));
    if let Some(alias) = &v.alias {
        out.push_str(&format!(" as {alias}"));
    }
    out.push_str(&format!(" ({})", v.kind.as_str()));
    if let Some(target) = &v.target {
        out.push_str(&format!(" target {target}"));
    }
    if v.optional {
        out.push_str(" optional");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn metadata() -> CargoMetadata {
        serde_json::from_str(
            r#"{
  "packages": [
    {
      "name": "app",
      "id": "path+file:///repo/crates/app#0.1.0",
      "dependencies": [
        {
          "name": "db",
          "rename": null,
          "kind": null,
          "target": null,
          "optional": false
        },
        {
          "name": "service-state",
          "rename": "service_state",
          "kind": "dev",
          "target": "cfg(unix)",
          "optional": true
        }
      ]
    },
    {
      "name": "db",
      "id": "path+file:///repo/crates/db#0.1.0",
      "dependencies": []
    },
    {
      "name": "service-state",
      "id": "path+file:///repo/crates/service-state#0.1.0",
      "dependencies": []
    }
  ],
  "workspace_members": [
    "path+file:///repo/crates/app#0.1.0",
    "path+file:///repo/crates/db#0.1.0",
    "path+file:///repo/crates/service-state#0.1.0"
  ]
}"#,
        )
        .unwrap()
    }

    #[test]
    fn direct_forbidden_dependency_is_reported() {
        let rules = vec![DependencyRule {
            name: Some("app-db-boundary".into()),
            from: vec!["app".into()],
            forbid: Some(vec!["db".into()]),
            allow: None,
            except: Vec::new(),
            kinds: Vec::new(),
            optional: None,
        }];
        let report = check_metadata(&metadata(), &rules).unwrap();
        assert_eq!(report.violations.len(), 1);
        let violation = &report.violations[0];
        assert_eq!(violation.from, "app");
        assert_eq!(violation.to, "db");
        assert_eq!(violation.kind, DependencyKind::Normal);
    }

    fn selector_rule(from: &[&str], except: &[&str]) -> DependencyRule {
        DependencyRule {
            name: None,
            from: from.iter().map(|s| (*s).to_owned()).collect(),
            forbid: Some(vec!["db".into()]),
            allow: None,
            except: except.iter().map(|s| (*s).to_owned()).collect(),
            kinds: Vec::new(),
            optional: None,
        }
    }

    #[test]
    fn selector_findings_report_every_bad_selector_and_keep_the_wildcard() {
        let rules = vec![
            // Two missing `from` names, and an except outside what did resolve.
            selector_rule(&["ghost", "app", "phantom"], &["nowhere"]),
            // `*` is the whole workspace; the extra name is not looked up.
            selector_rule(&["*", "ghost"], &["db"]),
            // An except outside `from`.
            selector_rule(&["app"], &["db"]),
        ];
        let findings = selector_findings(&metadata(), &rules);
        assert_eq!(findings.len(), 4, "{findings:?}");
        assert!(findings[0].contains("'ghost'") && findings[1].contains("'phantom'"));
        assert!(findings[2].contains("except entry \"nowhere\""));
        assert!(findings[3].contains("except entry \"db\""));
        // The phase still refuses with the first of them.
        let err = check_metadata(&metadata(), &rules).unwrap_err().to_string();
        assert!(err.contains("'ghost'"), "{err}");
    }

    #[test]
    fn rule_can_forbid_multiple_dependencies() {
        let rules = vec![DependencyRule {
            name: None,
            from: vec!["app".into()],
            forbid: Some(vec!["db".into(), "service-state".into()]),
            allow: None,
            except: Vec::new(),
            kinds: Vec::new(),
            optional: None,
        }];
        let report = check_metadata(&metadata(), &rules).unwrap();
        assert_eq!(report.violations.len(), 2);
        assert_eq!(report.violations[1].alias.as_deref(), Some("service_state"));
        assert_eq!(report.violations[1].kind, DependencyKind::Dev);
        assert!(report.violations[1].optional);
    }

    #[test]
    fn missing_from_package_is_a_config_error() {
        let rules = vec![DependencyRule {
            name: None,
            from: vec!["missing".into()],
            forbid: Some(vec!["db".into()]),
            allow: None,
            except: Vec::new(),
            kinds: Vec::new(),
            optional: None,
        }];
        let err = check_metadata(&metadata(), &rules).unwrap_err().to_string();
        assert!(err.contains("missing"), "got: {err}");
    }

    #[test]
    fn wildcard_from_scans_all_workspace_packages_minus_except() {
        // Only `app` depends on `db`. `from = "*"` scans every workspace
        // package and flags it; `except = ["app"]` then clears it.
        let rules = vec![DependencyRule {
            name: None,
            from: vec!["*".into()],
            forbid: Some(vec!["db".into()]),
            allow: None,
            except: Vec::new(),
            kinds: Vec::new(),
            optional: None,
        }];
        assert_eq!(
            check_metadata(&metadata(), &rules)
                .unwrap()
                .violations
                .len(),
            1
        );

        let excepted = vec![DependencyRule {
            name: None,
            from: vec!["*".into()],
            forbid: Some(vec!["db".into()]),
            allow: None,
            except: vec!["app".into()],
            kinds: Vec::new(),
            optional: None,
        }];
        assert!(
            check_metadata(&metadata(), &excepted)
                .unwrap()
                .violations
                .is_empty()
        );
    }

    #[test]
    fn unmatched_except_is_a_config_error() {
        let rules = vec![DependencyRule {
            name: None,
            from: vec!["*".into()],
            forbid: Some(vec!["db".into()]),
            allow: None,
            except: vec!["missing".into()],
            kinds: Vec::new(),
            optional: None,
        }];
        let error = check_metadata(&metadata(), &rules).unwrap_err().to_string();
        assert!(error.contains("except entry \"missing\""), "got: {error}");
    }

    /// Build an `app` rule forbidding `to`, scoped by `kinds` / `optional`.
    fn app_rule(to: &str, kinds: Vec<DependencyKind>, optional: Option<bool>) -> DependencyRule {
        DependencyRule {
            name: None,
            from: vec!["app".into()],
            forbid: Some(vec![to.into()]),
            allow: None,
            except: Vec::new(),
            kinds,
            optional,
        }
    }

    #[test]
    fn kinds_normal_allows_a_dev_dependency() {
        // `service-state` is a dev-dependency of `app`; `kinds = ["normal"]`
        // means the dev-dep is fine, while a normal dep (`db`) still trips.
        let dev = vec![app_rule(
            "service-state",
            vec![DependencyKind::Normal],
            None,
        )];
        assert!(
            check_metadata(&metadata(), &dev)
                .unwrap()
                .violations
                .is_empty()
        );

        let normal = vec![app_rule("db", vec![DependencyKind::Normal], None)];
        assert_eq!(
            check_metadata(&metadata(), &normal)
                .unwrap()
                .violations
                .len(),
            1
        );
    }

    #[test]
    fn kinds_dev_matches_only_the_dev_dependency() {
        // The inverse scope: `kinds = ["dev"]` catches `service-state` (dev)
        // but not `db` (normal).
        let dev = vec![app_rule("service-state", vec![DependencyKind::Dev], None)];
        assert_eq!(
            check_metadata(&metadata(), &dev).unwrap().violations.len(),
            1
        );

        let normal = vec![app_rule("db", vec![DependencyKind::Dev], None)];
        assert!(
            check_metadata(&metadata(), &normal)
                .unwrap()
                .violations
                .is_empty()
        );
    }

    #[test]
    fn optional_false_matches_only_non_optional_deps() {
        // `optional = false` matches only non-optional deps: `db` (optional
        // false) trips, `service-state` (optional true) does not - i.e. the
        // rule fires on deps that are declared non-optional.
        let db = vec![app_rule("db", Vec::new(), Some(false))];
        assert_eq!(
            check_metadata(&metadata(), &db).unwrap().violations.len(),
            1
        );

        let svc = vec![app_rule("service-state", Vec::new(), Some(false))];
        assert!(
            check_metadata(&metadata(), &svc)
                .unwrap()
                .violations
                .is_empty()
        );
    }

    /// Build an allow-list rule over `from`.
    fn allow_rule(from: &[&str], allow: &[&str], kinds: Vec<DependencyKind>) -> DependencyRule {
        DependencyRule {
            name: Some("leaf".into()),
            from: from.iter().map(|s| (*s).to_owned()).collect(),
            forbid: None,
            allow: Some(allow.iter().map(|s| (*s).to_owned()).collect()),
            except: Vec::new(),
            kinds,
            optional: None,
        }
    }

    #[test]
    fn allow_flags_every_unlisted_dependency() {
        // `app` depends on `db` (normal) and `service-state` (dev); allowing
        // only `db` flags `service-state`, reported by real name + rename.
        let rules = vec![allow_rule(&["app"], &["db"], Vec::new())];
        let report = check_metadata(&metadata(), &rules).unwrap();
        assert_eq!(report.violations.len(), 1);
        let v = &report.violations[0];
        assert_eq!(v.rule.as_deref(), Some("leaf"));
        assert_eq!(v.to, "service-state");
        assert_eq!(v.alias.as_deref(), Some("service_state"));
    }

    #[test]
    fn allow_is_judged_by_real_name_not_rename() {
        // Listing the rename key does not permit the renamed package.
        let rules = vec![allow_rule(&["app"], &["db", "service_state"], Vec::new())];
        assert_eq!(
            check_metadata(&metadata(), &rules)
                .unwrap()
                .violations
                .len(),
            1
        );
    }

    #[test]
    fn allow_scoped_by_kinds_leaves_dev_deps_unconstrained() {
        let rules = vec![allow_rule(&["app"], &["db"], vec![DependencyKind::Normal])];
        assert!(
            check_metadata(&metadata(), &rules)
                .unwrap()
                .violations
                .is_empty()
        );
    }

    #[test]
    fn allow_empty_list_and_unused_entries() {
        // `allow = []` means no dependencies at all: both of `app`'s edges trip.
        let none = vec![allow_rule(&["app"], &[], Vec::new())];
        assert_eq!(
            check_metadata(&metadata(), &none).unwrap().violations.len(),
            2
        );

        // An entry nothing depends on is not an error; leaves pass trivially.
        let unused = vec![allow_rule(&["db", "service-state"], &["libc"], Vec::new())];
        assert!(
            check_metadata(&metadata(), &unused)
                .unwrap()
                .violations
                .is_empty()
        );
    }

    #[test]
    fn allow_with_wildcard_from_and_except() {
        let rules = vec![DependencyRule {
            except: vec!["app".into()],
            ..allow_rule(&["*"], &[], Vec::new())
        }];
        assert!(
            check_metadata(&metadata(), &rules)
                .unwrap()
                .violations
                .is_empty()
        );
    }

    // An unknown kind never reaches the phase: the config loader refuses it.
    #[test]
    fn unknown_kind_is_a_config_error() {
        let err = toml::from_str::<DependencyRule>(
            "from = \"app\"\nforbid = [\"db\"]\nkinds = [\"regular\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("regular"), "got: {err}");
        assert!(err.contains("normal"), "got: {err}");
        let one = toml::from_str::<DependencyRule>(
            "from = \"app\"\nforbid = [\"db\"]\nkinds = \"dev\"\n",
        )
        .unwrap();
        assert_eq!(one.kinds, vec![DependencyKind::Dev]);
    }
}
