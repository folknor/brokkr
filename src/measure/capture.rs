//! Capture measured children and their hotpath profiles.

use super::execution::Iteration;
use super::kv::parse_kv_lines_us;
use super::wall::{elapsed_to_ms, elapsed_to_us};
use super::{BenchHarness, BenchResult};
use crate::db;
use crate::error::DevError;
use crate::output::child;
use std::path::Path;
use std::time::Duration;

/// The metadata key (`meta.exit_code`) recording the exit status of a
/// counted iteration whose command accepts a non-zero answer code.
pub(crate) const EXIT_CODE_KEY: &str = "exit_code";

impl BenchHarness {
    /// Capture one child under the loop's FIFO and the session's held lock.
    #[allow(clippy::too_many_arguments)]
    pub fn capture(
        &self,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        extra_env: &[(&str, &str)],
        ok_codes: &[i32],
        fifo: &mut crate::sidecar::SidecarFifo,
        index: usize,
        ceiling: Option<Duration>,
    ) -> Result<Iteration, DevError> {
        let fifo_path = fifo.path_str()?.to_owned();
        let mut env = extra_env.to_vec();
        env.push((crate::sidecar::MARKER_FIFO_ENV, &fifo_path));
        let program_str = program.display().to_string();
        // Held until the sidecar has reaped the child: the grouped child leaves
        // brokkr's terminal process group, so signals must be bridged to it.
        let signal_guard = crate::shutdown::SigtermGuard::install();
        let start = std::time::Instant::now();
        let process = child::spawn_captured(
            &program_str,
            args,
            cwd,
            &env,
            crate::shutdown::ChildIsolation::ProcessGroup(&signal_guard),
            crate::output::SpawnProfile::Plain,
        )?;
        let pid = process.id();
        self.lock.set_child_pid(pid);
        let capture = crate::sidecar::run_sidecar_with_deadline(
            process,
            fifo,
            index,
            start,
            self.stop_marker.as_deref(),
            ceiling,
            &crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES,
            crate::output::child::CaptureCap::MEASUREMENT,
            &mut crate::sidecar::ProcSidecarSampler,
        );
        drop(signal_guard);
        self.lock.clear_child_pid();
        let ending = capture.data.ending.clone();
        let capture_failure = if capture.stdout_truncated {
            Some(crate::error::ChildFailureKind::CaptureLimit {
                stream: "stdout",
                limit: crate::output::child::CaptureCap::MEASUREMENT.stdout,
            })
        } else if capture.stderr_truncated {
            Some(crate::error::ChildFailureKind::CaptureLimit {
                stream: "stderr",
                limit: crate::output::child::CaptureCap::MEASUREMENT.stderr,
            })
        } else if capture.output_cut {
            Some(crate::error::ChildFailureKind::OutputDrain {
                grace: crate::output::child::ChildOptions::MEASUREMENT.drain_grace,
            })
        } else {
            None
        };
        let incomplete_output = capture_failure.is_some();
        let error = if matches!(&ending, child::Ending::Interrupted) {
            Some(DevError::Interrupted)
        } else if let Some(failure) = capture_failure {
            Some(DevError::ChildFailure {
                program: program_str.clone(),
                failure,
            })
        } else {
            match child::verdict(&ending, &program_str, &capture.stderr, ok_codes) {
                child::IterationVerdict::Counted => None,
                child::IterationVerdict::Failed(error) => Some(error),
                child::IterationVerdict::Interrupted => Some(DevError::Interrupted),
            }
        };
        let (_, mut kv) = if incomplete_output {
            (None, Vec::new())
        } else {
            parse_kv_lines_us(&capture.stderr)
        };
        // A command whose exit status is its answer (`diff` found differences,
        // `check` found violations) stores both answers as completed runs, so
        // the row says which one it measured. Commands without accepted
        // non-zero codes keep their rows as they were.
        if !ok_codes.is_empty()
            && error.is_none()
            && let Some(code) = ending.exit_code()
        {
            kv.push(db::KvPair::int(
                db::KvKey::metadata(EXIT_CODE_KEY),
                i64::from(code),
            ));
        }
        Ok(Iteration {
            result: Some(BenchResult {
                elapsed_ms: elapsed_to_ms(&capture.elapsed),
                elapsed_us: Some(elapsed_to_us(&capture.elapsed)),
                kv,
                iterations: Vec::new(),
                distribution: None,
                hotpath: None,
            }),
            wall: Some(capture.elapsed),
            ending: Some(ending),
            sidecar: Some(capture.data),
            stdout: capture.stdout,
            stderr: capture.stderr,
            pid: Some(pid),
            error,
        })
    }

