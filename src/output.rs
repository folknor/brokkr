//! Console rendering and child-process supervision.
//!
//! The two halves live in `console` (prefixed messages, the quiet switch, the
//! run log, the status line) and `child` (the one supervisor every captured,
//! sidecar and passthrough child runs under, and its typed [`Ending`]). Both
//! are re-exported here, so `output::run_msg` and `output::run_captured` stay
//! the call paths.

pub(crate) mod child;
pub(crate) mod console;

pub(crate) use child::{
    CapturedOutput, DeadlineCapture, Ending, SpawnProfile, run_captured, run_captured_observed,
    run_captured_with_env, run_captured_with_env_and_deadline,
    run_captured_with_env_and_deadline_allow_output_cut,
    run_captured_with_env_and_deadline_with_options, run_captured_with_idle_deadline, run_cargo,
    run_passthrough_in, run_passthrough_timed,
};
pub(crate) use console::*;
