//! The brokkr side of the bench store: where the store lives, and how a
//! sidecar capture, a child's ending and its environment become the evidence
//! a run document holds. The store itself never sees brokkr's types, so a
//! server can include it alone; everything brokkr-specific is here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::store;
use crate::error::DevError;
use crate::output::Ending;
use crate::sidecar::{Record, SidecarData, SinkEnd};

impl From<store::StoreError> for DevError {
    fn from(error: store::StoreError) -> Self {
        DevError::Database(error.to_string())
    }
}

/// The per-user bench store's path, `$XDG_DATA_HOME/brokkr/bench.db`.
pub(crate) fn store_path() -> Result<PathBuf, DevError> {
    crate::user_dirs::bench_db().ok_or_else(|| {
        DevError::Config(
            "cannot determine data directory: neither XDG_DATA_HOME nor HOME is set".into(),
        )
    })
}

/// Open the bench store at `path` (normally [`store_path`]), creating its
/// directory.
pub(crate) fn open_store(path: &Path) -> Result<store::Store, DevError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(store::Store::open(path)?)
}

/// A child's environment additions as a document stores them: credential
/// values redacted by the rule results rows use.
pub(crate) fn redacted_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter()
        .map(|(name, value)| (name.clone(), crate::history::redact_env_value(name, value)))
        .collect()
}

/// A supervisor's ending, without loss.
pub(crate) fn child_ending(ending: &Ending) -> store::ChildEnding {
    match ending {
        Ending::Exited(code) => store::ChildEnding::Exited { code: *code },
        Ending::Signalled(signal) => store::ChildEnding::Signalled { signal: *signal },
        Ending::StopMarker(marker) => store::ChildEnding::StopMarker {
            marker: marker.clone(),
        },
        Ending::Deadline(limit) => store::ChildEnding::Deadline {
            secs: limit.as_secs(),
            nanos: limit.subsec_nanos(),
        },
        Ending::Interrupted => store::ChildEnding::Interrupted,
        Ending::WaitFailed(why) => store::ChildEnding::WaitFailed {
            reason: why.clone(),
        },
    }
}

fn sample(s: &crate::sidecar::Sample) -> store::Sample {
    store::Sample {
        timestamp_us: s.timestamp_us,
        rss_kb: s.rss_kb,
        anon_kb: s.anon_kb,
        file_kb: s.file_kb,
        shmem_kb: s.shmem_kb,
        swap_kb: s.swap_kb,
        vsize_kb: s.vsize_kb,
        vm_hwm_kb: s.vm_hwm_kb,
        utime: s.utime,
        stime: s.stime,
        num_threads: s.num_threads,
        minflt: s.minflt,
        majflt: s.majflt,
        rchar: s.rchar,
        wchar: s.wchar,
        read_bytes: s.read_bytes,
        write_bytes: s.write_bytes,
        cancelled_write_bytes: s.cancelled_write_bytes,
        syscr: s.syscr,
        syscw: s.syscw,
        vol_cs: s.vol_cs,
        nonvol_cs: s.nonvol_cs,
    }
}

fn record(r: &Record) -> store::Record {
    match r {
        Record::Marker(m) => store::Record::Marker {
            timestamp_us: m.timestamp_us,
            call: m.call.clone(),
            name: m.name.clone(),
        },
        Record::Counter(c) => store::Record::Counter {
            timestamp_us: c.timestamp_us,
            call: c.call.clone(),
            name: c.name.clone(),
            value: c.value,
        },
        Record::Sealing(s) => store::Record::Sealing {
            timestamp_us: s.timestamp_us,
            call: s.call.clone(),
            name: s.name.clone(),
            value: s.value,
        },
        Record::Terminal(t) => store::Record::Terminal {
            timestamp_us: t.timestamp_us,
            call: t.call.clone(),
            end: match t.end {
                SinkEnd::Sealed => store::SinkEnd::Sealed,
                SinkEnd::Failed => store::SinkEnd::Failed,
            },
            drops: t.drops,
        },
    }
}

/// One child's stored evidence in the forms `brokkr sidecar`'s views read,
/// each record tagged with the child's iteration as its run.
pub(crate) struct ChildEvidence {
    pub(crate) samples: Vec<crate::sidecar::Sample>,
    pub(crate) markers: Vec<crate::sidecar::Marker>,
    pub(crate) counters: Vec<crate::sidecar::Counter>,
    pub(crate) sink_ends: Vec<crate::sidecar::StoredSinkEnd>,
}

fn position<T: TryFrom<usize> + Copy>(idx: usize, max: T) -> T {
    T::try_from(idx).unwrap_or(max)
}

