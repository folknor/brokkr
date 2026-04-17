// ---------------------------------------------------------------------------
// Public formatting API
// ---------------------------------------------------------------------------
//
// Formatting for results DB rows is split across four submodules:
//
//   * `table`   — the column-aligned summary table (`format_table`) plus
//                 shared helpers (`format_input`, `compute_rewrite_pct`,
//                 `find_output_bytes`, `format_blob_counts`) that
//                 `compare` reuses. `format_elapsed` is used by `table`
//                 and `single` only.
//   * `single`  — the standalone labelled block for a single result
//                 (`format_single_result`).
//   * `details` — the compact detail block used as subheading under table
//                 rows (`format_details`).
//   * `compare` — side-by-side two-commit comparison (`format_compare`).

mod compare;
mod details;
mod single;
mod table;

pub use compare::format_compare;
pub use details::format_details;
pub use single::format_single_result;
pub use table::format_table;
