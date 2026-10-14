mod activation;
pub(crate) mod selection;
use crate::output::format_duration as fmt_wall;
mod conventions;
mod reporter;
mod verdict;

pub(crate) use activation::decide_active_sweeps;
use activation::{build_clippy_sweep, effective_profile_name};
use conventions::{
    LazyTree, report_script_stage, run_dependency_rules, run_gremlins, run_header, run_manifest,
    run_publish_cycle, run_script_checks, run_textlint, scope_order,
};
use reporter::Reporter;

use crate::test_runner::hash_file;
pub(crate) use verdict::TestTiming;
#[cfg(test)]
use verdict::verdict_context;
use verdict::{FinishInput, emit_timings, finish_check, run_stop};

include!("check_cmd/phase_type.rs");
include!("check_cmd/phase.rs");
include!("check_cmd/diagnostics.rs");
include!("check_cmd/clippy_cmd.rs");
include!("check_cmd/test_phase.rs");
include!("check_cmd/concurrency.rs");
pub(crate) use crate::build_purpose::environment::{
    isolated_target_dir, merged_env, sweep_cargo_env, sweep_runtime_env,
};
#[cfg(test)]
pub(crate) use crate::build_purpose::environment::build_test_env;
#[cfg(test)]
use crate::build_purpose::features;
pub(crate) use crate::build_purpose::features::FeatureOracle;
#[cfg(test)]
pub(crate) use crate::build_purpose::features::feature_fixture;
pub(crate) use crate::build_purpose::requests::SupportRequest;
pub(crate) use selection::{
    AdmissionNote, AdmissionRule, Attempt, CheckSelections, DeclaredShape, ForwardedArgs,
    InstallSelection, LaneEntry, PackageSource, PhaseSelection, ReachLedger, ReproLine, Resolution,
    ResolutionRun, Selection, SelectionPhase, ShapeId, feature_drop_lines, join_notes,
    reject_direct_execution_args, reject_forwarded_package_selectors, resolve_invocation_sweeps,
    resolve_planned_sweep,
};
#[cfg(test)]
pub(crate) use selection::{package_args, resolve_unification};
include!("check_cmd/test_execution.rs");
include!("check_cmd/test_execution_tests.rs");
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
