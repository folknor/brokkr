//! `native_code` phase.
//!
//! Lists dependencies that pull non-Rust code into the build. Two
//! orthogonal signals, deliberately not name-based - the `-sys`
//! convention is advisory and both over- and under-includes (e.g.
//! `js-sys` / `windows-sys` are pure-Rust FFI declarations, while
//! plenty of native bundlers don't carry the suffix):
//!
//! - **links a native library** - the manifest's `links` key is set.
//!   This is the canonical marker Cargo reserves for a crate that
//!   links against a system/bundled native library; it's what makes a
//!   real `-sys` crate.
//! - **compiles non-Rust code** - the crate has a *build-dependency*
//!   on a known native-toolchain crate (`cc` for C/C++, `cmake`,
//!   `cxx-build` for C++, `nasm-rs` for assembly). That's a build
//!   script invoking a C/C++/asm compiler.
//!
//! A crate can hit one signal, the other, or both (`libsqlite3-sys`
//! both compiles SQLite via `cc` and links `sqlite3`). Informational,
//! like `outdated`/`stale`: native code is a portability / cross-
//! compile / supply-chain heads-up, not a "you broke something" smell,
//! so it doesn't drive the exit code.
//!
//! Runs on the host-filtered metadata (same as `duplicate_version`),
//! so crates that exist only for inactive targets (e.g. wasm-only
//! `sqlite-wasm-rs`) don't show up on a native host. Workspace members
//! are skipped - this phase is about *dependencies*, and a workspace
//! member building native code is the user's own first-party choice.
//!
//! Limitation: a build script that shells out to a compiler directly
//! (not via `cc`/`cmake`/...) is invisible here. The `links` signal
//! still catches it if it links the result, but a pure private codegen
//! step won't register.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::{CargoMetadata, NativeDependencyEvent};

/// Build-dependency crate names that mean "this build script compiles
/// non-Rust source". `cxx-build` depends on `cc`, but a crate using
/// cxx lists `cxx-build` as its own direct build-dep, so scanning
/// direct build edges catches it without chasing transitives.
const TOOLCHAIN_CRATES: &[&str] = &["cc", "cmake", "cxx-build", "nasm-rs"];

pub fn run(metadata: &CargoMetadata) -> Vec<NativeDependencyEvent> {
    let workspace_set: HashSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();

    let id_to_name: HashMap<&str, &str> = metadata
        .packages
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();

    let toolchain: HashSet<&str> = TOOLCHAIN_CRATES.iter().copied().collect();

    // package id -> the toolchain build-deps it declares. Build kind is
    // `dep_kinds[*].kind == Some("build")`; we only count edges whose
    // target name is a known compiler-driver crate.
    let mut compiles: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for node in &metadata.resolve.nodes {
        for d in &node.deps {
            let is_build = d.dep_kinds.iter().any(|dk| dk.kind.as_deref() == Some("build"));
            if !is_build {
                continue;
            }
            let Some(dep_name) = id_to_name.get(d.pkg.as_str()) else {
                continue;
            };
            if toolchain.contains(dep_name) {
                compiles
                    .entry(node.id.as_str())
                    .or_default()
                    .insert(dep_name);
            }
        }
    }

    let mut events = Vec::new();
    for pkg in &metadata.packages {
        if workspace_set.contains(pkg.id.as_str()) {
            continue;
        }
        let links = pkg.links.clone();
        let toolchains: Vec<String> = compiles
            .get(pkg.id.as_str())
            .map(|set| set.iter().map(ToString::to_string).collect())
            .unwrap_or_default();

        let Some(reason) = classify(links.is_some(), !toolchains.is_empty()) else {
            continue;
        };
        events.push(NativeDependencyEvent {
            krate: pkg.name.clone(),
            version: pkg.version.clone(),
            reason,
            links,
            toolchains,
        });
    }
    events.sort_by(|a, b| a.krate.cmp(&b.krate).then(a.version.cmp(&b.version)));
    events
}

/// `None` when neither signal fired (not a native dep). Otherwise the
/// label that goes in the event and JSON output.
fn classify(links: bool, compiles: bool) -> Option<&'static str> {
    match (links, compiles) {
        (true, true) => Some("both"),
        (true, false) => Some("links"),
        (false, true) => Some("compiles"),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_covers_all_combinations() {
        assert_eq!(classify(true, true), Some("both"));
        assert_eq!(classify(true, false), Some("links"));
        assert_eq!(classify(false, true), Some("compiles"));
        assert_eq!(classify(false, false), None);
    }
}
