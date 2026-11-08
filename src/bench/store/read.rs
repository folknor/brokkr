//! Loading a stored run back into the document it was published as.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::StoreError;
use super::document::{
    BoundParam, Build, CaptureOutcome, Child, ChildEnding, Clock, Conditions, Evidence, Executable,
    ExecutableIdentity, HotpathCapture, Iteration, IterationOutcome, Loss, Measurement, ParamValue,
    Record, ResolvedBuild, RunDocument, RunOutcome, Sample, SinkEnd,
};
use super::list::StoredRun;

fn unreadable(what: &str) -> StoreError {
    StoreError::Failed(format!("stored {what} does not read back"))
}

fn json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, StoreError> {
    Ok(serde_json::from_str(text)?)
}

fn index(value: i64) -> Result<usize, StoreError> {
    usize::try_from(value).map_err(|_| unreadable("index"))
}

/// The run stored under `uuid`, or `None`.
pub(super) fn load(conn: &Connection, uuid: &str) -> Result<Option<RunDocument>, StoreError> {
    Ok(load_run(conn, uuid, true)?.map(|run| run.doc))
}

/// The run stored under `uuid` with what the store derived from it, its
/// children loaded only when `children` is set.
pub(super) fn load_run(
    conn: &Connection,
    uuid: &str,
    children: bool,
) -> Result<Option<StoredRun>, StoreError> {
    let Some((run_id, columns, derived)) = conn
        .query_row(
            "SELECT r.id, r.format, r.project, r.bench, r.checkout, r.hostname, r.git_commit,
                    r.dirty, r.subject, r.started_unix_ms, r.brokkr_version, r.argv, r.kernel,
                    r.cpu_governor, r.avail_memory_mb, r.captured_env, r.target, r.profile,
                    r.features, r.default_features, r.resolved_bin, r.resolved_features,
                    r.artifact, c.text, r.iterations_requested, r.outcome, r.failure,
                    r.config_digest, r.pairing_key, r.best_iteration, r.measurement
             FROM runs r JOIN configs c ON c.digest = r.config_digest
             WHERE r.uuid = ?1",
            [uuid],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    run_columns(row)?,
                    (
                        row.get::<_, String>(27)?,
                        row.get::<_, String>(28)?,
                        row.get::<_, Option<i64>>(29)?,
                    ),
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let RunColumns {
        mut doc,
        json: cols,
    } = columns.ok_or_else(|| unreadable("run outcome"))?;
    doc.uuid = uuid.to_owned();
    doc.measurement =
        Measurement::parse(&cols.measurement).ok_or_else(|| unreadable("run measurement"))?;
    doc.argv = json(&cols.argv)?;
    doc.conditions.captured_env = json(&cols.captured_env)?;
    doc.build.features = json(&cols.features)?;
    if let (Some(bin), Some(features)) = (cols.resolved_bin, cols.resolved_features) {
        doc.build.resolved = Some(ResolvedBuild {
            bin,
            features: json(&features)?,
        });
    }
    doc.params = load_params(conn, run_id)?;
    doc.iterations = load_iterations(conn, run_id, children)?;
    let (config_digest, pairing_key, best) = derived;
    let best_iteration = best.map(index).transpose()?;
    if best_iteration.is_some_and(|best| best >= doc.iterations.len()) {
        return Err(unreadable("best iteration"));
    }
    Ok(Some(StoredRun {
        doc,
        config_digest,
        pairing_key,
        best_iteration,
        children_loaded: children,
    }))
}

/// The JSON-encoded and paired columns of a run row, decoded after the row
/// closure, which cannot return a [`StoreError`].
struct RunJson {
    argv: String,
    captured_env: String,
    features: String,
    resolved_bin: Option<String>,
    resolved_features: Option<String>,
    measurement: String,
}

struct RunColumns {
    doc: RunDocument,
    json: RunJson,
}

/// The run row, its JSON columns left for [`load`] to decode; `None` for an
/// outcome and failure that do not agree.
fn run_columns(row: &Row<'_>) -> rusqlite::Result<Option<RunColumns>> {
    let outcome = match (row.get::<_, String>(25)?.as_str(), row.get(26)?) {
        ("completed", None) => RunOutcome::Completed,
        ("failed", Some(reason)) => RunOutcome::Failed { reason },
        ("interrupted", None) => RunOutcome::Interrupted,
        _ => return Ok(None),
    };
    let doc = RunDocument {
        format: row.get(1)?,
        uuid: String::new(),
        project: row.get(2)?,
        bench: row.get(3)?,
        checkout: row.get(4)?,
        hostname: row.get(5)?,
        commit: row.get(6)?,
        dirty: row.get(7)?,
        subject: row.get(8)?,
        started_unix_ms: row.get(9)?,
        brokkr_version: row.get(10)?,
        argv: Vec::new(),
        conditions: Conditions {
            kernel: row.get(12)?,
            cpu_governor: row.get(13)?,
            avail_memory_mb: row.get(14)?,
            captured_env: BTreeMap::new(),
        },
        build: Build {
            target: row.get(16)?,
            profile: row.get(17)?,
            features: Vec::new(),
            default_features: row.get(19)?,
            resolved: None,
            artifact: row.get(22)?,
        },
        measurement: Measurement::Timing,
        params: BTreeMap::new(),
        config: row.get(23)?,
        iterations_requested: row.get(24)?,
        outcome,
        iterations: Vec::new(),
    };
    Ok(Some(RunColumns {
        doc,
        json: RunJson {
            argv: row.get(11)?,
            captured_env: row.get(15)?,
            features: row.get(18)?,
            resolved_bin: row.get(20)?,
            resolved_features: row.get(21)?,
            measurement: row.get(30)?,
        },
    }))
}

fn load_params(conn: &Connection, run_id: i64) -> Result<BTreeMap<String, BoundParam>, StoreError> {
    let mut params = BTreeMap::new();
    let mut headers =
        conn.prepare_cached("SELECT name, many, from_path FROM run_params WHERE run_id = ?1")?;
    let rows = headers.query_map([run_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
    })?;
    for row in rows {
        let (name, many, from) = row?;
        params.insert(
            name,
            BoundParam {
                many,
                from,
                values: Vec::new(),
            },
        );
    }
    let mut values = conn.prepare_cached(
        "SELECT name, key, digest FROM run_param_values WHERE run_id = ?1
         ORDER BY name, position",
    )?;
    let rows = values.query_map([run_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
    })?;
    for row in rows {
        let (name, key, digest) = row?;
        params
            .get_mut(&name)
            .ok_or_else(|| unreadable("param value"))?
            .values
            .push(ParamValue { key, digest });
    }
    Ok(params)
}

fn load_iterations(
    conn: &Connection,
    run_id: i64,
    children: bool,
) -> Result<Vec<Iteration>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT idx, outcome, verdict, failure, wall_us, record FROM iterations
         WHERE run_id = ?1 ORDER BY idx",
    )?;
    let rows = stmt.query_map([run_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut iterations = Vec::new();
    for row in rows {
        let (idx, outcome, verdict, failure, wall_us, record) = row?;
        if index(idx)? != iterations.len() {
            return Err(unreadable("iteration order"));
        }
        let outcome = match (outcome.as_str(), failure) {
            ("completed", None) => IterationOutcome::Completed { verdict },
            ("failed", Some(reason)) => IterationOutcome::Failed { reason },
            ("interrupted", None) => IterationOutcome::Interrupted,
            _ => return Err(unreadable("iteration outcome")),
        };
        iterations.push(Iteration {
            outcome,
            wall_us,
            record: record.as_deref().map(json).transpose()?,
            children: if children {
                load_children(conn, run_id, idx)?
            } else {
                Vec::new()
            },
        });
    }
    Ok(iterations)
}

/// A child row's columns, decoded after the row closure.
struct ChildRow {
    id: i64,
    idx: i64,
    program: String,
    argv: String,
    env: String,
    pid: Option<u32>,
    origin_offset_us: i64,
    wall_us: i64,
    ending: (String, Option<i64>, Option<String>),
    loss: Loss,
    clock: Clock,
    calls_seen: String,
    calls_overflowed: bool,
}

/// A non-negative count column; a negative one does not read back.
fn count(row: &Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let raw: i64 = row.get(column)?;
    u64::try_from(raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Integer, e.into())
    })
}

