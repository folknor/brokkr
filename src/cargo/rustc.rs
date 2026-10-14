//! Version information from the rustc selected for a project directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::DevError;
use crate::output;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RustcInfo {
    pub(crate) host: String,
    pub(crate) release: String,
    pub(crate) commit_hash: Option<String>,
}

type CachedLoad = Result<Arc<RustcInfo>, Arc<DevError>>;

static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedLoad>>> = OnceLock::new();

/// Read the toolchain rustup selects from `root`, once per root in this
/// invocation. Running in the project directory is essential: a pinned
/// toolchain is selected from the directory tree, not from brokkr's caller.
pub(crate) fn load(root: &Path) -> Result<Arc<RustcInfo>, DevError> {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cached) = cache.get(root) {
        return match cached {
            Ok(info) => Ok(Arc::clone(info)),
            Err(error) => Err(clone_error(error)),
        };
    }

    let result = load_uncached(root).map(Arc::new);
    let cached_result = match &result {
        Ok(info) => Ok(Arc::clone(info)),
        Err(error) => Err(Arc::new(clone_error(error))),
    };
    cache.insert(root.to_owned(), cached_result);
    result
}

fn clone_error(error: &DevError) -> DevError {
    match error {
        DevError::Build(message) => DevError::Build(message.clone()),
        DevError::Io(error) => DevError::Io(std::io::Error::new(error.kind(), error.to_string())),
        DevError::Spawn { program, error } => DevError::Spawn {
            program: program.clone(),
            error: std::io::Error::new(error.kind(), error.to_string()),
        },
        DevError::Interrupted => DevError::Interrupted,
        other => DevError::Build(other.to_string()),
    }
}

fn load_uncached(root: &Path) -> Result<RustcInfo, DevError> {
    let captured = output::run_captured("rustc", &["-vV"], root)?;
    if !captured.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("rustc -vV failed: {stderr}")));
    }

    let text = String::from_utf8_lossy(&captured.stdout);
    let mut host = None;
    let mut release = None;
    let mut commit_hash = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("host: ") {
            host = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("release: ") {
            release = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("commit-hash: ") {
            commit_hash = Some(value.trim().to_owned());
        }
    }

    Ok(RustcInfo {
        host: host
            .ok_or_else(|| DevError::Build("rustc -vV did not report a host triple".to_owned()))?,
        release: release
            .ok_or_else(|| DevError::Build("rustc -vV did not report a release".to_owned()))?,
        commit_hash,
    })
}
