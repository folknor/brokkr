use crate::error::DevError;
use serde::Deserialize;

/// Sluggrs visual snapshot testing configuration from `[sluggrs]` in brokkr.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SluggrsConfig {
    pub width: u32,
    pub height: u32,
    pub pixel_diff_threshold: f64,
    #[serde(rename = "snapshot")]
    pub snapshots: Vec<SluggrsSnapshot>,
}

/// A single sluggrs snapshot definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SluggrsSnapshot {
    pub id: String,
    pub description: String,
    pub fonts: Vec<String>,
    #[serde(default)]
    pub optional_fonts: Option<Vec<String>>,
}

impl SluggrsConfig {
    pub fn snapshot_by_id(&self, id: &str) -> Option<&SluggrsSnapshot> {
        self.snapshots.iter().find(|s| s.id == id)
    }
}

/// Parse the required `[sluggrs]` section from the root table.
pub(crate) fn parse(table: &toml::Table) -> Result<SluggrsConfig, DevError> {
    let value = table.get("sluggrs").ok_or_else(|| {
        DevError::Config("missing required [sluggrs] section in brokkr.toml".into())
    })?;
    let config: SluggrsConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("sluggrs: {e}")))?;
    Ok(config)
}
