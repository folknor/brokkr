//! Static Cargo dependency-boundary checks for `brokkr check`.
//!
//! Rules come from `[[dependency_rule]]` entries in `brokkr.toml`.
//! Each rule judges the direct dependencies of one or more workspace
//! packages, either against a `forbid` list (listed names are violations) or
//! an `allow` list (unlisted names are violations).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

use crate::config::DependencyRule;
use crate::error::DevError;
use crate::output;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyKind {
    Normal,
    Dev,
    Build,
    Unknown,
}

impl DependencyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Dev => "dev",
            Self::Build => "build",
            Self::Unknown => "unknown",
        }
    }
}

impl From<Option<&str>> for DependencyKind {
    fn from(value: Option<&str>) -> Self {
        match value {
            None => Self::Normal,
            Some("dev") => Self::Dev,
            Some("build") => Self::Build,
            Some(_) => Self::Unknown,
        }
    }
}

/// Parse a `kinds` config token into a filter kind. `Unknown` is never a valid
/// config value (it only exists for future/odd metadata), so it is rejected.
fn parse_config_kind(s: &str) -> Option<DependencyKind> {
    match s {
        "normal" => Some(DependencyKind::Normal),
        "dev" => Some(DependencyKind::Dev),
        "build" => Some(DependencyKind::Build),
        _ => None,
    }
}

#[derive(Debug)]
pub struct DependencyReport {
    pub rules: usize,
    pub packages: usize,
    pub violations: Vec<DependencyViolation>,
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    id: String,
    #[serde(default)]
    dependencies: Vec<CargoDependency>,
}

#[derive(Deserialize)]
struct CargoDependency {
    name: String,
    rename: Option<String>,
    kind: Option<String>,
    target: Option<String>,
    optional: bool,
}

