//! Run nidhogg-update with passthrough arguments.
//!
//! Replaces `update.sh`. The caller is responsible for building the
//! `nidhogg-update` binary before calling this function.

use std::path::Path;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the `nidhogg-update` binary with the given arguments.
pub fn run(
    binary: &Path,
    args: &[String],
    project_root: &Path,
) -> Result<(), DevError> {
    let binary_str = binary
        .to_str()
        .ok_or_else(|| DevError::Config("binary path is not valid UTF-8".into()))?;

    output::run_msg(&format!(
        "nidhogg-update {}",
        args.join(" "),
    ));

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let captured = output::run_captured(binary_str, &arg_refs, project_root)?;

    // Show stdout.
    let stdout = String::from_utf8_lossy(&captured.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }

    // Show stderr.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    if !captured.status.success() {
        return Err(DevError::Subprocess {
            program: binary_str.to_owned(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    Ok(())
}
