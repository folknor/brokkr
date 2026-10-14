//! Status policy and typed outcomes shared by visual subjects.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Status {
    Pass,
    /// Rendered fine, but there is no approved baseline to compare against.
    /// This is the expected state of a new subject and does not fail a run.
    NoBaseline,
    FailThreshold,
    Regression,
    ExpectedFail,
    Error,
}

impl Status {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::NoBaseline => "NO_BASELINE",
            Status::FailThreshold => "FAIL_THRESHOLD",
            Status::Regression => "REGRESSION",
            Status::ExpectedFail => "EXPECTED_FAIL",
            Status::Error => "ERROR",
        }
    }

    pub(crate) fn fails_run(self) -> bool {
        matches!(
            self,
            Status::FailThreshold | Status::Regression | Status::Error
        )
    }

    pub(crate) fn annotation(
        self,
        current_pixel_diff: Option<f64>,
        approved_pixel_diff: Option<f64>,
        show_unapproved_pass: bool,
    ) -> &'static str {
        match self {
            Status::Pass
                if matches!(
                    (current_pixel_diff, approved_pixel_diff),
                    (Some(current), Some(approved))
                        if current < approved - REGRESSION_TOLERANCE
                ) =>
            {
                " (improved)"
            }
            Status::Pass if show_unapproved_pass => " (unapproved)",
            Status::NoBaseline => " (run `brokkr approve`)",
            _ => "",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug)]
pub(crate) struct UnknownStatus(String);

impl fmt::Display for UnknownStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown visual result status {:?}", self.0)
    }
}

impl std::error::Error for UnknownStatus {}

impl FromStr for Status {
    type Err = UnknownStatus;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "PASS" => Ok(Status::Pass),
            "NO_BASELINE" => Ok(Status::NoBaseline),
            "FAIL_THRESHOLD" => Ok(Status::FailThreshold),
            "REGRESSION" => Ok(Status::Regression),
            "EXPECTED_FAIL" => Ok(Status::ExpectedFail),
            "ERROR" => Ok(Status::Error),
            other => Err(UnknownStatus(other.to_owned())),
        }
    }
}

impl rusqlite::types::FromSql for Status {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
    }
}

/// Stable count model shared by all visual runners.
#[derive(Default)]
pub(crate) struct StatusTally {
    counts: BTreeMap<Status, u32>,
    unexpected_passes: u32,
}

impl StatusTally {
    pub(crate) fn record(&mut self, status: Status, unexpected_pass: bool) {
        *self.counts.entry(status).or_default() += 1;
        if unexpected_pass && status == Status::Pass {
            self.unexpected_passes += 1;
        }
    }

    pub(crate) fn count(&self, status: Status) -> u32 {
        self.counts.get(&status).copied().unwrap_or_default()
    }

    pub(crate) fn ordinary_passes(&self) -> u32 {
        self.count(Status::Pass) - self.unexpected_passes
    }

    pub(crate) fn unexpected_passes(&self) -> u32 {
        self.unexpected_passes
    }

    pub(crate) fn fails_run(&self) -> bool {
        self.counts
            .iter()
            .any(|(status, count)| *count > 0 && status.fails_run())
    }
}

pub(crate) const REGRESSION_TOLERANCE: f64 = 0.5;
const DISPLAY_DELTA_TOLERANCE: f64 = 0.05;

pub(crate) struct ComparisonThresholds {
    pub(crate) pixel: f64,
    pub(crate) element: Option<f64>,
}

pub(crate) struct ApprovedScores {
    pub(crate) pixel: Option<f64>,
    pub(crate) element: Option<f64>,
}

pub(crate) struct StatusInput {
    pub(crate) pixel_diff_pct: f64,
    pub(crate) element_match_pct: Option<f64>,
    pub(crate) thresholds: ComparisonThresholds,
    pub(crate) expected_fail: bool,
    pub(crate) approved: ApprovedScores,
}

pub(crate) fn determine_status(input: &StatusInput) -> Status {
    let pixel_exceeds = input.pixel_diff_pct > input.thresholds.pixel;
    let element_below = match (input.element_match_pct, input.thresholds.element) {
        (Some(match_pct), Some(threshold)) => match_pct < threshold,
        _ => false,
    };
    let threshold_exceeded = pixel_exceeds || element_below;

    if input.expected_fail && threshold_exceeded {
        return Status::ExpectedFail;
    }
    if threshold_exceeded {
        return Status::FailThreshold;
    }
    if let Some(approved) = input.approved.pixel
        && input.pixel_diff_pct > approved + REGRESSION_TOLERANCE
    {
        return Status::Regression;
    }
    if let (Some(current), Some(approved)) = (input.element_match_pct, input.approved.element)
        && current < approved - REGRESSION_TOLERANCE
    {
        return Status::Regression;
    }
    Status::Pass
}

pub(crate) fn format_delta(current: Option<f64>, approved: Option<f64>) -> String {
    match (current, approved) {
        (Some(current), Some(approved)) => {
            let delta = current - approved;
            if delta.abs() < DISPLAY_DELTA_TOLERANCE {
                "\u{2014}".into()
            } else {
                format!("{delta:+.1}%")
            }
        }
        _ => "\u{2014}".into(),
    }
}

pub(crate) struct StatusColumns {
    pub(crate) pixel: String,
    pub(crate) delta: String,
    pub(crate) status: String,
    pub(crate) annotation: String,
}

pub(crate) fn status_columns(
    status: Option<Status>,
    current_pixel_diff: Option<f64>,
    approved_pixel_diff: Option<f64>,
    show_unapproved_pass: bool,
) -> StatusColumns {
    let Some(status) = status else {
        return StatusColumns {
            pixel: "\u{2014}".into(),
            delta: "\u{2014}".into(),
            status: "\u{2014}".into(),
            annotation: String::new(),
        };
    };

    StatusColumns {
        pixel: current_pixel_diff.map_or_else(|| "\u{2014}".into(), |value| format!("{value:.1}%")),
        delta: format_delta(current_pixel_diff, approved_pixel_diff),
        status: status.as_str().to_owned(),
        annotation: status
            .annotation(
                current_pixel_diff,
                approved_pixel_diff,
                show_unapproved_pass,
            )
            .to_owned(),
    }
}
