//! Small wrapper over `globset` for the path-glob lists shared by the `[header]`,
//! `[[textlint]]`, and `[manifest]` checks. These name which files a rule
//! applies to or is excused from.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::DevError;

/// Compile a list of glob patterns into a matcher. An empty list yields a set
/// that matches nothing (the natural identity for an `exempt`/`paths` list).
pub fn build_set(patterns: &[String], field: &str) -> Result<GlobSet, DevError> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        // Match a single `*` within one path component. `**` retains globset's
        // recursive behavior for patterns that intentionally cross `/`.
        let glob = GlobBuilder::new(pat)
            .literal_separator(true)
            .build()
            .map_err(|e| DevError::Config(format!("{field}: invalid glob {pat:?}: {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| DevError::Config(format!("{field}: {e}")))
}

/// Warn about each configured glob that matches no path in the scan's scope.
/// Checking patterns individually prevents a live pattern from hiding a dead
/// sibling in the same list.
///
/// A warning, not a refusal. A dead glob is usually a typo or a stale entry,
/// but not always: an exclusion can deliberately guard a generated or vendored
/// file that is absent today and may come back, and a user-wide rule (which
/// applies to every tree brokkr checks) legitimately names paths most trees do
/// not have. Failing the whole check for either would make a correct config
/// unusable; saying so on every run is enough to get a real typo fixed.
pub fn warn_unmatched(patterns: &[String], paths: &[PathBuf], field: &str) -> Result<(), DevError> {
    for pattern in patterns {
        let one = build_set(std::slice::from_ref(pattern), field)?;
        if !paths.iter().any(|path| matches(&one, path)) {
            crate::output::warn(&format!(
                "{field}: glob {pattern:?} matches no in-scope tracked path"
            ));
        }
    }
    Ok(())
}

/// Whether `rel` (a project-root-relative path) matches any glob in the set.
pub fn matches(set: &GlobSet, rel: &Path) -> bool {
    set.is_match(rel)
}
