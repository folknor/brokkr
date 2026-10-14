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

/// The meaning assigned to a stored key-value pair by its key namespace.
/// Keep the prefix vocabulary here so query and display paths agree about
/// which values are provenance, thread summaries, or runtime counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KvClass {
    Metadata,
    Environment,
    Predecessor,
    ThreadSummary,
    Counter,
}

impl KvClass {
    pub(crate) fn for_key(key: &str) -> Self {
        if key.starts_with("meta.") {
            Self::Metadata
        } else if key.starts_with("env.") {
            Self::Environment
        } else if key.starts_with("prev.") {
            Self::Predecessor
        } else if key.starts_with("threads.") {
            Self::ThreadSummary
        } else {
            Self::Counter
        }
    }

    fn prefix(self) -> Option<&'static str> {
        match self {
            Self::Metadata => Some("meta."),
            Self::Environment => Some("env."),
            Self::Predecessor => Some("prev."),
            Self::ThreadSummary => Some("threads."),
            Self::Counter => None,
        }
    }

    pub(crate) fn suffix(self, key: &str) -> &str {
        self.prefix()
            .and_then(|prefix| key.strip_prefix(prefix))
            .unwrap_or(key)
    }

    pub(crate) fn qualify(self, key: &str) -> String {
        let Some(prefix) = self.prefix() else {
            return key.to_owned();
        };
        if key.starts_with(prefix) {
            key.to_owned()
        } else {
            format!("{prefix}{key}")
        }
    }
}

impl KvPair {
    pub(crate) fn class(&self) -> KvClass {
        KvClass::for_key(&self.key)
    }
}

/// Stable display and comparison label for a comma-separated Cargo feature
/// set. Cargo treats feature order and duplicate requests as immaterial, so
/// persisted rows use the same sorted, unique spelling regardless of which
/// caller assembled the build configuration.
pub(crate) fn canonical_cargo_features(features: Option<&str>) -> Option<String> {
    let mut names: Vec<&str> = features?
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    (!names.is_empty()).then(|| names.join(","))
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
///
/// The `*_ms` fields are integer milliseconds and always present - every
/// historical row has them. They are the `us` readings rounded to nearest
/// whenever `us` exists, so never treat the two as independent measurements.
#[derive(Clone)]
pub struct Distribution {
    pub samples: i64,
    pub min_ms: i64,
    pub p50_ms: i64,
    pub p95_ms: i64,
    pub max_ms: i64,
    /// The same statistics in microseconds. `None` for every row recorded
    /// before the v18->v19 schema bump: those samples were rounded to whole
    /// milliseconds at measure time, so a nidhogg API query under half a
    /// millisecond stored 0, and the detail cannot be reconstructed. Not
    /// backfilled from `*_ms * 1000`, which would manufacture a precision
    /// the row never had.
    pub us: Option<DistributionUs>,
}

/// Microsecond distribution statistics; see [`Distribution::us`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistributionUs {
    pub min: i64,
    pub p50: i64,
    pub p95: i64,
    pub max: i64,
}

/// The dimension carried by a hotpath quantity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuantityKind {
    Duration,
    Bytes,
    Percent,
}

impl QuantityKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Duration => "duration_ns",
            Self::Bytes => "bytes",
            Self::Percent => "percent",
        }
    }
}

/// A hotpath metric in a canonical unit.
///
/// Signed integer storage is intentional: `mem_diff` can be negative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Quantity {
    DurationNs(i64),
    Bytes(i64),
    Percent(f64),
}

