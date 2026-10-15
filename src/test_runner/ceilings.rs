//! Time bounds for test execution, from inner clocks to outer watchdogs.
use std::time::Duration;

/// Hard per-test cap. Lost lifecycle events may blur attribution but never
/// excuse a run with no completed test for longer than this bound.
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const MAX_TEST_TIMEOUT: Duration = Duration::from_secs(280);
/// No test in flight: bounds compilation, startup and teardown wedges.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub(crate) const SOURCE_PHASE_TIMEOUT: Duration = Duration::from_secs(2 * 60);
pub(crate) const BUILD_PHASE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub(crate) const TEST_PHASE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub(crate) const CHECK_CEILING: Duration = Duration::from_secs(25 * 60);
/// Standalone shared-harness backstop. The phase watchdog fires first in
/// check/test; this also bounds a runner used without a command watchdog.
pub(crate) const SWEEP_WALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(super) const WATCHDOG_POLL: Duration = Duration::from_millis(250);
pub(crate) const UNWIND_GRACE: Duration = Duration::from_secs(20);

/// Shorter source phases contain no tests; build phases can contain listings
/// and prebuilds. The timeout override must expire before the idle bound.
const _: () = {
    assert!(TEST_TIMEOUT.as_secs() < SOURCE_PHASE_TIMEOUT.as_secs());
    assert!(TEST_TIMEOUT.as_secs() < MAX_TEST_TIMEOUT.as_secs());
    assert!(MAX_TEST_TIMEOUT.as_secs() < IDLE_TIMEOUT.as_secs());
    assert!(IDLE_TIMEOUT.as_secs() <= BUILD_PHASE_TIMEOUT.as_secs());
    assert!(BUILD_PHASE_TIMEOUT.as_secs() < TEST_PHASE_TIMEOUT.as_secs());
    assert!(TEST_PHASE_TIMEOUT.as_secs() < CHECK_CEILING.as_secs());
    assert!(CHECK_CEILING.as_secs() < SWEEP_WALL_TIMEOUT.as_secs());
};

pub(crate) const fn phase_ceiling(phase: crate::cargo_env::Phase) -> Duration {
    use crate::cargo_env::Phase;
    match phase {
        Phase::Prepare | Phase::Test => TEST_PHASE_TIMEOUT,
        Phase::Clippy
        | Phase::Rustdoc
        | Phase::Coverage
        | Phase::InstallFeature
        | Phase::ScriptCheck => BUILD_PHASE_TIMEOUT,
        _ => SOURCE_PHASE_TIMEOUT,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn documented_per_test_bound_matches_the_runner() {
        let manual = include_str!("../../reference/check-internals.md");
        assert!(manual.contains(&format!(
            "| `check` parallel sweep | {}s |",
            super::TEST_TIMEOUT.as_secs()
        )));
    }
}
