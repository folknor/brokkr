//! What the fixture resolves, held against what the product resolves.
//!
//! Two claims, both stated narrowly:
//!
//! - **External identity containment** (with `seed_lock`): every sourced
//!   package in the fixture's lock - registry or git - appears in the main
//!   lock with the same name, version and source (a git rev included). A
//!   package the fixture alone pulls in fails rather than resolving freely.
//!   This is containment, not graph equivalence: where the main lock holds
//!   two versions of a crate, a fixture edge may land on either.
//! - **Path identity** (always): every path package of the fixture resolve
//!   other than its root is a path package of the main resolve - same name,
//!   version and canonical manifest - and every direct dependency of the
//!   fixture root whose RESOLVED package name is a main workspace member
//!   resolves to that member's manifest. The second half catches a registry
//!   or git twin of a product crate (renamed or not), which containment alone
//!   admits whenever the main lock carries that twin for some other reason.
//!
//! Neither reads `[patch]` or `[replace]` tables: cargo applies only the
//! root's overrides, and their effect is visible in the resolved packages,
//! which is what is compared.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::cargo::metadata::{CargoMetadata, CargoPackage, PackageId};

/// `(name, version, source)` of every sourced package in a `Cargo.lock`.
pub(super) fn sourced_packages(lock: &str) -> Result<BTreeSet<(String, String, String)>, String> {
    #[derive(serde::Deserialize)]
    struct Lock {
        #[serde(default)]
        package: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        version: String,
        source: Option<String>,
    }
    let lock: Lock = toml::from_str(lock).map_err(|e| e.to_string())?;
    Ok(lock
        .package
        .into_iter()
        .filter_map(|e| Some((e.name, e.version, e.source?)))
        .collect())
}

/// Every sourced fixture package the main lock does not hold identically.
pub(super) fn containment_failures(main: &str, fixture: &str) -> Result<Vec<String>, String> {
    let main = sourced_packages(main).map_err(|e| format!("main Cargo.lock: {e}"))?;
    let fixture = sourced_packages(fixture).map_err(|e| format!("fixture Cargo.lock: {e}"))?;
    Ok(fixture
        .difference(&main)
        .map(|(name, version, source)| {
            format!("{name} {version} ({source}) is in the fixture's lock but not the main lock")
        })
        .collect())
}

fn canonical(path: &str) -> PathBuf {
    let p = Path::new(path);
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn package<'a>(meta: &'a CargoMetadata, id: &PackageId) -> Option<&'a CargoPackage> {
    meta.packages.iter().find(|p| &p.id == id)
}

/// The path-identity failures of a fixture resolve against the main one.
pub(super) fn path_failures(
    main: &CargoMetadata,
    fixture: &CargoMetadata,
    root: &PackageId,
) -> Vec<String> {
    let mut failures = Vec::new();
    let main_paths: BTreeSet<(&str, &str, PathBuf)> = main
        .packages
        .iter()
        .filter(|p| p.source.is_none())
        .map(|p| {
            (
                p.name.as_str(),
                p.version.as_str(),
                canonical(&p.manifest_path),
            )
        })
        .collect();
    for p in fixture
        .packages
        .iter()
        .filter(|p| p.source.is_none() && &p.id != root)
    {
        let key = (
            p.name.as_str(),
            p.version.as_str(),
            canonical(&p.manifest_path),
        );
        if !main_paths.contains(&key) {
            failures.push(format!(
                "path package {} {} ({}) is not a path package of the main resolve",
                p.name,
                p.version,
                key.2.display()
            ));
        }
    }
    let members: BTreeMap<&str, PathBuf> = main
        .packages
        .iter()
        .filter(|p| main.is_member(&p.id))
        .map(|p| (p.name.as_str(), canonical(&p.manifest_path)))
        .collect();
    for dep in fixture.resolved_dependencies(root) {
        let Some(p) = package(fixture, &dep.package) else {
            continue;
        };
        let Some(member) = members.get(p.name.as_str()) else {
            continue;
        };
        let got = canonical(&p.manifest_path);
        if &got != member {
            failures.push(format!(
                "the fixture depends on {} from {} - not the workspace member at {}",
                p.name,
                p.source
                    .as_deref()
                    .unwrap_or_else(|| got.to_str().unwrap_or("?")),
                member.display()
            ));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const MAIN: &str = r#"
version = 4
[[package]]
name = "product"
version = "0.1.0"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "nautilus-model"
version = "0.62.0"
source = "git+https://github.com/x/nautilus?rev=abc#abc"
"#;

    #[test]
    fn identical_sourced_packages_are_contained_and_path_packages_ignored() {
        let fixture = r#"
version = 4
[[package]]
name = "origin-seal"
version = "0.0.0"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        assert!(containment_failures(MAIN, fixture).unwrap().is_empty());
    }

    // A version drift, a source drift (another git rev), and a package the
    // fixture alone pulls in: all three fail.
    #[test]
    fn drift_and_fixture_only_packages_fail() {
        let fixture = r#"
version = 4
[[package]]
name = "serde"
version = "1.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "nautilus-model"
version = "0.62.0"
source = "git+https://github.com/x/nautilus?rev=def#def"

[[package]]
name = "fixture-only"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        assert_eq!(containment_failures(MAIN, fixture).unwrap().len(), 3);
    }
}
