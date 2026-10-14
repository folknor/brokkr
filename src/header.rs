//! `[header]` check: selected files must contain configured header text with
//! the current year.
//!
//! Ported from nautilus_trader's `check_copyright_year` hook. The one thing a
//! plain regex rule cannot express is the *dynamic* element - "the copyright
//! year must be this year" - so it gets a dedicated feature. A file matching
//! `paths` (minus `exempt`) must contain `pattern` with `{year}` substituted by
//! the current UTC year. The byte substring may occur anywhere in the file;
//! this phase does not check its position. Missing text or a stale year fails
//! the same check (neither contains the expanded string).
//!
//! Enabled by a `[header]` section with `paths` and `pattern`; absent by
//! default.

use std::path::PathBuf;

use crate::config::HeaderConfig;
use crate::error::DevError;
use crate::globs;

/// A file that does not contain the expected header text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderViolation {
    pub file: PathBuf,
}

/// `` file: missing or stale header (expected `<pattern>`) ``.
pub fn format_one(v: &HeaderViolation, expected: &str) -> String {
    format!(
        "{}: missing or stale header (expected `{expected}`)",
        v.file.display()
    )
}

/// The current year in UTC, via libc `gmtime` (no date-crate dependency).
///
/// Returns an error rather than a bogus year if `gmtime_r` fails: on failure it
/// leaves `tm` zeroed, which would yield 1900 and mark every file's header
/// stale. `gmtime_r` reports failure by returning a null pointer.
pub fn current_utc_year() -> Result<i32, DevError> {
    // SAFETY: `time` accepts a null pointer and returns the epoch seconds;
    // `gmtime_r` fills a caller-owned `tm`. Neither reads uninitialised memory.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        utc_year_at(t)
    }
}

/// Convert injected epoch seconds to a UTC year. Kept separate from the clock
/// read so the calendar boundary can be tested without depending on today's
/// date.
fn utc_year_at(seconds: libc::time_t) -> Result<i32, DevError> {
    // SAFETY: `gmtime_r` reads the initialized scalar and fills this caller-
    // owned `tm`; a null return is handled instead of reading a failed result.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::gmtime_r(&seconds, &mut tm).is_null() {
            return Err(DevError::Config(
                "gmtime_r failed to convert epoch seconds to UTC".into(),
            ));
        }
        Ok(tm.tm_year + 1900)
    }
}

/// The header text with `{year}` expanded to `year`.
pub fn expand(pattern: &str, year: i32) -> String {
    pattern.replace("{year}", &year.to_string())
}

/// Scan snapshot files matching `cfg.paths` (minus `cfg.exempt`) for the
/// expanded header text anywhere in each file. `year` is injected so the check
/// is deterministic and testable.
pub fn scan(
    tree: &crate::scope::TreeSnapshot,
    cfg: &HeaderConfig,
    year: i32,
) -> Result<Vec<HeaderViolation>, DevError> {
    if cfg.paths.is_empty() {
        return Err(DevError::Config(
            "[header].paths is empty - list the files that require the header".into(),
        ));
    }
    let paths = globs::build_set(&cfg.paths, "[header].paths")?;
    let exempt = globs::build_set(&cfg.exempt, "[header].exempt")?;
    let expected = expand(&cfg.pattern, year);

    let files = tree.files();
    // Dead scope is warned about, not refused - see `globs::warn_unmatched`.
    globs::warn_unmatched(&cfg.paths, files, "[header].paths")?;
    let selected: Vec<PathBuf> = files
        .iter()
        .filter(|rel| globs::matches(&paths, rel))
        .cloned()
        .collect();
    globs::warn_unmatched(&cfg.exempt, &selected, "[header].exempt")?;
    if !selected.is_empty() && !selected.iter().any(|rel| !globs::matches(&exempt, rel)) {
        crate::output::warn("[header].exempt excuses every file [header].paths selects");
    }

    let mut out = Vec::new();
    for rel in &selected {
        if globs::matches(&exempt, rel) {
            continue;
        }
        // Bytes, not a `String`: a file that is not valid UTF-8 used to fail
        // `read_to_string` and be skipped - i.e. pass. Matching the header's
        // bytes judges it on content like any other file. An unreadable file
        // is an error (see `TreeSnapshot::read_file`); only a path deleted
        // from the working tree is skipped.
        let Some(bytes) = tree.read_file(rel)? else {
            continue;
        };
        if !contains_bytes(bytes.as_ref(), expected.as_bytes()) {
            out.push(HeaderViolation { file: rel.clone() });
        }
    }
    Ok(out)
}

/// Whether `needle` occurs in `haystack`. An empty needle always does, as
/// with `str::contains`.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
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
        assert_eq!(
            expand("SPDX-License-Identifier: MIT", 2026),
            "SPDX-License-Identifier: MIT"
        );
    }

    #[test]
    fn header_match_works_on_non_utf8_bytes() {
        // A Latin-1 file is judged on its content, not skipped as a pass.
        assert!(contains_bytes(b"\xA9 Copyright 2026\n", b"Copyright 2026"));
        assert!(!contains_bytes(b"\xA9 nothing here\n", b"Copyright 2026"));
        assert!(contains_bytes(b"", b""));
        assert!(!contains_bytes(b"ab", b"abc"));
    }

    #[test]
    fn utc_year_uses_the_supplied_epoch() {
        assert_eq!(utc_year_at(0).unwrap(), 1970);
        assert_eq!(utc_year_at(946_684_800).unwrap(), 2000);
    }

    #[test]
    fn scan_observes_globs_exemptions_and_file_contents() {
        let root = crate::scope::test_git_tree("header", "scan_globs");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "missing header\n").unwrap();
        std::fs::write(root.join("tests/helper.rs"), "missing but exempt\n").unwrap();
        let cfg = HeaderConfig {
            paths: vec!["**/*.rs".into()],
            pattern: "Copyright {year}".into(),
            exempt: vec!["tests/**".into()],
        };

        let tree = crate::scope::TreeSnapshot::load(&root).unwrap();
        let violations = scan(&tree, &cfg, 2025).unwrap();
        assert_eq!(
            violations,
            vec![HeaderViolation {
                file: PathBuf::from("src/lib.rs")
            }]
        );
    }
}
