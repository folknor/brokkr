//! Measured row construction, reporting and sidecar persistence.

use super::execution::{Iteration, Measurement, wall_clock_epoch};
use super::wall::{all_or_none, bench_ordering_key, result_wall};
use super::{BenchConfig, BenchHarness, BenchResult};
use crate::build::CargoProfile;
use crate::config::DriveConfig;
use crate::db::{KvKey, KvPair, KvValue, ResultsDb, RunRow, Wall};
use crate::error::DevError;
use crate::git::GitInfo;
use crate::output::{child, console};
use crate::sidecar_backup::backup_sidecar;
use std::path::Path;

/// Build a result summary string with key=value pairs.
///
/// `mode` is the session's measurement mode, shared by rows and sidecar records.
pub(super) fn format_result_line(
    config: &BenchConfig,
    mode: Option<&str>,
    result: &BenchResult,
    git: &GitInfo,
) -> String {
    let mut parts = Vec::with_capacity(8);
    parts.push(format!("command={}", config.command));

    if let Some(v) = mode {
        parts.push(format!("mode={v}"));
    }

    // Print the exact figure when the target reported one - rounding a 6.847
    // ms region to `elapsed_ms=7` on the console would hide precisely the
    // signal a sub-millisecond workload is being measured for.
    parts.push(format!(
        "elapsed_ms={}",
        ms_value(result_wall(result.elapsed_ms, result.elapsed_us))
    ));
    parts.push(format!("commit={}", git.commit));

    if let Some(ref input) = config.input_file {
        parts.push(format!("input={input}"));
    }

    append_kv_fields(&mut parts, &result.kv);

    // Compute I/O throughput when input size and elapsed time are known -
    // from the microsecond wall when there is one, so a run that rounds to
    // 0 ms still gets a figure instead of silently skipping the line.
    let wall_us = bench_ordering_key(result);
    if let Some(input_mb) = config.input_mb
        && wall_us > 0
    {
        #[allow(clippy::cast_precision_loss)]
        let secs = wall_us as f64 / 1_000_000.0;
        let read_mbs = input_mb / secs;
        parts.push(format!("read_mbs={read_mbs:.1}"));
        if let Some(output_bytes) = find_kv_int(&result.kv, "output_bytes") {
            #[allow(clippy::cast_precision_loss)]
            let output_mb = output_bytes as f64 / 1_000_000.0;
            let write_mbs = output_mb / secs;
            parts.push(format!("write_mbs={write_mbs:.1}"));
        }
    }

    if let Some(ref dist) = result.distribution {
        parts.push(format!("samples={}", dist.samples));
        parts.push(format!("min_ms={}", ms_value(dist.min)));
        parts.push(format!("p50_ms={}", ms_value(dist.p50)));
        parts.push(format!("p95_ms={}", ms_value(dist.p95)));
        parts.push(format!("max_ms={}", ms_value(dist.max)));
    }

    parts.join("  ")
}

/// A millisecond value for a `[result]` field: three decimals from the
/// microsecond reading when there is one (`0.312`), else the integer.
pub(super) fn ms_value(wall: Wall) -> String {
    match wall.micros() {
        Some(us) => {
            #[allow(clippy::cast_precision_loss)]
            let exact = us as f64 / 1000.0;
            format!("{exact:.3}")
        }
        None => wall.ms().to_string(),
    }
}

/// Emit a `[result]` line (respects quiet mode).
pub(super) fn emit_result_lines(
    config: &BenchConfig,
    mode: Option<&str>,
    result: &BenchResult,
    git: &GitInfo,
) {
    console::result_msg(&format_result_line(config, mode, result, git));
}

/// Emit a `[result]` line unconditionally (ignores quiet mode).
/// Used for results that can't be looked up later: a dirty tree, or a
/// clean one whose results.db insert failed.
pub(super) fn force_emit_result_lines(
    config: &BenchConfig,
    mode: Option<&str>,
    result: &BenchResult,
    git: &GitInfo,
) {
    console::result_forced(&format_result_line(config, mode, result, git));
}

