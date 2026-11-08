//! Publication: one validated document applied inside the caller's
//! transaction, every row or none.

use rusqlite::{Connection, params};

use super::StoreError;
use super::document::{
    CaptureOutcome, Child, ChildEnding, Evidence, Executable, ExecutableIdentity, HotpathCapture,
    Iteration, IterationOutcome, Record, RunDocument, RunOutcome, Sample,
};

/// A count or index in SQLite's integer domain.
fn int(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The content address of a config text.
pub(super) fn config_digest(text: &str) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(text.as_bytes()))
}

/// Store the config text once per digest. A digest already holding another
/// text is refused rather than shared.
fn insert_config(tx: &Connection, text: &str) -> Result<String, StoreError> {
    let digest = config_digest(text);
    tx.execute(
        "INSERT OR IGNORE INTO configs (digest, text) VALUES (?1, ?2)",
        params![digest, text],
    )?;
    let stored: String = tx.query_row(
        "SELECT text FROM configs WHERE digest = ?1",
        [&digest],
        |row| row.get(0),
    )?;
    if stored != text {
        return Err(StoreError::Refused(format!(
            "config digest {digest} already names a different text"
        )));
    }
    Ok(digest)
}

/// Insert the whole document; returns the run's row id.
pub(super) fn insert(tx: &Connection, doc: &RunDocument) -> Result<i64, StoreError> {
    let config_digest = insert_config(tx, &doc.config)?;
    let run_id = insert_run(tx, doc, &config_digest)?;
    insert_params(tx, run_id, doc)?;
    for (idx, iteration) in doc.iterations.iter().enumerate() {
        insert_iteration(tx, run_id, idx, iteration)?;
    }
    Ok(run_id)
}