impl Quantity {
    /// Parse the display spelling written by the hotpath JSON reporter.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        if let Some(number) = value.strip_suffix('%') {
            let number = number.trim().parse::<f64>().ok()?;
            return number.is_finite().then_some(Self::Percent(number));
        }

        let split = value.rfind(char::is_whitespace)?;
        let number = value[..split].trim().parse::<f64>().ok()?;
        if !number.is_finite() {
            return None;
        }
        let unit = value[split..].trim();
        let (kind, scale) = match unit {
            "ns" => (QuantityKind::Duration, 1.0),
            "µs" | "μs" | "us" => (QuantityKind::Duration, 1_000.0),
            "ms" => (QuantityKind::Duration, 1_000_000.0),
            "s" => (QuantityKind::Duration, 1_000_000_000.0),
            "B" => (QuantityKind::Bytes, 1.0),
            "KB" => (QuantityKind::Bytes, 1_024.0),
            "MB" => (QuantityKind::Bytes, 1_048_576.0),
            "GB" => (QuantityKind::Bytes, 1_073_741_824.0),
            _ => return None,
        };
        let value = checked_rounded_i64(number * scale)?;
        match kind {
            QuantityKind::Duration => Some(Self::DurationNs(value)),
            QuantityKind::Bytes => Some(Self::Bytes(value)),
            QuantityKind::Percent => None,
        }
    }

    pub(crate) fn kind(self) -> QuantityKind {
        match self {
            Self::DurationNs(_) => QuantityKind::Duration,
            Self::Bytes(_) => QuantityKind::Bytes,
            Self::Percent(_) => QuantityKind::Percent,
        }
    }

    pub(crate) fn storage_parts(self) -> (&'static str, Option<i64>, Option<f64>) {
        match self {
            Self::DurationNs(value) => (QuantityKind::Duration.as_str(), Some(value), None),
            Self::Bytes(value) => (QuantityKind::Bytes.as_str(), Some(value), None),
            Self::Percent(value) => (QuantityKind::Percent.as_str(), None, Some(value)),
        }
    }

    pub(crate) fn from_storage(
        kind: &str,
        integer: Option<i64>,
        real: Option<f64>,
    ) -> Option<Self> {
        match kind {
            "duration_ns" => integer.map(Self::DurationNs),
            "bytes" => integer.map(Self::Bytes),
            "percent" => real.filter(|value| value.is_finite()).map(Self::Percent),
            _ => None,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn numeric_value(self) -> f64 {
        match self {
            Self::DurationNs(value) | Self::Bytes(value) => value as f64,
            Self::Percent(value) => value,
        }
    }

    pub(crate) fn relative_change_from(self, baseline: Self) -> Option<f64> {
        if self.kind() != baseline.kind() {
            return None;
        }
        let old = baseline.numeric_value();
        if old.abs() <= f64::EPSILON {
            return None;
        }
        Some((self.numeric_value() - old) / old * 100.0)
    }
}

fn checked_rounded_i64(value: f64) -> Option<i64> {
    let rounded = value.round();
    // The upper bound is exclusive: i64::MAX rounds to 2^63 as an f64.
    if !rounded.is_finite() || rounded < i64::MIN as f64 || rounded >= -(i64::MIN as f64) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(rounded as i64)
}

impl std::fmt::Display for Quantity {
    #[allow(clippy::cast_precision_loss)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::DurationNs(value) => {
                let magnitude = value.unsigned_abs();
                let (scaled, unit) = if magnitude < 1_000 {
                    (value as f64, "ns")
                } else if magnitude < 1_000_000 {
                    (value as f64 / 1_000.0, "µs")
                } else if magnitude < 1_000_000_000 {
                    (value as f64 / 1_000_000.0, "ms")
                } else {
                    (value as f64 / 1_000_000_000.0, "s")
                };
                write!(f, "{scaled} {unit}")
            }
            Self::Bytes(value) => {
                let magnitude = value.unsigned_abs();
                let (scaled, unit) = if magnitude < 1_024 {
                    (value as f64, "B")
                } else if magnitude < 1_048_576 {
                    (value as f64 / 1_024.0, "KB")
                } else if magnitude < 1_073_741_824 {
                    (value as f64 / 1_048_576.0, "MB")
                } else {
                    (value as f64 / 1_073_741_824.0, "GB")
                };
                write!(f, "{scaled} {unit}")
            }
            Self::Percent(value) => write!(f, "{value}%"),
        }
    }
}

/// A percentile in the inclusive range 0 to 100. `total_cmp` keeps fractional
/// percentiles such as 99.9 in numeric order as map keys.
#[derive(Clone, Copy, Debug)]
pub struct Percentile(f64);

impl Percentile {
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(Self(if value == 0.0 {
            0.0
        } else {
            value
        }))
    }

    pub(crate) fn parse_key(key: &str) -> Option<Self> {
        Self::new(key.strip_prefix('p')?.parse().ok()?)
    }

    pub(crate) fn value(self) -> f64 {
        self.0
    }
}

impl PartialEq for Percentile {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0).is_eq()
    }
}

impl Eq for Percentile {}

impl PartialOrd for Percentile {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Percentile {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl std::fmt::Display for Percentile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "p{}", self.0)
    }
}

/// A single function row from hotpath profiling.
#[derive(Clone)]
pub struct HotpathFunction {
    pub section: String,
    pub description: Option<String>,
    pub ordinal: i64,
    pub name: String,
    pub calls: Option<i64>,
    pub avg: Option<Quantity>,
    pub total: Option<Quantity>,
    pub percent_total: Option<Quantity>,
    pub percentiles: BTreeMap<Percentile, Quantity>,
}

