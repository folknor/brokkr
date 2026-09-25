
// ---------------------------------------------------------------------------
// Hooks for measurement loops that live outside this module
// ---------------------------------------------------------------------------

/// The pieces of the harness's own loops that a loop living elsewhere needs
/// to record the same provenance.
///
/// ratatoskr's sync bench (`ratatoskr_sync/bench_gate.rs`) drives its own
/// best-of-N loop - it keeps one sæhrimnir alive across iterations and ranks
/// by a marker span, neither of which `run_external_*` models. Without these
/// it could not stamp the measurement start (so `prev.gap_seconds` counted the
/// run's own duration) or build sidecar provenance (`run_info` was `None`),
/// because both are private to the harness. Exposed as narrow hooks rather
/// than by widening the private helpers, so the harness's loops keep sole
/// ownership of how they use them.
impl BenchHarness {
    /// Stamp the start of a measurement loop and return the epoch stamped,
    /// for [`run_info_for`](Self::run_info_for). Call once, before the first
    /// iteration - the same moment the harness's own loops stamp.
    pub(crate) fn begin_measurement(&self) -> i64 {
        let start_epoch = wall_clock_epoch();
        self.measure_start_epoch.set(start_epoch);
        start_epoch
    }

    /// Sidecar provenance for a run of `program` that began at `start_epoch`
    /// (from [`begin_measurement`](Self::begin_measurement)). `exit` is the
    /// last iteration's status, `None` for a clean finish (recorded as exit
    /// code 0, as the harness's loops do).
    pub(crate) fn run_info_for(
        &self,
        config: &BenchConfig,
        program: &Path,
        start_epoch: i64,
        pid: u32,
        exit: Option<&std::process::ExitStatus>,
    ) -> crate::db::sidecar::RunInfo {
        let exit_code = exit.map_or(0, exit_code_from_status);
        self.build_run_info(config, program, start_epoch, pid, Some(exit_code))
    }
}
