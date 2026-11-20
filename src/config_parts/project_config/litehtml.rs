use crate::error::DevError;
use serde::Deserialize;
use std::collections::HashSet;

const DEFAULT_FALLBACK_ASPECT_RATIO: f64 = 2.0;
pub(crate) const DEFAULT_POSITION_TOLERANCE: f64 = 2.0;
pub(crate) const DEFAULT_SIZE_TOLERANCE: f64 = 5.0;
pub(crate) const DEFAULT_OFFENDER_PRINT_LIMIT: usize = 10;

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
    /// Ratchet slack in percentage points; absent means
    /// [`crate::visual::status::DEFAULT_REGRESSION_TOLERANCE`].
    #[serde(default)]
    pub regression_tolerance: Option<f64>,
    /// Per-channel pixel tolerance, percent of 0-255; absent means
    /// [`crate::visual::compare::DEFAULT_PIXEL_FUZZ_PERCENT`].
    #[serde(default)]
    pub pixel_fuzz_percent: Option<i64>,
    /// Element x/y tolerance in CSS px; absent means
    /// [`DEFAULT_POSITION_TOLERANCE`].
    #[serde(default)]
    pub position_tolerance: Option<f64>,
    /// Element w/h tolerance in CSS px; absent means
    /// [`DEFAULT_SIZE_TOLERANCE`].
    #[serde(default)]
    pub size_tolerance: Option<f64>,
    /// How many offenders a failing row prints; absent means
    /// [`DEFAULT_OFFENDER_PRINT_LIMIT`]. The full list is always written.
    #[serde(default)]
    pub offender_print_limit: Option<i64>,
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

    /// The width both sides of the comparison render at: the Chrome capture
    /// viewport and the pipeline's `--width`. One resolver for both, so the
    /// two can never be handed different widths.
    pub fn resolved_viewport_width(&self, config: &LitehtmlConfig) -> u32 {
        self.viewport_width.unwrap_or(config.viewport_width)
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
        if let Some(fuzz) = self.pixel_fuzz_percent {
            crate::visual::compare::validate_fuzz_percent("litehtml", fuzz)?;
        }
        if let Some(limit) = self.offender_print_limit
            && limit < 0
        {
            return Err(DevError::Config(format!(
                "[litehtml].offender_print_limit must be >= 0, got {limit}"
            )));
        }
        for (key, value) in [
            ("regression_tolerance", self.regression_tolerance),
            ("position_tolerance", self.position_tolerance),
            ("size_tolerance", self.size_tolerance),
        ] {
            if let Some(value) = value {
                crate::visual::status::validate_tolerance("litehtml", key, value)?;
            }
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

    pub fn effective_position_tolerance(&self) -> f64 {
        self.position_tolerance
            .unwrap_or(DEFAULT_POSITION_TOLERANCE)
    }

    pub fn effective_size_tolerance(&self) -> f64 {
        self.size_tolerance.unwrap_or(DEFAULT_SIZE_TOLERANCE)
    }

    pub fn effective_offender_print_limit(&self) -> usize {
        self.offender_print_limit
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(DEFAULT_OFFENDER_PRINT_LIMIT)
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
    fn comparison_policy_defaults_and_validation() {
        let cfg = parse_str(&format!("{BASE}viewport_width = 800\n")).unwrap();
        assert_eq!(cfg.effective_regression_tolerance(), 0.5);
        assert_eq!(cfg.effective_pixel_fuzz_percent(), 5);
        assert_eq!(cfg.effective_position_tolerance(), 2.0);
        assert_eq!(cfg.effective_size_tolerance(), 5.0);
        assert_eq!(cfg.effective_offender_print_limit(), 10);

        let cfg = parse_str(&format!(
            "{BASE}viewport_width = 800\nregression_tolerance = 1.0\npixel_fuzz_percent = 0\n\
             position_tolerance = 0.0\nsize_tolerance = 3.5\noffender_print_limit = 0\n"
        ))
        .unwrap();
        assert_eq!(cfg.effective_regression_tolerance(), 1.0);
        assert_eq!(cfg.effective_pixel_fuzz_percent(), 0);
        assert_eq!(cfg.effective_size_tolerance(), 3.5);
        assert_eq!(cfg.effective_offender_print_limit(), 0);

        for (key, bad) in [
            ("regression_tolerance", "-0.1"),
            ("regression_tolerance", "nan"),
            ("position_tolerance", "inf"),
            ("size_tolerance", "-1.0"),
            ("pixel_fuzz_percent", "101"),
            ("pixel_fuzz_percent", "-1"),
            ("offender_print_limit", "-1"),
        ] {
            let err = parse_str(&format!("{BASE}viewport_width = 800\n{key} = {bad}\n"))
                .expect_err(&format!("{key} = {bad} accepted"));
            assert!(err.to_string().contains(key), "{key}: {err}");
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

    #[test]
    fn viewport_width_resolves_fixture_override_over_global() {
        let body = "[litehtml]\nviewport_width = 800\nmode = \"default\"\n\
                    pixel_diff_threshold = 1.0\nelement_match_threshold = 90.0\n\
                    [[litehtml.fixture]]\nid = \"narrow\"\npath = \"narrow.html\"\n\
                    expected = \"pass\"\nviewport_width = 720\n\
                    [[litehtml.fixture]]\nid = \"wide\"\npath = \"wide.html\"\n\
                    expected = \"pass\"\n";
        let cfg = parse_str(body).unwrap();
        assert_eq!(cfg.fixtures[0].resolved_viewport_width(&cfg), 720);
        assert_eq!(cfg.fixtures[1].resolved_viewport_width(&cfg), 800);
    }
}