/// Look up an integer KV pair by key.
pub(super) fn find_kv_int(kv: &[KvPair], key: &str) -> Option<i64> {
    kv.iter()
        .find(|p| p.key == key)
        .and_then(|p| match &p.value {
            KvValue::Int(v) => Some(*v),
            _ => None,
        })
}

/// Flatten key-value pairs into the result line.
pub(super) fn append_kv_fields(parts: &mut Vec<String>, kv: &[KvPair]) {
    for pair in kv {
        parts.push(format!("{}={}", pair.key, pair.value));
    }
}

/// The single `[sidecar]` line a stored profile prints: the best run's
/// sample, marker and counter counts (with the run number when there were
/// several). `dirty` marks a trajectory filed under the `dirty` alias because
/// no results row exists for it; only then is the id named here, since
/// otherwise it is the results row's id, already printed.
pub(super) fn sidecar_summary_line(
    short: &str,
    dirty: bool,
    runs: &[&crate::sidecar::SidecarData],
    best_run_idx: usize,
) -> String {
    let which = if runs.len() > 1 {
        format!("best run {}/{}: ", best_run_idx + 1, runs.len())
    } else {
        String::new()
    };
    let counts = runs.get(best_run_idx).map_or_else(String::new, |d| {
        format!(
            "{which}{} samples, {} markers, {} counters{} ",
            d.samples.len(),
            d.markers.len(),
            d.counters.len(),
            if d.summary.capture_truncated {
                " (capture limited)"
            } else {
                ""
            }
        )
    });
    // A profile with a results row shares its id, which the `results.db` line
    // already printed; only a `dirty`-filed one needs its own.
    if dirty {
        format!("{counts}(sidecar.db {short}, filed as dirty)")
    } else {
        format!("{counts}(sidecar.db)")
    }
}

/// Build a storage notes string from the drive configuration.
pub(super) fn format_storage_notes(drives: &Option<DriveConfig>) -> Option<String> {
    let drives = drives.as_ref()?;

    let mut parts = Vec::with_capacity(4);
    push_drive_note(&mut parts, "source", &drives.source);
    push_drive_note(&mut parts, "data", &drives.data);
    push_drive_note(&mut parts, "scratch", &drives.scratch);
    push_drive_note(&mut parts, "target", &drives.target);

    if parts.is_empty() {
        return None;
    }

    Some(parts.join(", "))
}

/// Append a "label=value" note if the drive field is present.
pub(super) fn push_drive_note(parts: &mut Vec<String>, label: &str, value: &Option<String>) {
    if let Some(v) = value {
        parts.push(format!("{label}={v}"));
    }
}

impl BenchHarness {
    /// Record the row before the sidecar, preserving evidence under dirty on
    /// any measurement, postprocessing or row failure.
    pub fn record<F>(
        &self,
        config: &BenchConfig,
        measurement: &mut Measurement,
        post: F,
    ) -> Result<BenchResult, DevError>
    where
        F: FnOnce(&mut BenchResult, &[Iteration]) -> Result<(), DevError>,
    {
        if measurement.recorded {
            return Err(DevError::Config("measurement was already recorded".into()));
        }
        measurement.recorded = true;
        self.measure_start_epoch.set(measurement.start_epoch);
        let error = measurement.error.take().or_else(|| {
            measurement
                .iterations
                .iter_mut()
                .find_map(|iteration| iteration.error.take())
        });
        let index = if error.is_some() {
            measurement.iterations.len().saturating_sub(1)
        } else {
            measurement.winner.unwrap_or(0)
        };
        let info = measurement.program.as_deref().map(|program| {
            let iteration = measurement.iterations.get(index);
            self.build_run_info(
                config,
                program,
                measurement.start_epoch,
                measurement.binary_xxh128.clone(),
                iteration.and_then(|i| i.pid),
                iteration.and_then(|i| i.ending.clone()),
            )
        });
        let walls: Vec<i64> = measurement
            .iterations
            .iter()
            .filter_map(|i| i.result.as_ref())
            .map(|result| result.elapsed_ms)
            .collect();
        let walls_us = all_or_none(
            measurement
                .iterations
                .iter()
                .filter_map(|i| i.result.as_ref())
                .map(|result| result.elapsed_us)
                .collect(),
        );
        let mut outcome = match error {
            Some(error) => Err(error),
            None => match measurement
                .iterations
                .get_mut(index)
                .and_then(|i| i.result.take())
            {
                Some(mut result) => {
                    result.iterations = walls;
                    post(&mut result, &measurement.iterations).map(|()| result)
                }
                None => Err(DevError::Config(
                    "measurement has no successful iteration".into(),
                )),
            },
        };
        let fired = measurement
            .iterations
            .get(index)
            .and_then(|i| i.sidecar.as_ref())
            .and_then(|data| data.fired_stop_marker.as_deref());
        let uuid = match &outcome {
            Ok(result) => match self.record_measured(config, result, &walls_us, fired) {
                Ok(uuid) => uuid,
                Err(error) => {
                    outcome = Err(error);
                    None
                }
            },
            Err(_) => None,
        };
        let mut sidecar_index = 0;
        let mut sidecars = Vec::new();
        for (i, iteration) in measurement.iterations.iter().enumerate() {
            if let Some(data) = iteration.sidecar.as_ref() {
                if i == index {
                    sidecar_index = sidecars.len();
                }
                sidecars.push(data);
            }
        }
        if !sidecars.is_empty()
            && let Err(error) =
                self.store_sidecar(uuid.as_deref(), &sidecars, sidecar_index, info.as_ref())
        {
            if outcome.is_ok() {
                return Err(error);
            }
            console::warn(&format!("failed to preserve sidecar evidence: {error}"));
        }
        outcome
    }
}

