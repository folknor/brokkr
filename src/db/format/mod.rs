// ---------------------------------------------------------------------------
// Public formatting API
// ---------------------------------------------------------------------------
//
// Formatting for results DB rows is split across four submodules:
//
//   * `table`   - the column-aligned summary table (`format_table`) plus
//                 shared helpers (`format_input`, `compute_rewrite_pct`,
//                 `find_output_bytes`, `format_blob_counts`) that
//                 `compare` reuses, and `format_wall` / `format_us_as_ms`,
//                 the one wall rendering every view shares (microseconds
//                 when the row has them).
//   * `single`  - the standalone labelled block for a single result
//                 (`format_single_result`).
//   * `details` - the compact detail block used as subheading under table
//                 rows (`format_details`).
//   * `compare` - side-by-side two-commit comparison (`format_compare`).

mod compare;
mod dataset;
mod details;
pub(crate) mod library;
mod single;
mod table;

pub use compare::{Sides, format_compare_with_limit};
pub use dataset::DatasetMatcher;
pub use details::format_details;
pub use single::format_single_result;
pub use table::format_table;

/// A stored row with neutral defaults, for the view tests: only the fields a
/// pairing or label question turns on are arguments.
#[cfg(test)]
pub(crate) fn fixture_row(
    command: &str,
    mode: &str,
    input_file: &str,
    elapsed_ms: i64,
) -> super::StoredRow {
    super::StoredRow {
        id: 0,
        timestamp: String::from("2026-03-01 00:00:00"),
        hostname: String::from("testhost"),
        commit: "aabbccdd".parse().expect("fixture commit is valid"),
        subject: String::from("test commit"),
        command: command.to_owned(),
        mode: mode.to_owned(),
        input_file: input_file.to_owned(),
        input_mb: None,
        elapsed: super::Wall::LegacyMillis(elapsed_ms),
        cargo_features: String::new(),
        cargo_profile: Some(super::types::StoredProfile::Known(
            crate::build::BuildKind::default(),
        )),
        kernel: String::new(),
        cpu_governor: String::new(),
        avail_memory_mb: None,
        storage_notes: String::new(),
        peak_rss_mb: None,
        uuid: String::from("abcdef1234567890"),
        cli_args: String::new(),
        brokkr_args: String::new(),
        arm_key: None,
        project: String::from("test"),
        stop_marker_requested: String::new(),
        stop_marker: String::new(),
        kv: vec![],
        captured_env: std::collections::BTreeMap::new(),
        iterations: Vec::new(),
        distribution: None,
        hotpath: None,
    }
}
