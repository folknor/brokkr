//! `path_dependency` phase.
//!
//! Cargo represents both workspace members and `{ path = "..." }` deps as
//! packages with `source: null`. We emit only the ones that are *not*
//! workspace members - workspace path-linking is the whole point of a
//! workspace and never smelly. A path dep outside the workspace, however,
//! is usually either a dev shortcut (forgot to publish a fork) or a hand-
//! patched dependency that wouldn't reproduce on a clean checkout.

use serde::Serialize;

use super::{CargoMetadata, PackageSourceKind};

#[derive(Serialize)]
pub(super) struct PathDependencyEvent {
    #[serde(rename = "crate")]
    pub(super) krate: String,
    pub(super) version: String,
    /// Absolute path to the dep's `Cargo.toml`.
    pub(super) manifest_path: String,
}

pub fn run(metadata: &CargoMetadata) -> Vec<PathDependencyEvent> {
    let mut events = Vec::new();
    for pkg in &metadata.packages {
        if metadata.source_kind(&pkg.id) != PackageSourceKind::Path {
            continue;
        }
        events.push(PathDependencyEvent {
            krate: pkg.name.clone(),
            version: pkg.version.clone(),
            manifest_path: pkg.manifest_path.clone(),
        });
    }
    events.sort_by(|a, b| a.krate.cmp(&b.krate).then(a.version.cmp(&b.version)));
    events
}
