//! Project detection from brokkr.toml in the current working directory.

use std::path::PathBuf;

use crate::error::DevError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Project {
    Pbfhogg,
    Elivagar,
    Nidhogg,
}

impl Project {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pbfhogg => "pbfhogg",
            Self::Elivagar => "elivagar",
            Self::Nidhogg => "nidhogg",
        }
    }

    /// The cargo package name for the project's primary binary.
    /// `None` means single-crate project (no `-p` flag needed).
    pub fn cli_package(self) -> Option<&'static str> {
        match self {
            Self::Pbfhogg => Some("pbfhogg-cli"),
            Self::Elivagar => None,
            Self::Nidhogg => Some("nidhogg"),
        }
    }
}

impl std::fmt::Display for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Detect the project from `./brokkr.toml` in the current working directory.
///
/// Returns the project type and the project root directory (cwd).
pub fn detect() -> Result<(Project, PathBuf), DevError> {
    let cwd = std::env::current_dir().map_err(|e| {
        DevError::Config(format!("cannot determine current directory: {e}"))
    })?;

    let toml_path = cwd.join("brokkr.toml");
    let text = std::fs::read_to_string(&toml_path).map_err(|e| {
        DevError::Config(format!(
            "brokkr.toml not found in {}: {e}\nRun brokkr from the project root directory.",
            cwd.display()
        ))
    })?;

    let root: toml::Value = text.parse().map_err(|e: toml::de::Error| {
        DevError::Config(format!("brokkr.toml parse error: {e}"))
    })?;

    let project_str = root
        .get("project")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            DevError::Config("brokkr.toml missing required 'project' field".into())
        })?;

    let project = match project_str {
        "pbfhogg" => Project::Pbfhogg,
        "elivagar" => Project::Elivagar,
        "nidhogg" => Project::Nidhogg,
        other => {
            return Err(DevError::Config(format!(
                "unknown project '{other}' in brokkr.toml (expected: pbfhogg, elivagar, nidhogg)"
            )));
        }
    };

    Ok((project, cwd))
}

/// Require the current project matches the expected project.
/// Returns an error with a helpful message if mismatched.
pub fn require(current: Project, expected: Project, command: &str) -> Result<(), DevError> {
    if current != expected {
        return Err(DevError::Config(format!(
            "'brokkr {command}' is only available in {expected} projects (current: {current})"
        )));
    }
    Ok(())
}