fn child_row(row: &Row<'_>) -> rusqlite::Result<ChildRow> {
    Ok(ChildRow {
        id: row.get(0)?,
        idx: row.get(1)?,
        program: row.get(2)?,
        argv: row.get(3)?,
        env: row.get(4)?,
        pid: row.get(5)?,
        origin_offset_us: row.get(6)?,
        wall_us: row.get(7)?,
        ending: (row.get(8)?, row.get(9)?, row.get(10)?),
        loss: Loss {
            rejected_lines: count(row, 11)?,
            fifo_unread: row.get(12)?,
            records_capped: row.get(13)?,
            samples_capped: row.get(14)?,
            output_truncated: row.get(15)?,
        },
        clock: Clock {
            outliers: count(row, 16)?,
            min_us: row.get(17)?,
        },
        calls_seen: row.get(18)?,
        calls_overflowed: row.get(19)?,
    })
}

fn decode_ending(
    (kind, value, text): (String, Option<i64>, Option<String>),
) -> Result<ChildEnding, StoreError> {
    let int = |v: Option<i64>| v.and_then(|v| i32::try_from(v).ok());
    match (kind.as_str(), value, text) {
        ("exited", v, None) => int(v).map(|code| ChildEnding::Exited { code }),
        ("signal", v, None) => int(v).map(|signal| ChildEnding::Signalled { signal }),
        ("stop_marker", None, Some(marker)) => Some(ChildEnding::StopMarker { marker }),
        ("deadline", Some(nanos), Some(secs)) => secs
            .parse()
            .ok()
            .zip(u32::try_from(nanos).ok())
            .map(|(secs, nanos)| ChildEnding::Deadline { secs, nanos }),
        ("interrupted", None, None) => Some(ChildEnding::Interrupted),
        ("wait_failed", None, Some(reason)) => Some(ChildEnding::WaitFailed { reason }),
        _ => None,
    }
    .ok_or_else(|| unreadable("child ending"))
}

