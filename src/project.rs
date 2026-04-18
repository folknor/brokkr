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
    Litehtml,
    Sluggrs,
    /// Any project not in the hardcoded set. Gets generic command support
    /// (check, run, hotpath, results, env, clean, history).
    /// The `&'static str` is leaked once at startup from the TOML value.
    Other(&'static str),
}

impl Project {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pbfhogg => "pbfhogg",
            Self::Elivagar => "elivagar",
            Self::Nidhogg => "nidhogg",
            Self::Brokkr => "brokkr",
            Self::Litehtml => "litehtml-rs",
            Self::Sluggrs => "sluggrs",
            Self::Other(s) => s,
        }
    }

    /// The cargo package name for the project's primary binary.
    /// `None` means single-crate project (no `-p` flag needed).
    pub fn cli_package(self) -> Option<&'static str> {
        match self {
            Self::Pbfhogg => Some("pbfhogg-cli"),
            Self::Nidhogg => Some("nidhogg"),
            Self::Elivagar | Self::Brokkr | Self::Litehtml | Self::Sluggrs | Self::Other(_) => None,
        }
    }

    /// Whether this is one of the built-in projects with dedicated modules.
    #[allow(dead_code)]
    pub fn is_builtin(self) -> bool {
        !matches!(self, Self::Other(_))
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
/// (cwd). This is the single entry point - `brokkr.toml` is read and parsed
/// exactly once via [`config::load`].
pub fn detect() -> Result<(Project, DevConfig, PathBuf), DevError> {
    let cwd = std::env::current_dir()
        .map_err(|e| DevError::Config(format!("cannot determine current directory: {e}")))?;

    let (project, dev_config) = config::load(&cwd)?;

    Ok((project, dev_config, cwd))
}

/// Like [`detect`] but returns `Ok(None)` when `./brokkr.toml` is absent.
///
/// Used by commands that work fine without brokkr-specific config (`check`).
/// A malformed `brokkr.toml` still errors - we only swallow the file-not-found
/// case.
pub fn detect_optional() -> Result<Option<(Project, DevConfig, PathBuf)>, DevError> {
    let cwd = std::env::current_dir()
        .map_err(|e| DevError::Config(format!("cannot determine current directory: {e}")))?;
    if !cwd.join("brokkr.toml").exists() {
        return Ok(None);
    }
    let (project, dev_config) = config::load(&cwd)?;
    Ok(Some((project, dev_config, cwd)))
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
