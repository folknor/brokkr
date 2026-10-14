/// Shared check-phase facts layered onto the phase type used when cargo env
/// evaluation records its first failure.
pub(super) use crate::cargo_env::Phase;

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
        let minutes = match self {
            Self::Clippy | Self::Rustdoc => 5,
            Self::Prepare | Self::Test => 15,
            Self::Coverage | Self::InstallFeature | Self::ScriptCheck => 5,
            _ => 2,
        };
        std::time::Duration::from_secs(minutes * 60)
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
