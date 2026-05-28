//! The resolved-probe manifest brokkr hands to the harness.
//!
//! brokkr does all resolution and verification, then writes
//! `manifest.json` into the run dir and passes its path to the harness.
//! The harness consumes the manifest only - it never re-resolves paths or
//! re-checks hashes.
//!
//! Schema (the harness's contract): a top-level absolute `corpus_root`,
//! and per-probe a `probe_dir` plus the two pinned files, all expressed
//! **relative to `corpus_root`**. Each entry also carries the explicit
//! canonical `probe` id (the `pins.toml` key). The harness prefers
//! `probe` over deriving an id from `probe_dir`'s basename - the basename
//! holds for the upstream PineForge layout but is fragile once first-party
//! probes land and an id may not equal its directory name. With `probe`
//! present the harness emits the canonical id in its NDJSON and never
//! guesses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::DevError;
use crate::piners::registry::{Registry, VerifiedProbe};

/// A pinned file in the manifest: path relative to `corpus_root` + xxh128.
#[derive(Debug, Serialize)]
pub struct ManifestFile {
    pub path: PathBuf,
    pub xxh128: String,
}

/// One probe in the manifest.
#[derive(Debug, Serialize)]
pub struct ManifestProbe {
    /// Canonical probe id (the `pins.toml` key). Authoritative - the
    /// harness emits this id rather than inferring one from `probe_dir`.
    pub probe: String,
    /// Probe directory relative to `corpus_root`.
    pub probe_dir: PathBuf,
    pub pine: ManifestFile,
    pub csv: ManifestFile,
    /// Keywords in the registry that contain this id (provenance).
    pub keywords: Vec<String>,
}

/// Top-level manifest written to `manifest.json`.
#[derive(Debug, Serialize)]
pub struct Manifest {
    /// Schema version, so the harness can reject a manifest it predates.
    pub version: u32,
    /// Absolute corpus submodule root. All probe paths resolve under here.
    pub corpus_root: PathBuf,
    pub probes: Vec<ManifestProbe>,
    /// Shared OHLCV feed paths (absolute), passed through verbatim.
    pub feeds: BTreeMap<String, PathBuf>,
}

/// Current manifest schema version.
const MANIFEST_VERSION: u32 = 1;

impl Manifest {
    /// Build a manifest from the verified probe set. `corpus_root` is the
    /// absolute submodule root; probe paths stay relative to it.
    pub fn build(
        corpus_root: &Path,
        verified: &[VerifiedProbe],
        registry: &Registry,
        feeds: BTreeMap<String, PathBuf>,
    ) -> Self {
        let probes = verified
            .iter()
            .map(|v| ManifestProbe {
                probe: v.id.clone(),
                probe_dir: v
                    .pine_rel
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
                pine: ManifestFile {
                    path: v.pine_rel.clone(),
                    xxh128: v.pine_xxh128.clone(),
                },
                csv: ManifestFile {
                    path: v.csv_rel.clone(),
                    xxh128: v.csv_xxh128.clone(),
                },
                keywords: registry.keywords_for(&v.id),
            })
            .collect();
        Self {
            version: MANIFEST_VERSION,
            corpus_root: corpus_root.to_path_buf(),
            probes,
            feeds,
        }
    }

    /// Serialize to pretty JSON and write to `path`.
    pub fn write(&self, path: &Path) -> Result<(), DevError> {
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            DevError::Config(format!("piners: failed to serialize manifest: {e}"))
        })?;
        std::fs::write(path, json).map_err(DevError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::registry::Registry;
    use std::collections::BTreeMap;

    fn verified(id: &str) -> VerifiedProbe {
        VerifiedProbe {
            id: id.to_owned(),
            pine_rel: PathBuf::from(format!("validation/{id}/strategy.pine")),
            pine_xxh128: "aa".into(),
            csv_rel: PathBuf::from(format!("validation/{id}/tv_trades.csv")),
            csv_xxh128: "bb".into(),
        }
    }

    #[test]
    fn carries_explicit_id_and_relative_dir() {
        let mut keywords = BTreeMap::new();
        keywords.insert("k".to_owned(), vec!["probe-01".to_owned()]);
        let registry = Registry {
            pins: BTreeMap::new(),
            keywords,
        };
        let m = Manifest::build(
            Path::new("/abs/corpus"),
            &[verified("probe-01")],
            &registry,
            BTreeMap::new(),
        );
        assert_eq!(m.corpus_root, PathBuf::from("/abs/corpus"));
        let p = &m.probes[0];
        assert_eq!(p.probe, "probe-01");
        assert_eq!(p.probe_dir, PathBuf::from("validation/probe-01"));
        assert_eq!(p.pine.path, PathBuf::from("validation/probe-01/strategy.pine"));
        assert_eq!(p.keywords, vec!["k".to_owned()]);
    }

    #[test]
    fn serializes_with_expected_keys() {
        let m = Manifest::build(
            Path::new("/c"),
            &[verified("p")],
            &Registry::default(),
            BTreeMap::new(),
        );
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"corpus_root\""));
        assert!(json.contains("\"probe\":\"p\""));
        assert!(json.contains("\"probe_dir\""));
        assert!(json.contains("\"xxh128\""));
    }
}
