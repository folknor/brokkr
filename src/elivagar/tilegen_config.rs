//! Typed values and validation for elivagar's tilegen command-line contract.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;

use crate::config::project_config::elivagar::TilegenConfig;
use crate::error::DevError;

const MIB: usize = 1_024 * 1_024;
const SORT_BUDGET_MIN_BYTES: usize = 64 * MIB;
const STREAM_BUDGET_MIN_BYTES: usize = MIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileFormat {
    Mvt,
    Mlt,
}

impl fmt::Display for TileFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mvt => "mvt",
            Self::Mlt => "mlt",
        })
    }
}

impl FromStr for TileFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "mvt" => Ok(Self::Mvt),
            "mlt" => Ok(Self::Mlt),
            _ => Err(format!("expected mvt or mlt, got {value:?}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileCompression {
    Gzip,
    Brotli,
}

impl fmt::Display for TileCompression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gzip => "gzip",
            Self::Brotli => "brotli",
        })
    }
}

impl FromStr for TileCompression {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "gzip" => Ok(Self::Gzip),
            "brotli" => Ok(Self::Brotli),
            _ => Err(format!("expected gzip or brotli, got {value:?}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortChunkCompression {
    Lz4,
    Snappy,
}

impl fmt::Display for SortChunkCompression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lz4 => "lz4",
            Self::Snappy => "snappy",
        })
    }
}

impl FromStr for SortChunkCompression {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "lz4" => Ok(Self::Lz4),
            "snappy" => Ok(Self::Snappy),
            _ => Err(format!("expected lz4 or snappy, got {value:?}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteSize(usize);

impl ByteSize {
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.0
    }
}

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let (digits, multiplier) = if let Some(digits) =
            value.strip_suffix('M').or_else(|| value.strip_suffix('m'))
        {
            (digits, MIB)
        } else if let Some(digits) = value.strip_suffix('G').or_else(|| value.strip_suffix('g')) {
            (digits, MIB * 1_024)
        } else {
            (value, 1)
        };
        let digits = digits
            .trim()
            .parse::<usize>()
            .map_err(|_| format!("expected bytes, MiB (M), or GiB (G), got {value:?}"))?;
        digits
            .checked_mul(multiplier)
            .map(Self)
            .ok_or_else(|| format!("byte size overflows this platform: {value:?}"))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

fn validate_enum<T>(name: &str, value: &impl fmt::Display) -> Result<(), DevError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    value
        .to_string()
        .parse::<T>()
        .map(|_| ())
        .map_err(|error| DevError::Config(format!("tilegen {name}: {error}")))
}

fn validate_budget(name: &str, value: &impl fmt::Display, minimum: usize) -> Result<(), DevError> {
    let rendered = value.to_string();
    let size = rendered
        .parse::<ByteSize>()
        .map_err(|error| DevError::Config(format!("tilegen {name}: {error}")))?;
    if size.bytes() < minimum {
        return Err(DevError::Config(format!(
            "tilegen {name} must be at least {minimum} bytes"
        )));
    }
    Ok(())
}

/// Validate every forwarded knob before cargo starts building elivagar.
/// `TilegenConfig` currently deserializes enum and budget fields as strings and
/// compression_level as an unchecked u32; this rejects invalid values before
/// the build.
pub(crate) fn validate(config: &TilegenConfig) -> Result<(), DevError> {
    if let Some(value) = &config.tile_format {
        validate_enum::<TileFormat>("tile_format", value)?;
    }
    if let Some(value) = &config.tile_compression {
        validate_enum::<TileCompression>("tile_compression", value)?;
    }
    if let Some(value) = &config.compress_sort_chunks {
        validate_enum::<SortChunkCompression>("compress_sort_chunks", value)?;
    }
    if let Some(value) = &config.sort_budget {
        validate_budget("sort_budget", value, SORT_BUDGET_MIN_BYTES)?;
    }
    if let Some(value) = &config.way_budget {
        validate_budget("way_budget", value, STREAM_BUDGET_MIN_BYTES)?;
    }
    if let Some(value) = &config.assemble_budget {
        validate_budget("assemble_budget", value, STREAM_BUDGET_MIN_BYTES)?;
    }
    if let Some(level) = config.compression_level {
        let level = level
            .to_string()
            .parse::<u32>()
            .map_err(|error| DevError::Config(format!("tilegen compression_level: {error}")))?;
        if level > 10 {
            return Err(DevError::Config(
                "tilegen compression_level must be between 0 and 10".into(),
            ));
        }
    }
    config
        .ocean_specs()
        .map_err(|error| DevError::Config(format!("tilegen ocean: {error}")))?;
    Ok(())
}
