//! Consistent conversions for paths passed to tools that require UTF-8 argv.

use std::path::Path;

use crate::error::DevError;

pub(crate) fn path_str(path: &Path) -> Result<&str, DevError> {
    path.to_str()
        .ok_or_else(|| DevError::Config(format!("path is not valid UTF-8: {}", path.display())))
}

pub(crate) fn path_string(path: &Path) -> Result<String, DevError> {
    path_str(path).map(str::to_owned)
}

pub(crate) fn file_name(path: &Path) -> String {
    file_name_str(path).unwrap_or_default().to_owned()
}

pub(crate) fn file_name_str(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
}

pub(crate) fn path_strings(path: &Path) -> Result<(String, &str), DevError> {
    Ok((file_name(path), path_str(path)?))
}