    /// A fresh path for one instrumented child's hotpath report, unique per
    /// launch, so a child that outlived its run can never write the next
    /// run's report. A reserved launch declares it as an output
    /// (`Invocation::launch_with`) and removes it once released.
    pub(crate) fn new_hotpath_report_path(&self) -> Result<std::path::PathBuf, DevError> {
        let token = crate::lockfile::RunToken::mint()
            .ok_or_else(|| DevError::Internal("no entropy for a hotpath report name".into()))?;
        let name = crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.hotpath_report_filename;
        let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "json"));
        Ok(self
            .state
            .root()
            .join(format!("{stem}-{}.{ext}", token.as_str())))
    }

    /// Capture an instrumented child. A missing or malformed report fails the
    /// iteration, while its output and sidecar remain available for diagnosis.
    /// An unreserved launch: the report has one fixed name, cleared before and
    /// after, so a killed run leaves at most that one file; a reserved launch
    /// uses [`capture_hotpath_into`](Self::capture_hotpath_into) with a fresh
    /// name it declares.
    #[allow(clippy::too_many_arguments)]
    pub fn capture_hotpath(
        &self,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        extra_env: &[(&str, &str)],
        ok_codes: &[i32],
        fifo: &mut crate::sidecar::SidecarFifo,
        index: usize,
        ceiling: Option<Duration>,
    ) -> Result<Iteration, DevError> {
        let report = self
            .state
            .root()
            .join(crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.hotpath_report_filename);
        let stale = std::fs::remove_file(&report); // gate: brokkr's own state file
        match stale {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(DevError::Io(error)),
        }
        let iteration = self.capture_hotpath_into(
            program, args, cwd, extra_env, ok_codes, fifo, index, ceiling, &report,
        );
        std::fs::remove_file(&report).ok(); // gate: brokkr's own state file
        iteration
    }

    /// [`capture_hotpath`](Self::capture_hotpath) writing the report to
    /// `report`, a fresh path the caller removes afterwards.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture_hotpath_into(
        &self,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        extra_env: &[(&str, &str)],
        ok_codes: &[i32],
        fifo: &mut crate::sidecar::SidecarFifo,
        index: usize,
        ceiling: Option<Duration>,
        report: &Path,
    ) -> Result<Iteration, DevError> {
        let report_str = report.display().to_string();
        let mut env = extra_env.to_vec();
        env.extend_from_slice(&[
            ("HOTPATH_METRICS_SERVER_OFF", "true"),
            ("HOTPATH_OUTPUT_FORMAT", "json"),
            ("HOTPATH_OUTPUT_PATH", &report_str),
        ]);
        let mut iteration =
            self.capture(program, args, cwd, &env, ok_codes, fifo, index, ceiling)?;
        if iteration.error.is_none() {
            let profile = (|| {
                let text = std::fs::read_to_string(report).map_err(|error| {
                    DevError::Config(format!(
                        "cannot read hotpath report {}: {error}",
                        report.display()
                    ))
                })?;
                let value = serde_json::from_str(&text)
                    .map_err(|error| DevError::Config(format!("invalid hotpath JSON: {error}")))?;
                db::hotpath_data_from_json(&value)
                    .ok_or_else(|| DevError::Config("hotpath report has no profile".into()))
            })();
            match profile {
                Ok(profile) => {
                    iteration
                        .result
                        .as_mut()
                        .expect("an iteration without an error has a result")
                        .hotpath = Some(profile);
                }
                Err(error) => iteration.fail(error),
            }
        }
        Ok(iteration)
    }
}