pub fn check(
    project_root: &Path,
    rules: &[DependencyRule],
) -> Result<DependencyReport, DevError> {
    let captured = output::run_captured(
        "cargo",
        &["metadata", "--format-version", "1", "--no-deps"],
        project_root,
    )?;
    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("cargo metadata failed: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&captured.stdout);
    check_metadata(&stdout, rules)
}

pub fn check_metadata(
    metadata_json: &str,
    rules: &[DependencyRule],
) -> Result<DependencyReport, DevError> {
    let metadata: CargoMetadata = serde_json::from_str(metadata_json)?;
    let workspace_ids: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let workspace_packages: Vec<&CargoPackage> = metadata
        .packages
        .iter()
        .filter(|pkg| workspace_ids.contains(pkg.id.as_str()))
        .collect();
    let by_name: BTreeMap<&str, &CargoPackage> = workspace_packages
        .iter()
        .map(|pkg| (pkg.name.as_str(), *pkg))
        .collect();

    let mut violations = Vec::new();
    for rule in rules {
        // One polarity per rule (the parser enforces exactly one of the two).
        // An edge is judged by `dep.name`, the real package name - never the
        // rename key - so `foo = { package = "bar" }` can't dodge either list.
        let forbidden: Option<BTreeSet<&str>> = rule
            .forbid
            .as_ref()
            .map(|f| f.iter().map(String::as_str).collect());
        let allowed: Option<BTreeSet<&str>> = rule
            .allow
            .as_ref()
            .map(|a| a.iter().map(String::as_str).collect());
        let violates = |name: &str| match (&forbidden, &allowed) {
            (Some(f), _) => f.contains(name),
            (None, Some(a)) => !a.contains(name),
            (None, None) => false,
        };
        let except: BTreeSet<&str> = rule.except.iter().map(String::as_str).collect();

        // Resolve the kind filter once per rule. Empty = every kind.
        let mut kinds = Vec::with_capacity(rule.kinds.len());
        for k in &rule.kinds {
            let Some(kind) = parse_config_kind(k) else {
                return Err(DevError::Config(format!(
                    "[[dependency_rule]]{}: unknown kind {k:?} in `kinds`; \
                     expected \"normal\", \"dev\", or \"build\"",
                    rule.name.as_deref().map(|n| format!(" {n:?}")).unwrap_or_default(),
                )));
            };
            kinds.push(kind);
        }

        // Resolve the `from` set. `"*"` expands to every workspace package;
        // otherwise each named package is looked up. `except` drops packages
        // from either form.
        let from_pkgs: Vec<&CargoPackage> = if rule.from.iter().any(|f| f == "*") {
            workspace_packages
                .iter()
                .copied()
                .filter(|pkg| !except.contains(pkg.name.as_str()))
                .collect()
        } else {
            let mut pkgs = Vec::new();
            for from in &rule.from {
                if except.contains(from.as_str()) {
                    continue;
                }
                let Some(pkg) = by_name.get(from.as_str()) else {
                    return Err(DevError::Config(format!(
                        "[[dependency_rule]] references package '{from}' in `from`, \
                         but cargo metadata has no workspace package with that name"
                    )));
                };
                pkgs.push(*pkg);
            }
            pkgs
        };

        for pkg in from_pkgs {
            for dep in &pkg.dependencies {
                if !violates(dep.name.as_str()) {
                    continue;
                }
                let dep_kind = DependencyKind::from(dep.kind.as_deref());
                // `kinds` scopes to specific kinds (empty = all); `optional`
                // scopes to a specific optional flag (unset = either).
                if !kinds.is_empty() && !kinds.contains(&dep_kind) {
                    continue;
                }
                if let Some(want) = rule.optional
                    && dep.optional != want
                {
                    continue;
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

    fn metadata() -> &'static str {
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
}"#
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
        let report = check_metadata(metadata(), &rules).unwrap();
        assert_eq!(report.violations.len(), 1);
        let violation = &report.violations[0];
        assert_eq!(violation.from, "app");
        assert_eq!(violation.to, "db");
        assert_eq!(violation.kind, DependencyKind::Normal);
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
        let report = check_metadata(metadata(), &rules).unwrap();
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
        let err = check_metadata(metadata(), &rules).unwrap_err().to_string();
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
        assert_eq!(check_metadata(metadata(), &rules).unwrap().violations.len(), 1);

        let excepted = vec![DependencyRule {
            name: None,
            from: vec!["*".into()],
            forbid: Some(vec!["db".into()]),
            allow: None,
            except: vec!["app".into()],
            kinds: Vec::new(),
            optional: None,
        }];
        assert!(check_metadata(metadata(), &excepted).unwrap().violations.is_empty());
    }

    /// Build an `app` rule forbidding `to`, scoped by `kinds` / `optional`.
    fn app_rule(to: &str, kinds: Vec<String>, optional: Option<bool>) -> DependencyRule {
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
        let dev = vec![app_rule("service-state", vec!["normal".into()], None)];
        assert!(check_metadata(metadata(), &dev).unwrap().violations.is_empty());

        let normal = vec![app_rule("db", vec!["normal".into()], None)];
        assert_eq!(check_metadata(metadata(), &normal).unwrap().violations.len(), 1);
    }

    #[test]
    fn kinds_dev_matches_only_the_dev_dependency() {
        // The inverse scope: `kinds = ["dev"]` catches `service-state` (dev)
        // but not `db` (normal).
        let dev = vec![app_rule("service-state", vec!["dev".into()], None)];
        assert_eq!(check_metadata(metadata(), &dev).unwrap().violations.len(), 1);

        let normal = vec![app_rule("db", vec!["dev".into()], None)];
        assert!(check_metadata(metadata(), &normal).unwrap().violations.is_empty());
    }

    #[test]
    fn optional_false_matches_only_non_optional_deps() {
        // `optional = false` matches only non-optional deps: `db` (optional
        // false) trips, `service-state` (optional true) does not - i.e. the
        // rule fires on deps that are declared non-optional.
        let db = vec![app_rule("db", Vec::new(), Some(false))];
        assert_eq!(check_metadata(metadata(), &db).unwrap().violations.len(), 1);

        let svc = vec![app_rule("service-state", Vec::new(), Some(false))];
        assert!(check_metadata(metadata(), &svc).unwrap().violations.is_empty());
    }

    /// Build an allow-list rule over `from`.
    fn allow_rule(from: &[&str], allow: &[&str], kinds: Vec<String>) -> DependencyRule {
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
        let report = check_metadata(metadata(), &rules).unwrap();
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
        assert_eq!(check_metadata(metadata(), &rules).unwrap().violations.len(), 1);
    }

    #[test]
    fn allow_scoped_by_kinds_leaves_dev_deps_unconstrained() {
        let rules = vec![allow_rule(&["app"], &["db"], vec!["normal".into()])];
        assert!(check_metadata(metadata(), &rules).unwrap().violations.is_empty());
    }

    #[test]
    fn allow_empty_list_and_unused_entries() {
        // `allow = []` means no dependencies at all: both of `app`'s edges trip.
        let none = vec![allow_rule(&["app"], &[], Vec::new())];
        assert_eq!(check_metadata(metadata(), &none).unwrap().violations.len(), 2);

        // An entry nothing depends on is not an error; leaves pass trivially.
        let unused = vec![allow_rule(&["db", "service-state"], &["libc"], Vec::new())];
        assert!(check_metadata(metadata(), &unused).unwrap().violations.is_empty());
    }

    #[test]
    fn allow_with_wildcard_from_and_except() {
        let rules = vec![DependencyRule {
            except: vec!["app".into()],
            ..allow_rule(&["*"], &[], Vec::new())
        }];
        assert!(check_metadata(metadata(), &rules).unwrap().violations.is_empty());
    }

    #[test]
    fn unknown_kind_is_a_config_error() {
        let rules = vec![app_rule("db", vec!["regular".into()], None)];
        let err = check_metadata(metadata(), &rules).unwrap_err().to_string();
        assert!(err.contains("regular"), "got: {err}");
        assert!(err.contains("normal"), "got: {err}");
    }
}
