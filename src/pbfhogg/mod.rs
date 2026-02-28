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

pub mod bench_allocator;
pub mod bench_all;
pub mod bench_blob_filter;
pub mod bench_commands;
pub mod bench_extract;
pub mod bench_merge;
pub mod bench_planetiler;
pub mod bench_read;
pub mod bench_write;
pub mod download;
pub mod hotpath;
pub mod profile;
pub mod verify;
pub mod verify_add_locations;
pub mod verify_all;
pub mod verify_cat;
pub mod verify_check_refs;
pub mod verify_derive_changes;
pub mod verify_diff;
pub mod verify_extract;
pub mod verify_getid_removeid;
pub mod verify_merge;
pub mod verify_sort;
pub mod verify_tags_filter;

/// Parse a comma-separated compression list (e.g. "none,zlib,zstd:5").
///
/// When `add_default_levels` is true, bare `"zlib"` becomes `"zlib:6"` and
/// bare `"zstd"` becomes `"zstd:3"`. When false, they pass through as-is.
pub fn parse_compressions(
    input: &str,
    add_default_levels: bool,
) -> Result<Vec<String>, DevError> {
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
                return Err(DevError::Config(format!(
                    "unknown compression: {trimmed}"
                )));
            }
        };
        result.push(label);
    }
    Ok(result)
}
