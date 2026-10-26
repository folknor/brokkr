//! Small wrapper over `globset` for the path-glob lists shared by the `[header]`,
//! `[[textlint]]`, and `[manifest]` checks and `[gremlins].include`. These name
//! which files a rule applies to or is excused from.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::DevError;

/// Extensions understood by the gremlin scanner and the prose-only check
/// shortcut. Keeping the policy here means a prose extension cannot bypass
/// gremlin scanning just because one caller has a narrower list.
#[derive(Clone, Copy)]
struct ScannedExtension {
    name: &'static str,
    prose: bool,
}

const SCANNED_EXTENSIONS: &[ScannedExtension] = &[
    ScannedExtension {
        name: "rs",
        prose: false,
    },
    ScannedExtension {
        name: "toml",
        prose: false,
    },
    ScannedExtension {
        name: "md",
        prose: true,
    },
    ScannedExtension {
        name: "markdown",
        prose: true,
    },
    ScannedExtension {
        name: "js",
        prose: false,
    },
    ScannedExtension {
        name: "sh",
        prose: false,
    },
    ScannedExtension {
        name: "py",
        prose: false,
    },
    ScannedExtension {
        name: "yml",
        prose: false,
    },
    ScannedExtension {
        name: "yaml",
        prose: false,
    },
];

/// Whether an extension (without its leading dot) is scanned by gremlins.
/// Extension spelling is case-insensitive, matching common filesystem and
/// documentation conventions such as `README.MD`.
pub(crate) fn is_scanned_extension(extension: &str) -> bool {
    SCANNED_EXTENSIONS
        .iter()
        .any(|entry| entry.name.eq_ignore_ascii_case(extension))
}

/// Whether an extension is documentation eligible for `scope::dirt`'s prose
/// shortcut. This is a subset of the scanned set by construction.
pub(crate) fn is_prose_extension(extension: &str) -> bool {
    SCANNED_EXTENSIONS
        .iter()
        .any(|entry| entry.prose && entry.name.eq_ignore_ascii_case(extension))
}

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

/// One finding per configured glob that matches no path in the scan's scope.
/// Checking patterns individually prevents a live pattern from hiding a dead
/// sibling in the same list.
///
/// A finding for `brokkr check --audit`, never a refusal and never printed by
/// an ordinary check. A dead glob is usually a typo or a stale entry, but not
/// always: an exclusion can deliberately guard a generated or vendored file
/// that is absent today and may come back, and a user-wide rule (which applies
/// to every tree brokkr checks) legitimately names paths most trees do not
/// have. Failing the check, or warning on every run, would punish a correct
/// config; the audit is where a real typo gets found.
pub fn unmatched(
    patterns: &[String],
    paths: &[PathBuf],
    field: &str,
) -> Result<Vec<String>, DevError> {
    let mut out = Vec::new();
    for pattern in patterns {
        let one = build_set(std::slice::from_ref(pattern), field)?;
        if !paths.iter().any(|path| matches(&one, path)) {
            out.push(format!(
                "{field}: glob {pattern:?} matches no in-scope tracked path"
            ));
        }
    }
    Ok(out)
}

/// Whether `rel` (a project-root-relative path) matches any glob in the set.
pub fn matches(set: &GlobSet, rel: &Path) -> bool {
    set.is_match(rel)
}

#[cfg(test)]
mod extension_tests {
    use super::*;

    #[test]
    fn every_prose_extension_is_scanned_case_insensitively() {
        for entry in SCANNED_EXTENSIONS.iter().filter(|entry| entry.prose) {
            assert!(is_scanned_extension(entry.name));
            assert!(is_scanned_extension(&entry.name.to_ascii_uppercase()));
            assert!(is_prose_extension(entry.name));
        }
        assert!(is_scanned_extension("py"));
        assert!(is_scanned_extension("YML"));
        assert!(is_scanned_extension("yaml"));
    }
}
