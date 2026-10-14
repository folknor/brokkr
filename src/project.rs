//! Project detection from brokkr.toml at or one level above the working
//! directory.

use std::path::{Path, PathBuf};

use crate::config::{self, DevConfig, project_config::ProjectConfig};
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
    Dellingr,
    Mogwai,
    /// Any project not in the hardcoded set. Gets generic command support
    /// (check, run, hotpath, results, env, clean, history).
    /// The `&'static str` is leaked once at startup from the TOML value.
    Other(&'static str),
}

impl Project {
    /// Built-in project names and their identities. Keep this list as the
    /// single owner of the name-to-variant mapping used by config parsing,
    /// display, help and project-wide checks.
    pub const ALL: &[(&'static str, Self)] = &[
        ("pbfhogg", Self::Pbfhogg),
        ("elivagar", Self::Elivagar),
        ("nidhogg", Self::Nidhogg),
        ("brokkr", Self::Brokkr),
        ("litehtml-rs", Self::Litehtml),
        ("sluggrs", Self::Sluggrs),
        ("ratatoskr", Self::Ratatoskr),
        ("saehrimnir", Self::Saehrimnir),
        ("piners", Self::Piners),
        ("dellingr", Self::Dellingr),
        ("mogwai", Self::Mogwai),
    ];

    pub fn from_builtin_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find_map(|(known, project)| (*known == name).then_some(*project))
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Other(name) => name,
            _ => Self::ALL
                .iter()
                .find_map(|(name, project)| (*project == self).then_some(*name))
                .unwrap_or("unknown"),
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
            | Self::Dellingr
            // The CLI package is named by `[mogwai] package`, not hardcoded:
            // mogwai is a multi-crate workspace and the contract already has a
            // place to say which crate carries the binary.
            | Self::Mogwai
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

/// The configuration and durable state directory. Only detection constructs it.
#[derive(Debug, Clone)]
pub struct ProjectRoot(PathBuf);
impl ProjectRoot {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
    /// The project's `.brokkr` state, named by its one owner
    /// (`clean::layout::ProjectState`).
    pub(crate) fn state(&self) -> crate::clean::layout::ProjectState {
        crate::clean::layout::ProjectState::new(&self.0)
    }
    pub fn bench_home(&self) -> PathBuf {
        self.state().bench_dir()
    }
}

/// The checkout whose git identity and cargo products a command uses.
/// Its private representation prevents paths from being mistaken for roots.
pub struct BuildRoot {
    checkout: Checkout,
}
enum Checkout {
    Live(PathBuf),
    Worktree(crate::worktree::Worktree),
}
impl BuildRoot {
    fn live(path: PathBuf) -> Self {
        Self {
            checkout: Checkout::Live(path),
        }
    }
    pub fn as_path(&self) -> &Path {
        match &self.checkout {
            Checkout::Live(path) => path,
            Checkout::Worktree(worktree) => &worktree.path,
        }
    }
    pub(crate) fn record_name(&self) -> &str {
        match &self.checkout {
            Checkout::Worktree(wt) => &wt.record_name,
            Checkout::Live(_) => "",
        }
    }
    pub(crate) fn commit(&self) -> &str {
        match &self.checkout {
            Checkout::Worktree(wt) => wt.commit.as_str(),
            Checkout::Live(_) => "",
        }
    }
    pub(crate) fn subject(&self) -> &str {
        match &self.checkout {
            Checkout::Worktree(wt) => &wt.subject,
            Checkout::Live(_) => "",
        }
    }
    pub(crate) fn at_commit(
        &self,
        commit: &str,
        on_cut: impl FnOnce(&Path),
    ) -> Result<Self, DevError> {
        crate::worktree::Worktree::create(self.as_path(), commit, on_cut).map(|worktree| Self {
            checkout: Checkout::Worktree(worktree),
        })
    }
}

/// Mint both roots explicitly when a command permits an unconfigured checkout.
pub(crate) fn detect_roots() -> Result<(ProjectRoot, BuildRoot), DevError> {
    let cwd = std::env::current_dir()?;
    let state = find_config_dir(&cwd).unwrap_or_else(|| cwd.clone());
    Ok((ProjectRoot(state), BuildRoot::live(cwd)))
}

/// A resolved project: its identity, shared and project-specific config, and
/// the two roots that resolution distinguishes.
///
/// - `project_root` is the directory that holds `brokkr.toml`. It anchors
///   everything brokkr *owns*: the `data/`/`scratch/`/`output/` trees and
///   the `.brokkr/` state directory (`results.db`, `sidecar.db`, artefacts,
///   worktree records). Persistent checkouts live in the user state directory.
/// - `build_root` is the working directory, where git and cargo run.
///
/// The two coincide in the common case (`brokkr.toml` in cwd) and differ
/// only when the config was found one level *above* cwd - the layout used
/// to drive a checkout that isn't ours, keeping brokkr's config and state in
/// the parent so the foreign repo stays clean.
pub struct Detection {
    pub project: Project,
    /// Shared host, check, and command settings.
    pub config: DevConfig,
    /// The section parsed for `project`; other projects' sections cannot load.
    pub project_config: ProjectConfig,
    pub project_root: ProjectRoot,
    pub build_root: BuildRoot,
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
pub(crate) fn find_config_dir(cwd: &Path) -> Option<PathBuf> {
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
    let (project, config, project_config) = config::load(&config_dir)?;
    crate::cargo_env::install(&config.cargo_env);
    Ok(Detection {
        project,
        config,
        project_config,
        project_root: ProjectRoot(config_dir),
        build_root: BuildRoot::live(cwd),
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
    let (project, config, project_config) = config::load(&config_dir)?;
    crate::cargo_env::install(&config.cargo_env);
    Ok(Some(Detection {
        project,
        config,
        project_config,
        project_root: ProjectRoot(config_dir),
        build_root: BuildRoot::live(cwd),
    }))
}

/// Require the current project matches the expected project.
/// Returns an error with a helpful message if mismatched.
///
/// `command` is rendered as `'brokkr {command}'`, so it must be what the user
/// actually types: its first word a real subcommand name (one the command
/// catalogue knows, `cli::CommandId::from_name`), optionally followed by a sub-subcommand (`"verify batch"`).
/// A literal label that names no command is caught by
/// `tests::require_labels_are_real_commands`.
pub fn require(current: Project, expected: Project, command: &str) -> Result<(), DevError> {
    if let Some(id) =
        crate::cli::CommandId::from_name(command.split_whitespace().next().unwrap_or(""))
    {
        id.spec().require(current)?;
    }
    if current != expected {
        return Err(DevError::WrongProject {
            command: command.to_owned(),
            expected: expected.to_string(),
            current: current.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::find_config_dir;
    use std::fs;

    /// A fresh, empty scratch dir for one test. `test_name` must be unique
    /// within this module - the allocator enforces that.
    fn tmpdir(test_name: &str) -> std::path::PathBuf {
        crate::test_scratch::scratch("project", test_name)
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

    /// Every literal `project::require` label must start with a real
    /// subcommand name. Labels like `"litehtml extract"` rendered as
    /// `'brokkr litehtml extract' is only available...` - a command that does
    /// not exist, in the one message whose job is to name the right one.
    /// Scans the source rather than a registry because the labels are
    /// arguments at scattered call sites; non-literal labels (`command.id()`)
    /// are skipped.
    #[test]
    fn require_labels_are_real_commands() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        let mut files = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files.push(path);
                }
            }
        }

        // Built with concat! so this file's own source never matches it.
        let needle = concat!("project::", "require(");
        let mut bad = Vec::new();
        let mut seen = 0usize;
        for file in &files {
            let text = fs::read_to_string(file).unwrap();
            for (at, _) in text.match_indices(needle) {
                let call = &text[at + needle.len()..];
                let close = call.find(')').unwrap_or(call.len());
                let Some(open) = call[..close].find('"') else {
                    continue; // non-literal label
                };
                let rest = &call[open + 1..];
                let Some(end) = rest.find('"') else { continue };
                let label = &rest[..end];
                seen += 1;
                let head = label.split_whitespace().next().unwrap_or("");
                if crate::cli::CommandId::from_name(head).is_none() {
                    bad.push(format!("{}: {label:?}", file.display()));
                }
            }
        }
        assert!(
            seen > 0,
            "found no literal require labels - the scan is broken"
        );
        assert!(
            bad.is_empty(),
            "require labels naming no subcommand: {bad:#?}"
        );
    }

    #[test]
    fn none_when_absent() {
        let root = tmpdir("absent");
        let sub = root.join("someproject");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_config_dir(&sub), None);
    }
}