impl BenchHarness {
    /// Write and report the winning row; `record` owns sidecar persistence.
    pub(super) fn record_measured(
        &self,
        config: &BenchConfig,
        result: &BenchResult,
        iterations_us: &[i64],
        fired_stop_marker: Option<&str>,
    ) -> Result<Option<String>, DevError> {
        let mode = self.effective_mode();
        if !self.store_results {
            force_emit_result_lines(config, mode.as_deref(), result, &self.git);
            return Ok(None);
        }
        if self.git.is_clean {
            let row = self.build_row(config, result, iterations_us, fired_stop_marker)?;
            let (uuid, short) = match self.db.insert(&row) {
                Ok(pair) => pair,
                Err(e) => {
                    force_emit_result_lines(config, mode.as_deref(), result, &self.git);
                    console::error(&format!(
                        "results.db insert failed - result not stored: {e}"
                    ));
                    return Err(e);
                }
            };
            emit_result_lines(config, mode.as_deref(), result, &self.git);
            // The one line naming the stored row's id (also the sidecar
            // profile's). Not quiet-gated: it is how the run is found again.
            console::run_msg(&format!("results.db {short}"));
            Ok(Some(uuid))
        } else {
            // Dirty tree: no DB insert, no UUID. Always print result line
            // since the data can't be looked up later.
            force_emit_result_lines(config, mode.as_deref(), result, &self.git);
            Ok(None)
        }
    }

    /// The measurement mode a row, `[result]` line and sidecar record
    /// carry: the session's `measure_mode`, set from the request.
    pub fn effective_mode(&self) -> Option<String> {
        self.store_results
            .then(|| self.measure_mode.label().to_owned())
    }

    /// Build a `RunInfo` from harness state for sidecar provenance.
    pub(super) fn build_run_info(
        &self,
        config: &BenchConfig,
        program: &Path,
        start_epoch: i64,
        binary_xxh128: Option<String>,
        pid: Option<u32>,
        ending: Option<child::Ending>,
    ) -> crate::db::sidecar::RunInfo {
        crate::db::sidecar::RunInfo {
            run_start_epoch: Some(start_epoch),
            pid: pid.map(i64::from),
            command: Some(config.command.clone()),
            binary_path: Some(program.display().to_string()),
            binary_xxh128,
            git_commit: Some(self.git.commit.clone()),
            mode: self.effective_mode(),
            dataset: config.input_file.clone(),
            ending,
        }
    }

