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
pub fn run(binary: &Path, args: &[String], project_root: &Path) -> Result<(), DevError> {
    let binary_str = crate::utf8_path::path_str(binary)?;

    output::run_msg(&format!("nidhogg-update {}", args.join(" "),));

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let captured = output::run_streamed(binary_str, &arg_refs, project_root, output::Replay::Both)?;

    captured.check_success(binary_str)?;

    Ok(())
}
