//! The resolved-probe manifest brokkr hands to the harness.
//!
//! brokkr does all resolution and verification, then writes
//! `manifest.json` into the run dir and passes its path to the harness.
//! The harness consumes the manifest only - it never re-resolves paths or
//! re-checks hashes.
//!
//! Schema (the harness's contract, version 2): a top-level absolute
//! `corpus_root`, and per-probe a `probe_dir` plus the two pinned files,
//! all expressed **relative to `corpus_root`**. Each entry also carries
//! the explicit canonical `probe` id (the `pins.toml` key). The harness
//! prefers `probe` over deriving an id from `probe_dir`'s basename - the
//! basename holds for the upstream PineForge layout but is fragile once
//! first-party probes land and an id may not equal its directory name.
//! With `probe` present the harness emits the canonical id in its NDJSON
//! and never guesses.
//!
//! Version 2 additions: `feeds` is the selection's referenced feed groups
//! with roles resolved absolute (`{"<group>": {"primary": "/abs/..."}}`),
//! and each probe carries its `feed` group name plus the optional
//! `bar_budget` / `ohlcv_start_ms` / `tv_trades_csv_tz` overrides when
//! pinned. Harness behavior on these is piners' side of the contract.

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
    /// Feed group name (a key of the top-level `feeds`), when pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
    /// Override for the harness's scan bar cap, when pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bar_budget: Option<u64>,
    /// Piners-side OHLCV start override (epoch ms), when pinned.
    /// Probe-local `inputs.json` keeps precedence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ohlcv_start_ms: Option<i64>,
    /// Piners-side `tv_trades.csv` timezone override, when pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tv_trades_csv_tz: Option<String>,
}

/// Top-level manifest written to `manifest.json`.
#[derive(Debug, Serialize)]
pub struct Manifest {
    /// Schema version, so the harness can reject a manifest it predates.
    pub version: u32,
    /// Absolute corpus tree root. All probe paths resolve under here.
    pub corpus_root: PathBuf,
    pub probes: Vec<ManifestProbe>,
    /// The selection's referenced feed groups: group name -> role
    /// (`primary`/`warmup`/`lower`) -> absolute path.
    pub feeds: BTreeMap<String, BTreeMap<String, PathBuf>>,
}

/// Current manifest schema version.
const MANIFEST_VERSION: u32 = 2;

impl Manifest {
    /// Build a manifest from the verified probe set. `corpus_root` is the
    /// absolute corpus tree root; probe paths stay relative to it. The
    /// per-probe feed name and overrides come off the registry pins; the
    /// top-level `feeds` covers exactly the groups the selection
    /// references, roles resolved absolute.
    pub fn build(corpus_root: &Path, verified: &[VerifiedProbe], registry: &Registry) -> Self {
        let mut feeds: BTreeMap<String, BTreeMap<String, PathBuf>> = BTreeMap::new();
        let probes = verified
            .iter()
            .map(|v| {
                let pin = registry.pins.get(&v.id);
                let feed = pin.and_then(|p| p.feed.clone());
                if let Some(name) = &feed
                    && let Some(group) = registry.feeds.get(name)
                {
                    feeds.entry(name.clone()).or_insert_with(|| {
                        group
                            .roles()
                            .into_iter()
                            .map(|(role, f)| (role.to_owned(), corpus_root.join(&f.path)))
                            .collect()
                    });
                }
                ManifestProbe {
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
                    feed,
                    bar_budget: pin.and_then(|p| p.bar_budget),
                    ohlcv_start_ms: pin.and_then(|p| p.ohlcv_start_ms),
                    tv_trades_csv_tz: pin.and_then(|p| p.tv_trades_csv_tz.clone()),
                }
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
    use crate::piners::registry::{FeedGroup, FilePin, Pin, Registry};
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
            keywords,
            ..Registry::default()
        };
        let m = Manifest::build(Path::new("/abs/corpus"), &[verified("probe-01")], &registry);
        assert_eq!(m.version, 2);
        assert_eq!(m.corpus_root, PathBuf::from("/abs/corpus"));
        let p = &m.probes[0];
        assert_eq!(p.probe, "probe-01");
        assert_eq!(p.probe_dir, PathBuf::from("validation/probe-01"));
        assert_eq!(p.pine.path, PathBuf::from("validation/probe-01/strategy.pine"));
        assert_eq!(p.keywords, vec!["k".to_owned()]);
        assert_eq!(p.feed, None);
        assert!(m.feeds.is_empty());
    }

    #[test]
    fn resolves_referenced_feed_groups_and_per_probe_overrides() {
        let mut pin = Pin::new(
            FilePin {
                path: "validation/probe-01/strategy.pine".into(),
                xxh128: "aa".into(),
            },
            FilePin {
                path: "validation/probe-01/tv_trades.csv".into(),
                xxh128: "bb".into(),
            },
        );
        pin.feed = Some("eth-15m".to_owned());
        pin.bar_budget = Some(38000);
        pin.tv_trades_csv_tz = Some("America/New_York".to_owned());
        let mut pins = BTreeMap::new();
        pins.insert("probe-01".to_owned(), pin);

        let mut feeds = BTreeMap::new();
        feeds.insert(
            "eth-15m".to_owned(),
            FeedGroup {
                primary: FilePin {
                    path: "data/15m.csv".into(),
                    xxh128: "f0".into(),
                },
                warmup: Some(FilePin {
                    path: "data/15m_warmup.csv".into(),
                    xxh128: "f1".into(),
                }),
                lower: None,
            },
        );
        // An unreferenced group must NOT land in the manifest.
        feeds.insert(
            "unused".to_owned(),
            FeedGroup {
                primary: FilePin {
                    path: "data/other.csv".into(),
                    xxh128: "f2".into(),
                },
                warmup: None,
                lower: None,
            },
        );
        let registry = Registry {
            pins,
            feeds,
            ..Registry::default()
        };

        let m = Manifest::build(Path::new("/abs/corpus"), &[verified("probe-01")], &registry);

        let p = &m.probes[0];
        assert_eq!(p.feed.as_deref(), Some("eth-15m"));
        assert_eq!(p.bar_budget, Some(38000));
        assert_eq!(p.ohlcv_start_ms, None);
        assert_eq!(p.tv_trades_csv_tz.as_deref(), Some("America/New_York"));

        assert_eq!(m.feeds.len(), 1); // only the referenced group
        let group = &m.feeds["eth-15m"];
        assert_eq!(group["primary"], PathBuf::from("/abs/corpus/data/15m.csv"));
        assert_eq!(
            group["warmup"],
            PathBuf::from("/abs/corpus/data/15m_warmup.csv")
        );
        assert!(!group.contains_key("lower"));

        // Absent overrides stay out of the JSON entirely.
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"bar_budget\":38000"));
        assert!(!json.contains("ohlcv_start_ms"));
    }

    #[test]
    fn serializes_with_expected_keys() {
        let m = Manifest::build(Path::new("/c"), &[verified("p")], &Registry::default());
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"version\":2"));
        assert!(json.contains("\"corpus_root\""));
        assert!(json.contains("\"probe\":\"p\""));
        assert!(json.contains("\"probe_dir\""));
        assert!(json.contains("\"xxh128\""));
    }
}
