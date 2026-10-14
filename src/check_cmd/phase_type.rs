/// A check phase. Cargo environment failures may be tagged with one, but the
/// phase identity and its policy belong to the check command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Gremlins,
    Header,
    Textlint,
    Manifest,
    ScriptCheck,
    DependencyRules,
    PublishCycle,
    Clippy,
    Rustdoc,
    Prepare,
    Test,
    Coverage,
    InstallFeature,
}

impl Phase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gremlins => "gremlins",
            Self::Header => "header",
            Self::Textlint => "textlint",
            Self::Manifest => "manifest",
            Self::ScriptCheck => "script_check",
            Self::DependencyRules => "dependency_rules",
            Self::PublishCycle => "publish_cycle",
            Self::Clippy => "clippy",
            Self::Rustdoc => "rustdoc",
            Self::Prepare => "prepare",
            Self::Test => "test",
            Self::Coverage => "coverage",
            Self::InstallFeature => "install_feature",
        }
    }
}

/// A check phase's outcome before the command boundary decides how to render
/// it. Findings have already printed their report; operational errors have
/// not, while a stop has its own exit path.
#[derive(Debug)]
enum PhaseFailure {
    Findings(Phase),
    Printed(crate::error::DevError),
    Error(crate::error::DevError),
    Stopped(crate::error::DevError),
}

impl PhaseFailure {
    fn for_phase(phase: Phase, error: crate::error::DevError) -> Self {
        match error {
            crate::error::DevError::Reported(_) => Self::Findings(phase),
            other => other.into(),
        }
    }

    fn into_dev_error(self) -> crate::error::DevError {
        match self {
            Self::Findings(phase) => {
                crate::error::DevError::Reported(format!("{} failed", phase.as_str()))
            }
            Self::Printed(error) => error,
            Self::Error(error) | Self::Stopped(error) => error,
        }
    }
}

impl From<crate::error::DevError> for PhaseFailure {
    fn from(error: crate::error::DevError) -> Self {
        match error {
            error @ crate::error::DevError::Reported(_) => Self::Printed(error),
            crate::error::DevError::Interrupted | crate::error::DevError::ExitCode(_) => {
                Self::Stopped(error)
            }
            other => Self::Error(other),
        }
    }
}

impl Phase {
    pub(super) const ALL: [Self; 13] = [
        Self::Gremlins,
        Self::Header,
        Self::Textlint,
        Self::Manifest,
        Self::ScriptCheck,
        Self::DependencyRules,
        Self::PublishCycle,
        Self::Clippy,
        Self::Rustdoc,
        Self::Prepare,
        Self::Test,
        Self::Coverage,
        Self::InstallFeature,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::ScriptCheck => "script-check",
            Self::DependencyRules => "dependency rules",
            Self::PublishCycle => "publish cycle",
            _ => self.as_str(),
        }
    }

    pub(super) const fn skippable(self) -> bool {
        !matches!(self, Self::Prepare | Self::Coverage)
    }

    pub(super) const fn reads_prose(self) -> bool {
        matches!(self, Self::Gremlins | Self::Textlint | Self::ScriptCheck)
    }

    pub(super) const fn ceiling(self) -> std::time::Duration {
        crate::test_runner::ceilings::phase_ceiling(self)
    }

    pub(super) const fn cargo_verb(self) -> Option<&'static str> {
        match self {
            Self::Clippy => Some("clippy"),
            Self::Rustdoc => Some("doc"),
            _ => None,
        }
    }

    pub(super) fn judges_lint(self, lint: &crate::config::check::LintName) -> bool {
        match self {
            Self::Rustdoc => lint.tool() == crate::config::check::LintTool::Rustdoc,
            Self::Clippy => lint.tool() != crate::config::check::LintTool::Rustdoc,
            _ => false,
        }
    }
}

impl serde::Serialize for Phase {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl std::str::FromStr for Phase {
    type Err = &'static str;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|phase| phase.as_str() == name)
            .ok_or("unknown check phase")
    }
}
