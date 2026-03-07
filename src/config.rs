use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::DevError;
use crate::project::Project;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct DevConfig {
    pub hosts: HashMap<String, HostConfig>,
}

/// A single PBF file entry (one variant like raw, indexed, locations).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PbfEntry {
    pub file: String,
    #[serde(alias = "sha256")]
    pub xxhash: Option<String>,
    pub seq: Option<u64>,
}

/// A single OSC diff file entry, keyed by sequence number.
#[derive(Debug, Clone, Deserialize)]
pub struct OscEntry {
    pub file: String,
    #[serde(alias = "sha256")]
    pub xxhash: Option<String>,
}

/// A PMTiles archive entry, keyed by variant name (e.g. "elivagar").
#[derive(Debug, Clone, Deserialize)]
pub struct PmtilesEntry {
    pub file: String,
    #[serde(alias = "sha256")]
    pub xxhash: Option<String>,
}

/// A dataset with structured PBF variants and multiple OSC entries.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Dataset {
    pub origin: Option<String>,
    pub download_date: Option<String>,
    pub bbox: Option<String>,
    pub data_dir: Option<String>,
    /// PBF variants keyed by name (e.g. "raw", "indexed", "locations").
    #[serde(default)]
    pub pbf: HashMap<String, PbfEntry>,
    /// OSC files keyed by sequence number.
    #[serde(default)]
    pub osc: HashMap<String, OscEntry>,
    /// PMTiles archives keyed by variant name (e.g. "elivagar").
    #[serde(default)]
    pub pmtiles: HashMap<String, PmtilesEntry>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct HostConfig {
    pub data: Option<String>,
    pub scratch: Option<String>,
    pub target: Option<String>,
    pub port: Option<u16>,
    pub drives: Option<DriveConfig>,
    pub preview: Option<PreviewConfig>,
    /// Cargo features to enable by default for all build commands on this host.
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub datasets: HashMap<String, Dataset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DriveConfig {
    pub source: Option<String>,
    pub data: Option<String>,
    pub scratch: Option<String>,
    pub target: Option<String>,
}

/// Cross-project source tree paths for the preview pipeline.
#[derive(Debug, Clone, Deserialize)]
pub struct PreviewConfig {
    pub pbfhogg: String,
    pub elivagar: String,
    pub nidhogg: String,
}

#[allow(dead_code)]
pub struct ResolvedPaths {
    pub hostname: String,
    pub data_dir: PathBuf,
    pub scratch_dir: PathBuf,
    pub target_dir: PathBuf,
    pub drives: Option<DriveConfig>,
    pub preview: Option<PreviewConfig>,
    pub features: Vec<String>,
    pub datasets: HashMap<String, Dataset>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load `brokkr.toml` from the project root directory.
///
/// Returns both the detected `Project` and the parsed `DevConfig`.
/// This is the **single code path** that reads and parses `brokkr.toml`.
pub fn load(project_root: &Path) -> Result<(Project, DevConfig), DevError> {
    let path = project_root.join("brokkr.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        DevError::Config(format!("{}: {e}", path.display()))
    })?;

    let root: toml::Value = text.parse()?;

    let table = root
        .as_table()
        .ok_or_else(|| DevError::Config("brokkr.toml root is not a table".into()))?;

    let project_str = table
        .get("project")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            DevError::Config("brokkr.toml missing required 'project' field".into())
        })?;

    let project = match project_str {
        "pbfhogg" => Project::Pbfhogg,
        "elivagar" => Project::Elivagar,
        "nidhogg" => Project::Nidhogg,
        "brokkr" => Project::Brokkr,
        other => {
            return Err(DevError::Config(format!(
                "unknown project '{other}' in brokkr.toml (expected: pbfhogg, elivagar, nidhogg, brokkr)"
            )));
        }
    };

    let hosts = parse_hosts(table)?;

    Ok((project, DevConfig { hosts }))
}

