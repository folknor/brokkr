//! Shared streaming runner for cargo libtest invocations.
//!
//! Public paths stay here as a facade; implementation lives by concern in
//! `events`, `plan`, `deadline`, `libtest_json`, `procgroup`, `run`, and `hung`.

mod deadline;
mod events;
mod hung;
mod libtest_json;
mod plan;
mod procgroup;
mod run;

#[path = "test_runner/harness_shim.rs"]
mod harness_shim;

pub(crate) use deadline::{
    Attribution, Ceilings, HungTest, IDLE_TIMEOUT, PARALLEL_SWEEP_TIMEOUT, TEST_TIMEOUT,
    TimeoutReason, WallShape,
};
pub(crate) use events::{
    KillCause, ObsEvent, Observation, ObservationSink, StreamEnd, StreamSource, StreamTap,
    TestResult, next_stream_id,
};
pub(crate) use hung::{format_hung_test, is_bare_status_line};
use libtest_json::JsonReconstructor;
pub(crate) use plan::{StrictPlan, hash_file};
pub(crate) use procgroup::{Chunk, read_chunk, read_unless_cancelled};
pub(crate) use run::{
    Launch, Leader, LibtestOutcome, Observe, ShimMode, effective_test_threads, run_libtest_parallel, run_listing, streaming_run_libtest,
};

// Internal paths shared only among runner modules and the harness shim.
#[cfg(test)]
use deadline::{Expiry, watchdog_loop_with_timing};
use deadline::{DeadlineReading, TestTracker, WATCHDOG_POLL, enforce_deadline, watchdog_loop};
use hung::capture_hung_test;
#[cfg(test)]
use hung::sanitize_path_component;
use libtest_json::drain_libtest_json;
#[cfg(test)]
use procgroup::DRAIN_GRACE;
use procgroup::{
    clone_buffer, clone_hung, direct_child_pids, drain_stderr, drain_to_buffer,
    join_drains, kill_process_group, spawn_cargo_process_group, spawn_process_group,
    stop_process_group,
};

/// Run as the harness shim when this process was launched as one; `None`
/// otherwise. Checked before CLI parsing in `main`.
pub(crate) fn maybe_run_harness_shim() -> Option<i32> {
    harness_shim::maybe_run()
}

#[cfg(test)]
#[path = "test_runner/tests.rs"]
mod tests;