    /// Store sidecar data in the separate sidecar.db.
    ///
    /// `best_run_idx` records which of the N runs produced the reported
    /// elapsed_ms, so `brokkr sidecar <uuid>` defaults to the right run.
    ///
    /// `run_info` carries provenance metadata (timestamp, binary hash, git
    /// commit, mode, dataset, ending) so that `brokkr sidecar dirty`
    /// can identify exactly which run produced the sidecar data.
    ///
    /// When `uuid` is `None` (dirty tree or failed run), generates a fresh
    /// UUID and updates the `dirty` latest pointer so `brokkr results dirty`
    /// always finds the most recent unstored run.
    pub(super) fn store_sidecar(
        &self,
        uuid: Option<&str>,
        sidecar_runs: &[&crate::sidecar::SidecarData],
        best_run_idx: usize,
        run_info: Option<&crate::db::sidecar::RunInfo>,
    ) -> Result<(), DevError> {
        if !self.store_results {
            return Ok(());
        }

        let sidecar_db_path = self.state.sidecar_db();
        let sidecar_db = crate::db::sidecar::SidecarDb::open(&sidecar_db_path)?;

        let (store_uuid, is_dirty) = match uuid {
            Some(u) => (u.to_owned(), false),
            None => {
                // Generate a unique UUID for this dirty/failed run.
                (crate::db::generate_uuid()?, true)
            }
        };

        for (i, data) in sidecar_runs.iter().enumerate() {
            sidecar_db.store_run(&store_uuid, i, data)?;
        }

        sidecar_db.store_meta(
            &store_uuid,
            best_run_idx,
            sidecar_runs.len(),
            run_info,
            is_dirty,
        )?;

        if is_dirty {
            sidecar_db.set_latest("dirty", &store_uuid)?;
        }

        // The one sidecar summary line: what was stored (and the id, only for
        // a `dirty`-filed profile - otherwise the results.db line named it).
        // Per-run attach and sample counts are run-log detail (the sidecar run);
        // the wall time is on the `[result]` line.
        let short = &store_uuid[..8.min(store_uuid.len())];
        console::sidecar_msg(&sidecar_summary_line(
            short,
            is_dirty,
            sidecar_runs,
            best_run_idx,
        ));

        // Close the writer before backup. The backup API reads the logical
        // DB state regardless, but closing avoids holding two connections.
        drop(sidecar_db);

        // Back up sidecar DB while the lock is still held.
        if let Err(e) = backup_sidecar(&sidecar_db_path, &self.project) {
            // A warning, not narration: the quiet-gated sidecar line hid a
            // real loss of the backup.
            console::warn(&format!("sidecar backup failed (non-fatal): {e}"));
        }

        Ok(())
    }

