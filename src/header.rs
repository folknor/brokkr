//! `[header]` check: a required file header whose year must be current.
//!
//! Ported from nautilus_trader's `check_copyright_year` hook. The one thing a
//! plain regex rule cannot express is the *dynamic* element - "the copyright
//! year must be this year" - so it gets a dedicated feature. A file matching
//! `paths` (minus `exempt`) must contain `pattern` with `{year}` substituted by
//! the current UTC year; a missing header and a stale year both fail the same
//! check (neither contains the expanded string).
//!
//! Enabled by a `[header]` section with `paths` and `pattern`; absent by
//! default.

use std::path::{Path, PathBuf};

use crate::config::HeaderConfig;
use crate::error::DevError;
use crate::{globs, gremlins};

/// A file that is missing the header or carries a stale year.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderViolation {
    pub file: PathBuf,
}

/// `file: missing or stale header (expected `<pattern>`)`.
pub fn format_one(v: &HeaderViolation, expected: &str) -> String {
    format!("{}: missing or stale header (expected `{expected}`)", v.file.display())
}

/// The current year in UTC, via libc `gmtime` (no date-crate dependency).
pub fn current_utc_year() -> i32 {
    // SAFETY: `time` accepts a null pointer and returns the epoch seconds;
    // `gmtime_r` fills a caller-owned `tm`. Neither reads uninitialised memory.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&t, &mut tm);
        tm.tm_year + 1900
    }
}

/// The header text with `{year}` expanded to `year`.
pub fn expand(pattern: &str, year: i32) -> String {
    pattern.replace("{year}", &year.to_string())
}

/// Scan tracked files matching `cfg.paths` (minus `cfg.exempt`) for the
/// expanded header. `year` is injected so the check is deterministic and
/// testable.
pub fn scan(
    project_root: &Path,
    cfg: &HeaderConfig,
    year: i32,
) -> Result<Vec<HeaderViolation>, DevError> {
    let paths = globs::build_set(&cfg.paths, "[header].paths")?;
    let exempt = globs::build_set(&cfg.exempt, "[header].exempt")?;
    let expected = expand(&cfg.pattern, year);

    let files = gremlins::tracked_files(project_root)?;
    let mut out = Vec::new();
    for rel in &files {
        if !globs::matches(&paths, rel) || globs::matches(&exempt, rel) {
            continue;
        }
        let abs = project_root.join(rel);
        let Ok(content) = std::fs::read_to_string(&abs) else {
            continue;
        };
        if !content.contains(&expected) {
            out.push(HeaderViolation { file: rel.clone() });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn expands_year_placeholder() {
        assert_eq!(
            expand("Copyright (C) 2015-{year}", 2026),
            "Copyright (C) 2015-2026"
        );
        // No placeholder is a valid (static) header requirement.
        assert_eq!(expand("SPDX-License-Identifier: MIT", 2026), "SPDX-License-Identifier: MIT");
    }

    #[test]
    fn current_year_is_sane() {
        // The check ships in 2026; the year must be at least that and not wild.
        let y = current_utc_year();
        assert!((2024..2100).contains(&y), "got {y}");
    }
}
