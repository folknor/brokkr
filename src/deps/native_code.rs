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

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use super::CargoMetadata;

/// A dependency that pulls non-Rust code into the build. `reason` is
/// serialized from the two source signals so it cannot disagree with them.
pub(super) struct NativeDependencyEvent {
    pub(super) krate: String,
    pub(super) version: String,
    pub(super) links: Option<String>,
    pub(super) toolchains: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum NativeReason {
    Links,
    Compiles,
    Both,
}

impl Serialize for NativeDependencyEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let reason =
            classify(self.links.is_some(), !self.toolchains.is_empty()).ok_or_else(|| {
                <S::Error as serde::ser::Error>::custom(
                    "native dependency event has neither native signal",
                )
            })?;
        let field_count =
            3 + self.links.is_some() as usize + (!self.toolchains.is_empty()) as usize;
        let mut state = serializer.serialize_struct("NativeDependencyEvent", field_count)?;
        state.serialize_field("crate", &self.krate)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("reason", &reason)?;
        if let Some(links) = &self.links {
            state.serialize_field("links", links)?;
        }
        if !self.toolchains.is_empty() {
            state.serialize_field("toolchains", &self.toolchains)?;
        }
        state.end()
    }
}

/// Fixed classifier policy: build-dependency crate names that mean "this build
/// script compiles non-Rust source". This is not project configuration.
/// `cxx-build` depends on `cc`, but a crate using
/// cxx lists `cxx-build` as its own direct build-dep, so scanning
/// direct build edges catches it without chasing transitives.
const TOOLCHAIN_CRATES: &[&str] = &["cc", "cmake", "cxx-build", "nasm-rs"];

pub fn run(metadata: &CargoMetadata) -> Vec<NativeDependencyEvent> {
    let toolchain: HashSet<&str> = TOOLCHAIN_CRATES.iter().copied().collect();

    // package id -> the toolchain build-deps it declares. Only structured
    // `DependencyKind::Build` edges whose target name is a known compiler-
    // driver crate count.
    let mut compiles: HashMap<super::PackageId, BTreeSet<String>> = HashMap::new();
    for pkg in &metadata.packages {
        for dependency in metadata.resolved_dependencies(&pkg.id) {
            if dependency.kind != super::DependencyKind::Build {
                continue;
            }
            let Some(dep_name) = metadata.name(&dependency.package) else {
                continue;
            };
            if toolchain.contains(dep_name) {
                compiles
                    .entry(pkg.id.clone())
                    .or_default()
                    .insert(dep_name.to_owned());
            }
        }
    }

    let mut events = Vec::new();
    for pkg in &metadata.packages {
        if metadata.is_member(&pkg.id) {
            continue;
        }
        let links = pkg.links.clone();
        let toolchains: Vec<String> = compiles
            .get(&pkg.id)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();

        if links.is_none() && toolchains.is_empty() {
            continue;
        }
        events.push(NativeDependencyEvent {
            krate: pkg.name.clone(),
            version: pkg.version.clone(),
            links,
            toolchains,
        });
    }
    events.sort_by(|a, b| a.krate.cmp(&b.krate).then(a.version.cmp(&b.version)));
    events
}

/// `None` when neither signal fired (not a native dep). Otherwise the
/// label that goes in the event and JSON output.
fn classify(links: bool, compiles: bool) -> Option<NativeReason> {
    match (links, compiles) {
        (true, true) => Some(NativeReason::Both),
        (true, false) => Some(NativeReason::Links),
        (false, true) => Some(NativeReason::Compiles),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_covers_all_combinations() {
        assert_eq!(classify(true, true), Some(NativeReason::Both));
        assert_eq!(classify(true, false), Some(NativeReason::Links));
        assert_eq!(classify(false, true), Some(NativeReason::Compiles));
        assert_eq!(classify(false, false), None);
    }
}
