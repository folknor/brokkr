//! Project detection from brokkr.toml at or one level above the working
//! directory.

use std::path::{Path, PathBuf};

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
    Ratatoskr,
    Saehrimnir,
    Piners,
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
            Self::Ratatoskr => "ratatoskr",
            Self::Saehrimnir => "saehrimnir",
            Self::Piners => "piners",
            Self::Other(s) => s,
        }
    }

    /// The cargo package name for the project's primary binary.
    /// `None` means single-crate project (no `-p` flag needed).
    /// Ratatoskr is a multi-crate workspace; it returns `None` and relies
    /// on `[test] default_package` in `brokkr.toml` to point `brokkr test`
    /// at the right cargo package.
    pub fn cli_package(self) -> Option<&'static str> {
        match self {
            Self::Pbfhogg => Some("pbfhogg-cli"),
            Self::Nidhogg => Some("nidhogg"),
            Self::Elivagar
            | Self::Brokkr
            | Self::Litehtml
            | Self::Sluggrs
            | Self::Ratatoskr
            | Self::Saehrimnir
            | Self::Piners
            | Self::Other(_) => None,
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

/// A resolved project: the project type, its parsed config, and the two
/// roots that resolution distinguishes.
///
/// - `project_root` is the directory that holds `brokkr.toml`. It anchors
///   everything brokkr *owns*: the `data/`/`scratch/`/`output/` trees and
///   the `.brokkr/` state directory (`results.db`, `sidecar.db`, artefacts,
///   worktrees).
/// - `build_root` is the working directory, where git and cargo run.
///
/// The two coincide in the common case (`brokkr.toml` in cwd) and differ
/// only when the config was found one level *above* cwd - the layout used
/// to drive a checkout that isn't ours, keeping brokkr's config and state in
/// the parent so the foreign repo stays clean.
pub struct Detection {
    pub project: Project,
    pub config: DevConfig,
    pub project_root: PathBuf,
    pub build_root: PathBuf,
}

/// Locate the `brokkr.toml` governing `cwd`: the file in `cwd` itself, or
/// failing that, in `cwd`'s immediate parent (one level up). Returns the
/// directory the file was found in.
///
/// The search deliberately stops after one level. A deeper walk toward the
/// filesystem root risks silently attaching to an unrelated project's config
/// (a stray `brokkr.toml` in a home directory, or a parent that is itself a
/// brokkr project). One level up is the documented layout for driving a
/// foreign checkout and nothing more.
fn find_config_dir(cwd: &Path) -> Option<PathBuf> {
    if cwd.join("brokkr.toml").exists() {
        return Some(cwd.to_path_buf());
    }
    let parent = cwd.parent()?;
    if parent.join("brokkr.toml").exists() {
        return Some(parent.to_path_buf());
    }
    None
}

/// Detect the project from the `brokkr.toml` governing the working directory
/// (in cwd, or one level up).
///
/// This is the single entry point - `brokkr.toml` is read and parsed exactly
/// once via [`config::load`]. Errors when no `brokkr.toml` is found at either
/// location.
pub fn detect() -> Result<Detection, DevError> {
    let cwd = std::env::current_dir()
        .map_err(|e| DevError::Config(format!("cannot determine current directory: {e}")))?;
    let config_dir = find_config_dir(&cwd).ok_or_else(|| {
        DevError::Config(format!(
            "no brokkr.toml found in {} or its parent directory",
            cwd.display()
        ))
    })?;
    let (project, config) = config::load(&config_dir)?;
    Ok(Detection {
        project,
        config,
        project_root: config_dir,
        build_root: cwd,
    })
}

/// Like [`detect`] but returns `Ok(None)` when no `brokkr.toml` is found in
/// cwd or its parent.
///
/// Used by commands that work fine without brokkr-specific config (`check`).
/// A malformed `brokkr.toml` still errors - we only swallow the file-not-found
/// case.
pub fn detect_optional() -> Result<Option<Detection>, DevError> {
    let cwd = std::env::current_dir()
        .map_err(|e| DevError::Config(format!("cannot determine current directory: {e}")))?;
    let Some(config_dir) = find_config_dir(&cwd) else {
        return Ok(None);
    };
    let (project, config) = config::load(&config_dir)?;
    Ok(Some(Detection {
        project,
        config,
        project_root: config_dir,
        build_root: cwd,
    }))
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

#[cfg(test)]
mod tests {
    use super::find_config_dir;
    use std::fs;

    /// A fresh, empty scratch dir under the crate's gitignored `target/`
    /// (project rules forbid `/tmp`).
    fn tmpdir(test_name: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/project")
            .join(test_name);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_config_in_cwd() {
        let root = tmpdir("in_cwd");
        fs::write(root.join("brokkr.toml"), "project = \"x\"\n").unwrap();
        assert_eq!(find_config_dir(&root), Some(root.clone()));
    }

    #[test]
    fn finds_config_one_level_up() {
        let root = tmpdir("one_up");
        fs::write(root.join("brokkr.toml"), "project = \"x\"\n").unwrap();
        let sub = root.join("someproject");
        fs::create_dir_all(&sub).unwrap();
        // Found in the parent, not the (config-less) subdirectory.
        assert_eq!(find_config_dir(&sub), Some(root));
    }

    #[test]
    fn cwd_wins_over_parent() {
        let root = tmpdir("cwd_wins");
        fs::write(root.join("brokkr.toml"), "project = \"parent\"\n").unwrap();
        let sub = root.join("someproject");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("brokkr.toml"), "project = \"child\"\n").unwrap();
        // The nearer config (in cwd) takes precedence over the parent's.
        assert_eq!(find_config_dir(&sub), Some(sub));
    }

    #[test]
    fn stops_after_one_level() {
        let root = tmpdir("two_up");
        fs::write(root.join("brokkr.toml"), "project = \"x\"\n").unwrap();
        // Two levels down: the config is a grandparent, out of reach.
        let deep = root.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_config_dir(&deep), None);
    }

    #[test]
    fn none_when_absent() {
        let root = tmpdir("absent");
        let sub = root.join("someproject");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_config_dir(&sub), None);
    }
}