    /// Build a `RunRow` from harness state, config, and result.
    pub(super) fn build_row(
        &self,
        config: &BenchConfig,
        result: &BenchResult,
        iterations_us: &[i64],
        fired_stop_marker: Option<&str>,
    ) -> Result<RunRow, DevError> {
        let mut kv = config.metadata.clone();
        // Sources are appended in increasing precedence - metadata, env
        // capture, prev.*, then the run's own counters - and the insert
        // keeps the LAST pair per key (`db::write::insert_inner`), so
        // runtime counters win the unlikely collision (env.foo vs a
        // runtime env.foo).
        kv.extend(self.env_kv.iter().cloned());
        kv.extend(previous_run_kv(&self.db, self.measure_start_epoch.get())?);
        let mut peak_rss_mb: Option<f64> = None;
        for pair in &result.kv {
            if pair.key == "peak_rss_kb" {
                if let KvValue::Int(kb) = &pair.value {
                    #[allow(clippy::cast_precision_loss)]
                    {
                        peak_rss_mb = Some(*kb as f64 / 1024.0);
                    }
                }
                continue; // promoted to column, don't duplicate in run_kv
            }
            kv.push(pair.clone());
        }

        // Harness-level values take precedence. The individual writers
        // don't have to know or care about the measurement mode or the
        // brokkr invocation string - the harness attaches them.
        let mode = self.effective_mode();

        Ok(RunRow {
            hostname: self.host.hostname.clone(),
            commit: self.git.commit.clone(),
            subject: self.git.subject.clone(),
            command: config.command.clone(),
            mode,
            input_file: config.input_file.clone(),
            input_mb: config.input_mb,
            peak_rss_mb,
            cargo_features: config
                .cargo_features
                .clone()
                .or_else(|| self.cargo_features.clone()),
            // An external tool's own identity, else what the session built.
            // A session that measures a binary it did not build (a server the
            // command started, a suite's own release builds) records release,
            // the profile every such build path uses.
            cargo_profile: config
                .external_build
                .or_else(|| self.build_identity.as_ref().map(|b| b.profile))
                .unwrap_or(CargoProfile::Release),
            // Carried through, not recomputed: only the harness path that
            // produced the result knows whether sub-millisecond timing was
            // ever measured. No microseconds means "never measured", which
            // is not the same as `elapsed_ms * 1000`.
            elapsed: result_wall(result.elapsed_ms, result.elapsed_us),
            kernel: self.host.kernel.clone(),
            cpu_governor: self.host.governor.clone(),
            avail_memory_mb: i64::try_from(self.host.memory_available_mb).ok(),
            storage_notes: self.storage_notes.clone(),
            cli_args: config.cli_args.clone(),
            invocation: self.invocation.clone(),
            project: self.project.name().to_owned(),
            stop_marker: if self.stop_marker.is_some() || fired_stop_marker.is_some() {
                Some((
                    self.stop_marker.clone(),
                    fired_stop_marker.map(str::to_owned),
                ))
            } else {
                None
            },
            kv,
            // Microsecond walls are all-or-none (`all_or_none`), so either
            // every iteration is exact or every one is millisecond-only.
            iterations: if iterations_us.len() == result.iterations.len() {
                iterations_us
                    .iter()
                    .copied()
                    .map(Wall::from_micros)
                    .collect()
            } else {
                result
                    .iterations
                    .iter()
                    .copied()
                    .map(Wall::from_legacy_millis)
                    .collect()
            },
            distribution: result.distribution.clone(),
            hotpath: result.hotpath.clone(),
        })
    }
}

/// Describe the run recorded just before this one as `prev.*` kv pairs.
///
/// Records what the machine was doing immediately beforehand, which is the
/// cheapest available proxy for page-cache state: a cell that reads the same
/// file its predecessor just read starts warm, and reports a wall the code
/// didn't earn. `prev.gap_s` and `prev.input_file` together are usually enough
/// to tell "this baseline was warm" from "this baseline was fast".
///
/// Must be called before the current row is inserted, or it describes the
/// current run. Lookup errors are returned so the row cannot silently lose
/// predecessor provenance.
///
/// `measure_start_epoch` is when this run began; the gap is measured to that,
/// not to now, because "now" is after every iteration has run and would count
/// this run's own duration as idle time. Falls back to the current clock when
/// it is 0 (a caller that recorded a result without going through a `run_*`
/// loop), which is the same reading as before the loop for an instant record.
fn previous_run_kv(db: &ResultsDb, measure_start_epoch: i64) -> Result<Vec<KvPair>, DevError> {
    let Some(prev) = db.previous_run()? else {
        return Ok(Vec::new());
    };
    let start = if measure_start_epoch > 0 {
        measure_start_epoch
    } else {
        wall_clock_epoch()
    };
    // `prev.gap_seconds`, not `prev.gap_s`: the single-result view renders kv
    // keys with underscores as spaces, and "prev.gap s" reads as a typo.
    // Clamped at 0 - a backwards system clock shouldn't record a negative gap.
    let gap_seconds = (start - prev.timestamp_epoch).max(0);
    let mut kv = vec![
        KvPair::text(KvKey::predecessor("command"), prev.command),
        KvPair::int(KvKey::predecessor("gap_seconds"), gap_seconds),
    ];
    if !prev.uuid.is_empty() {
        // Short form, matching what `brokkr results` prints and accepts as a
        // prefix - so the recorded value is directly pasteable.
        kv.push(KvPair::text(
            KvKey::predecessor("uuid"),
            &prev.uuid[..8.min(prev.uuid.len())],
        ));
    }
    if let Some(input) = prev.input_file {
        kv.push(KvPair::text(KvKey::predecessor("input_file"), input));
    }
    Ok(kv)
}
