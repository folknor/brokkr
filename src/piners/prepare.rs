//! Load, select, and verify the piners parity corpus for a run.

use std::path::{Path, PathBuf};

use crate::config::project_config::piners::PinersConfig;
use crate::error::DevError;
use crate::lockfile::LockGuard;
use crate::output;
use crate::piners::registry::{self, Registry, VerifiedProbe};
use crate::piners::select::{self, SelectArgs};

/// A locked, verified corpus selection shared by parity and measured runs.
pub(crate) struct PreparedSelection {
    pub registry: Registry,
    pub registry_dir: PathBuf,
    pub corpus_root: PathBuf,
    pub ids: Vec<String>,
    pub verified: Vec<VerifiedProbe>,
    pub feed_count: usize,
    pub file_count: usize,
}

/// Load the registry, resolve one selection, and verify every input it names.
pub(crate) fn prepare(
    project_root: &Path,
    config: &PinersConfig,
    lock: &LockGuard,
    selection: &SelectArgs,
) -> Result<PreparedSelection, DevError> {
    let registry_dir = project_root.join(config.registry_dir());
    let corpus_root = project_root.join(config.corpus_root());
    let registry = Registry::load(&registry_dir, &corpus_root, lock)?;
    registry.lint()?;
    let ids = select::resolve(&registry, selection)?;

    output::corpus_msg(&format!(
        "verifying {} against {}",
        output::count(ids.len(), "probe"),
        corpus_root.display()
    ));
    let mut verified = Vec::with_capacity(ids.len());
    for id in &ids {
        let (pin, probe_config) =
            registry
                .pins
                .get(id)
                .zip(registry.config(id))
                .ok_or_else(|| {
                    DevError::Internal(format!("piners: selected id '{id}' absent from pins"))
                })?;
        verified.push(registry::verify_probe(
            id,
            pin,
            probe_config,
            &corpus_root,
            project_root,
        )?);
    }
    let feed_count = verify_selected_feeds(&ids, &registry, &corpus_root, project_root)?;
    let file_count = registry::verify_harness_files(&registry, &corpus_root, project_root)?;

    Ok(PreparedSelection {
        registry,
        registry_dir,
        corpus_root,
        ids,
        verified,
        feed_count,
        file_count,
    })
}

/// Verify the feed groups referenced by this selection as part of the same
/// integrity gate as probe and harness-file pins.
fn verify_selected_feeds(
    ids: &[String],
    registry: &Registry,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<usize, DevError> {
    let referenced: std::collections::BTreeSet<String> = ids
        .iter()
        .filter_map(|id| registry.config(id).and_then(|config| config.feed))
        .collect();
    for name in &referenced {
        let group = registry.feeds.get(name).ok_or_else(|| {
            DevError::Internal(format!(
                "piners: referenced feed group '{name}' absent from [feeds]"
            ))
        })?;
        registry::verify_feed_group(name, group, corpus_root, project_root)?;
    }
    Ok(referenced.len())
}
