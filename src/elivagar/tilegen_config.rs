//! Typed values deserialized from elivagar's tilegen config blocks.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;

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

/// A byte count parsed from a config string with bytes or MiB/GiB suffixes.
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
        const MIB: usize = 1_024 * 1_024;

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

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A gzip compression level whose config value has already been range checked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompressionLevel(u8);

impl<'de> Deserialize<'de> for CompressionLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let level = u32::deserialize(deserializer)?;
        match u8::try_from(level) {
            Ok(level) if level <= 10 => Ok(Self(level)),
            _ => Err(D::Error::custom(
                "tilegen compression_level must be between 0 and 10",
            )),
        }
    }
}

impl fmt::Display for CompressionLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

const MIB: usize = 1_024 * 1_024;
const SORT_BUDGET_MIN_BYTES: usize = 64 * MIB;
const STREAM_BUDGET_MIN_BYTES: usize = MIB;

pub(crate) fn deserialize_sort_budget<'de, D>(deserializer: D) -> Result<Option<ByteSize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_budget(deserializer, "sort_budget", SORT_BUDGET_MIN_BYTES)
}

pub(crate) fn deserialize_way_budget<'de, D>(deserializer: D) -> Result<Option<ByteSize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_budget(deserializer, "way_budget", STREAM_BUDGET_MIN_BYTES)
}

pub(crate) fn deserialize_assemble_budget<'de, D>(
    deserializer: D,
) -> Result<Option<ByteSize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_budget(deserializer, "assemble_budget", STREAM_BUDGET_MIN_BYTES)
}

fn deserialize_budget<'de, D>(
    deserializer: D,
    name: &str,
    minimum: usize,
) -> Result<Option<ByteSize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    let Some(value) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let size = value
        .parse::<ByteSize>()
        .map_err(|error| D::Error::custom(format!("tilegen {name}: {error}")))?;
    if size.bytes() < minimum {
        return Err(D::Error::custom(format!(
            "tilegen {name} must be at least {minimum} bytes"
        )));
    }
    Ok(Some(size))
}
