//! Ingestion: run `nidhogg ingest <pbf> <data_dir>`.
//!
//! Replaces `ingest.sh`. Creates the output directory if needed, runs the
//! ingest command, and reports output file sizes on success.

use std::path::Path;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run `nidhogg ingest` to convert a PBF file into the nidhogg disk format.
pub fn run(
    binary: &Path,
    pbf_path: &Path,
    data_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let binary_str = super::client::path_str(binary)?;
    let pbf_str = super::client::path_str(pbf_path)?;
    let data_str = super::client::path_str(data_dir)?;

    // Ensure the output directory exists.
    std::fs::create_dir_all(data_dir)?;

    output::run_msg(&format!(
        "nidhogg ingest {} -> {}",
        pbf_path.display(),
        data_dir.display(),
    ));

    let captured = output::run_captured(binary_str, &["ingest", pbf_str, data_str], project_root)?;

    // Show stderr (progress output).
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    captured.check_success(binary_str)?;

    // List output files with sizes.
    list_output_files(data_dir)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// List files in the output directory with their sizes.
fn list_output_files(data_dir: &Path) -> Result<(), DevError> {
    let entries = std::fs::read_dir(data_dir)?;
    let mut files: Vec<(String, u64)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = std::fs::metadata(&path) {
            let name = entry.file_name().to_string_lossy().into_owned();
            files.push((name, meta.len()));
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));

    output::run_msg("output files:");
    for (name, size) in &files {
        let mb = *size as f64 / 1_048_576.0;
        output::run_msg(&format!("  {name}: {mb:.1} MB"));
    }

    Ok(())
}
