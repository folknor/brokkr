use crate::error::DevError;
use serde::Deserialize;

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
    Ok(config)
}