/// A single thread row from hotpath profiling.
#[derive(Clone)]
pub struct HotpathThread {
    pub name: String,
    pub status: Option<String>,
    pub cpu_percent: Option<Quantity>,
    pub cpu_percent_max: Option<Quantity>,
    pub cpu_percent_avg: Option<Quantity>,
    pub alloc_bytes: Option<Quantity>,
    pub dealloc_bytes: Option<Quantity>,
    pub mem_diff: Option<Quantity>,
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
    /// Exact wall in microseconds when the harness path knew it; see
    /// [`crate::harness::BenchResult::elapsed_us`]. `None` for every path
    /// that only ever had integer milliseconds.
    pub elapsed_us: Option<i64>,
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
    /// Stop request and observed event as `(requested, fired)`, kept together
    /// so the configured spelling cannot be mistaken for proof that it fired.
    pub stop_marker: Option<(Option<String>, Option<String>)>,
    pub kv: Vec<KvPair>,
    /// Per-iteration wall times in execution order (index = `run_idx`), as
    /// measured by the `--bench N` loop. Empty for single-run modes and for
    /// harness paths with no meaningful per-iteration wall.
    ///
    /// Ordered, and stored ordered: see [`Distribution`] for the sorted
    /// summary, which is a different question. `elapsed_ms` is the best of
    /// these; keeping them lets a reader see drift *within* one row.
    pub iterations: Vec<i64>,
    /// The same walls in microseconds, index for index, when the loop
    /// measured them. Empty when it did not; stored only when its length
    /// matches `iterations` (a partial list would misalign the order that is
    /// the table's whole point). Each `iterations[i]` is `iterations_us[i]`
    /// rounded to nearest.
    pub iterations_us: Vec<i64>,
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
    /// Exact wall in microseconds. `None` for every row recorded before the
    /// v16->v17 schema bump, and for every harness path that only ever had
    /// integer milliseconds. Not backfilled - the detail was never measured.
    pub elapsed_us: Option<i64>,
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
    /// The configured `--stop` marker, which may not have fired.
    pub stop_marker_requested: String,
    /// The marker that actually ended the child, empty unless `--stop` fired.
    pub stop_marker: String,
    pub kv: Vec<KvPair>,
    /// Env vars captured per `capture_env` in brokkr.toml, extracted from
    /// the `env.*` kv pairs at query time. First-class axis alongside
    /// `cargo_features` / `mode` / `brokkr_args`: env-gated code paths
    /// change *what* ran, not just metadata. Empty for the vast majority
    /// of historical rows predating the capture feature.
    pub captured_env: BTreeMap<String, String>,
    /// Per-iteration wall times in execution order (index = `run_idx`).
    /// Empty for single-run modes and for every row recorded before the
    /// v15->v16 schema bump - those walls were discarded at measure time and
    /// cannot be reconstructed from `elapsed_ms`.
    pub iterations: Vec<i64>,
    /// Per-iteration walls in microseconds, parallel to `iterations`. Empty
    /// unless every iteration of the row carries one - rows recorded before
    /// the v18->v19 schema bump, and loops that never measured microseconds.
    pub iterations_us: Vec<i64>,
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

/// The run recorded immediately before this one, as captured at record time.
///
/// Cheap answer to "what state was the machine in when this row was recorded".
/// A benchmark's wall time depends heavily on what ran just before it: a cell
/// reading a 92 GB file four minutes after another cell read the same file
/// finds the headers warm in the page cache and reports a wall that the code
/// had nothing to do with. Storing the predecessor makes that visible from the
/// row alone instead of requiring a reconstruction of the suite's ordering.
///
/// `input_file` is what it read, and the caller turns `timestamp_epoch` into a
/// gap - together they say whether this run could plausibly have inherited a
/// warm cache.
pub struct PreviousRun {
    pub uuid: String,
    pub command: String,
    pub input_file: Option<String>,
    /// The previous run's recorded timestamp as whole Unix seconds (SQLite
    /// converts the stored timestamp with `strftime('%s', ...)`). Deliberately
    /// raw rather than a pre-computed gap: rows are timestamped when they are
    /// *inserted*, i.e. when that run finished, and the gap that matters is
    /// measured to the moment the current run *started*. Computing it here
    /// against `now` would silently fold the current run's own duration into
    /// the answer.
    pub timestamp_epoch: i64,
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
    /// Substring matches against the row's invocation: `cli_args`,
    /// `brokkr_args`, or a captured env var rendered as `NAME=VALUE`. Like
    /// `git log --grep`, repeatable: each term must match the row (AND);
    /// within a term, any of the three sources can supply the hit.
    pub grep: Vec<String>,
    /// Negative substring matches against the same three invocation sources.
    /// A row is excluded when ANY term matches any source - the dual of
    /// [`grep`](Self::grep)'s AND, and the only way to select on the
    /// *absence* of a flag (e.g. the OFF arm of an A/B).
    pub grep_v: Vec<String>,
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
pub(crate) fn generate_uuid() -> Result<String, DevError> {
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
pub(crate) fn short_uuid(uuid: &str) -> String {
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
            elapsed_us: None,
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
            stop_marker_requested: String::new(),
            stop_marker: String::new(),
            kv: Vec::new(),
            captured_env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            iterations: Vec::new(),
            iterations_us: Vec::new(),
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
        let row_two_vars_collision =
            stored_row_with_env(&[("MALLOC_CONF", "dirty_decay_ms:0"), ("narenas:1", "")]);
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