fn insert_run(tx: &Connection, doc: &RunDocument, config_digest: &str) -> Result<i64, StoreError> {
    let (outcome, failure) = match &doc.outcome {
        RunOutcome::Completed => ("completed", None),
        RunOutcome::Failed { reason } => ("failed", Some(reason.as_str())),
        RunOutcome::Interrupted => ("interrupted", None),
    };
    let build = &doc.build;
    let resolved_features = build
        .resolved
        .as_ref()
        .map(|r| serde_json::to_string(&r.features))
        .transpose()?;
    tx.execute(
        "INSERT INTO runs (
            uuid, format, project, bench, checkout, hostname, git_commit, dirty, subject,
            started_unix_ms, brokkr_version, argv, kernel, cpu_governor, avail_memory_mb,
            captured_env, target, profile, features, default_features, resolved_bin,
            resolved_features, artifact, config_digest, iterations_requested, outcome,
            failure, best_iteration, pairing_key, measurement
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30)",
        params![
            doc.uuid,
            doc.format,
            doc.project,
            doc.bench,
            doc.checkout,
            doc.hostname,
            doc.commit,
            doc.dirty,
            doc.subject,
            doc.started_unix_ms,
            doc.brokkr_version,
            serde_json::to_string(&doc.argv)?,
            doc.conditions.kernel,
            doc.conditions.cpu_governor,
            doc.conditions.avail_memory_mb,
            serde_json::to_string(&doc.conditions.captured_env)?,
            build.target,
            build.profile,
            serde_json::to_string(&build.features)?,
            build.default_features,
            build.resolved.as_ref().map(|r| r.bin.as_str()),
            resolved_features,
            build.artifact,
            config_digest,
            doc.iterations_requested,
            outcome,
            failure,
            doc.best_iteration().map(int),
            doc.pairing_key(),
            doc.measurement.name(),
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

fn insert_params(tx: &Connection, run_id: i64, doc: &RunDocument) -> Result<(), StoreError> {
    let mut header = tx.prepare_cached(
        "INSERT INTO run_params (run_id, name, many, from_path) VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut value = tx.prepare_cached(
        "INSERT INTO run_param_values (run_id, name, position, key, digest)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (name, param) in &doc.params {
        header.execute(params![run_id, name, param.many, param.from])?;
        for (position, v) in param.values.iter().enumerate() {
            value.execute(params![run_id, name, int(position), v.key, v.digest])?;
        }
    }
    Ok(())
}

fn insert_iteration(
    tx: &Connection,
    run_id: i64,
    idx: usize,
    iteration: &Iteration,
) -> Result<(), StoreError> {
    let (outcome, verdict, failure) = match &iteration.outcome {
        IterationOutcome::Completed { verdict } => ("completed", verdict.as_deref(), None),
        IterationOutcome::Failed { reason } => ("failed", None, Some(reason.as_str())),
        IterationOutcome::Interrupted => ("interrupted", None, None),
    };
    let record = iteration
        .record
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    tx.execute(
        "INSERT INTO iterations (run_id, idx, outcome, verdict, failure, wall_us, record)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            run_id,
            int(idx),
            outcome,
            verdict,
            failure,
            iteration.wall_us,
            record
        ],
    )?;
    for (child_idx, child) in iteration.children.iter().enumerate() {
        let child_id = insert_child(tx, run_id, idx, child_idx, child)?;
        insert_evidence(tx, child_id, &child.evidence)?;
        if let Some(capture) = &child.hotpath {
            insert_capture(tx, child_id, capture)?;
        }
        if let Some(executable) = &child.executable {
            insert_executable(tx, child_id, executable)?;
        }
    }
    Ok(())
}

fn insert_executable(
    tx: &Connection,
    child_id: i64,
    executable: &Executable,
) -> Result<(), StoreError> {
    let (digest, error) = match &executable.identity {
        ExecutableIdentity::Digest { digest } => (Some(digest.as_str()), None),
        ExecutableIdentity::Unread { reason } => (None, Some(reason.as_str())),
    };
    tx.execute(
        "INSERT INTO child_executable (child_id, path, digest, digest_error)
         VALUES (?1, ?2, ?3, ?4)",
        params![child_id, executable.path, digest, error],
    )?;
    Ok(())
}

fn insert_capture(
    tx: &Connection,
    child_id: i64,
    capture: &HotpathCapture,
) -> Result<(), StoreError> {
    let (reason, report) = match &capture.outcome {
        CaptureOutcome::Captured { report } => (None, Some(serde_json::to_string(report)?)),
        CaptureOutcome::Invalid { reason } | CaptureOutcome::Unreadable { reason } => {
            (Some(reason.as_str()), None)
        }
        CaptureOutcome::Missing | CaptureOutcome::OverBudget => (None, None),
    };
    tx.execute(
        "INSERT INTO child_hotpath (child_id, stage, occurrence, status, reason, report)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            child_id,
            capture.stage,
            capture.occurrence,
            capture.outcome.name(),
            reason,
            report
        ],
    )?;
    Ok(())
}

/// The stored spelling of an ending: a kind, an integer and a text, each
/// used by the kinds that carry one. A deadline's seconds span a `u64`, so
/// they are decimal text, its nanoseconds the integer.
fn encode_ending(ending: &ChildEnding) -> (&'static str, Option<i64>, Option<String>) {
    match ending {
        ChildEnding::Exited { code } => ("exited", Some(i64::from(*code)), None),
        ChildEnding::Signalled { signal } => ("signal", Some(i64::from(*signal)), None),
        ChildEnding::StopMarker { marker } => ("stop_marker", None, Some(marker.clone())),
        ChildEnding::Deadline { secs, nanos } => {
            ("deadline", Some(i64::from(*nanos)), Some(secs.to_string()))
        }
        ChildEnding::Interrupted => ("interrupted", None, None),
        ChildEnding::WaitFailed { reason } => ("wait_failed", None, Some(reason.clone())),
    }
}

