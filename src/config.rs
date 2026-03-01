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
    pub datasets: HashMap<String, Dataset>,
    pub hosts: HashMap<String, HostConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Dataset {
    pub pbf: Option<String>,
    pub pbf_raw: Option<String>,
    pub osc: Option<String>,
    pub bbox: Option<String>,
    pub data_dir: Option<String>,
    pub origin: Option<String>,
    pub sha256_pbf: Option<String>,
    pub sha256_osc: Option<String>,
}

impl Dataset {
    /// Merge with a host override. Host `Some` fields win; `None` fields inherit from self.
    pub fn merge(&self, over: &Dataset) -> Dataset {
        Dataset {
            pbf: over.pbf.clone().or_else(|| self.pbf.clone()),
            pbf_raw: over.pbf_raw.clone().or_else(|| self.pbf_raw.clone()),
            osc: over.osc.clone().or_else(|| self.osc.clone()),
            bbox: over.bbox.clone().or_else(|| self.bbox.clone()),
            data_dir: over.data_dir.clone().or_else(|| self.data_dir.clone()),
            origin: over.origin.clone().or_else(|| self.origin.clone()),
            sha256_pbf: over.sha256_pbf.clone().or_else(|| self.sha256_pbf.clone()),
            sha256_osc: over.sha256_osc.clone().or_else(|| self.sha256_osc.clone()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct HostConfig {
    pub data: Option<String>,
    pub scratch: Option<String>,
    pub target: Option<String>,
    pub port: Option<u16>,
    pub drives: Option<DriveConfig>,
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

#[allow(dead_code)]
pub struct ResolvedPaths {
    pub hostname: String,
    pub data_dir: PathBuf,
    pub scratch_dir: PathBuf,
    pub target_dir: PathBuf,
    pub drives: Option<DriveConfig>,
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

    let datasets = parse_global_datasets(table)?;
    let hosts = parse_hosts(table)?;

    Ok((project, DevConfig { datasets, hosts }))
}

/// Every top-level key that is a table and is not `datasets` is
/// treated as a hostname section.
fn parse_hosts(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<HashMap<String, HostConfig>, DevError> {
    let mut out = HashMap::new();
    for (key, value) in table {
        if key == "project" || key == "datasets" {
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

/// Parse the top-level `[datasets]` table into global dataset definitions.
/// Returns an empty map if the section is absent.
fn parse_global_datasets(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<HashMap<String, Dataset>, DevError> {
    match table.get("datasets") {
        Some(v) => {
            let map: HashMap<String, Dataset> = v.clone().try_into().map_err(
                |e: toml::de::Error| DevError::Config(format!("datasets: {e}")),
            )?;
            Ok(map)
        }
        None => Ok(HashMap::new()),
    }
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

    // Start with global datasets, then overlay host-specific overrides.
    let mut datasets = config.datasets.clone();
    if let Some(h) = host {
        for (name, host_ds) in &h.datasets {
            datasets
                .entry(name.clone())
                .and_modify(|global_ds| *global_ds = global_ds.merge(host_ds))
                .or_insert_with(|| host_ds.clone());
        }
    }

    ResolvedPaths {
        hostname: hostname.to_owned(),
        data_dir,
        scratch_dir,
        target_dir,
        drives,
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

    fn empty_dataset() -> Dataset {
        Dataset {
            pbf: None, pbf_raw: None, osc: None, bbox: None,
            data_dir: None, origin: None, sha256_pbf: None, sha256_osc: None,
        }
    }

    // -------------------------------------------------------------------
    // Dataset::merge
    // -------------------------------------------------------------------

    #[test]
    fn dataset_merge_override_wins() {
        let global = Dataset {
            pbf: Some("global.pbf".into()),
            osc: Some("global.osc.gz".into()),
            bbox: Some("1,2,3,4".into()),
            ..empty_dataset()
        };
        let host = Dataset {
            pbf: Some("host.pbf".into()),
            ..empty_dataset()
        };
        let merged = global.merge(&host);
        assert_eq!(merged.pbf.as_deref(), Some("host.pbf"));
        assert_eq!(merged.osc.as_deref(), Some("global.osc.gz"));
        assert_eq!(merged.bbox.as_deref(), Some("1,2,3,4"));
    }

    #[test]
    fn dataset_merge_no_override() {
        let global = Dataset {
            pbf: Some("g.pbf".into()),
            bbox: Some("1,2,3,4".into()),
            ..empty_dataset()
        };
        let merged = global.merge(&empty_dataset());
        assert_eq!(merged.pbf.as_deref(), Some("g.pbf"));
        assert_eq!(merged.bbox.as_deref(), Some("1,2,3,4"));
    }

    // -------------------------------------------------------------------
    // resolve_paths dataset merging
    // -------------------------------------------------------------------

    fn make_config(
        global_datasets: HashMap<String, Dataset>,
        hosts: HashMap<String, HostConfig>,
    ) -> DevConfig {
        DevConfig { datasets: global_datasets, hosts }
    }

    #[test]
    fn v1_host_only_datasets() {
        let mut host_ds = HashMap::new();
        host_ds.insert("dk".into(), Dataset {
            pbf: Some("dk.pbf".into()),
            ..empty_dataset()
        });
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), HostConfig {
            data: None, scratch: None, target: None, port: None,
            drives: None,
            datasets: host_ds,
        });
        let config = make_config(HashMap::new(), hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        assert_eq!(resolved.datasets.get("dk").unwrap().pbf.as_deref(), Some("dk.pbf"));
    }

    #[test]
    fn v2_global_datasets_no_host_override() {
        let mut global = HashMap::new();
        global.insert("dk".into(), Dataset {
            pbf: Some("global.pbf".into()),
            bbox: Some("1,2,3,4".into()),
            ..empty_dataset()
        });
        let config = make_config(global, HashMap::new());
        // Unknown host — gets global datasets.
        let resolved = resolve_paths(&config, "unknown", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.pbf.as_deref(), Some("global.pbf"));
        assert_eq!(dk.bbox.as_deref(), Some("1,2,3,4"));
    }

    #[test]
    fn v2_global_with_host_field_override() {
        let mut global = HashMap::new();
        global.insert("dk".into(), Dataset {
            pbf: Some("global.pbf".into()),
            osc: Some("global.osc.gz".into()),
            bbox: Some("1,2,3,4".into()),
            ..empty_dataset()
        });
        let mut host_ds = HashMap::new();
        host_ds.insert("dk".into(), Dataset {
            pbf: Some("host-specific.pbf".into()),
            ..empty_dataset()
        });
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), HostConfig {
            data: None, scratch: None, target: None, port: None,
            drives: None,
            datasets: host_ds,
        });
        let config = make_config(global, hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.pbf.as_deref(), Some("host-specific.pbf"), "host override wins");
        assert_eq!(dk.osc.as_deref(), Some("global.osc.gz"), "global inherited");
        assert_eq!(dk.bbox.as_deref(), Some("1,2,3,4"), "global inherited");
    }

    #[test]
    fn v2_host_only_dataset_not_in_global() {
        let mut global = HashMap::new();
        global.insert("dk".into(), Dataset {
            pbf: Some("dk.pbf".into()),
            ..empty_dataset()
        });
        let mut host_ds = HashMap::new();
        host_ds.insert("se".into(), Dataset {
            pbf: Some("sweden.pbf".into()),
            ..empty_dataset()
        });
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), HostConfig {
            data: None, scratch: None, target: None, port: None,
            drives: None,
            datasets: host_ds,
        });
        let config = make_config(global, hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        assert!(resolved.datasets.contains_key("dk"), "global dataset present");
        assert!(resolved.datasets.contains_key("se"), "host-only dataset present");
    }

    // -------------------------------------------------------------------
    // parse_global_datasets
    // -------------------------------------------------------------------

    #[test]
    fn parse_global_datasets_from_toml() {
        let toml_str = r#"
project = "pbfhogg"

[datasets.denmark]
pbf = "dk.pbf"
bbox = "8.0,54.5,13.0,58.0"
"#;
        let root: toml::Value = toml_str.parse().unwrap();
        let table = root.as_table().unwrap();
        let ds = parse_global_datasets(table).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds["denmark"].pbf.as_deref(), Some("dk.pbf"));
        assert_eq!(ds["denmark"].bbox.as_deref(), Some("8.0,54.5,13.0,58.0"));
    }

    #[test]
    fn parse_global_datasets_missing_section() {
        let toml_str = r#"project = "pbfhogg""#;
        let root: toml::Value = toml_str.parse().unwrap();
        let table = root.as_table().unwrap();
        let ds = parse_global_datasets(table).unwrap();
        assert!(ds.is_empty());
    }
}
