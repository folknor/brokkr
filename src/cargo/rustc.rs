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

/// What a compiler selection is keyed by: the directory it runs in, the
/// command (a wrapper chain, the compiler last), and the env it runs under.
type SelectedKey = (PathBuf, Vec<String>, Vec<(String, String)>);

static SELECTED: OnceLock<Mutex<HashMap<SelectedKey, CachedLoad>>> = OnceLock::new();

/// As [`load`], for a compiler chosen by cargo's own precedence rather than
/// the `rustc` on `PATH` (a sweep's `RUSTC`, `build.rustc`): `program` run in
/// `root` under `env`, once per distinct triple of the three.
pub(crate) fn load_selected(
    root: &Path,
    program: &str,
    env: &[(&str, &str)],
) -> Result<Arc<RustcInfo>, DevError> {
    load_chain(root, &[program.to_owned()], env)
}

/// As [`load_selected`], through a wrapper chain as cargo runs it: `chain`'s
/// first word is spawned with the rest, then `-vV`, as arguments.
pub(crate) fn load_chain(
    root: &Path,
    chain: &[String],
    env: &[(&str, &str)],
) -> Result<Arc<RustcInfo>, DevError> {
    let Some((program, prefix)) = chain.split_first() else {
        return Err(DevError::Internal("empty compiler chain".into()));
    };
    let key: SelectedKey = (
        root.to_owned(),
        chain.to_vec(),
        env.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    );
    let cache = SELECTED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cached) = cache.get(&key) {
        return match cached {
            Ok(info) => Ok(Arc::clone(info)),
            Err(error) => Err(clone_error(error)),
        };
    }
    let mut args: Vec<&str> = prefix.iter().map(String::as_str).collect();
    args.push("-vV");
    let result = output::run_captured_with_env(program, &args, root, env)
        .and_then(|captured| parse_captured(program, &captured))
        .map(Arc::new);
    let cached_result = match &result {
        Ok(info) => Ok(Arc::clone(info)),
        Err(error) => Err(Arc::new(clone_error(error))),
    };
    cache.insert(key, cached_result);
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
    parse_captured("rustc", &captured)
}

fn parse_captured(program: &str, captured: &output::CapturedOutput) -> Result<RustcInfo, DevError> {
    if !captured.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("{program} -vV failed: {stderr}")));
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
        host: host.ok_or_else(|| {
            DevError::Build(format!("{program} -vV did not report a host triple"))
        })?,
        release: release
            .ok_or_else(|| DevError::Build(format!("{program} -vV did not report a release")))?,
        commit_hash,
    })
}
