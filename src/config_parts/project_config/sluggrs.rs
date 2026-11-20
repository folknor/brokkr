use crate::error::DevError;
use serde::Deserialize;
use std::collections::HashSet;

/// Sluggrs visual snapshot testing configuration from `[sluggrs]` in brokkr.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SluggrsConfig {
    pub width: u32,
    pub height: u32,
    pub pixel_diff_threshold: f64,
    /// Ratchet slack in percentage points; absent means
    /// [`crate::visual::status::DEFAULT_REGRESSION_TOLERANCE`].
    #[serde(default)]
    pub regression_tolerance: Option<f64>,
    /// Per-channel pixel tolerance, percent of 0-255; absent means
    /// [`crate::visual::compare::DEFAULT_PIXEL_FUZZ_PERCENT`].
    #[serde(default)]
    pub pixel_fuzz_percent: Option<i64>,
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
    /// Validate snapshot identities at config load, including snapshots that
    /// are not selected by the current command.
    fn validate(&self) -> Result<(), DevError> {
        if let Some(tolerance) = self.regression_tolerance {
            crate::visual::status::validate_tolerance(
                "sluggrs",
                "regression_tolerance",
                tolerance,
            )?;
        }
        if let Some(fuzz) = self.pixel_fuzz_percent {
            crate::visual::compare::validate_fuzz_percent("sluggrs", fuzz)?;
        }
        let mut ids = HashSet::new();
        for snapshot in &self.snapshots {
            crate::visual::compare::validate_artifact_id(&snapshot.id, "snapshot")?;
            if !ids.insert(snapshot.id.as_str()) {
                return Err(DevError::Config(format!(
                    "duplicate sluggrs snapshot ID {:?}",
                    snapshot.id
                )));
            }
        }
        Ok(())
    }

    pub fn effective_regression_tolerance(&self) -> f64 {
        self.regression_tolerance
            .unwrap_or(crate::visual::status::DEFAULT_REGRESSION_TOLERANCE)
    }

    /// Load-time validation guarantees 0..=100.
    pub fn effective_pixel_fuzz_percent(&self) -> u8 {
        self.pixel_fuzz_percent
            .and_then(|p| u8::try_from(p).ok())
            .unwrap_or(crate::visual::compare::DEFAULT_PIXEL_FUZZ_PERCENT)
    }

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
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::unwrap_in_result)]
    use super::*;

    fn parse_str(body: &str) -> Result<SluggrsConfig, DevError> {
        let table: toml::Table = toml::from_str(body).unwrap();
        parse(&table)
    }

    const BASE: &str = "[sluggrs]\nwidth = 64\nheight = 64\npixel_diff_threshold = 0.5\n\
                        snapshot = []\n";

    #[test]
    fn comparison_policy_defaults_and_validation() {
        let cfg = parse_str(BASE).unwrap();
        assert_eq!(cfg.effective_regression_tolerance(), 0.5);
        assert_eq!(cfg.effective_pixel_fuzz_percent(), 5);

        let cfg = parse_str(&format!(
            "{BASE}regression_tolerance = 0.0\npixel_fuzz_percent = 10\n"
        ))
        .unwrap();
        assert_eq!(cfg.effective_regression_tolerance(), 0.0);
        assert_eq!(cfg.effective_pixel_fuzz_percent(), 10);

        for (key, bad) in [
            ("regression_tolerance", "-1.0"),
            ("regression_tolerance", "inf"),
            ("pixel_fuzz_percent", "101"),
            ("pixel_fuzz_percent", "-1"),
        ] {
            let err = parse_str(&format!("{BASE}{key} = {bad}\n"))
                .expect_err(&format!("{key} = {bad} accepted"));
            assert!(err.to_string().contains(key), "{key}: {err}");
        }
    }
}
