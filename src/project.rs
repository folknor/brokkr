//! Project detection from brokkr.toml in the current working directory.

use std::path::PathBuf;

use crate::config::{self, DevConfig};
use crate::error::DevError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Project {
    Pbfhogg,
    Elivagar,
    Nidhogg,
    Brokkr,
}

impl Project {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pbfhogg => "pbfhogg",
            Self::Elivagar => "elivagar",
            Self::Nidhogg => "nidhogg",
            Self::Brokkr => "brokkr",
        }
    }

    /// The cargo package name for the project's primary binary.
    /// `None` means single-crate project (no `-p` flag needed).
    pub fn cli_package(self) -> Option<&'static str> {
        match self {
            Self::Pbfhogg => Some("pbfhogg-cli"),
            Self::Elivagar => None,
            Self::Nidhogg => Some("nidhogg"),
            Self::Brokkr => None,
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
/// Returns the project type, the parsed config, and the project root directory
/// (cwd). This is the single entry point — `brokkr.toml` is read and parsed
/// exactly once via [`config::load`].
pub fn detect() -> Result<(Project, DevConfig, PathBuf), DevError> {
    let cwd = std::env::current_dir().map_err(|e| {
        DevError::Config(format!("cannot determine current directory: {e}"))
    })?;

    let (project, dev_config) = config::load(&cwd)?;

    Ok((project, dev_config, cwd))
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
