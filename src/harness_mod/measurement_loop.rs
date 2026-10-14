/// A fixed timing policy for every iteration in one measurement.
#[derive(Clone, Copy)]
pub enum WallSource {
    External,
    SelfReported,
    Closure,
    /// All iterations must have the requested span. Missing markers fail the
    /// measurement rather than comparing a span against a whole-process wall.
    MarkerSpan { start: &'static str, end: &'static str },
}

/// Last start and first later end, preserving exact marker microseconds.
pub(crate) fn marker_span_us(markers: &[crate::sidecar::Marker], start: &str, end: &str) -> Option<i64> {
    let begin = markers.iter().rfind(|marker| marker.name == start)?;
    let finish = markers.iter().find(|marker|
        marker.name == end && marker.timestamp_us > begin.timestamp_us)?;
    finish.timestamp_us.checked_sub(begin.timestamp_us)
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
        Self { result: Some(result), wall: None, ending: None, sidecar: None,
            stdout: Vec::new(), stderr: Vec::new(), pid: None, error: None }
    }

    /// Attach observer failures without losing the captured streams or trajectory.
    pub fn fail(&mut self, error: DevError) {
        if self.error.is_none() { self.error = Some(error); }
    }
}

/// Ordered execution evidence and the first fastest successful iteration.
pub struct Measurement {
    pub iterations: Vec<Iteration>,
    pub winner: Option<usize>,
    start_epoch: i64,
    program: Option<PathBuf>,
    binary_xxh128: Option<String>,
    error: Option<DevError>,
    recorded: bool,
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
    where F: FnMut(usize, Option<&mut crate::sidecar::SidecarFifo>) -> Result<Iteration, DevError>,
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
        let mut measurement = Measurement { iterations: Vec::with_capacity(self.runs.get()),
            winner: None, start_epoch, program: program.map(Path::to_path_buf), binary_xxh128,
            error: None, recorded: false };
        for i in 0..self.runs.get() {
            announce_run(i, self.runs.get());
            self.harness.lock.set_progress(clamp_u32(i + 1), clamp_u32(self.runs.get()));
            if i > 0 && let Some(fifo) = &mut self.fifo
                && let Err(error) = fifo.reopen()
            {
                measurement.error = Some(error);
                break;
            }
            let mut iteration = match f(i, self.fifo.as_mut()) {
                Ok(iteration) => iteration,
                Err(error) => { measurement.error = Some(error); break; }
            };
            if iteration.error.is_none() {
                let timing = match self.source {
                    WallSource::Closure => Ok(()),
                    WallSource::External => match iteration.wall {
                        Some(wall) => {
                            if let Some(result) = &mut iteration.result {
                                result.elapsed_us = Some(elapsed_to_us(&wall));
                                result.elapsed_ms = elapsed_to_ms(&wall);
                            }
                            Ok(())
                        }
                        None => Err(DevError::Config("external timing requires a captured wall".into())),
                    },
                    WallSource::SelfReported => parse_kv_stderr(&iteration.stderr).map(|result| {
                        iteration.result = Some(result);
                    }),
                    WallSource::MarkerSpan { start, end } => {
                        let markers = iteration.sidecar.as_ref().map(|data| &data.markers);
                        let span = markers.and_then(|markers| marker_span_us(markers, start, end));
                        match span {
                            Some(us) => {
                                if let Some(result) = &mut iteration.result {
                                    result.elapsed_us = Some(us);
                                    result.elapsed_ms = us_to_ms(us);
                                    result.kv.push(KvPair::text("meta.timing_source", "marker_span"));
                                }
                                Ok(())
                            }
                            None => Err(DevError::Config(format!("missing {start} -> {end} marker span"))),
                        }
                    }
                };
                if let Err(error) = timing { iteration.fail(error); }
                if iteration.result.is_none() && iteration.error.is_none() {
                    iteration.fail(DevError::Config("iteration has no measured result".into()));
                }
            }
            let failed = iteration.error.is_some();
            if !failed {
                let candidate = iteration.result.as_ref().expect("successful iteration has a result");
                measurement.winner = Some(winning_index(measurement.winner.map(|winner|
                    (winner, measurement.iterations[winner].result.as_ref()
                        .expect("a winning iteration has a result"))), i, candidate));
            }
            measurement.iterations.push(iteration);
            if failed { break; }
        }
        measurement
    }
}