fn load_children(conn: &Connection, run_id: i64, iteration: i64) -> Result<Vec<Child>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, idx, program, argv, env, pid, origin_offset_us, wall_us, ending_kind,
                ending_value, ending_text, rejected_lines, fifo_unread, records_capped,
                samples_capped, output_truncated, clock_outliers, clock_outlier_min_us,
                calls_seen, calls_overflowed
         FROM children WHERE run_id = ?1 AND iteration = ?2 ORDER BY idx",
    )?;
    let rows = stmt
        .query_map(params![run_id, iteration], child_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut children = Vec::with_capacity(rows.len());
    for row in rows {
        if index(row.idx)? != children.len() {
            return Err(unreadable("child order"));
        }
        children.push(Child {
            program: row.program,
            argv: json(&row.argv)?,
            env: json(&row.env)?,
            pid: row.pid,
            origin_offset_us: row.origin_offset_us,
            wall_us: row.wall_us,
            ending: decode_ending(row.ending)?,
            evidence: Evidence {
                samples: load_samples(conn, row.id)?,
                records: load_records(conn, row.id)?,
                loss: row.loss,
                clock: row.clock,
                calls_seen: json(&row.calls_seen)?,
                calls_overflowed: row.calls_overflowed,
            },
            hotpath: load_capture(conn, row.id)?,
            executable: load_executable(conn, row.id)?,
        });
    }
    Ok(children)
}

