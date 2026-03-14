//! Comparison logic for litehtml visual reference testing.

use std::fmt;
use std::path::Path;

use crate::error::DevError;

#[allow(dead_code)]
pub(crate) struct PixelDiffResult {
    pub(crate) diff_pct: f64,
    pub(crate) total_pixels: u64,
    pub(crate) diff_pixels: u64,
}

#[allow(dead_code)]
pub(crate) struct ElementMatchResult {
    pub(crate) match_pct: f64,
    pub(crate) total_elements: usize,
    pub(crate) matched_elements: usize,
}

pub(crate) enum Status {
    Pass,
    FailThreshold,
    Regression,
    ExpectedFail,
    Error,
}

impl Status {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::FailThreshold => "FAIL_THRESHOLD",
            Status::Regression => "REGRESSION",
            Status::ExpectedFail => "EXPECTED_FAIL",
            Status::Error => "ERROR",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub(crate) fn compare_pixels(
    _pipeline_png: &Path,
    _reference_png: &Path,
    _diff_output: &Path,
) -> Result<PixelDiffResult, DevError> {
    Err(DevError::Verify(
        "pixel comparison not yet implemented".into(),
    ))
}

pub(crate) fn compare_elements(
    _pipeline_json: &Path,
    _reference_json: &Path,
) -> Result<ElementMatchResult, DevError> {
    Err(DevError::Verify(
        "element comparison not yet implemented".into(),
    ))
}

/// Regression tolerance: if the current pixel diff is within this margin of
/// the approved value, it is not considered a regression.
const REGRESSION_TOLERANCE: f64 = 0.5;

pub(crate) fn determine_status(
    pixel_diff_pct: f64,
    element_match_pct: Option<f64>,
    pixel_threshold: f64,
    element_threshold: Option<f64>,
    expected_fail: bool,
    approved_pixel: Option<f64>,
) -> Status {
    let pixel_exceeds = pixel_diff_pct > pixel_threshold;

    let element_below = match (element_match_pct, element_threshold) {
        (Some(match_pct), Some(threshold)) => match_pct < threshold,
        _ => false,
    };

    let threshold_exceeded = pixel_exceeds || element_below;

    if expected_fail && threshold_exceeded {
        return Status::ExpectedFail;
    }

    if threshold_exceeded {
        return Status::FailThreshold;
    }

    if let Some(approved) = approved_pixel
        && pixel_diff_pct > approved + REGRESSION_TOLERANCE
    {
        return Status::Regression;
    }

    Status::Pass
}