/// The reverse of [`evidence`] for one child of `iteration`: a sample's index
/// is its position, a marker's its position among markers, a sink end's its
/// arrival position across every kind, the positions the store derives
/// (`reference/bench-store.md`).
pub(crate) fn child_evidence(stored: &store::Evidence, iteration: usize) -> ChildEvidence {
    use crate::sidecar::{Counter, Marker, Sample, SinkEndKind, StoredSinkEnd};
    let samples = stored
        .samples
        .iter()
        .enumerate()
        .map(|(idx, s)| Sample {
            sample_idx: position(idx, i32::MAX),
            timestamp_us: s.timestamp_us,
            rss_kb: s.rss_kb,
            anon_kb: s.anon_kb,
            file_kb: s.file_kb,
            shmem_kb: s.shmem_kb,
            swap_kb: s.swap_kb,
            vsize_kb: s.vsize_kb,
            vm_hwm_kb: s.vm_hwm_kb,
            utime: s.utime,
            stime: s.stime,
            num_threads: s.num_threads,
            minflt: s.minflt,
            majflt: s.majflt,
            rchar: s.rchar,
            wchar: s.wchar,
            read_bytes: s.read_bytes,
            write_bytes: s.write_bytes,
            cancelled_write_bytes: s.cancelled_write_bytes,
            syscr: s.syscr,
            syscw: s.syscw,
            vol_cs: s.vol_cs,
            nonvol_cs: s.nonvol_cs,
        })
        .collect();
    let mut out = ChildEvidence {
        samples,
        markers: Vec::new(),
        counters: Vec::new(),
        sink_ends: Vec::new(),
    };
    for (record_idx, record) in stored.records.iter().enumerate() {
        match record {
            store::Record::Marker {
                timestamp_us,
                call,
                name,
            } => out.markers.push(Marker {
                marker_idx: position(out.markers.len(), i32::MAX),
                timestamp_us: *timestamp_us,
                name: name.clone(),
                run_idx: iteration,
                call: call.clone(),
            }),
            store::Record::Counter {
                timestamp_us,
                call,
                name,
                value,
            } => out.counters.push(Counter {
                timestamp_us: *timestamp_us,
                name: name.clone(),
                value: *value,
                run_idx: iteration,
                call: call.clone(),
            }),
            store::Record::Sealing {
                timestamp_us,
                call,
                name,
                value,
            } => out.sink_ends.push(StoredSinkEnd {
                run_idx: iteration,
                record_idx: position(record_idx, i64::MAX),
                timestamp_us: *timestamp_us,
                call: call.clone(),
                kind: SinkEndKind::Sealing {
                    name: name.clone(),
                    value: *value,
                },
            }),
            store::Record::Terminal {
                timestamp_us,
                call,
                end,
                drops,
            } => out.sink_ends.push(StoredSinkEnd {
                run_idx: iteration,
                record_idx: position(record_idx, i64::MAX),
                timestamp_us: *timestamp_us,
                call: call.clone(),
                kind: SinkEndKind::Terminal {
                    end: match end {
                        store::SinkEnd::Sealed => SinkEnd::Sealed,
                        store::SinkEnd::Failed => SinkEnd::Failed,
                    },
                    drops: *drops,
                },
            }),
        }
    }
    out
}

/// A child's stored loss as the capture's own type, for its wording.
pub(crate) fn capture_loss(loss: &store::Loss) -> crate::sidecar::CaptureLoss {
    crate::sidecar::CaptureLoss {
        rejected_lines: usize::try_from(loss.rejected_lines).unwrap_or(usize::MAX),
        fifo_unread: loss.fifo_unread,
        records_capped: loss.records_capped,
        samples_capped: loss.samples_capped,
        output_truncated: loss.output_truncated,
    }
}

/// Everything a capture holds for one child. Call boundaries are left out:
/// only a library call document supplies them, and a script bench parses
/// none.
pub(crate) fn evidence(data: &SidecarData) -> store::Evidence {
    let loss = &data.summary.loss;
    store::Evidence {
        samples: data.samples.iter().map(sample).collect(),
        records: data.records.iter().map(record).collect(),
        loss: store::Loss {
            rejected_lines: u64::try_from(loss.rejected_lines).unwrap_or(u64::MAX),
            fifo_unread: loss.fifo_unread,
            records_capped: loss.records_capped,
            samples_capped: loss.samples_capped,
            output_truncated: loss.output_truncated,
        },
        clock: store::Clock {
            outliers: u64::try_from(data.summary.clock.count).unwrap_or(u64::MAX),
            min_us: data.summary.clock.min_us,
        },
        calls_seen: data.calls_seen.ids.clone(),
        calls_overflowed: data.calls_seen.overflowed,
    }
}
