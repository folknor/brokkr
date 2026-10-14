use serde::{Serialize, Serializer};

use super::DepsEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    DuplicateVersion,
    GitDependency,
    PathDependency,
    PublishCycle,
    WorkspaceDep,
    NativeCode,
    Outdated,
    Stale,
}

impl Phase {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateVersion => "duplicate_version",
            Self::GitDependency => "git_dependency",
            Self::PathDependency => "path_dependency",
            Self::PublishCycle => "publish_cycle",
            Self::WorkspaceDep => "workspace_dep",
            Self::NativeCode => "native_code",
            Self::Outdated => "outdated",
            Self::Stale => "stale",
        }
    }

    /// Only these offline smell phases affect the command's exit status.
    pub(super) const fn is_gating(self) -> bool {
        matches!(
            self,
            Self::DuplicateVersion
                | Self::GitDependency
                | Self::PathDependency
                | Self::PublishCycle
                | Self::WorkspaceDep
        )
    }
}

impl Serialize for Phase {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

pub(super) struct PhaseOutcome {
    pub(super) phase: Phase,
    pub(super) events: Vec<DepsEvent>,
    state: PhaseState,
}

enum PhaseState {
    Ran,
    Skipped(String),
}

impl PhaseOutcome {
    pub(super) fn ran(phase: Phase, events: Vec<DepsEvent>) -> Self {
        Self {
            phase,
            events,
            state: PhaseState::Ran,
        }
    }

    pub(super) fn skipped(phase: Phase, reason: String, events: Vec<DepsEvent>) -> Self {
        Self {
            phase,
            events,
            state: PhaseState::Skipped(reason),
        }
    }

    pub(super) fn findings(&self) -> usize {
        match &self.state {
            PhaseState::Ran if self.phase.is_gating() => self.events.len(),
            _ => 0,
        }
    }

    pub(super) fn summary(&self) -> PhaseSummary {
        match &self.state {
            PhaseState::Ran => PhaseSummary {
                phase: self.phase,
                status: PhaseStatus::Ran,
                reason: None,
            },
            PhaseState::Skipped(reason) => PhaseSummary {
                phase: self.phase,
                status: PhaseStatus::Skipped,
                reason: Some(reason.clone()),
            },
        }
    }

    pub(super) fn did_run(&self) -> bool {
        matches!(&self.state, PhaseState::Ran)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PhaseStatus {
    Ran,
    Skipped,
}

#[derive(Serialize)]
pub(super) struct PhaseSummary {
    pub(super) phase: Phase,
    pub(super) status: PhaseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<String>,
}
