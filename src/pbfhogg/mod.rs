use std::path::Path;

use crate::error::DevError;

/// Extract the basename and UTF-8 string for a PBF (or similar) path.
///
/// Returns `(basename, path_str)` or an error if the path is not valid UTF-8.
pub fn path_strs(path: &Path) -> Result<(String, &str), DevError> {
    let basename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let path_str = path
        .to_str()
        .ok_or_else(|| DevError::Config(format!("path is not valid UTF-8: {}", path.display())))?;
    Ok((basename, path_str))
}

pub mod bench_all;
pub mod bench_allocator;
pub mod bench_blob_filter;
pub mod bench_commands;
pub mod bench_extract;
pub mod bench_merge;
pub mod bench_planetiler;
pub mod bench_read;
pub mod bench_write;
pub(crate) mod cmd;
pub mod commands;
pub mod download;
pub mod verify;
pub mod verify_add_locations;
pub mod verify_all;
pub mod verify_cat;
pub mod verify_check_refs;
pub mod verify_derive_changes;
pub mod verify_diff;
pub mod verify_extract;
pub mod verify_getid_removeid;
pub mod verify_multi_extract;
pub mod verify_merge;
pub mod verify_sort;
pub mod verify_tags_filter;

/// Parse a comma-separated compression list (e.g. "none,zlib,zstd:5").
///
/// When `add_default_levels` is true, bare `"zlib"` becomes `"zlib:6"` and
/// bare `"zstd"` becomes `"zstd:3"`. When false, they pass through as-is.
pub fn parse_compressions(input: &str, add_default_levels: bool) -> Result<Vec<String>, DevError> {
    let mut result = Vec::new();
    for token in input.split(',') {
        let trimmed = token.trim();
        let label = match trimmed {
            "none" => "none".to_owned(),
            "zlib" if add_default_levels => "zlib:6".to_owned(),
            "zstd" if add_default_levels => "zstd:3".to_owned(),
            "zlib" | "zstd" => trimmed.to_owned(),
            s if s.starts_with("zlib:") || s.starts_with("zstd:") => {
                let colon = s.find(':').unwrap_or(0);
                let level_str = &s[colon + 1..];
                if level_str.parse::<i32>().is_err() {
                    return Err(DevError::Config(format!(
                        "invalid compression level: {trimmed}"
                    )));
                }
                trimmed.to_owned()
            }
            _ => {
                return Err(DevError::Config(format!("unknown compression: {trimmed}")));
            }
        };
        result.push(label);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_none() {
        let result = parse_compressions("none", true).unwrap();
        assert_eq!(result, vec!["none"]);
    }

    #[test]
    fn default_level_zlib() {
        let result = parse_compressions("zlib", true).unwrap();
        assert_eq!(result, vec!["zlib:6"]);
    }

    #[test]
    fn default_level_zstd() {
        let result = parse_compressions("zstd", true).unwrap();
        assert_eq!(result, vec!["zstd:3"]);
    }

    #[test]
    fn no_default_levels_zlib() {
        let result = parse_compressions("zlib", false).unwrap();
        assert_eq!(result, vec!["zlib"]);
    }

    #[test]
    fn no_default_levels_zstd() {
        let result = parse_compressions("zstd", false).unwrap();
        assert_eq!(result, vec!["zstd"]);
    }

    #[test]
    fn explicit_level_passes_through() {
        let result = parse_compressions("zlib:9", true).unwrap();
        assert_eq!(result, vec!["zlib:9"]);
    }

    #[test]
    fn explicit_zstd_level() {
        let result = parse_compressions("zstd:19", false).unwrap();
        assert_eq!(result, vec!["zstd:19"]);
    }

    #[test]
    fn multiple_mixed() {
        let result = parse_compressions("none,zlib,zstd:5", true).unwrap();
        assert_eq!(result, vec!["none", "zlib:6", "zstd:5"]);
    }

    #[test]
    fn whitespace_trimmed() {
        let result = parse_compressions("  none , zlib , zstd:3 ", true).unwrap();
        assert_eq!(result, vec!["none", "zlib:6", "zstd:3"]);
    }

    #[test]
    fn invalid_compression_name() {
        let err = parse_compressions("lz4", true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown compression"), "got: {msg}");
        assert!(msg.contains("lz4"), "got: {msg}");
    }

    #[test]
    fn invalid_level_not_a_number() {
        let err = parse_compressions("zlib:abc", true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid compression level"), "got: {msg}");
    }

    #[test]
    fn negative_level_is_valid_i32() {
        // Negative levels are valid i32 parses (zstd supports negative levels).
        let result = parse_compressions("zstd:-1", true).unwrap();
        assert_eq!(result, vec!["zstd:-1"]);
    }

    #[test]
    fn explicit_level_with_defaults_off_still_works() {
        let result = parse_compressions("zlib:1,zstd:7", false).unwrap();
        assert_eq!(result, vec!["zlib:1", "zstd:7"]);
    }

    #[test]
    fn all_three_with_defaults() {
        let result = parse_compressions("none,zlib,zstd", true).unwrap();
        assert_eq!(result, vec!["none", "zlib:6", "zstd:3"]);
    }
}
