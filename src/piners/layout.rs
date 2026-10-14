//! Filesystem layout constraints shared by the piners corpus commands.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::config::PinersConfig;
use crate::error::DevError;

/// Keep the independently shaped registries from occupying one directory.
/// Each loader treats every sibling TOML file as a keyword file, so overlap
/// makes `pins.toml` and `lints.toml` parse as each other's registry data.
pub(crate) fn ensure_distinct_registry_dirs(
    project_root: &Path,
    config: &PinersConfig,
) -> Result<(), DevError> {
    let Some(lint) = &config.lint else {
        return Ok(());
    };
    let corpus = resolved_path(project_root, config.registry_dir());
    let lint = resolved_path(project_root, lint.registry_dir());
    if corpus == lint {
        return Err(DevError::Config(format!(
            "[piners] registry_dir and [piners.lint] registry_dir resolve to the same directory: {}",
            corpus.display()
        )));
    }
    Ok(())
}

fn resolved_path(root: &Path, configured: &Path) -> PathBuf {
    let joined = root.join(configured);
    let absolute = if joined.is_absolute() {
        joined
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(joined)
    };

    // Canonicalize the nearest existing ancestor to account for symlinks
    // even while reseed is about to create the final registry directory.
    let mut ancestor = absolute.clone();
    let mut suffix = Vec::<OsString>::new();
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(&ancestor) {
            for component in suffix.iter().rev() {
                resolved.push(component);
            }
            return lexical_normalize(&resolved);
        }
        let Some(name) = ancestor.file_name() else {
            return lexical_normalize(&absolute);
        };
        suffix.push(name.to_os_string());
        if !ancestor.pop() {
            return lexical_normalize(&absolute);
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}
