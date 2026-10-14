use crate::error::DevError;
use serde::Deserialize;
use std::collections::HashSet;

const DEFAULT_FALLBACK_ASPECT_RATIO: f64 = 2.0;

/// Which pipeline renders a fixture: `ahem` (the Ahem-font fixture mode) or
/// `default` (the normal invocation, which omits `--fixture`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PipelineMode {
    Ahem,
    Default,
}

/// Whether a fixture is expected to pass or is a known failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExpectedOutcome {
    Pass,
    Fail,
}

impl std::fmt::Display for ExpectedOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `pad`, not `write_str`: the `list` table formats this with a width.
        f.pad(match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
        })
    }
}

/// Litehtml visual reference testing configuration from `[litehtml]` in brokkr.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LitehtmlConfig {
    pub viewport_width: u32,
    pub mode: PipelineMode,
    pub pixel_diff_threshold: f64,
    pub element_match_threshold: f64,
    pub fallback_aspect_ratio: Option<f64>,
    #[serde(rename = "fixture")]
    pub fixtures: Vec<LitehtmlFixture>,
}

/// A single litehtml test fixture entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct LitehtmlFixture {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub viewport_width: Option<u32>,
    pub mode: Option<PipelineMode>,
    pub pixel_diff_threshold: Option<f64>,
    pub element_match_threshold: Option<f64>,
    pub expected: ExpectedOutcome,
    #[serde(default)]
    pub waive_element_threshold: bool,
    pub notes: Option<String>,
}

impl LitehtmlFixture {
    pub fn resolved_pixel_threshold(&self, config: &LitehtmlConfig) -> f64 {
        self.pixel_diff_threshold
            .unwrap_or(config.pixel_diff_threshold)
    }

    pub fn resolved_mode(&self, config: &LitehtmlConfig) -> PipelineMode {
        self.mode.unwrap_or(config.mode)
    }

    pub fn resolved_element_threshold(&self, config: &LitehtmlConfig) -> f64 {
        self.element_match_threshold
            .unwrap_or(config.element_match_threshold)
    }
}

impl LitehtmlConfig {
    /// Validate the complete static configuration when it is loaded, so an
    /// invalid fixture ID or geometry setting fails even when that fixture is
    /// not selected by the current command.
    fn validate(&self) -> Result<(), DevError> {
        if self.viewport_width == 0 {
            return Err(DevError::Config(
                "[litehtml].viewport_width must be greater than zero".into(),
            ));
        }
        let fallback_aspect_ratio = self.effective_fallback_aspect_ratio();
        if !fallback_aspect_ratio.is_finite() || fallback_aspect_ratio <= 0.0 {
            return Err(DevError::Config(
                "[litehtml].fallback_aspect_ratio must be a finite number greater than zero".into(),
            ));
        }

        let mut ids = HashSet::new();
        for fixture in &self.fixtures {
            crate::visual::compare::validate_artifact_id(&fixture.id, "fixture")?;
            if !ids.insert(fixture.id.as_str()) {
                return Err(DevError::Config(format!(
                    "duplicate litehtml fixture ID {:?}",
                    fixture.id
                )));
            }
            if fixture.viewport_width == Some(0) {
                return Err(DevError::Config(format!(
                    "fixture {:?} viewport_width must be greater than zero",
                    fixture.id
                )));
            }
        }
        Ok(())
    }

    /// The fallback aspect ratio in effect: the configured value or the
    /// default. Load-time validation guarantees it is finite and positive.
    pub fn effective_fallback_aspect_ratio(&self) -> f64 {
        self.fallback_aspect_ratio
            .unwrap_or(DEFAULT_FALLBACK_ASPECT_RATIO)
    }

    pub fn fixtures_for_suite(&self, suite: &str) -> Vec<&LitehtmlFixture> {
        self.fixtures
            .iter()
            .filter(|f| f.tags.iter().any(|t| t == suite))
            .collect()
    }

    pub fn fixture_by_id(&self, id: &str) -> Option<&LitehtmlFixture> {
        self.fixtures.iter().find(|f| f.id == id)
    }
}

/// Parse the required `[litehtml]` section from the root table.
pub(crate) fn parse(table: &toml::Table) -> Result<LitehtmlConfig, DevError> {
    let value = table.get("litehtml").ok_or_else(|| {
        DevError::Config("missing required [litehtml] section in brokkr.toml".into())
    })?;
    let config: LitehtmlConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("litehtml: {e}")))?;
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::unwrap_in_result)]
    use super::*;

    fn parse_str(body: &str) -> Result<LitehtmlConfig, DevError> {
        let table: toml::Table = toml::from_str(body).unwrap();
        parse(&table)
    }

    const BASE: &str = "[litehtml]\nmode = \"default\"\npixel_diff_threshold = 1.0\n\
                        element_match_threshold = 90.0\nfixture = []\n";

    #[test]
    fn aspect_ratio_uses_one_positive_finite_default() {
        let cfg = parse_str(&format!("{BASE}viewport_width = 800\n")).unwrap();
        assert_eq!(
            cfg.effective_fallback_aspect_ratio(),
            DEFAULT_FALLBACK_ASPECT_RATIO
        );

        for invalid in ["0.0", "nan", "inf", "-1.0"] {
            let body = format!("{BASE}viewport_width = 800\nfallback_aspect_ratio = {invalid}\n");
            assert!(parse_str(&body).is_err(), "{invalid} accepted");
        }
    }

    #[test]
    fn capture_dimensions_must_be_positive() {
        assert!(parse_str(&format!("{BASE}viewport_width = 0\n")).is_err());

        let body = "[litehtml]\nviewport_width = 800\nmode = \"default\"\n\
                    pixel_diff_threshold = 1.0\nelement_match_threshold = 90.0\n\
                    [[litehtml.fixture]]\nid = \"small\"\npath = \"small.html\"\n\
                    expected = \"pass\"\nviewport_width = 0\n";
        assert!(parse_str(body).is_err());
    }
}
