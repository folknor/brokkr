//! Compare feature counts between two PMTiles archives.
//!
//! Replaces `compare-tiles.sh`. Builds the `compare_tiles` example and runs it
//! with passthrough output.

use std::path::Path;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(
    target_dir: &Path,
    project_root: &Path,
    file_a: &str,
    file_b: &str,
    sample: Option<usize>,
) -> Result<(), DevError> {
    // Build the example.
    output::build_msg("cargo build --release --example compare_tiles");

    let captured = output::run_captured(
        "cargo",
        &[
            "build",
            "--release",
            "--example",
            "compare_tiles",
        ],
        project_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!(
            "compare_tiles build failed: {stderr}"
        )));
    }

    // Run the example binary.
    let binary = target_dir.join("release/examples/compare_tiles");
    if !binary.exists() {
        return Err(DevError::Build(format!(
            "compare_tiles binary not found at {}",
            binary.display()
        )));
    }

    let mut args: Vec<String> = vec![file_a.into(), file_b.into()];
    if let Some(n) = sample {
        args.push("--sample".into());
        args.push(n.to_string());
    }

    output::bench_msg(&format!("compare_tiles: {file_a} vs {file_b}"));

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let binary_str = binary.display().to_string();

    let captured = output::run_captured(
        &binary_str,
        &args_refs,
        project_root,
    )?;

    // Print stdout.
    let stdout = String::from_utf8_lossy(&captured.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }

    // Print stderr.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    if !captured.status.success() {
        return Err(DevError::Subprocess {
            program: binary_str,
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    Ok(())
}
