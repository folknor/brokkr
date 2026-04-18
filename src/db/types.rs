//! Data types for the results database.

use std::collections::BTreeMap;
use std::io::Read;

use crate::build::CargoProfile;
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
    /// Measurement mode - exactly one of `"bench"`, `"hotpath"`, or
    /// `"alloc"`. `None` only occurs for pre-v13 rows that haven't had
    /// a mode assigned yet (should not exist in practice after the
    /// v12→v13 migration). Was previously called `variant`.
    pub mode: Option<String>,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub elapsed_ms: i64,
    pub peak_rss_mb: Option<f64>,
    pub cargo_features: Option<String>,
    pub cargo_profile: CargoProfile,
    pub kernel: Option<String>,
    pub cpu_governor: Option<String>,
    pub avail_memory_mb: Option<i64>,
    pub storage_notes: Option<String>,
    pub cli_args: Option<String>,
    pub brokkr_args: Option<String>,
    pub project: String,
    pub stop_marker: Option<String>,
    pub kv: Vec<KvPair>,
    pub distribution: Option<Distribution>,
    pub hotpath: Option<HotpathData>,
}

/// A row read back from the database.
///
/// Nullable columns (`variant`, `input_file`, `cargo_features`, etc.) are
/// mapped to `String` via `unwrap_or_default()` - `NULL` becomes `""`.
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
    /// Measurement mode - `"bench"`, `"hotpath"`, or `"alloc"`. Empty
    /// string for (very old) pre-v13 rows that predate the shape.
    /// Was previously called `variant`.
    pub mode: String,
    pub input_file: String,
    pub input_mb: Option<f64>,
    pub elapsed_ms: i64,
    pub peak_rss_mb: Option<f64>,
    pub cargo_features: String,
    /// `None` for legacy rows whose `cargo_profile` column is `NULL` or
    /// empty - formatters skip the cargo field entirely rather than
    /// inventing a `release` value.
    pub cargo_profile: Option<CargoProfile>,
    pub kernel: String,
    pub cpu_governor: String,
    pub avail_memory_mb: Option<i64>,
    pub storage_notes: String,
    pub uuid: String,
    pub cli_args: String,
    pub brokkr_args: String,
    pub project: String,
    pub stop_marker: String,
    pub kv: Vec<KvPair>,
    /// Env vars captured per `capture_env` in brokkr.toml, extracted from
    /// the `env.*` kv pairs at query time. First-class axis alongside
    /// `cargo_features` / `mode` / `brokkr_args`: env-gated code paths
    /// change *what* ran, not just metadata. Empty for the vast majority
    /// of historical rows predating the capture feature.
    pub captured_env: BTreeMap<String, String>,
    pub distribution: Option<Distribution>,
    pub hotpath: Option<HotpathData>,
}

impl StoredRow {
    /// Stable fingerprint of the captured env set, for pair-key dedup.
    /// Empty string when no env was captured (keeps pre-capture pair
    /// keys unchanged).
    ///
    /// Joined with `\x1f` (ASCII unit separator) rather than `,` because
    /// env values legitimately contain commas - `MALLOC_CONF` is
    /// comma-delimited by glibc convention (e.g.
    /// `dirty_decay_ms:0,narenas:1`). A comma joiner would make two
    /// one-var rows with `MALLOC_CONF=a,b=1` indistinguishable from one
    /// two-var row with `MALLOC_CONF=a` + `b=1`.
    pub fn env_fingerprint(&self) -> String {
        if self.captured_env.is_empty() {
            return String::new();
        }
        // BTreeMap iterates sorted, so the produced order is stable
        // without a separate sort step.
        self.captured_env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\x1f")
    }
}

/// Filters for querying stored rows.
#[derive(Default)]
pub struct QueryFilter {
    pub commit: Option<String>,
    pub command: Option<String>,
    /// Substring match against the `mode` column (post-v13 - only
    /// `"bench"`/`"hotpath"`/`"alloc"`). Was previously called `variant`.
    pub mode: Option<String>,
    /// Substring match against the `input_file` column. Useful for filtering
    /// by the dataset name embedded in benchmark input filenames (e.g.
    /// `europe-20260301-seq4714-with-indexdata.osm` matches `europe`).
    pub dataset: Option<String>,
    /// Metadata filters as `(key, value)` pairs. The key is the user-facing
    /// name without the `meta.` prefix (e.g. `("format", "osc")` matches rows
    /// with `meta.format = "osc"` in the run_kv table). Multiple filters AND
    /// together. Rows missing the key are silently excluded.
    pub meta: Vec<(String, String)>,
    /// Substring match against `cli_args` OR `brokkr_args` (the two
    /// literal-invocation columns). Like `git log --grep`: a single
    /// pattern that scans the freeform-text columns for a token.
    pub grep: Option<String>,
    /// Captured-env filters as `(key, value)` pairs. The key is the bare
    /// env var name without the `env.` prefix (e.g. `("PBFHOGG_USE_NEW_PATH",
    /// "1")` matches rows where the captured var equals `"1"`). Multiple
    /// filters AND together; rows missing the key are excluded (no
    /// missing-as-0 coercion - set the baseline explicitly).
    pub env: Vec<(String, String)>,
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

    fn stored_row_with_env(env: &[(&str, &str)]) -> StoredRow {
        StoredRow {
            id: 0,
            timestamp: String::new(),
            hostname: String::new(),
            commit: String::new(),
            subject: String::new(),
            command: String::new(),
            mode: String::new(),
            input_file: String::new(),
            input_mb: None,
            elapsed_ms: 0,
            peak_rss_mb: None,
            cargo_features: String::new(),
            cargo_profile: None,
            kernel: String::new(),
            cpu_governor: String::new(),
            avail_memory_mb: None,
            storage_notes: String::new(),
            uuid: String::new(),
            cli_args: String::new(),
            brokkr_args: String::new(),
            project: String::new(),
            stop_marker: String::new(),
            kv: Vec::new(),
            captured_env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            distribution: None,
            hotpath: None,
        }
    }

    #[test]
    fn env_fingerprint_empty_when_no_captures() {
        assert_eq!(stored_row_with_env(&[]).env_fingerprint(), "");
    }

    #[test]
    fn env_fingerprint_disambiguates_comma_in_value() {
        // Regression: the fingerprint used to join with ',' so a single
        // var whose value contains ',' was indistinguishable from two
        // vars. `MALLOC_CONF` is literally the motivating example -
        // glibc accepts `dirty_decay_ms:0,narenas:1` syntax.
        let row_one_comma_value =
            stored_row_with_env(&[("MALLOC_CONF", "dirty_decay_ms:0,narenas:1")]);
        let row_two_vars_collision = stored_row_with_env(&[
            ("MALLOC_CONF", "dirty_decay_ms:0"),
            ("narenas:1", ""),
        ]);
        assert_ne!(
            row_one_comma_value.env_fingerprint(),
            row_two_vars_collision.env_fingerprint()
        );
    }

    #[test]
    fn env_fingerprint_stable_order() {
        // BTreeMap iteration is already sorted; two rows built in
        // different insertion orders must fingerprint identically.
        let a = stored_row_with_env(&[("A", "1"), ("B", "2")]);
        let b = stored_row_with_env(&[("B", "2"), ("A", "1")]);
        assert_eq!(a.env_fingerprint(), b.env_fingerprint());
    }
}
