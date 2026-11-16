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
