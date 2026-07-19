//! Small wrapper over `globset` for the path-glob lists shared by the `[header]`
//! and `[[textlint]]` checks. Both take lists of globs (`crates/**/*.rs`,
//! `examples/**`) naming which files a rule applies to or is excused from.

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::error::DevError;

/// Compile a list of glob patterns into a matcher. An empty list yields a set
/// that matches nothing (the natural identity for an `exempt`/`paths` list).
pub fn build_set(patterns: &[String], field: &str) -> Result<GlobSet, DevError> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let glob = Glob::new(pat)
            .map_err(|e| DevError::Config(format!("{field}: invalid glob {pat:?}: {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| DevError::Config(format!("{field}: {e}")))
}

/// Whether `rel` (a project-root-relative path) matches any glob in the set.
pub fn matches(set: &GlobSet, rel: &Path) -> bool {
    set.is_match(rel)
}
