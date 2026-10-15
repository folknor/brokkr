//! Command mode and run verdict model for the piners corpus registry.

use crate::error::DevError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateMode {
    Enforce,
    ReportOnly,
}

/// The operational mode after CLI flags have crossed the command boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    VerifyOnly,
    Reseed,
    Bless,
    Run { gate: GateMode },
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ModeFlags {
    pub verify_only: bool,
    pub reseed: bool,
    pub bless: bool,
    pub no_gate: bool,
}

impl Mode {
    /// Resolve the mutually exclusive command modes once, before dispatch.
    pub(crate) fn resolve(flags: ModeFlags) -> Result<Self, DevError> {
        let mode_count = flags.verify_only as u8 + flags.reseed as u8 + flags.bless as u8;
        if mode_count > 1 {
            return Err(DevError::Config(
                "piners: --verify-only, --reseed, and --bless are mutually exclusive".into(),
            ));
        }
        if flags.verify_only {
            return Ok(Self::VerifyOnly);
        }
        if flags.reseed {
            return Ok(Self::Reseed);
        }
        if flags.bless {
            return Ok(Self::Bless);
        }
        Ok(Self::Run {
            gate: if flags.no_gate {
                GateMode::ReportOnly
            } else {
                GateMode::Enforce
            },
        })
    }

    pub(crate) fn is_verify_only(self) -> bool {
        self == Self::VerifyOnly
    }

    pub(crate) fn is_reseed(self) -> bool {
        self == Self::Reseed
    }

    pub(crate) fn is_bless(self) -> bool {
        self == Self::Bless
    }

    pub(crate) fn gated(self) -> bool {
        matches!(
            self,
            Self::Run {
                gate: GateMode::Enforce
            }
        )
    }

    pub(crate) fn blocks_on(self, deviations: bool) -> bool {
        self.gated() && deviations
    }
}

/// A run's single result and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunVerdict {
    Pass,
    Fail(String),
    Error(String),
    Interrupted(String),
}

impl RunVerdict {
    /// Apply the common precedence: execution/integrity failure first, then
    /// an enforced policy deviation, otherwise pass.
    pub(crate) fn decide(
        primary_failure: Option<String>,
        mode: Mode,
        gate_failure: Option<String>,
    ) -> Self {
        if let Some(reason) = primary_failure {
            Self::Fail(reason)
        } else {
            match gate_failure {
                Some(reason) if mode.blocks_on(true) => Self::Fail(reason),
                _ => Self::Pass,
            }
        }
    }

    pub(crate) fn error(reason: impl Into<String>) -> Self {
        Self::Error(reason.into())
    }

    pub(crate) fn interrupted(reason: impl Into<String>) -> Self {
        Self::Interrupted(reason.into())
    }

    pub(crate) fn result(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail(_) => "fail",
            Self::Error(_) => "error",
            Self::Interrupted(_) => "interrupted",
        }
    }

    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            Self::Pass => None,
            Self::Fail(reason) | Self::Error(reason) | Self::Interrupted(reason) => Some(reason),
        }
    }

    pub(crate) fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }
}