impl BenchHarness {
    /// Measure without storage. Pass all captured evidence back even on failure;
    /// `record` owns failure storage and returns the original error.
    pub fn measure<F>(&self, config: &BenchConfig, program: Option<&Path>, source: WallSource,
        sidecar: bool, f: F) -> Result<Measurement, DevError>
    where F: FnMut(usize, Option<&mut crate::sidecar::SidecarFifo>) -> Result<Iteration, DevError>,
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
        Ok(MeasurementLoop { harness: self, runs, source, fifo }.run(program, f))
    }

    /// Record the row before the sidecar, preserving evidence under dirty on
    /// any measurement, postprocessing or row failure.
    pub fn record<F>(&self, config: &BenchConfig, measurement: &mut Measurement, post: F)
        -> Result<BenchResult, DevError>
    where F: FnOnce(&mut BenchResult, &[Iteration]) -> Result<(), DevError>,
    {
        if measurement.recorded {
            return Err(DevError::Config("measurement was already recorded".into()));
        }
        measurement.recorded = true;
        self.measure_start_epoch.set(measurement.start_epoch);
        let error = measurement.error.take().or_else(||
            measurement.iterations.iter_mut().find_map(|iteration| iteration.error.take()));
        let index = if error.is_some() { measurement.iterations.len().saturating_sub(1) }
            else { measurement.winner.unwrap_or(0) };
        let info = measurement.program.as_deref().map(|program| {
            let iteration = measurement.iterations.get(index);
            self.build_run_info(config, program, measurement.start_epoch,
                measurement.binary_xxh128.clone(), iteration.and_then(|i| i.pid),
                iteration.and_then(|i| i.ending.clone()))
        });
        let walls: Vec<i64> = measurement.iterations.iter().filter_map(|i| i.result.as_ref())
            .map(|result| result.elapsed_ms).collect();
        let walls_us = all_or_none(measurement.iterations.iter().filter_map(|i| i.result.as_ref())
            .map(|result| result.elapsed_us).collect());
        let mut outcome = match error {
            Some(error) => Err(error),
            None => match measurement.iterations.get_mut(index).and_then(|i| i.result.take()) {
                Some(mut result) => {
                    result.iterations = walls;
                    post(&mut result, &measurement.iterations).map(|()| result)
                }
                None => Err(DevError::Config("measurement has no successful iteration".into())),
            },
        };
        let fired = measurement.iterations.get(index).and_then(|i| i.sidecar.as_ref())
            .and_then(|data| data.fired_stop_marker.as_deref());
        let uuid = match &outcome {
            Ok(result) => match self.record_measured(config, result, &walls_us, fired) {
                Ok(uuid) => uuid,
                Err(error) => { outcome = Err(error); None }
            },
            Err(_) => None,
        };
        let mut sidecar_index = 0;
        let mut sidecars = Vec::new();
        for (i, iteration) in measurement.iterations.iter().enumerate() {
            if let Some(data) = iteration.sidecar.as_ref() {
                if i == index { sidecar_index = sidecars.len(); }
                sidecars.push(data);
            }
        }
        if !sidecars.is_empty()
            && let Err(error) = self.store_sidecar(uuid.as_deref(), &sidecars, sidecar_index, info.as_ref())
        {
            if outcome.is_ok() { return Err(error); }
            console::warn(&format!("failed to preserve sidecar evidence: {error}"));
        }
        outcome
    }

    /// Capture one child under the loop's FIFO and the session's held lock.
    #[allow(clippy::too_many_arguments)]
    pub fn capture(&self, program: &Path, args: &[&str], cwd: &Path, extra_env: &[(&str, &str)],
        ok_codes: &[i32], fifo: &mut crate::sidecar::SidecarFifo, index: usize,
        ceiling: Option<Duration>) -> Result<Iteration, DevError>
    {
        let fifo_path = fifo.path_str()?.to_owned();
        let mut env = extra_env.to_vec();
        env.push((crate::sidecar::MARKER_FIFO_ENV, &fifo_path));
        let program_str = program.display().to_string();
        // Held until the sidecar has reaped the child: the grouped child leaves
        // brokkr's terminal process group, so signals must be bridged to it.
        let signal_guard = crate::shutdown::SigtermGuard::install();
        let start = std::time::Instant::now();
        let process = child::spawn_captured(&program_str, args, cwd, &env,
            crate::shutdown::ChildIsolation::ProcessGroup(&signal_guard),
            crate::output::SpawnProfile::Plain)?;
        let pid = process.id();
        self.lock.set_child_pid(pid);
        let capture = crate::sidecar::run_sidecar_with_deadline(process, fifo, index, start,
            self.stop_marker.as_deref(), ceiling, &crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES,
            &mut crate::sidecar::ProcSidecarSampler);
        drop(signal_guard);
        self.lock.clear_child_pid();
        let ending = capture.data.ending.clone();
        let error = match child::verdict(&ending, &program_str, &capture.stderr, ok_codes) {
            child::IterationVerdict::Counted => None,
            child::IterationVerdict::Failed(error) => Some(error),
            child::IterationVerdict::Interrupted => Some(DevError::Interrupted),
        };
        let (_, kv) = parse_kv_lines_us(&capture.stderr);
        Ok(Iteration {
            result: Some(BenchResult { elapsed_ms: elapsed_to_ms(&capture.elapsed),
                elapsed_us: Some(elapsed_to_us(&capture.elapsed)), kv, iterations: Vec::new(),
                distribution: None, hotpath: None }),
            wall: Some(capture.elapsed), ending: Some(ending), sidecar: Some(capture.data),
            stdout: capture.stdout, stderr: capture.stderr, pid: Some(pid), error,
        })
    }

    /// Capture an instrumented child. A missing or malformed report fails the
    /// iteration, while its output and sidecar remain available for diagnosis.
    #[allow(clippy::too_many_arguments)]
    pub fn capture_hotpath(&self, program: &Path, args: &[&str], cwd: &Path,
        extra_env: &[(&str, &str)], ok_codes: &[i32], fifo: &mut crate::sidecar::SidecarFifo,
        index: usize, ceiling: Option<Duration>) -> Result<Iteration, DevError>
    {
        let report = self.state.root()
            .join(crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.hotpath_report_filename);
        match std::fs::remove_file(&report) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(DevError::Io(error)),
        }
        let report_str = report.display().to_string();
        let mut env = extra_env.to_vec();
        env.extend_from_slice(&[("HOTPATH_METRICS_SERVER_OFF", "true"),
            ("HOTPATH_OUTPUT_FORMAT", "json"), ("HOTPATH_OUTPUT_PATH", &report_str)]);
        let mut iteration = self.capture(program, args, cwd, &env, ok_codes, fifo, index, ceiling)?;
        if iteration.error.is_none() {
            let profile = (|| {
                let text = std::fs::read_to_string(&report).map_err(|error|
                    DevError::Config(format!("cannot read hotpath report {}: {error}", report.display())))?;
                let value = serde_json::from_str(&text)
                    .map_err(|error| DevError::Config(format!("invalid hotpath JSON: {error}")))?;
                db::hotpath_data_from_json(&value)
                    .ok_or_else(|| DevError::Config("hotpath report has no profile".into()))
            })();
            match profile {
                Ok(profile) => {
                    iteration.result.as_mut()
                        .expect("an iteration without an error has a result")
                        .hotpath = Some(profile);
                }
                Err(error) => iteration.fail(error),
            }
        }
        std::fs::remove_file(report).ok();
        Ok(iteration)
    }
}