fn load_executable(conn: &Connection, child_id: i64) -> Result<Option<Executable>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT path, digest, digest_error FROM child_executable WHERE child_id = ?1",
    )?;
    let row = stmt
        .query_row([child_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .optional()?;
    let Some((path, digest, error)) = row else {
        return Ok(None);
    };
    let identity = match (digest, error) {
        (Some(digest), None) => ExecutableIdentity::Digest { digest },
        (None, Some(reason)) => ExecutableIdentity::Unread { reason },
        _ => return Err(unreadable("child executable")),
    };
    Ok(Some(Executable { path, identity }))
}

fn load_capture(conn: &Connection, child_id: i64) -> Result<Option<HotpathCapture>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT stage, occurrence, status, reason, report FROM child_hotpath
         WHERE child_id = ?1",
    )?;
    let row = stmt
        .query_row([child_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .optional()?;
    let Some((stage, occurrence, status, reason, report)) = row else {
        return Ok(None);
    };
    let outcome = match (status.as_str(), reason, report) {
        ("captured", None, Some(report)) => CaptureOutcome::Captured {
            report: json(&report)?,
        },
        ("missing", None, None) => CaptureOutcome::Missing,
        ("invalid", Some(reason), None) => CaptureOutcome::Invalid { reason },
        ("unreadable", Some(reason), None) => CaptureOutcome::Unreadable { reason },
        ("over_budget", None, None) => CaptureOutcome::OverBudget,
        _ => return Err(unreadable("hotpath capture")),
    };
    Ok(Some(HotpathCapture {
        stage,
        occurrence,
        outcome,
    }))
}

fn load_samples(conn: &Connection, child_id: i64) -> Result<Vec<Sample>, StoreError> {
    let sql = format!(
        "SELECT sample_idx, {} FROM samples WHERE child_id = ?1 ORDER BY sample_idx",
        Sample::COLUMNS.join(", ")
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([child_id], |row| {
        Ok((row.get::<_, i64>(0)?, Sample::from_row(row, 1)?))
    })?;
    let mut samples = Vec::new();
    for row in rows {
        let (idx, sample) = row?;
        if index(idx)? != samples.len() {
            return Err(unreadable("sample order"));
        }
        samples.push(sample);
    }
    Ok(samples)
}

/// Every record kind, merged back into the one arrival order by position.
fn load_records(conn: &Connection, child_id: i64) -> Result<Vec<Record>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT record_idx, 'marker', timestamp_us, call_id, name, NULL, NULL, NULL
           FROM markers WHERE child_id = ?1
         UNION ALL
         SELECT record_idx, 'counter', timestamp_us, call_id, name, value, NULL, NULL
           FROM counters WHERE child_id = ?1
         UNION ALL
         SELECT record_idx, kind, timestamp_us, call_id, name, value, end_state, drops
           FROM sink_records WHERE child_id = ?1
         ORDER BY 1",
    )?;
    let rows = stmt.query_map([child_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    let mut records = Vec::new();
    for row in rows {
        let (idx, kind, timestamp_us, call, name, value, end, drops) = row?;
        if index(idx)? != records.len() {
            return Err(unreadable("record order"));
        }
        let record = match (kind.as_str(), call, name, value) {
            ("marker", call, Some(name), None) => Record::Marker {
                timestamp_us,
                call,
                name,
            },
            ("counter", call, Some(name), Some(value)) => Record::Counter {
                timestamp_us,
                call,
                name,
                value,
            },
            ("sealing", Some(call), Some(name), Some(value)) => Record::Sealing {
                timestamp_us,
                call,
                name,
                value,
            },
            ("terminal", Some(call), None, None) => Record::Terminal {
                timestamp_us,
                call,
                end: end
                    .as_deref()
                    .and_then(SinkEnd::parse)
                    .ok_or_else(|| unreadable("sink end"))?,
                drops: drops
                    .as_deref()
                    .and_then(|d| d.parse().ok())
                    .ok_or_else(|| unreadable("sink drops"))?,
            },
            _ => return Err(unreadable("record")),
        };
        records.push(record);
    }
    Ok(records)
}
