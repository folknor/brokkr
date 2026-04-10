//! Data types for the results database.

use std::io::Read;

use crate::error::DevError;

// ---------------------------------------------------------------------------
// Key-value pairs
// ---------------------------------------------------------------------------

/// A typed key-value pair for benchmark metadata and subprocess metrics.
#[derive(Clone)]
pub struct KvPair {
    pub key: String,
    pub value: KvValue,
}

/// Typed value for a key-value pair.
#[derive(Clone)]
pub enum KvValue {
    Int(i64),
    Real(f64),
    Text(String),
}

impl KvPair {
    pub fn int(key: impl Into<String>, value: i64) -> Self {
        Self {
            key: key.into(),
            value: KvValue::Int(value),
        }
    }
    pub fn real(key: impl Into<String>, value: f64) -> Self {
        Self {
            key: key.into(),
            value: KvValue::Real(value),
        }
    }
    pub fn text(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: KvValue::Text(value.into()),
        }
    }
}

impl std::fmt::Display for KvValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Int(v) => write!(f, "{v}"),
            Self::Real(v) => write!(f, "{v}"),
            Self::Text(v) => write!(f, "{v}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Profiling data
// ---------------------------------------------------------------------------

/// Distribution statistics from `harness.run_distribution()`.
#[derive(Clone)]
pub struct Distribution {
    pub samples: i64,
    pub min_ms: i64,
    pub p50_ms: i64,
    pub p95_ms: i64,
    pub max_ms: i64,
}

/// A single function row from hotpath profiling.
#[derive(Clone)]
pub struct HotpathFunction {
    pub section: String,
    pub description: Option<String>,
    pub ordinal: i64,
    pub name: String,
    pub calls: Option<i64>,
    pub avg: Option<String>,
    pub total: Option<String>,
    pub percent_total: Option<String>,
    pub p50: Option<String>,
    pub p95: Option<String>,
    pub p99: Option<String>,
}

/// A single thread row from hotpath profiling.
#[derive(Clone)]
pub struct HotpathThread {
    pub name: String,
    pub status: Option<String>,
    pub cpu_percent: Option<String>,
    pub cpu_percent_max: Option<String>,
    pub cpu_percent_avg: Option<String>,
    pub alloc_bytes: Option<String>,
    pub dealloc_bytes: Option<String>,
    pub mem_diff: Option<String>,
}

/// Structured hotpath profiling data (functions + threads).
#[derive(Clone)]
pub struct HotpathData {
    pub functions: Vec<HotpathFunction>,
    pub threads: Vec<HotpathThread>,
    pub thread_summary: Vec<KvPair>,
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// A benchmark result row to insert.
pub struct RunRow {
    pub hostname: String,
    pub commit: String,
    pub subject: String,
    pub command: String,
    pub variant: Option<String>,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub elapsed_ms: i64,
    pub peak_rss_mb: Option<f64>,
    pub cargo_features: Option<String>,
    pub cargo_profile: String,
    pub kernel: Option<String>,
    pub cpu_governor: Option<String>,
    pub avail_memory_mb: Option<i64>,
    pub storage_notes: Option<String>,
    pub cli_args: Option<String>,
    pub project: String,
    pub kv: Vec<KvPair>,
    pub distribution: Option<Distribution>,
    pub hotpath: Option<HotpathData>,
}

/// A row read back from the database.
///
/// Nullable columns (`variant`, `input_file`, `cargo_features`, etc.) are
/// mapped to `String` via `unwrap_or_default()` — `NULL` becomes `""`.
/// This is intentional: all consumers use `.is_empty()` checks, so the
/// distinction between NULL and empty string is not needed.
#[allow(dead_code)]
pub struct StoredRow {
    pub id: i64,
    pub timestamp: String,
    pub hostname: String,
    pub commit: String,
    pub subject: String,
    pub command: String,
    pub variant: String,
    pub input_file: String,
    pub input_mb: Option<f64>,
    pub elapsed_ms: i64,
    pub peak_rss_mb: Option<f64>,
    pub cargo_features: String,
    pub cargo_profile: String,
    pub kernel: String,
    pub cpu_governor: String,
    pub avail_memory_mb: Option<i64>,
    pub storage_notes: String,
    pub uuid: String,
    pub cli_args: String,
    pub project: String,
    pub kv: Vec<KvPair>,
    pub distribution: Option<Distribution>,
    pub hotpath: Option<HotpathData>,
}

/// Two-commit comparison: (commit_a, rows_a, commit_b, rows_b).
pub type CompareResult = (String, Vec<StoredRow>, String, Vec<StoredRow>);

/// Filters for querying stored rows.
pub struct QueryFilter {
    pub commit: Option<String>,
    pub command: Option<String>,
    pub variant: Option<String>,
    /// Substring match against the `input_file` column. Useful for filtering
    /// by the dataset name embedded in benchmark input filenames (e.g.
    /// `europe-20260301-seq4714-with-indexdata.osm` matches `europe`).
    pub dataset: Option<String>,
    pub limit: usize,
}

// ---------------------------------------------------------------------------
// UUID helpers
// ---------------------------------------------------------------------------

/// Generate a UUIDv4 as 32 hex chars (no dashes).
pub(super) fn generate_uuid() -> Result<String, DevError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    // Set version 4.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // Set variant 1.
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(hex)
}

/// Return the first 8 hex chars of a UUID.
pub(super) fn short_uuid(uuid: &str) -> String {
    uuid[..8.min(uuid.len())].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_uuid_normal() {
        let result = short_uuid("abcdef1234567890abcdef1234567890");
        assert_eq!(result, "abcdef12");
    }

    #[test]
    fn short_uuid_exactly_8() {
        let result = short_uuid("12345678");
        assert_eq!(result, "12345678");
    }

    #[test]
    fn short_uuid_shorter_than_8() {
        let result = short_uuid("abc");
        assert_eq!(result, "abc");
    }

    #[test]
    fn short_uuid_empty() {
        let result = short_uuid("");
        assert_eq!(result, "");
    }
}
