//! Iteration scheduling and execution evidence.

use super::kv::parse_kv_stderr;
use super::wall::{
    WallSource, elapsed_to_ms, elapsed_to_us, marker_span_us, us_to_ms, winning_index,
};
use super::{BenchConfig, BenchHarness, BenchResult};
use crate::db::KvPair;
use crate::error::DevError;
use crate::output::{child, console};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) fn clamp_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The line a harness loop prints before iteration `i` (zero-based) of `runs`.
/// Silent for a single run: "run 1/1" says nothing.
pub(super) fn run_progress_line(i: usize, runs: usize) -> Option<String> {
    (runs > 1).then(|| format!("run {}/{}", i + 1, runs))
}

/// Print [`run_progress_line`] as bench narration, if there is one.
pub(crate) fn announce_run(i: usize, runs: usize) {
    if let Some(line) = run_progress_line(i, runs) {
        console::bench_msg(&line);
    }
}

/// Current wall-clock time as seconds since the Unix epoch.
pub(super) fn wall_clock_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// One captured iteration, including evidence when its child or observer failed.
pub struct Iteration {
    /// Transferred to the returned aggregate when this iteration wins recording.
    pub result: Option<BenchResult>,
    /// Observed process wall; the fixed source writes the ranking wall on `result`.
    pub wall: Option<Duration>,
    pub ending: Option<child::Ending>,
    pub sidecar: Option<crate::sidecar::SidecarData>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub pid: Option<u32>,
    pub error: Option<DevError>,
}

impl Iteration {
    pub fn from_result(result: BenchResult) -> Self {
        Self {
            result: Some(result),
            wall: None,
            ending: None,
            sidecar: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            pid: None,
            error: None,
        }
    }

    /// Attach observer failures without losing the captured streams or trajectory.
    pub fn fail(&mut self, error: DevError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }
}

/// Ordered execution evidence and the first fastest successful iteration.
pub struct Measurement {
    pub iterations: Vec<Iteration>,
    pub winner: Option<usize>,
    pub(super) start_epoch: i64,
    pub(super) program: Option<PathBuf>,
    pub(super) binary_xxh128: Option<String>,
    pub(super) error: Option<DevError>,
    pub(super) recorded: bool,
}

/// Owns iteration count, timing policy, FIFO, progress and winner selection.
struct MeasurementLoop<'a> {
    harness: &'a BenchHarness,
    runs: std::num::NonZeroUsize,
    source: WallSource,
    fifo: Option<crate::sidecar::SidecarFifo>,
}

impl MeasurementLoop<'_> {
    fn run<F>(mut self, program: Option<&Path>, mut f: F) -> Measurement
    where
        F: FnMut(usize, Option<&mut crate::sidecar::SidecarFifo>) -> Result<Iteration, DevError>,
    {
        let start_epoch = wall_clock_epoch();
        // Capture binary identity before launching the first measured child;
        // record-time hashing could describe a replacement made during the run.
        let binary_xxh128 = program.and_then(|path| {
            crate::preflight::compute_file_digest(path)
                .ok()
                .map(|digest| digest.to_string())
        });
        self.harness.measure_start_epoch.set(start_epoch);
        let mut measurement = Measurement {
            iterations: Vec::with_capacity(self.runs.get()),
            winner: None,
            start_epoch,
            program: program.map(Path::to_path_buf),
            binary_xxh128,
            error: None,
            recorded: false,
        };
        let activity = crate::lockfile::activity::Activity::begin(format!(
            "measured runs{}",
            program
                .and_then(Path::file_name)
                .map(|name| format!(" of {}", name.to_string_lossy()))
                .unwrap_or_default()
        ));
        for i in 0..self.runs.get() {
            announce_run(i, self.runs.get());
            self.harness
                .lock
                .set_progress(clamp_u32(i + 1), clamp_u32(self.runs.get()));
            activity.progress(i as u64 + 1, self.runs.get() as u64);
            if i > 0
                && let Some(fifo) = &mut self.fifo
                && let Err(error) = fifo.reopen()
            {
                measurement.error = Some(error);
                break;
            }
            let mut iteration = match f(i, self.fifo.as_mut()) {
                Ok(iteration) => iteration,
                Err(error) => {
                    measurement.error = Some(error);
                    break;
                }
            };
            if iteration.error.is_none() {
                if let Err(error) = apply_wall_source(self.source, &mut iteration) {
                    iteration.fail(error);
                }
                if iteration.result.is_none() && iteration.error.is_none() {
                    iteration.fail(DevError::Config("iteration has no measured result".into()));
                }
            }
            let failed = iteration.error.is_some();
            if !failed {
                let candidate = iteration
                    .result
                    .as_ref()
                    .expect("successful iteration has a result");
                measurement.winner = Some(winning_index(
                    measurement.winner.map(|winner| {
                        (
                            winner,
                            measurement.iterations[winner]
                                .result
                                .as_ref()
                                .expect("a winning iteration has a result"),
                        )
                    }),
                    i,
                    candidate,
                ));
            }
            measurement.iterations.push(iteration);
            if failed {
                break;
            }
        }
        measurement
    }
}

/// Fill the iteration's measured result from the timing source the loop was
/// configured with.
fn apply_wall_source(source: WallSource, iteration: &mut Iteration) -> Result<(), DevError> {
    match source {
        WallSource::Closure => Ok(()),
        WallSource::External => match iteration.wall {
            Some(wall) => {
                if let Some(result) = &mut iteration.result {
                    result.elapsed_us = Some(elapsed_to_us(&wall));
                    result.elapsed_ms = elapsed_to_ms(&wall);
                }
                Ok(())
            }
            None => Err(DevError::Config(
                "external timing requires a captured wall".into(),
            )),
        },
        WallSource::SelfReported => parse_kv_stderr(&iteration.stderr).map(|result| {
            iteration.result = Some(result);
        }),
        WallSource::MarkerSpan { start, end } => {
            let span = iteration
                .sidecar
                .as_ref()
                .and_then(|data| marker_span_us(data.markers(), start, end));
            match span {
                Some(us) => {
                    if let Some(result) = &mut iteration.result {
                        result.elapsed_us = Some(us);
                        result.elapsed_ms = us_to_ms(us);
                        result
                            .kv
                            .push(KvPair::text("meta.timing_source", "marker_span"));
                    }
                    Ok(())
                }
                None => Err(DevError::Config(format!(
                    "missing {start} -> {end} marker span"
                ))),
            }
        }
    }
}

impl BenchHarness {
    /// Measure without storage. Pass all captured evidence back even on failure;
    /// `record` owns failure storage and returns the original error.
    pub fn measure<F>(
        &self,
        config: &BenchConfig,
        program: Option<&Path>,
        source: WallSource,
        sidecar: bool,
        f: F,
    ) -> Result<Measurement, DevError>
    where
        F: FnMut(usize, Option<&mut crate::sidecar::SidecarFifo>) -> Result<Iteration, DevError>,
    {
        let runs = std::num::NonZeroUsize::new(config.runs)
            .ok_or_else(|| DevError::Config("benchmark requires at least 1 run".into()))?;
        let fifo = if sidecar {
            Some(crate::sidecar::SidecarFifo::create(
                self.state.root(),
                &crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES,
            )?)
        } else {
            None
        };
        Ok(MeasurementLoop {
            harness: self,
            runs,
            source,
            fifo,
        }
        .run(program, f))
    }
}
