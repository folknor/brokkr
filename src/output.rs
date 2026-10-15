//! Console rendering and child-process supervision.
//!
//! The two halves live in `console` (prefixed messages, the quiet switch, the
//! run log, the status line) and `child` (the one supervisor every captured,
//! sidecar and passthrough child runs under, and its typed [`Ending`]). Both
//! are re-exported here, so `output::run_msg` and `output::run_captured` stay
//! the call paths. Operator messages use these printers, which preserve the
//! run log and clear/redraw a terminal status line. Operator output defaults to
//! stdout; sidecar query provenance uses stderr to keep stdout data clean, as
//! do late bookkeeping warnings after a possible JSON trailer. Forced shutdown
//! messages attempt logging and rendering with bounded mutex waits.
//!
//! Quiet progress is logged but hidden on the console. Errors, warnings,
//! requested test verdicts and user-facing lock/guard/stray actions stay visible.
//! `[result]` framing belongs to console, including unretrievable quiet results.
//! Data printers keep their exact streams and bytes with function-scoped Clippy
//! exceptions; raw captured streams can use `raw_stdout` / `raw_stderr` without
//! newline conversion. See `reference/output.md` for the channel contract.

pub(crate) mod child;
pub(crate) mod console;

pub(crate) use child::{
    CapturedOutput, DeadlineCapture, Ending, Replay, SpawnProfile, run_captured,
    run_captured_observed, run_captured_with_env, run_captured_with_env_and_deadline,
    run_captured_with_env_and_deadline_allow_output_cut,
    run_captured_with_env_and_deadline_with_options, run_captured_with_idle_deadline, run_cargo,
    run_cargo_unsetting, run_passthrough_in, run_passthrough_timed, run_streamed,
};
pub(crate) use console::*;
