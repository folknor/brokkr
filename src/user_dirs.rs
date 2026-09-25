//! Per-user directories: the XDG base directories brokkr keeps its own files
//! in, and the cargo home and config files it inspects or edits.
//!
//! Each rule used to be written out at every call site, and the copies drifted:
//! an empty `XDG_DATA_HOME` was taken at its word by each of them, so
//! `history.db` landed in `./brokkr/` under whatever directory brokkr happened
//! to run in, and the sidecar backups became an untracked directory that
//! dirtied the next run's tree. The cargo config lookup disagreed with itself
//! about which spelling wins. One module, one rule each.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// An environment variable's value, with an empty value treated as unset.
fn non_empty_var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|v| !v.is_empty())
}

/// `$HOME`, if set to something non-empty.
pub(crate) fn home() -> Option<PathBuf> {
    non_empty_var("HOME").map(PathBuf::from)
}

/// An XDG base directory: `$<var>` when it is set to an absolute path,
/// otherwise `$HOME/<fallback>`.
///
/// The XDG Base Directory spec says an empty value is to be treated as unset
/// and that a relative path is invalid and should be ignored. Both matter
/// here: either one taken literally resolves against the cwd.
fn xdg_dir(var: &str, fallback: &[&str]) -> Option<PathBuf> {
    resolve_xdg(non_empty_var(var), home(), fallback)
}

/// [`xdg_dir`]'s rule over explicit inputs, so it is testable without
/// mutating the process environment.
fn resolve_xdg(value: Option<OsString>, home: Option<PathBuf>, fallback: &[&str]) -> Option<PathBuf> {
    if let Some(v) = value.filter(|v| !v.is_empty()) {
        let path = PathBuf::from(v);
        if path.is_absolute() {
            return Some(path);
        }
    }
    let mut dir = home?;
    for part in fallback {
        dir.push(part);
    }
    Some(dir)
}

/// `$XDG_DATA_HOME`, falling back to `$HOME/.local/share`. `None` when
/// neither yields a usable directory (a sandbox, typically).
pub(crate) fn xdg_data_home() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", &[".local", "share"])
}

/// `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`.
pub(crate) fn xdg_config_home() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", &[".config"])
}

/// The cargo home: `$CARGO_HOME`, falling back to `$HOME/.cargo`.
///
/// Mirrors cargo's own resolution (the `home` crate): an empty `CARGO_HOME`
/// is ignored, and a relative one is taken relative to the cwd, which is what
/// cargo itself would read.
pub(crate) fn cargo_home() -> Option<PathBuf> {
    if let Some(v) = non_empty_var("CARGO_HOME") {
        let path = PathBuf::from(v);
        if path.is_absolute() {
            return Some(path);
        }
        return std::env::current_dir().ok().map(|cwd| cwd.join(path));
    }
    home().map(|h| h.join(".cargo"))
}

/// The cargo config file cargo reads from `dir` (a `.cargo` directory or the
/// cargo home), if any.
///
/// Cargo accepts two spellings and reads exactly one per directory. When both
/// exist it reads the extensionless legacy `config` and warns that it is
/// ignoring `config.toml` - so that is the one whose contents matter, whether
/// the question is "what flags apply" or "which file do I edit".
pub(crate) fn cargo_config_file(dir: &Path) -> Option<PathBuf> {
    let legacy = dir.join("config");
    if legacy.is_file() {
        return Some(legacy);
    }
    let modern = dir.join("config.toml");
    if modern.is_file() {
        return Some(modern);
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{cargo_config_file, resolve_xdg};

    #[test]
    fn xdg_empty_or_relative_value_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/u"));
        let want = Some(PathBuf::from("/home/u/.local/share"));
        let fallback = [".local", "share"];
        assert_eq!(resolve_xdg(None, home.clone(), &fallback), want);
        assert_eq!(resolve_xdg(Some(OsString::new()), home.clone(), &fallback), want);
        assert_eq!(resolve_xdg(Some("rel/dir".into()), home.clone(), &fallback), want);
        assert_eq!(
            resolve_xdg(Some("/xdg/data".into()), home, &fallback),
            Some(PathBuf::from("/xdg/data"))
        );
        assert_eq!(resolve_xdg(Some(OsString::new()), None, &fallback), None);
    }

    #[test]
    fn cargo_config_file_prefers_legacy_when_both_exist() {
        let dir = crate::test_scratch::scratch("user_dirs", "both_exist");
        std::fs::write(dir.join("config"), "").unwrap();
        std::fs::write(dir.join("config.toml"), "").unwrap();
        assert_eq!(cargo_config_file(&dir), Some(dir.join("config")));
    }

    #[test]
    fn cargo_config_file_finds_either_spelling_alone() {
        let modern = crate::test_scratch::scratch("user_dirs", "modern_only");
        std::fs::write(modern.join("config.toml"), "").unwrap();
        assert_eq!(cargo_config_file(&modern), Some(modern.join("config.toml")));

        let legacy = crate::test_scratch::scratch("user_dirs", "legacy_only");
        std::fs::write(legacy.join("config"), "").unwrap();
        assert_eq!(cargo_config_file(&legacy), Some(legacy.join("config")));

        let neither = crate::test_scratch::scratch("user_dirs", "neither");
        assert_eq!(cargo_config_file(&neither), None);
    }
}