/// Every top-level key that is a table and is not `project` is
/// treated as a hostname section.
fn parse_hosts(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<HashMap<String, HostConfig>, DevError> {
    let mut out = HashMap::new();
    for (key, value) in table {
        if key == "project" {
            continue;
        }
        if !value.is_table() {
            return Err(DevError::Config(format!(
                "unknown key '{key}' in brokkr.toml"
            )));
        }
        let hc: HostConfig = value.clone().try_into().map_err(|e: toml::de::Error| {
            DevError::Config(format!("{key}: {e}"))
        })?;
        out.insert(key.clone(), hc);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Host features
// ---------------------------------------------------------------------------

/// Return the default cargo features configured for the current host.
pub fn host_features(config: &DevConfig) -> Vec<String> {
    let Ok(name) = hostname() else { return Vec::new() };
    config.hosts.get(&name)
        .map(|h| h.features.clone())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Hostname
// ---------------------------------------------------------------------------

/// Get the current hostname via `libc::gethostname()`.
pub fn hostname() -> Result<String, DevError> {
    let mut buf = [0u8; 256];
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if ret != 0 {
        return Err(DevError::Config("gethostname failed".into()));
    }

    let len = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| DevError::Config("hostname not null-terminated".into()))?;

    String::from_utf8(buf[..len].to_vec())
        .map_err(|e| DevError::Config(format!("hostname is not utf-8: {e}")))
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve host-specific paths from config, with defaults for unknown hosts.
///
/// - `project_root`: the root of the project
/// - `target_dir`: from cargo metadata (resolved elsewhere)
pub fn resolve_paths(
    config: &DevConfig,
    hostname: &str,
    project_root: &Path,
    target_dir: &Path,
) -> ResolvedPaths {
    let host = config.hosts.get(hostname);

    let data_rel = host
        .and_then(|h| h.data.as_deref())
        .unwrap_or("data");

    let scratch_rel = host
        .and_then(|h| h.scratch.as_deref())
        .unwrap_or("data/scratch");

    let data_dir = resolve_relative(project_root, data_rel);
    let scratch_dir = resolve_relative(project_root, scratch_rel);

    let target_dir = match host.and_then(|h| h.target.as_deref()) {
        Some(t) => resolve_relative(project_root, t),
        None => target_dir.to_path_buf(),
    };

    let drives = host.and_then(|h| h.drives.clone());
    let preview = host.and_then(|h| h.preview.clone());

    let features = host
        .map(|h| h.features.clone())
        .unwrap_or_default();

    let datasets = host
        .map(|h| h.datasets.clone())
        .unwrap_or_default();

    ResolvedPaths {
        hostname: hostname.to_owned(),
        data_dir,
        scratch_dir,
        target_dir,
        drives,
        preview,
        features,
        datasets,
    }
}

/// Resolve a potentially relative path against a base directory.
/// Absolute paths are returned as-is.
fn resolve_relative(base: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(hosts: HashMap<String, HostConfig>) -> DevConfig {
        DevConfig { hosts }
    }

    fn empty_dataset() -> Dataset {
        Dataset {
            origin: None, download_date: None, bbox: None, data_dir: None,
            pbf: HashMap::new(), osc: HashMap::new(), pmtiles: HashMap::new(),
        }
    }

    // -------------------------------------------------------------------
    // resolve_paths
    // -------------------------------------------------------------------

    #[test]
    fn host_datasets_resolved() {
        let mut pbf = HashMap::new();
        pbf.insert("indexed".into(), PbfEntry {
            file: "dk-indexed.osm.pbf".into(), xxhash: None, seq: Some(4704),
        });
        let mut host_ds = HashMap::new();
        host_ds.insert("dk".into(), Dataset {
            bbox: Some("1,2,3,4".into()),
            pbf,
            ..empty_dataset()
        });
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), HostConfig {
            data: None, scratch: None, target: None, port: None,
            drives: None, preview: None, features: Vec::new(), datasets: host_ds,
        });
        let config = make_config(hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.pbf.get("indexed").unwrap().file, "dk-indexed.osm.pbf");
        assert_eq!(dk.bbox.as_deref(), Some("1,2,3,4"));
    }

    #[test]
    fn unknown_host_gets_empty_datasets() {
        let config = make_config(HashMap::new());
        let resolved = resolve_paths(&config, "unknown", Path::new("/proj"), Path::new("/target"));
        assert!(resolved.datasets.is_empty());
    }

    #[test]
    fn multiple_pbf_variants() {
        let mut pbf = HashMap::new();
        pbf.insert("raw".into(), PbfEntry {
            file: "dk-raw.osm.pbf".into(), xxhash: Some("aaa".into()), seq: Some(4704),
        });
        pbf.insert("indexed".into(), PbfEntry {
            file: "dk-indexed.osm.pbf".into(), xxhash: Some("bbb".into()), seq: None,
        });
        pbf.insert("locations".into(), PbfEntry {
            file: "dk-locations.osm.pbf".into(), xxhash: None, seq: None,
        });
        let mut host_ds = HashMap::new();
        host_ds.insert("dk".into(), Dataset { pbf, ..empty_dataset() });
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), HostConfig {
            data: None, scratch: None, target: None, port: None,
            drives: None, preview: None, features: Vec::new(), datasets: host_ds,
        });
        let config = make_config(hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.pbf.len(), 3);
        assert_eq!(dk.pbf.get("raw").unwrap().xxhash.as_deref(), Some("aaa"));
        assert_eq!(dk.pbf.get("indexed").unwrap().xxhash.as_deref(), Some("bbb"));
    }

    #[test]
    fn multiple_osc_entries() {
        let mut osc = HashMap::new();
        osc.insert("4705".into(), OscEntry {
            file: "dk-4705.osc.gz".into(), xxhash: Some("ccc".into()),
        });
        osc.insert("4706".into(), OscEntry {
            file: "dk-4706.osc.gz".into(), xxhash: None,
        });
        let mut host_ds = HashMap::new();
        host_ds.insert("dk".into(), Dataset { osc, ..empty_dataset() });
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), HostConfig {
            data: None, scratch: None, target: None, port: None,
            drives: None, preview: None, features: Vec::new(), datasets: host_ds,
        });
        let config = make_config(hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.osc.len(), 2);
        assert_eq!(dk.osc.get("4705").unwrap().file, "dk-4705.osc.gz");
    }

    // -------------------------------------------------------------------
    // TOML parsing
    // -------------------------------------------------------------------

    #[test]
    fn parse_nested_dataset_from_toml() {
        let toml_str = r#"
project = "pbfhogg"

[myhost.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "8.0,54.5,13.0,58.0"

[myhost.datasets.denmark.pbf.raw]
file = "dk-raw.osm.pbf"
sha256 = "aaa"
seq = 4704

[myhost.datasets.denmark.pbf.indexed]
file = "dk-indexed.osm.pbf"
sha256 = "bbb"

[myhost.datasets.denmark.osc.4705]
file = "dk-4705.osc.gz"
sha256 = "ccc"
"#;
        let root: toml::Value = toml_str.parse().unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        let host = hosts.get("myhost").unwrap();
        let dk = host.datasets.get("denmark").unwrap();
        assert_eq!(dk.origin.as_deref(), Some("Geofabrik"));
        assert_eq!(dk.download_date.as_deref(), Some("2026-02-20"));
        assert_eq!(dk.bbox.as_deref(), Some("8.0,54.5,13.0,58.0"));
        assert_eq!(dk.pbf.get("raw").unwrap().file, "dk-raw.osm.pbf");
        assert_eq!(dk.pbf.get("raw").unwrap().seq, Some(4704));
        assert_eq!(dk.pbf.get("indexed").unwrap().xxhash.as_deref(), Some("bbb"));
        assert_eq!(dk.osc.get("4705").unwrap().file, "dk-4705.osc.gz");
    }

    #[test]
    fn parse_no_host_section() {
        let toml_str = r#"project = "pbfhogg""#;
        let root: toml::Value = toml_str.parse().unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        assert!(hosts.is_empty());
    }

    #[test]
    fn parse_pmtiles_entries() {
        let toml_str = r#"
project = "nidhogg"

[myhost.datasets.denmark.pmtiles.elivagar]
file = "denmark-elivagar.pmtiles"
sha256 = "ddd"
"#;
        let root: toml::Value = toml_str.parse().unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        let host = hosts.get("myhost").unwrap();
        let dk = host.datasets.get("denmark").unwrap();
        assert_eq!(dk.pmtiles.len(), 1);
        assert_eq!(dk.pmtiles.get("elivagar").unwrap().file, "denmark-elivagar.pmtiles");
        assert_eq!(dk.pmtiles.get("elivagar").unwrap().xxhash.as_deref(), Some("ddd"));
    }
}
