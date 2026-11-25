//! Shared streaming runner for cargo libtest invocations.
//!
//! Public paths stay here as a facade; implementation lives by concern in
//! `context`, `events`, `outcome`, `plan`, `deadline`, `libtest_json`,
//! `procgroup`, `run`, `leader` (group signals and reaps), `evidence` (the
//! off-kill-path `/proc` collectors), `hung`, and `output` (the display-only
//! libtest text parser and compact failure rendering). `harness_shim` isolates each
//! cargo-launched harness and rustdoc stream; startup checks for its entry
//! point first (`maybe_run_harness_shim`).
//!
//! Every libtest lane reads the JSON event stream
//! (`-Z unstable-options --format json`), so nightly is required.

pub(crate) mod ceilings;
mod context;
mod deadline;
mod events;
mod evidence;
mod hung;
mod invocation;
mod leader;
mod libtest_json;
mod outcome;
mod output;
mod plan;
mod procgroup;
mod run;

#[path = "test_runner/harness_shim.rs"]
mod harness_shim;

pub(crate) use context::RunContext;
#[cfg(test)]
pub(crate) use context::StopSignal;
pub(crate) use deadline::{
    Attribution, Ceilings, HungTest, IDLE_TIMEOUT, TEST_TIMEOUT, TimeoutReason, WallShape,
};
pub(crate) use events::{
    AttributionRefusal, ExecutionFailure, KillCause, ObsEvent, Observation, ObservationSink,
    StreamEnd, StreamSource, StreamTap, TestResult, next_stream_id,
};
pub(crate) use harness_shim::is_session_dir_name;
pub(crate) use hung::{format_hung_test, is_bare_status_line};
pub(crate) use invocation::LibtestArgs;
use libtest_json::JsonReconstructor;
pub(crate) use outcome::{
    ExecutionOutcome, LaneVerdict, ProtocolIssue, StreamOutcome, lane_verdict,
};
pub(crate) use output::{
    Completeness, ParsedTestResults, filter_test, parse_test_output, parse_test_output_with_stderr,
    signal_stderr_tail,
};
pub(crate) use plan::{
    BuildGate, GATE_PENDING_BOUND, GateState, HashSite, LibraryDoctest, StrictPlan,
    canonical_package_dir, hash_file, hash_file_at, hash_report,
};
pub(crate) use procgroup::{Chunk, read_chunk};
pub(crate) use run::{
    Launch, Leader, LibtestInvocation, LibtestRun, Observe, ShimMode, effective_test_threads,
    run_libtest, run_listing,
};

// Internal paths shared only among runner modules and the harness shim.
use deadline::{
    DeadlineReading, Doorbell, Enforcement, TestTracker, WATCHDOG_POLL, candidates,
    enforce_deadline, watchdog_loop,
};
#[cfg(test)]
use deadline::{Expiry, watchdog_loop_with_timing};
#[cfg(test)]
use hung::sanitize_path_component;
use leader::LeaderHandle;
use libtest_json::drain_libtest_json;
#[cfg(test)]
use procgroup::DRAIN_GRACE;
#[cfg(test)]
use procgroup::direct_child_pids;
use procgroup::{
    clone_buffer, clone_hung, drain_stderr, drain_to_buffer, join_drains,
    spawn_cargo_process_group, spawn_process_group,
};

/// Run as the harness shim when this process was launched as one; `None`
/// otherwise. Checked before CLI parsing in `main`.
pub(crate) fn maybe_run_harness_shim() -> Option<i32> {
    harness_shim::maybe_run()
}

#[cfg(test)]
#[path = "test_runner/tests.rs"]
mod tests;
