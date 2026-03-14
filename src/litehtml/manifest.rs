//! Parses `fixtures/fixtures.toml` from the litehtml-rs project.

use std::path::Path;

use serde::Deserialize;

use crate::error::DevError;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub(crate) struct Defaults {
    pub viewport_width: u32,
    pub mode: String,
    pub pixel_diff_threshold: f64,
    pub element_match_threshold: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub(crate) struct FixtureEntry {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub viewport_width: Option<u32>,
    pub mode: Option<String>,
    pub pixel_diff_threshold: Option<f64>,
    pub element_match_threshold: Option<f64>,
    pub expected: String,
    #[serde(default)]
    pub waive_element_threshold: bool,
    pub notes: Option<String>,
}

impl FixtureEntry {
    pub(crate) fn resolved_pixel_threshold(&self, defaults: &Defaults) -> f64 {
        self.pixel_diff_threshold
            .unwrap_or(defaults.pixel_diff_threshold)
    }

    pub(crate) fn resolved_element_threshold(&self, defaults: &Defaults) -> f64 {
        self.element_match_threshold
            .unwrap_or(defaults.element_match_threshold)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub defaults: Defaults,
    #[serde(rename = "fixture")]
    pub fixtures: Vec<FixtureEntry>,
}

impl Manifest {
    pub(crate) fn load(project_root: &Path) -> Result<Manifest, DevError> {
        let path = project_root.join("fixtures/fixtures.toml");
        let content = std::fs::read_to_string(&path).map_err(|e| {
            DevError::Config(format!("failed to read {}: {e}", path.display()))
        })?;
        let manifest: Manifest = toml::from_str(&content)?;
        Ok(manifest)
    }

    pub(crate) fn fixtures_for_suite(&self, suite: &str) -> Vec<&FixtureEntry> {
        self.fixtures
            .iter()
            .filter(|f| f.tags.iter().any(|t| t == suite))
            .collect()
    }

    #[allow(dead_code)]
    pub(crate) fn fixture_by_id(&self, id: &str) -> Option<&FixtureEntry> {
        self.fixtures.iter().find(|f| f.id == id)
    }

    pub(crate) fn fixture_by_path(&self, path: &str) -> Option<&FixtureEntry> {
        let stem = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str());

        self.fixtures.iter().find(|f| {
            if f.path == path {
                return true;
            }
            if let Some(stem) = stem
                && let Some(f_stem) = Path::new(&f.path).file_stem().and_then(|s| s.to_str())
            {
                return f_stem == stem;
            }
            false
        })
    }
}
