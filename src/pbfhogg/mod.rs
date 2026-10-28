pub(crate) mod argv;
pub mod bench_all;
pub mod bench_blob_filter;
pub mod bench_commands;
pub mod bench_extract;
pub mod bench_merge;
pub mod bench_planetiler;
pub mod bench_read;
pub mod bench_write;
pub(crate) mod binary;
pub(crate) mod cmd;
pub mod commands;
pub mod context;
pub mod download;
pub(crate) mod inspect_report;
pub(crate) mod plan;
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
pub mod verify_multi_extract;
pub mod verify_renumber;
pub mod verify_sort;
pub mod verify_tags_filter;

use std::fmt;
use std::str::FromStr;

use crate::error::DevError;

/// Default compression list used by the `bench all` write preset.
pub const DEFAULT_BENCH_WRITE_COMPRESSIONS: &str = "none,zlib:6,zstd:3";

/// Default compression list used by the `bench all` merge preset.
pub const DEFAULT_BENCH_MERGE_COMPRESSIONS: &str = "zlib,none";

const DEFAULT_ZLIB_LEVEL: u8 = 6;
const DEFAULT_ZSTD_LEVEL: u32 = 3;

/// One validated compression choice accepted by pbfhogg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compression {
    codec: CompressionCodec,
    spelling: String,
}

/// Parsed comma-separated compression choices for the benchmark CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompressionList(Vec<Compression>);

impl CompressionList {
    pub(crate) fn as_slice(&self) -> &[Compression] {
        &self.0
    }
}

impl FromStr for CompressionList {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .split(',')
            .map(str::parse::<Compression>)
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompressionCodec {
    None,
    Zlib(Option<u8>),
    Zstd(Option<u32>),
}

impl Compression {
    fn with_default_level(self) -> Self {
        match self.codec {
            CompressionCodec::Zlib(None) => Self {
                codec: CompressionCodec::Zlib(Some(DEFAULT_ZLIB_LEVEL)),
                spelling: format!("zlib:{DEFAULT_ZLIB_LEVEL}"),
            },
            CompressionCodec::Zstd(None) => Self {
                codec: CompressionCodec::Zstd(Some(DEFAULT_ZSTD_LEVEL)),
                spelling: format!("zstd:{DEFAULT_ZSTD_LEVEL}"),
            },
            _ => self,
        }
    }
}

/// Keep the CLI's already-parsed compression choices typed until the
/// benchmark builds the child argv. Bare codecs get their command-specific
/// defaults here without parsing their spelling a second time.
pub(crate) fn compression_args(
    compressions: &[Compression],
    add_default_levels: bool,
) -> Vec<String> {
    compressions
        .iter()
        .cloned()
        .map(|compression| {
            if add_default_levels {
                compression.with_default_level()
            } else {
                compression
            }
        })
        .map(|compression| compression.to_string())
        .collect()
}

impl FromStr for Compression {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        match value {
            "none" => Ok(Self {
                codec: CompressionCodec::None,
                spelling: value.to_owned(),
            }),
            "zlib" => Ok(Self {
                codec: CompressionCodec::Zlib(None),
                spelling: value.to_owned(),
            }),
            "zstd" => Ok(Self {
                codec: CompressionCodec::Zstd(None),
                spelling: value.to_owned(),
            }),
            value if value.starts_with("zlib:") => {
                let level_text = &value["zlib:".len()..];
                let level = level_text
                    .parse::<u8>()
                    .map_err(|_| format!("invalid compression level: {value}"))?;
                if !(1..=9).contains(&level) {
                    return Err(format!("zlib level {level} out of range, expected 1-9"));
                }
                Ok(Self {
                    codec: CompressionCodec::Zlib(Some(level)),
                    spelling: value.to_owned(),
                })
            }
            value if value.starts_with("zstd:") => {
                let level_text = &value["zstd:".len()..];
                let level = level_text
                    .parse::<u32>()
                    .map_err(|_| format!("invalid compression level: {value}"))?;
                Ok(Self {
                    codec: CompressionCodec::Zstd(Some(level)),
                    spelling: value.to_owned(),
                })
            }
            _ => Err(format!("unknown compression: {value}")),
        }
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.spelling)
    }
}

/// Parse a comma-separated compression list (e.g. "none,zlib,zstd:5").
///
/// When `add_default_levels` is true, bare `"zlib"` becomes `"zlib:6"` and
/// bare `"zstd"` becomes `"zstd:3"`. When false, they pass through as-is.
pub fn parse_compressions(input: &str, add_default_levels: bool) -> Result<Vec<String>, DevError> {
    let parsed = input.parse::<CompressionList>().map_err(DevError::Config)?;
    Ok(compression_args(parsed.as_slice(), add_default_levels))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
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
    fn compression_levels_follow_cli_ranges() {
        assert!(parse_compressions("zlib:0", true).is_err());
        assert!(parse_compressions("zlib:10", true).is_err());
        assert!(parse_compressions("zstd:-1", true).is_err());
        assert_eq!(parse_compressions("zstd:0", true).unwrap(), vec!["zstd:0"]);
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