fn insert_child(
    tx: &Connection,
    run_id: i64,
    iteration: usize,
    idx: usize,
    child: &Child,
) -> Result<i64, StoreError> {
    let (kind, value, text) = encode_ending(&child.ending);
    let evidence = &child.evidence;
    tx.execute(
        "INSERT INTO children (
            run_id, iteration, idx, program, argv, env, pid, origin_offset_us, wall_us,
            ending_kind, ending_value, ending_text, rejected_lines, fifo_unread,
            records_capped, samples_capped, output_truncated, clock_outliers,
            clock_outlier_min_us, calls_seen, calls_overflowed
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20, ?21)",
        params![
            run_id,
            int(iteration),
            int(idx),
            child.program,
            serde_json::to_string(&child.argv)?,
            serde_json::to_string(&child.env)?,
            child.pid,
            child.origin_offset_us,
            child.wall_us,
            kind,
            value,
            text,
            i64::try_from(evidence.loss.rejected_lines).unwrap_or(i64::MAX),
            evidence.loss.fifo_unread,
            evidence.loss.records_capped,
            evidence.loss.samples_capped,
            evidence.loss.output_truncated,
            i64::try_from(evidence.clock.outliers).unwrap_or(i64::MAX),
            evidence.clock.min_us,
            serde_json::to_string(&evidence.calls_seen)?,
            evidence.calls_overflowed,
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// The sample insert, its columns from [`Sample::COLUMNS`].
fn sample_insert_sql() -> String {
    let columns = Sample::COLUMNS.join(", ");
    let placeholders = (1..=Sample::COLUMNS.len() + 2)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO samples (child_id, sample_idx, {columns}) VALUES ({placeholders})")
}

/// Samples by position, and every record with its arrival position; a
/// marker also takes its position among markers.
fn insert_evidence(tx: &Connection, child_id: i64, evidence: &Evidence) -> Result<(), StoreError> {
    let mut samples = tx.prepare_cached(&sample_insert_sql())?;
    for (idx, sample) in evidence.samples.iter().enumerate() {
        let mut values = vec![child_id, int(idx)];
        values.extend(sample.values());
        samples.execute(rusqlite::params_from_iter(values))?;
    }
    let mut markers = tx.prepare_cached(
        "INSERT INTO markers (child_id, record_idx, marker_idx, timestamp_us, call_id, name)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut counters = tx.prepare_cached(
        "INSERT INTO counters (child_id, record_idx, timestamp_us, call_id, name, value)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut sink = tx.prepare_cached(
        "INSERT INTO sink_records (child_id, record_idx, timestamp_us, call_id, kind, name,
                                   value, end_state, drops)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut marker_idx = 0_usize;
    for (idx, record) in evidence.records.iter().enumerate() {
        let idx = int(idx);
        match record {
            Record::Marker {
                timestamp_us,
                call,
                name,
            } => {
                markers.execute(params![
                    child_id,
                    idx,
                    int(marker_idx),
                    timestamp_us,
                    call,
                    name
                ])?;
                marker_idx += 1;
            }
            Record::Counter {
                timestamp_us,
                call,
                name,
                value,
            } => {
                counters.execute(params![child_id, idx, timestamp_us, call, name, value])?;
            }
            Record::Sealing {
                timestamp_us,
                call,
                name,
                value,
            } => {
                sink.execute(params![
                    child_id,
                    idx,
                    timestamp_us,
                    call,
                    "sealing",
                    name,
                    value,
                    Option::<&str>::None,
                    Option::<String>::None
                ])?;
            }
            Record::Terminal {
                timestamp_us,
                call,
                end,
                drops,
            } => {
                sink.execute(params![
                    child_id,
                    idx,
                    timestamp_us,
                    call,
                    "terminal",
                    Option::<&str>::None,
                    Option::<i64>::None,
                    end.name(),
                    drops.to_string()
                ])?;
            }
        }
    }
    Ok(())
}
