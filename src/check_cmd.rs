mod activation;
mod conventions;
mod verdict;

pub(crate) use activation::decide_active_sweeps;
use activation::{build_clippy_sweep, effective_profile_name};
use conventions::{
    report_script_stage, run_dependency_rules, run_gremlins, run_header, run_manifest,
    run_publish_cycle, run_script_checks, run_textlint, scope_order,
};

use crate::test_runner::hash_file;
pub(crate) use verdict::TestTiming;
#[cfg(test)]
use verdict::verdict_context;
use verdict::{emit_timings, finish_check, fmt_wall, run_stop};

include!("check_cmd/phase.rs");
include!("check_cmd/concurrency.rs");
include!("check_cmd/features.rs");
include!("check_cmd/selection.rs");
include!("check_cmd/output.rs");
include!("check_cmd/isolate.rs");
include!("check_cmd/binary_timings.rs");
include!("check_cmd/parallel.rs");
include!("check_cmd/prepare.rs");
include!("check_cmd/accounting.rs");
include!("check_cmd/continuation.rs");
include!("check_cmd/coverage.rs");
include!("check_cmd/install_shape.rs");
include!("check_cmd/binaries.rs");
include!("check_cmd/direct_runtime.rs");
include!("check_cmd/nextest.rs");
include!("check_cmd/nextest_lane.rs");
include!("check_cmd/watchdog.rs");
include!("check_cmd/report.rs");
