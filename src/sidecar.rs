//! Monitoring sidecar that runs alongside benchmark processes.
//!
//! Samples `/proc/{pid}/*` at fixed intervals, reads application phase markers
//! from a FIFO, and hands everything back to the harness, which stores it in
//! SQLite after the child exits. Nothing is written during the benchmark
//! itself.
//!
//! # Last marker
//!
//! Each drain that saw a phase marker records the latest name, with the time
//! it was seen, on the live hold's observation table
//! (`lockfile::activity::note_marker`), which `brokkr lock` reads over the
//! holder's control socket as "last marker ... ago". It is cleared when a
//! FIFO is created and dropped, so a marker from an earlier run in the same
//! hold is never shown as the current one. It is the last marker *observed*:
//! FIFO draining lags the child, so it does not prove the child's current
//! step. This replaced a PID-keyed status file beside the lock, which a
//! reader in another PID namespace could not trust.
//!
//! # Deadlines
//!
//! [`run_sidecar_with_deadline`] kills the child's process group once it has
//! run longer than the given limit, and the run records `Ending::Deadline`.
//!
//! # Pipe draining
//!
//! The child's stdout and stderr are piped (for error reporting). To avoid
//! deadlock when the child fills the OS pipe buffer (~64 KiB), we take
//! ownership of the pipe handles and drain them in background threads while
//! the shared child supervisor polls. A sidecar observer supplies `/proc`
//! samples and marker checks on each supervisor tick.
//! After the child exits, drains get a bounded grace period to reach EOF. If
//! a descendant still holds a pipe open, capture is stopped and its end is
//! closed so the sidecar cannot hang waiting for that descendant.
//!
//! # Timing
//!
//! `Instant::now()` provides sample timestamps and drift-free tick scheduling
//! through the shared child supervisor. The child's wall-clock time is
//! captured when the supervisor observes the ending, before pipe drains settle.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use crate::error::DevError;
use crate::output::child::{self, ChildLimit, Ending};
use crate::output::console;
use crate::process::{Pid, ProcStat};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Environment variable the measured child uses to find the marker FIFO.
/// This name is part of the sidecar emitter protocol.
pub(crate) const MARKER_FIFO_ENV: &str = "BROKKR_MARKER_FIFO";

/// Measurement defaults shared by the sidecar, harness output handling and
/// sidecar file naming. Callers may copy this value and override fields in
/// focused tests or alternate measurement runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MeasurementTunables {
    pub sample_interval: Duration,
    pub sidecar_backup_copies: usize,
    pub stderr_preview_chars: usize,
    pub hotpath_report_filename: &'static str,
    /// Maximum profile samples retained across every run in one measurement.
    pub max_samples: usize,
    /// Maximum phase markers retained across every run in one measurement.
    pub max_markers: usize,
    /// Maximum application counters retained across every run in one measurement.
    pub max_counters: usize,
    /// Maximum bytes retained from one FIFO protocol line.
    pub max_fifo_line_bytes: usize,
}

pub(crate) const DEFAULT_MEASUREMENT_TUNABLES: MeasurementTunables = MeasurementTunables {
    sample_interval: Duration::from_micros(100_000),
    // A bounded recovery history: the current snapshot plus two prior copies.
    sidecar_backup_copies: 3,
    stderr_preview_chars: 500,
    hotpath_report_filename: "hotpath-report.json",
    max_samples: 100_000,
    max_markers: 10_000,
    max_counters: 20_000,
    max_fifo_line_bytes: 1_024,
};

/// Compute a percentile from a sorted sample using linear interpolation.
/// Benchmark and sidecar summaries share this calculation so they cannot
/// disagree for the same sample set.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn linear_percentile(sorted: &[i64], pct: usize) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let len = sorted.len();
    if len == 1 {
        return sorted[0];
    }
    #[allow(clippy::cast_precision_loss)]
    let pos = (pct as f64 / 100.0) * (len - 1) as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = pos as usize;
    let hi = (lo + 1).min(len - 1);
    #[allow(clippy::cast_precision_loss)]
    let frac = pos - lo as f64;
    #[allow(clippy::cast_precision_loss)]
    let result = sorted[lo] as f64 + frac * (sorted[hi] - sorted[lo]) as f64;
    result.round() as i64
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One sidecar sample field, with its public CLI/JSON name.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SampleField {
    pub name: &'static str,
}

/// Define the sample shape and all derived field/storage views together.
/// The aliases are the public `--fields`/JSON names; the Rust names are also
/// the SQLite column names. Keeping these pairs here lets the database and
/// formatter derive their projections from the same list.
macro_rules! define_sample {
    ($($field:ident => $name:literal;)+) => {
        /// A single point-in-time sample of process metrics from `/proc`.
        #[derive(Debug, Clone)]
        pub struct Sample {
            pub sample_idx: i32,
            pub timestamp_us: i64,
            $(pub $field: i64,)*
        }

        pub(crate) const SAMPLE_FIELDS: &[SampleField] = &[
            SampleField { name: "i" },
            $(SampleField { name: $name },)*
        ];

        impl Sample {
            pub(crate) fn field_value(&self, name: &str) -> Option<i64> {
                match name {
                    "i" => Some(i64::from(self.sample_idx)),
                    $($name => Some(self.$field),)*
                    _ => None,
                }
            }

            pub(crate) fn db_column_names() -> Vec<&'static str> {
                vec!["sample_idx", "timestamp_us", $(stringify!($field),)*]
            }

            pub(crate) fn db_column_definitions() -> Vec<String> {
                vec![
                    "sample_idx INTEGER NOT NULL".to_owned(),
                    "timestamp_us INTEGER NOT NULL".to_owned(),
                    $(format!("{} INTEGER", stringify!($field)),)*
                ]
            }

            pub(crate) fn db_values(&self) -> Vec<rusqlite::types::Value> {
                vec![
                    rusqlite::types::Value::Integer(i64::from(self.sample_idx)),
                    rusqlite::types::Value::Integer(self.timestamp_us),
                    $(rusqlite::types::Value::Integer(self.$field),)*
                ]
            }

            pub(crate) fn from_db_row(
                row: &rusqlite::Row<'_>,
            ) -> rusqlite::Result<Self> {
                Ok(Self {
                    sample_idx: row.get("sample_idx")?,
                    timestamp_us: row.get("timestamp_us")?,
                    $($field: row.get(stringify!($field))?,)*
                })
            }
        }
    };
}

define_sample! {
    rss_kb => "rss";
    anon_kb => "anon";
    file_kb => "file";
    shmem_kb => "shmem";
    swap_kb => "swap";
    vsize_kb => "vsize";
    vm_hwm_kb => "hwm";
    utime => "utime";
    stime => "stime";
    num_threads => "threads";
    minflt => "minflt";
    majflt => "majflt";
    rchar => "rchar";
    wchar => "wchar";
    read_bytes => "rd";
    write_bytes => "wr";
    cancelled_write_bytes => "cwr";
    syscr => "syscr";
    syscw => "syscw";
    vol_cs => "vcs";
    nonvol_cs => "nvcs";
}

/// A phase marker emitted by the target process.
#[derive(Debug, Clone)]
pub struct Marker {
    /// Arrival order across every stream of one run.
    pub marker_idx: i32,
    pub timestamp_us: i64,
    pub name: String,
    /// The iteration this marker belongs to. Capture leaves it 0; the store
    /// fills it on read.
    pub run_idx: usize,
    /// The call stream (`%<call_id>` on the wire), or `None` for the
    /// process-wide stream.
    pub call: Option<String>,
}

/// Longest call id the FIFO accepts.
pub(crate) const MAX_CALL_ID_BYTES: usize = 64;

/// A call id is 1..=64 bytes of `[A-Za-z0-9_.-]`, so it needs no escaping on
/// the wire, in JSON or on the command line.
pub(crate) fn is_valid_call_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CALL_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Longest sealing-emission name: the snapshot counter name domain of a
/// library call document, so every sealing record fits the producer's line.
pub(crate) const MAX_SEALING_NAME_BYTES: usize = 400;

/// How a call sink ended, as its terminal record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkEnd {
    Sealed,
    Failed,
}

impl SinkEnd {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sealed => "sealed",
            Self::Failed => "failed",
        }
    }
}

/// One parsed FIFO record.
#[derive(Debug, PartialEq, Eq)]
enum FifoRecord<'a> {
    Blank,
    Rejected,
    Marker {
        ts: i64,
        call: Option<&'a str>,
        name: &'a str,
    },
    Counter {
        ts: i64,
        call: Option<&'a str>,
        name: &'a str,
        value: i64,
    },
    /// `%CALL @@NAME=VALUE`: one final counter value a sealing sink emits.
    Sealing {
        ts: i64,
        call: &'a str,
        name: &'a str,
        value: i64,
    },
    /// `%CALL !sealed drops=N` or `%CALL !failed drops=N`: a sink's last line.
    Terminal {
        ts: i64,
        call: &'a str,
        end: SinkEnd,
        drops: u64,
    },
}

impl FifoRecord<'_> {
    /// The call a tagged record names.
    fn call(&self) -> Option<&str> {
        match self {
            Self::Marker { call, .. } | Self::Counter { call, .. } => *call,
            Self::Sealing { call, .. } | Self::Terminal { call, .. } => Some(call),
            Self::Blank | Self::Rejected => None,
        }
    }

    /// The record to keep; a marker takes `marker_idx`. `None` for a line
    /// that carries none.
    fn into_record(self, marker_idx: i32) -> Option<Record> {
        Some(match self {
            Self::Blank | Self::Rejected => return None,
            Self::Marker { ts, call, name } => Record::Marker(Marker {
                marker_idx,
                timestamp_us: ts,
                name: name.to_owned(),
                run_idx: 0,
                call: call.map(str::to_owned),
            }),
            Self::Counter {
                ts,
                call,
                name,
                value,
            } => Record::Counter(Counter {
                timestamp_us: ts,
                name: name.to_owned(),
                value,
                run_idx: 0,
                call: call.map(str::to_owned),
            }),
            Self::Sealing {
                ts,
                call,
                name,
                value,
            } => Record::Sealing(Sealing {
                timestamp_us: ts,
                call: call.to_owned(),
                name: name.to_owned(),
                value,
            }),
            Self::Terminal {
                ts,
                call,
                end,
                drops,
            } => Record::Terminal(Terminal {
                timestamp_us: ts,
                call: call.to_owned(),
                end,
                drops,
            }),
        })
    }
}

/// `N` of a terminal record: a u64 in decimal, without sign or leading zero.
fn parse_drops(text: &str) -> Option<u64> {
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if canonical { text.parse().ok() } else { None }
}

/// The body of a tagged record that starts `@@` or `!`, which only sealing
/// sinks write; anything else of those shapes is rejected.
fn parse_sink_end<'a>(ts: i64, call: &'a str, body: &'a str) -> Option<FifoRecord<'a>> {
    if let Some(sealing) = body.strip_prefix("@@") {
        let (name, value) = sealing.split_once('=')?;
        if name.is_empty() || name.len() > MAX_SEALING_NAME_BYTES {
            return None;
        }
        return Some(FifoRecord::Sealing {
            ts,
            call,
            name,
            value: value.parse().ok()?,
        });
    }
    let terminal = body.strip_prefix('!')?;
    let (end, drops) = match terminal.strip_prefix("sealed drops=") {
        Some(n) => (SinkEnd::Sealed, n),
        None => (SinkEnd::Failed, terminal.strip_prefix("failed drops=")?),
    };
    Some(FifoRecord::Terminal {
        ts,
        call,
        end,
        drops: parse_drops(drops)?,
    })
}

/// Parse one FIFO line. `<ts> <name>` and `<ts> @<name>=<i64>` are
/// process-wide; `<ts> %<call_id> <name>` and `<ts> %<call_id> @<name>=<i64>`
/// belong to that call. A payload starting with `%` is only ever a tagged
/// record: a malformed one is rejected, never read as an untagged marker. In
/// a tagged record a body starting `@@` is a sealing emission and one
/// starting `!` a terminal record; untagged lines parse as they always did.
fn parse_fifo_record(line: &str) -> FifoRecord<'_> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return FifoRecord::Blank;
    }
    let Some((ts_str, payload)) = trimmed.split_once(' ') else {
        return FifoRecord::Rejected;
    };
    let Ok(ts) = ts_str.parse::<i64>() else {
        return FifoRecord::Rejected;
    };
    let (call, body) = if let Some(tagged) = payload.strip_prefix('%') {
        let Some((call, body)) = tagged.split_once(' ') else {
            return FifoRecord::Rejected;
        };
        if !is_valid_call_id(call) || body.is_empty() {
            return FifoRecord::Rejected;
        }
        if body.starts_with("@@") || body.starts_with('!') {
            return parse_sink_end(ts, call, body).unwrap_or(FifoRecord::Rejected);
        }
        (Some(call), body)
    } else {
        (None, payload)
    };
    if let Some(counter_body) = body.strip_prefix('@') {
        return match counter_body.split_once('=') {
            Some((name, value)) if !name.is_empty() => match value.parse::<i64>() {
                Ok(value) => FifoRecord::Counter {
                    ts,
                    call,
                    name,
                    value,
                },
                Err(_) => FifoRecord::Rejected,
            },
            _ => FifoRecord::Rejected,
        };
    }
    FifoRecord::Marker {
        ts,
        call,
        name: body,
    }
}

/// A completed span derived from a matching `_START` and later `_END` marker.
#[derive(Debug, Clone)]
pub(crate) struct TimelineSpan {
    pub name: String,
    pub start_us: i64,
    pub end_us: i64,
}

/// A segment between adjacent markers, used by phase-oriented views.
#[derive(Debug, Clone)]
pub(crate) struct TimelineSegment {
    pub name: String,
    pub start_us: i64,
    /// `None` for a call stream's final segment: no call boundary is known.
    pub end_us: Option<i64>,
}

/// A single interpretation surface for one marker stream of one run.
///
/// The FIFO protocol remains a sequence of point markers. This model exposes
/// the documented projections over that sequence: adjacent-marker segments,
/// `_START`/`_END` spans, and phase-name lookup. Every projection orders the
/// markers by `(timestamp_us, marker_idx)`: threads stamp a marker before
/// competing for the write, so arrival order is not time order.
pub(crate) struct Timeline<'a> {
    markers: Vec<&'a Marker>,
    final_us: Option<i64>,
    first_sample_us: Option<i64>,
}

impl<'a> Timeline<'a> {
    /// The process stream of a stored run, using its sampled end as the final
    /// boundary and retaining the whole-run fallback for markerless runs.
    pub(crate) fn new(markers: &'a [Marker], samples: &[Sample]) -> Self {
        Self {
            markers: chronological(markers),
            final_us: Some(samples.last().map_or(0, |s| s.timestamp_us + 1)),
            first_sample_us: samples.first().map(|s| s.timestamp_us),
        }
    }

    /// A call stream. Process samples say nothing about when a call
    /// returned, so its final segment is open rather than borrowed from them.
    pub(crate) fn open(markers: &'a [Marker]) -> Self {
        Self {
            markers: chronological(markers),
            final_us: None,
            first_sample_us: None,
        }
    }

    /// A call stream whose return is known: its final segment ends at
    /// `final_us`.
    pub(crate) fn closed(markers: &'a [Marker], final_us: i64) -> Self {
        Self {
            markers: chronological(markers),
            final_us: Some(final_us),
            first_sample_us: None,
        }
    }

    /// Inter-marker segments. Every marker opens a segment ending at the next
    /// marker; the last one ends at the final boundary. With no markers, the
    /// sampled run is one `(all)` segment.
    pub(crate) fn segments(&self) -> Vec<TimelineSegment> {
        if self.markers.is_empty() {
            return self.first_sample_us.map_or_else(Vec::new, |start_us| {
                vec![TimelineSegment {
                    name: "(all)".to_owned(),
                    start_us,
                    end_us: self.final_us,
                }]
            });
        }

        self.markers
            .iter()
            .enumerate()
            .map(|(i, marker)| TimelineSegment {
                name: marker.name.clone(),
                start_us: marker.timestamp_us,
                end_us: self.segment_end(i),
            })
            .collect()
    }

    fn segment_end(&self, index: usize) -> Option<i64> {
        self.markers
            .get(index + 1)
            .map_or(self.final_us, |next| Some(next.timestamp_us))
    }

    /// Derive completed `_START`/`_END` spans and the point markers that do
    /// not participate in one. Unmatched starts remain standalone points.
    pub(crate) fn durations(&self) -> (Vec<TimelineSpan>, Vec<&'a Marker>) {
        let mut spans = Vec::new();
        let mut consumed = vec![false; self.markers.len()];

        for (i, marker) in self.markers.iter().enumerate() {
            if consumed[i] {
                continue;
            }
            let Some(base) = marker.name.strip_suffix("_START") else {
                continue;
            };

            let end_name = format!("{base}_END");
            if let Some((j, candidate)) = self.markers[i + 1..]
                .iter()
                .enumerate()
                .find(|(j, candidate)| !consumed[i + 1 + *j] && candidate.name == end_name)
            {
                consumed[i] = true;
                consumed[i + 1 + j] = true;
                let end_us = candidate.timestamp_us;
                spans.push(TimelineSpan {
                    name: base.to_owned(),
                    start_us: marker.timestamp_us,
                    end_us,
                });
            }
        }

        let standalone = self
            .markers
            .iter()
            .enumerate()
            .filter_map(|(i, marker)| (!consumed[i]).then_some(*marker))
            .collect();
        (spans, standalone)
    }

    /// Resolve `--phase`: exact event segment, then a base-name span, then a
    /// substring event segment, the first match in time order each time. The
    /// end is `None` when the match is a call stream's final segment.
    pub(crate) fn resolve_phase_range(&self, phase: &str) -> Option<(i64, Option<i64>)> {
        if let Some(index) = self.markers.iter().position(|m| m.name == phase) {
            return Some((self.markers[index].timestamp_us, self.segment_end(index)));
        }

        if let Some(span) = self
            .durations()
            .0
            .into_iter()
            .find(|span| span.name == phase)
        {
            return Some((span.start_us, Some(span.end_us)));
        }

        self.markers
            .iter()
            .position(|m| m.name.contains(phase))
            .map(|index| (self.markers[index].timestamp_us, self.segment_end(index)))
    }

    pub(crate) fn marker_names(&self) -> Vec<&str> {
        self.markers
            .iter()
            .map(|marker| marker.name.as_str())
            .collect()
    }
}

/// Markers in time order, arrival order breaking ties.
fn chronological(markers: &[Marker]) -> Vec<&Marker> {
    let mut sorted: Vec<&Marker> = markers.iter().collect();
    sorted.sort_by_key(|m| (m.timestamp_us, m.marker_idx));
    sorted
}

/// An application-level counter emitted by the target process.
#[derive(Debug, Clone)]
pub struct Counter {
    pub timestamp_us: i64,
    pub name: String,
    pub value: i64,
    /// As on [`Marker`]: 0 at capture, filled on read.
    pub run_idx: usize,
    /// As on [`Marker`]: `None` is the process-wide stream.
    pub call: Option<String>,
}

/// One final counter value a call's sink emitted while sealing,
/// `%CALL @@NAME=VALUE` on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sealing {
    pub timestamp_us: i64,
    pub call: String,
    pub name: String,
    pub value: i64,
}

/// A call sink's terminal record, `%CALL !sealed drops=N` on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Terminal {
    pub timestamp_us: i64,
    pub call: String,
    pub end: SinkEnd,
    /// Lines the sink dropped before this one.
    pub drops: u64,
}

/// One captured FIFO record. A capture keeps them in one list in arrival
/// order, so the order between a marker, a counter and a sink's last lines
/// is never lost.
#[derive(Debug, Clone)]
pub(crate) enum Record {
    Marker(Marker),
    Counter(Counter),
    Sealing(Sealing),
    Terminal(Terminal),
}

impl Record {
    /// The call stream it belongs to; `None` is the process-wide stream.
    pub(crate) fn call(&self) -> Option<&str> {
        match self {
            Self::Marker(m) => m.call.as_deref(),
            Self::Counter(c) => c.call.as_deref(),
            Self::Sealing(s) => Some(&s.call),
            Self::Terminal(t) => Some(&t.call),
        }
    }

    pub(crate) fn timestamp_us(&self) -> i64 {
        match self {
            Self::Marker(m) => m.timestamp_us,
            Self::Counter(c) => c.timestamp_us,
            Self::Sealing(s) => s.timestamp_us,
            Self::Terminal(t) => t.timestamp_us,
        }
    }

    /// The record in its wire spelling, without the newline.
    pub(crate) fn wire(&self) -> String {
        let tag = self.call().map_or_else(String::new, |c| format!("%{c} "));
        let ts = self.timestamp_us();
        match self {
            Self::Marker(m) => format!("{ts} {tag}{}", m.name),
            Self::Counter(c) => format!("{ts} {tag}@{}={}", c.name, c.value),
            Self::Sealing(s) => format!("{ts} {tag}@@{}={}", s.name, s.value),
            Self::Terminal(t) => format!("{ts} {tag}!{} drops={}", t.end.name(), t.drops),
        }
    }
}

/// What a capture omitted, by cause. A record the FIFO reader may not hold
/// (`stream_lost`) is a different fact from a sample or output byte it did
/// not keep: a library verification's call-stream verdict reads the first
/// only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CaptureLoss {
    /// FIFO lines the reader refused: malformed, overlong, invalid UTF-8,
    /// or a fragment left without a newline when the writers closed.
    pub rejected_lines: usize,
    /// A FIFO read failed, or the last drain's byte budget left records
    /// possibly unread.
    pub fifo_unread: bool,
    /// The marker or counter budget dropped a parsed record.
    pub records_capped: bool,
    /// The sample budget dropped a sample.
    pub samples_capped: bool,
    /// stdout or stderr went past its cap, or did not close in time.
    pub output_truncated: bool,
}

impl CaptureLoss {
    /// Some FIFO record may be missing from the capture.
    pub(crate) fn stream_lost(&self) -> bool {
        self.rejected_lines > 0 || self.fifo_unread || self.records_capped
    }

    pub(crate) fn any(&self) -> bool {
        self.stream_lost() || self.samples_capped || self.output_truncated
    }

    /// Each cause, as a short phrase.
    pub(crate) fn causes(&self) -> Vec<String> {
        let mut out = self.stream_causes();
        if self.samples_capped {
            out.push("the sample limit dropped samples".to_owned());
        }
        if self.output_truncated {
            out.push("child output was cut".to_owned());
        }
        out
    }

    /// Each cause that may have lost a FIFO record, as a short phrase.
    pub(crate) fn stream_causes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.rejected_lines > 0 {
            out.push(format!(
                "the FIFO reader rejected {}",
                crate::output::count(self.rejected_lines, "line")
            ));
        }
        if self.fifo_unread {
            out.push("FIFO records may have been left unread".to_owned());
        }
        if self.records_capped {
            out.push("the marker or counter limit dropped records".to_owned());
        }
        out
    }
}

/// Every call id the tagged records of one capture named, whether or not
/// the record budgets kept the record, up to a bound. Past the bound it
/// only says that more ids arrived, so a caller that declared fewer calls
/// than the bound knows some id was not one of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CallCensus {
    pub ids: std::collections::BTreeSet<String>,
    pub overflowed: bool,
}

impl CallCensus {
    fn note(&mut self, call: &str, limit: usize) {
        if self.ids.contains(call) {
            return;
        }
        if self.ids.len() < limit {
            self.ids.insert(call.to_owned());
        } else {
            self.overflowed = true;
        }
    }
}

/// The census bound a capture uses unless its caller sets one.
const DEFAULT_CALL_CENSUS_LIMIT: usize = 4096;

/// Summary of a sidecar run.
#[derive(Debug, Clone)]
pub struct SidecarSummary {
    pub vm_hwm_kb: i64,
    pub sample_count: i32,
    pub marker_count: i32,
    pub wall_time_ms: i64,
    /// What the capture omitted, by cause.
    pub(crate) loss: CaptureLoss,
}

/// Complete sidecar data for one run of a benchmark.
pub struct SidecarData {
    pub samples: Vec<Sample>,
    /// Every FIFO record kept, in arrival order.
    pub(crate) records: Vec<Record>,
    /// The call ids tagged records named, kept or not.
    pub(crate) calls_seen: CallCensus,
    /// Each library call's boundaries and its sink's own account, from its
    /// call document; empty for every other capture, whose call streams have
    /// no known boundary.
    pub(crate) calls: Vec<CallSpan>,
    /// Marker that actually ended the child, if a configured stop marker hit.
    /// This is `None` when the child exited naturally, even if a stop was
    /// requested.
    pub fired_stop_marker: Option<String>,
    /// The termination cause assigned by the child supervisor.
    pub ending: Ending,
    pub summary: SidecarSummary,
}

impl SidecarData {
    /// The phase markers, in arrival order.
    pub(crate) fn markers(&self) -> impl Iterator<Item = &Marker> {
        self.records.iter().filter_map(|r| match r {
            Record::Marker(m) => Some(m),
            _ => None,
        })
    }

    /// The ordinary counters, in arrival order.
    pub(crate) fn counters(&self) -> impl Iterator<Item = &Counter> {
        self.records.iter().filter_map(|r| match r {
            Record::Counter(c) => Some(c),
            _ => None,
        })
    }

    /// Whether the stored trajectory omits anything the capture saw: a
    /// capture loss. Every kept record, sealing and terminal records
    /// included, has a place in sidecar.db.
    pub(crate) fn stored_incomplete(&self) -> bool {
        self.summary.loss.any()
    }
}

/// One library call's boundaries, from its call document (`brokkr man
/// library`, The artefacts): `MarkerFifo::now_us()` readings on the clock its
/// records carry, and how its sink ended by its own account. A call stream
/// with one closes its last phase at `return_us`; markers stamped after it
/// are the call's tail, not timed work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallSpan {
    pub call: String,
    pub entry_us: i64,
    pub return_us: i64,
    pub seal_us: i64,
    /// The artefact `stream.state`: `sealed`, `failed` or `open`.
    pub stream_state: String,
    /// The artefact `stream.drops`, exact.
    pub stream_drops: u64,
    /// As on [`Marker`]: 0 at capture, filled on read.
    pub run_idx: usize,
}

/// A sealing emission or terminal record as stored, with the run it came
/// from and its arrival position in that run's capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredSinkEnd {
    pub run_idx: usize,
    pub record_idx: i64,
    pub timestamp_us: i64,
    pub call: String,
    pub kind: SinkEndKind,
}

/// What a stored sink end says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SinkEndKind {
    Sealing { name: String, value: i64 },
    Terminal { end: SinkEnd, drops: u64 },
}

/// Result of a sidecar-monitored child process run.
///
/// Contains bounded captured output with per-stream completeness flags, the
/// child's ending and wall-clock time, and the sidecar profile data.
pub struct SidecarRunResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub output_cut: bool,
    pub elapsed: Duration,
    pub data: SidecarData,
}

// ---------------------------------------------------------------------------
// /proc parsing
// ---------------------------------------------------------------------------

/// Fields extracted from `/proc/{pid}/io`.
struct ProcIo {
    rchar: i64,
    wchar: i64,
    read_bytes: i64,
    write_bytes: i64,
    cancelled_write_bytes: i64,
    syscr: i64,
    syscw: i64,
}

/// Fields extracted from `/proc/{pid}/status`.
struct ProcStatus {
    vm_rss_kb: i64,
    rss_anon_kb: i64,
    rss_file_kb: i64,
    rss_shmem_kb: i64,
    vm_swap_kb: i64,
    vm_hwm_kb: i64,
    vol_cs: i64,
    nonvol_cs: i64,
}

/// Read `/proc/{pid}/stat` through the shared process parser.
fn read_proc_stat(pid: u32) -> Option<ProcStat> {
    let pid = Pid::new(pid)?;
    let path = format!("/proc/{}/stat", pid.get());
    let contents = fs::read_to_string(path).ok()?;
    let stat = parse_proc_stat(&contents)?;
    (stat.identity().pid() == pid).then_some(stat)
}

/// Parse a `/proc/{pid}/stat` row without consulting the process table.
fn parse_proc_stat(contents: &str) -> Option<ProcStat> {
    ProcStat::parse(contents)
}

/// Read and parse `/proc/{pid}/io`.
///
/// Requires same-UID or `CAP_SYS_PTRACE`. Since brokkr spawns the child
/// process, same-UID is guaranteed.
///
/// Uses `continue` (not `?`) so one unexpected line doesn't discard the
/// entire read.
fn read_proc_io(pid: u32) -> Option<ProcIo> {
    let path = format!("/proc/{pid}/io");
    let contents = fs::read_to_string(&path).ok()?;
    parse_proc_io(&contents)
}

fn parse_proc_io(contents: &str) -> Option<ProcIo> {
    let mut io = ProcIo {
        rchar: 0,
        wchar: 0,
        read_bytes: 0,
        write_bytes: 0,
        cancelled_write_bytes: 0,
        syscr: 0,
        syscw: 0,
    };

    for line in contents.lines() {
        let (key, val) = match line.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        let val: i64 = match val.trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        match key {
            "rchar" => io.rchar = val,
            "wchar" => io.wchar = val,
            "read_bytes" => io.read_bytes = val,
            "write_bytes" => io.write_bytes = val,
            "cancelled_write_bytes" => io.cancelled_write_bytes = val,
            "syscr" => io.syscr = val,
            "syscw" => io.syscw = val,
            _ => {}
        }
    }

    Some(io)
}

/// Read and parse `/proc/{pid}/status`.
fn read_proc_status(pid: u32) -> Option<ProcStatus> {
    let path = format!("/proc/{pid}/status");
    let contents = fs::read_to_string(&path).ok()?;
    parse_proc_status(&contents)
}

/// This process's peak resident set (`VmHWM`, kB) through the same status
/// parser the sampler uses. `None` when it cannot be read or is not reported.
pub(crate) fn self_peak_rss_kb() -> Option<u64> {
    let contents = fs::read_to_string("/proc/self/status").ok()?;
    let kb = parse_proc_status(&contents)?.vm_hwm_kb;
    u64::try_from(kb).ok().filter(|kb| *kb > 0)
}

fn parse_proc_status(contents: &str) -> Option<ProcStatus> {
    let mut status = ProcStatus {
        vm_rss_kb: 0,
        rss_anon_kb: 0,
        rss_file_kb: 0,
        rss_shmem_kb: 0,
        vm_swap_kb: 0,
        vm_hwm_kb: 0,
        vol_cs: 0,
        nonvol_cs: 0,
    };

    for line in contents.lines() {
        // Format: "Key:\tValue kB" or "Key:\tValue"
        let (key, rest) = match line.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        let val_str = rest.trim().trim_end_matches(" kB");
        let val: i64 = match val_str.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        match key {
            "VmRSS" => status.vm_rss_kb = val,
            "RssAnon" => status.rss_anon_kb = val,
            "RssFile" => status.rss_file_kb = val,
            "RssShmem" => status.rss_shmem_kb = val,
            "VmSwap" => status.vm_swap_kb = val,
            "VmHWM" => status.vm_hwm_kb = val,
            "voluntary_ctxt_switches" => status.vol_cs = val,
            "nonvoluntary_ctxt_switches" => status.nonvol_cs = val,
            _ => {}
        }
    }

    Some(status)
}

/// Read all /proc metrics for a pid and assemble into a `Sample`.
///
/// All three `/proc` reads must succeed for the sample to be emitted - if
/// any fails (usually because the process exited between reads), the
/// sample is dropped entirely. Earlier versions substituted zeros for a
/// failed io read, which corrupted phase deltas (the final sample's
/// `read_bytes = 0` made `last_io.0 - first_io.0` go deeply negative on
/// the tail phase). Dropping is safer: the preceding sample's values
/// still anchor the phase correctly.
fn read_proc_metrics(pid: u32, sample_idx: i32, timestamp_us: i64) -> Option<Sample> {
    let stat = read_proc_stat(pid)?;
    let io = read_proc_io(pid)?;
    let status = read_proc_status(pid)?;

    Some(Sample {
        sample_idx,
        timestamp_us,

        rss_kb: status.vm_rss_kb,
        anon_kb: status.rss_anon_kb,
        file_kb: status.rss_file_kb,
        shmem_kb: status.rss_shmem_kb,
        swap_kb: status.vm_swap_kb,
        vm_hwm_kb: status.vm_hwm_kb,
        vsize_kb: i64::try_from(stat.virtual_bytes()).ok()? / 1024,

        utime: i64::try_from(stat.user_ticks()).ok()?,
        stime: i64::try_from(stat.system_ticks()).ok()?,
        num_threads: i64::try_from(stat.num_threads()).ok()?,

        minflt: i64::try_from(stat.minor_faults()).ok()?,
        majflt: i64::try_from(stat.major_faults()).ok()?,

        rchar: io.rchar,
        wchar: io.wchar,
        read_bytes: io.read_bytes,
        write_bytes: io.write_bytes,
        cancelled_write_bytes: io.cancelled_write_bytes,
        syscr: io.syscr,
        syscw: io.syscw,

        vol_cs: status.vol_cs,
        nonvol_cs: status.nonvol_cs,
    })
}

/// Injectable clock and metric source for sidecar sampling. The production
/// implementation reads procfs; tests can supply parsed fixtures and a
/// deterministic clock without depending on their own process row.
pub(crate) trait SidecarSampler {
    fn elapsed_since(&mut self, start: Instant) -> Duration;
    fn sample(&mut self, pid: u32, sample_idx: i32, timestamp_us: i64) -> Option<Sample>;
}

pub(crate) struct ProcSidecarSampler;

impl SidecarSampler for ProcSidecarSampler {
    fn elapsed_since(&mut self, start: Instant) -> Duration {
        start.elapsed()
    }

    fn sample(&mut self, pid: u32, sample_idx: i32, timestamp_us: i64) -> Option<Sample> {
        read_proc_metrics(pid, sample_idx, timestamp_us)
    }
}

// ---------------------------------------------------------------------------
// FIFO management
// ---------------------------------------------------------------------------

/// FIFO handle: path + read end for the sidecar.
///
/// Implements `Drop` to clean up the FIFO file on panic or early error return.
pub(crate) struct SidecarFifo {
    path: PathBuf,
    reader: BufReader<File>,
    pending_line: Vec<u8>,
    pending_line_too_long: bool,
    samples_remaining: usize,
    markers_remaining: usize,
    counters_remaining: usize,
    call_census_limit: usize,
}

/// What the drains of one child's run have gathered.
#[derive(Default)]
struct FifoCapture {
    records: Vec<Record>,
    next_marker_idx: i32,
    loss: CaptureLoss,
    calls_seen: CallCensus,
}

impl FifoCapture {
    /// The markers and the ordinary counters kept.
    fn kept_counts(&self) -> (usize, usize) {
        self.records.iter().fold((0, 0), |(m, c), r| match r {
            Record::Marker(_) => (m + 1, c),
            Record::Counter(_) => (m, c + 1),
            Record::Sealing(_) | Record::Terminal(_) => (m, c),
        })
    }
}

/// The pipe capacity asked for each FIFO instance, so a producer whose
/// reader drains once a tick drops fewer lines; a refusal keeps the
/// kernel's default and changes nothing else.
const FIFO_PIPE_BYTES: libc::c_int = 1024 * 1024;

impl SidecarFifo {
    /// Create the FIFO and open the read end.
    ///
    /// Must be called BEFORE spawning the child so the reader exists when
    /// the child tries to open the write end with `O_NONBLOCK`.
    pub(crate) fn create(
        scratch_dir: &Path,
        tunables: &MeasurementTunables,
    ) -> Result<Self, DevError> {
        let pid = std::process::id();
        let path = scratch_dir.join(format!(".sidecar-{pid}.fifo"));

        // Remove stale FIFO if it exists.
        drop(fs::remove_file(&path));

        let c_path = std::ffi::CString::new(
            path.to_str()
                .ok_or_else(|| DevError::Config("scratch path not UTF-8".into()))?,
        )
        .map_err(|e| DevError::Config(format!("invalid FIFO path: {e}")))?;

        let ret = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        if ret != 0 {
            return Err(DevError::Io(std::io::Error::last_os_error()));
        }

        let file = Self::open_read_end(&path)?;

        // A marker from an earlier run in this hold must not be shown as this
        // run's until this run emits one.
        crate::lockfile::activity::note_marker(None);

        Ok(Self {
            path,
            reader: BufReader::new(file),
            pending_line: Vec::with_capacity(tunables.max_fifo_line_bytes.min(256)),
            pending_line_too_long: false,
            samples_remaining: tunables.max_samples,
            markers_remaining: tunables.max_markers,
            counters_remaining: tunables.max_counters,
            call_census_limit: DEFAULT_CALL_CENSUS_LIMIT,
        })
    }

    /// Bound the census of call ids each capture keeps. A caller that
    /// declared `n` calls sets `n + 1`: a census that overflows it proves
    /// an id beyond the declared ones.
    pub(crate) fn limit_call_census(&mut self, limit: usize) {
        self.call_census_limit = limit;
    }

    /// Open the FIFO read end with `O_RDONLY|O_NONBLOCK`, asking for a
    /// larger pipe (each open of a FIFO with no other end open is a fresh
    /// pipe, so `reopen` asks again).
    fn open_read_end(path: &Path) -> Result<File, DevError> {
        use std::os::fd::AsRawFd as _;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(DevError::Io)?;
        // A refusal (a per-user pipe limit) leaves the default capacity.
        let _: libc::c_int =
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETPIPE_SZ, FIFO_PIPE_BYTES) };
        Ok(file)
    }

    /// Reopen the read end after a child exits.
    ///
    /// When the child closes the write end, the reader gets EOF. For
    /// multi-run benchmarks, we must reopen to accept the next child's
    /// write end.
    pub(crate) fn reopen(&mut self) -> Result<(), DevError> {
        let file = Self::open_read_end(&self.path)?;
        self.reader = BufReader::new(file);
        self.pending_line.clear();
        self.pending_line_too_long = false;
        Ok(())
    }

    /// Drain pending lines from the FIFO, parsing markers and counters, until
    /// it is empty, closed, or `budget` bytes were consumed (discarded bytes
    /// of an overlong line included), so a child writing without pause cannot
    /// keep the supervisor from its shutdown and deadline checks. A line cut
    /// by the budget resumes on the next drain. Returns the stop marker that
    /// matched, if any, and why draining stopped.
    ///
    /// Non-blocking: returns immediately if no data is available.
    ///
    /// Protocol (see [`parse_fifo_record`]):
    /// - `<timestamp_us> <name>\n` - phase marker
    /// - `<timestamp_us> @<name>=<value>\n` - counter
    /// - either body after `%<call_id> ` - the same record in that call's stream
    /// - `@@<name>=<value>` and `!sealed drops=N`/`!failed drops=N` after a
    ///   tag - a call sink's sealing emission and terminal record
    fn drain(
        &mut self,
        capture: &mut FifoCapture,
        stop_spec: Option<&StopSpec>,
        max_line_bytes: usize,
        budget: usize,
    ) -> (Option<String>, DrainStop) {
        let mut rejected_lines = 0usize;
        let mut last_name: Option<String> = None;
        let mut preferred_stop: Option<String> = None;
        let mut fallback_stop: Option<String> = None;
        let mut budget = budget;
        let census_limit = self.call_census_limit;
        let stop = loop {
            match self.read_bounded_line(max_line_bytes, &mut budget) {
                Ok(BoundedLine::Pending) => break DrainStop::Closed,
                Ok(BoundedLine::Budget) => break DrainStop::Budget,
                // An overlong record, or one cut off by the writers closing:
                // damage, not data.
                Ok(BoundedLine::TooLong | BoundedLine::Fragment) => rejected_lines += 1,
                Ok(BoundedLine::Line(line)) => {
                    let Ok(line) = std::str::from_utf8(&line) else {
                        rejected_lines += 1;
                        continue;
                    };
                    let parsed = parse_fifo_record(line);
                    // Every call id a tagged record names is counted before
                    // any budget can drop the record.
                    if let Some(call) = parsed.call() {
                        capture.calls_seen.note(call, census_limit);
                    }
                    if let FifoRecord::Marker { call, name, .. } = &parsed {
                        last_name = Some((*name).to_owned());
                        // `--stop` is scoped to the process stream: a
                        // call's marker never kills the whole scenario.
                        if let Some(spec) = stop_spec
                            && call.is_none()
                        {
                            match spec.match_priority(name) {
                                Some(true) if preferred_stop.is_none() => {
                                    preferred_stop = Some((*name).to_owned());
                                }
                                Some(false) if fallback_stop.is_none() => {
                                    fallback_stop = Some((*name).to_owned());
                                }
                                _ => {}
                            }
                        }
                    }
                    match parsed {
                        FifoRecord::Blank => {}
                        FifoRecord::Rejected => rejected_lines += 1,
                        record => {
                            let marker_idx = capture.next_marker_idx;
                            if let Some(owned) = record.into_record(marker_idx) {
                                if matches!(owned, Record::Marker(_)) {
                                    capture.next_marker_idx = marker_idx.saturating_add(1);
                                }
                                self.keep(capture, owned);
                            }
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    break DrainStop::Drained;
                }
                Err(e) => {
                    // Records may be lost with the read: damage, not data.
                    capture.loss.fifo_unread = true;
                    console::warn(&format!("sidecar FIFO read failed: {e}"));
                    break DrainStop::Failed;
                }
            }
        };
        if rejected_lines > 0 {
            // Stored, so a later query still says the trajectory is damaged.
            capture.loss.rejected_lines =
                capture.loss.rejected_lines.saturating_add(rejected_lines);
            console::warn(&format!(
                "sidecar FIFO rejected {rejected_lines} malformed lines"
            ));
        }
        if let Some(name) = last_name {
            crate::lockfile::activity::note_marker(Some(&name));
        }
        (preferred_stop.or(fallback_stop), stop)
    }

    /// The last drain, once the child ended, under the final budget. An
    /// exhausted budget may leave records unread (possible, not proven
    /// drops: nothing is read past it to find out), which marks the capture
    /// incomplete.
    fn drain_after_exit(&mut self, capture: &mut FifoCapture, max_line_bytes: usize) {
        self.drain_final(capture, max_line_bytes, FINAL_DRAIN_BUDGET);
    }

    /// [`drain_after_exit`](Self::drain_after_exit) under `budget`.
    fn drain_final(&mut self, capture: &mut FifoCapture, max_line_bytes: usize, budget: usize) {
        let (_, stop) = self.drain(capture, None, max_line_bytes, budget);
        // A line still pending (a writer holds the FIFO open mid-line) is
        // never read: the reader is dropped or reopened next.
        let pending = !self.pending_line.is_empty() || self.pending_line_too_long;
        if stop == DrainStop::Budget || pending {
            capture.loss.fifo_unread = true;
        }
    }

    /// Keep `record` if its budget allows (sealing emissions count as
    /// counters, terminal records as markers), else mark the loss.
    fn keep(&mut self, capture: &mut FifoCapture, record: Record) {
        let remaining = match record {
            Record::Marker(_) | Record::Terminal(_) => &mut self.markers_remaining,
            Record::Counter(_) | Record::Sealing(_) => &mut self.counters_remaining,
        };
        if *remaining > 0 {
            *remaining -= 1;
            capture.records.push(record);
        } else {
            capture.loss.records_capped = true;
        }
    }

    /// Read one FIFO line without allowing a child to make the sidecar's
    /// temporary line buffer grow beyond `max_bytes`. An overlong record is
    /// consumed through its newline and reported as `TooLong`. At most
    /// `budget` bytes are consumed, and it is decreased by what was: a line
    /// the budget cuts is kept pending and `Budget` returned, and a newline
    /// counts only when it lies within the budget.
    fn read_bounded_line(
        &mut self,
        max_bytes: usize,
        budget: &mut usize,
    ) -> std::io::Result<BoundedLine> {
        loop {
            if *budget == 0 {
                return Ok(BoundedLine::Budget);
            }
            let next = {
                let available = self.reader.fill_buf()?;
                if available.is_empty() {
                    None
                } else {
                    let window = available
                        .get(..(*budget).min(available.len()))
                        .unwrap_or_default();
                    let line_end = window.iter().position(|byte| *byte == b'\n');
                    let consume = line_end.map_or(window.len(), |index| index + 1);
                    let body_len = line_end.unwrap_or(consume);
                    let copy_len = if self.pending_line_too_long {
                        0
                    } else {
                        body_len.min(max_bytes.saturating_sub(self.pending_line.len()))
                    };
                    Some((consume, line_end.is_some(), body_len, copy_len))
                }
            };
            let Some((consume, line_end, body_len, copy_len)) = next else {
                return Ok(self.finish_pending_line(false));
            };
            if copy_len > 0 {
                // The buffer borrow from `fill_buf` ends before `consume`.
                let available = self.reader.fill_buf()?;
                self.pending_line.extend_from_slice(&available[..copy_len]);
            }
            let line_bytes_before = self.pending_line.len().saturating_sub(copy_len);
            if !self.pending_line_too_long && body_len > max_bytes.saturating_sub(line_bytes_before)
            {
                self.pending_line_too_long = true;
                self.pending_line.clear();
            }
            self.reader.consume(consume);
            *budget = budget.saturating_sub(consume);
            if line_end {
                return Ok(self.finish_pending_line(true));
            }
        }
    }

    fn finish_pending_line(&mut self, complete: bool) -> BoundedLine {
        if self.pending_line_too_long {
            self.pending_line_too_long = false;
            self.pending_line.clear();
            BoundedLine::TooLong
        } else if self.pending_line.is_empty() {
            if complete {
                BoundedLine::Line(Vec::new())
            } else {
                BoundedLine::Pending
            }
        } else if complete {
            BoundedLine::Line(std::mem::take(&mut self.pending_line))
        } else {
            // An empty read on this O_NONBLOCK FIFO means every writer has
            // closed, so a record still missing its newline never completes.
            self.pending_line.clear();
            BoundedLine::Fragment
        }
    }

    fn keep_sample(&mut self) -> bool {
        if self.samples_remaining == 0 {
            return false;
        }
        self.samples_remaining -= 1;
        true
    }

    /// The FIFO path, for passing to child processes via environment variable.
    pub(crate) fn path_str(&self) -> Result<&str, DevError> {
        self.path
            .to_str()
            .ok_or_else(|| DevError::Config("FIFO path not UTF-8".into()))
    }
}

enum BoundedLine {
    /// The writers closed with nothing pending.
    Pending,
    /// The drain's byte budget ran out; any partial line stays pending.
    Budget,
    TooLong,
    /// Bytes left without a newline when the writers closed.
    Fragment,
    Line(Vec<u8>),
}

/// Why one drain stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainStop {
    /// Nothing more to read for now; writers are still open.
    Drained,
    /// Every writer closed and nothing is left.
    Closed,
    /// The byte budget ran out; records may remain unread.
    Budget,
    /// A read failed; the capture is marked incomplete.
    Failed,
}

/// Bytes one supervisor tick may consume from the FIFO.
const TICK_DRAIN_BUDGET: usize = 4 * 1024 * 1024;

/// Bytes the drain after the child ended may consume. A descendant still
/// writing past it leaves records unread, which marks the capture incomplete.
const FINAL_DRAIN_BUDGET: usize = 16 * 1024 * 1024;

impl Drop for SidecarFifo {
    fn drop(&mut self) {
        drop(fs::remove_file(&self.path));
        crate::lockfile::activity::note_marker(None);
    }
}

/// Get current CLOCK_MONOTONIC time in nanoseconds.
#[cfg(test)]
fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

// ---------------------------------------------------------------------------
// Sidecar loop
// ---------------------------------------------------------------------------

/// A parsed `--stop` request. Resolve the spelling once before entering the
/// sidecar loop, then compare each fresh FIFO drain against the requested name.
enum StopSpec {
    /// An explicit marker name, such as `FOO_END`.
    Exact(String),
    /// The span-close spelling `-FOO`, resolved to `FOO_END`.
    SpanEnd(String),
    /// A bare name such as `FOO`: prefer a verbatim marker, then `FOO_END`.
    Lenient { exact: String, span_end: String },
}

impl StopSpec {
    fn parse(raw: &str) -> Self {
        if let Some(suffix) = raw.strip_prefix('-') {
            return Self::SpanEnd(format!("{suffix}_END"));
        }
        if raw.ends_with("_END") {
            return Self::Exact(raw.to_owned());
        }
        Self::Lenient {
            exact: raw.to_owned(),
            span_end: format!("{raw}_END"),
        }
    }

    /// Whether a marker name matches the preferred or fallback spelling.
    /// Lenient requests prefer an exact marker over the `_END` fallback even
    /// when the fallback arrived first in the same FIFO drain.
    fn match_priority(&self, name: &str) -> Option<bool> {
        match self {
            Self::Exact(expected) | Self::SpanEnd(expected) => (name == expected).then_some(true),
            Self::Lenient { exact, .. } if name == exact => Some(true),
            Self::Lenient { span_end, .. } if name == span_end => Some(false),
            Self::Lenient { .. } => None,
        }
    }
}

/// Say why the supervisor ended the child early, when it did.
fn announce_ending(ending: &Ending) {
    if let Ending::Deadline(limit) = ending {
        console::sidecar_msg(&format!(
            "child exceeded its {} deadline, killing it",
            console::format_duration(*limit)
        ));
    } else if matches!(ending, Ending::Interrupted) {
        console::sidecar_msg("shutdown requested, killing child");
    }
}

/// The console line announcing that a stop marker fired and the child is dying.
fn stop_marker_message(requested: &str, matched: &str) -> String {
    if matched == requested {
        format!("stop marker \"{requested}\" received, killing child")
    } else {
        format!("stop marker \"{requested}\" → \"{matched}\" received, killing child")
    }
}

/// The per-run summary: `counts` is samples, markers and counters in order.
fn run_summary_line(
    run_idx: usize,
    counts: [usize; 3],
    wall_time_ms: impl std::fmt::Display,
) -> String {
    let [samples, markers, counters] = counts;
    format!(
        "sidecar run {}: {samples} samples, {markers} markers, {counters} counters, {wall_time_ms}ms",
        run_idx + 1,
    )
}

fn warn_if_truncated(loss: &CaptureLoss) {
    if loss.any() {
        console::warn(&format!(
            "sidecar capture is incomplete: {}",
            loss.causes().join("; ")
        ));
    }
}

/// Run one sidecar-observed child, killing its process group once it has run
/// longer than `deadline` (measured from the caller's `start`) when one is
/// given. Process polling, stop/deadline killing,
/// output draining, and waiting are owned by the shared child supervisor;
/// this observer only samples procfs and drains the marker FIFO.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_sidecar_with_deadline(
    child_process: Child,
    fifo: &mut SidecarFifo,
    run_idx: usize,
    start: Instant,
    stop_marker: Option<&str>,
    deadline: Option<Duration>,
    tunables: &MeasurementTunables,
    capture_cap: child::CaptureCap,
    sampler: &mut dyn SidecarSampler,
) -> SidecarRunResult {
    // The shared child supervisor owns process polling and process-group
    // signals. This guard only makes the sidecar's own shutdown path observe
    // the process-wide termination request.
    let shutdown_guard = crate::shutdown::SigtermGuard::install();
    let pid = child_process.id();

    let mut samples = Vec::new();
    let mut capture = FifoCapture::default();
    let mut last_hwm = 0;
    let mut sample_idx = 0;
    let requested_stop = stop_marker.unwrap_or_default().to_owned();
    let stop_spec = stop_marker.map(StopSpec::parse);
    let mut fired_stop_marker = None;

    console::detail(&format!(
        "sidecar attached to pid {pid}, run {}",
        run_idx + 1
    ));

    let tick_interval = tunables.sample_interval;
    let limit = deadline.map_or(ChildLimit::None, ChildLimit::Wall);
    let label = format!("sidecar run {}", run_idx + 1);
    let options = child::ChildOptions {
        poll_interval: tick_interval,
        capture_cap,
        ..child::ChildOptions::MEASUREMENT
    };
    let run = child::supervise_child(
        child_process,
        &label,
        start,
        limit,
        crate::shutdown::ChildIsolation::ProcessGroup(&shutdown_guard),
        options,
        || {
            let elapsed_us =
                i64::try_from(sampler.elapsed_since(start).as_micros()).unwrap_or(i64::MAX);
            if let Some(sample) = sampler.sample(pid, sample_idx, elapsed_us) {
                sample_idx = sample_idx.saturating_add(1);
                last_hwm = last_hwm.max(sample.vm_hwm_kb);
                if fifo.keep_sample() {
                    samples.push(sample);
                } else {
                    capture.loss.samples_capped = true;
                }
            }

            let (matched, _) = fifo.drain(
                &mut capture,
                stop_spec.as_ref(),
                tunables.max_fifo_line_bytes,
                TICK_DRAIN_BUDGET,
            );
            let matched = matched?;
            fired_stop_marker = Some(matched.clone());
            console::sidecar_msg(&stop_marker_message(&requested_stop, &matched));
            Some(matched)
        },
    );

    fifo.drain_after_exit(&mut capture, tunables.max_fifo_line_bytes);
    let (stdout_truncated, stderr_truncated) = (run.stdout_truncated, run.stderr_truncated);
    capture.loss.output_truncated = stdout_truncated || stderr_truncated || run.output_cut;
    warn_if_truncated(&capture.loss);
    let (marker_total, counter_total) = capture.kept_counts();
    announce_ending(&run.ending);

    // The shared nearest-millisecond rule, so the sidecar summary agrees with
    // the results row's rounded wall.
    let wall_time_ms = crate::harness::elapsed_to_ms(&run.elapsed);
    console::detail(&run_summary_line(
        run_idx,
        [samples.len(), marker_total, counter_total],
        wall_time_ms,
    ));
    let count = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
    let (sample_count, marker_count) = (count(samples.len()), count(marker_total));
    SidecarRunResult {
        stdout: run.stdout,
        stderr: run.stderr,
        stdout_truncated,
        stderr_truncated,
        output_cut: run.output_cut,
        elapsed: run.elapsed,
        data: SidecarData {
            samples,
            records: capture.records,
            calls_seen: capture.calls_seen,
            calls: Vec::new(),
            fired_stop_marker,
            ending: run.ending,
            summary: SidecarSummary {
                vm_hwm_kb: last_hwm,
                sample_count,
                marker_count,
                wall_time_ms,
                loss: capture.loss,
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;

    #[test]
    fn parse_proc_stat_self() {
        let pid = std::process::id();
        let stat = read_proc_stat(pid);
        assert!(stat.is_some(), "should be able to read own /proc/stat");
        let stat = stat.unwrap();
        assert!(stat.num_threads() >= 1);
        assert!(stat.virtual_bytes() > 0);
    }

    #[test]
    fn parse_proc_status_self() {
        let pid = std::process::id();
        let status = read_proc_status(pid);
        assert!(status.is_some(), "should be able to read own /proc/status");
        let status = status.unwrap();
        assert!(status.vm_rss_kb > 0);
        assert!(status.vm_hwm_kb > 0);
    }

    #[test]
    fn parse_proc_io_self() {
        let pid = std::process::id();
        let io = read_proc_io(pid);
        assert!(io.is_some(), "should be able to read own /proc/io");
        let io = io.unwrap();
        assert!(io.rchar > 0);
    }

    #[test]
    fn full_sample_self() {
        let pid = std::process::id();
        let sample = read_proc_metrics(pid, 0, 12345);
        assert!(sample.is_some());
        let sample = sample.unwrap();
        assert_eq!(sample.sample_idx, 0);
        assert_eq!(sample.timestamp_us, 12345);
        assert!(sample.rss_kb > 0);
        assert!(sample.utime >= 0);
        assert!(sample.num_threads >= 1);
    }

    #[test]
    fn nonexistent_pid_returns_none() {
        let stat = read_proc_stat(0);
        assert!(stat.is_none());
    }

    #[test]
    fn monotonic_clock_advances() {
        let t1 = monotonic_ns();
        let t2 = monotonic_ns();
        assert!(t2 >= t1);
    }

    /// The wire lines are pbfhogg's own goldens (`line_formats_marker_exactly`
    /// and `line_formats_counter_exactly` in pbfhogg `src/debug.rs`), so a
    /// format change on either side fails a test here or there.
    #[test]
    fn fifo_parses_pbfhogg_golden_lines() {
        use std::io::Write;
        let dir = crate::test_scratch::scratch("sidecar", "pbfhogg_goldens");
        let mut fifo = SidecarFifo::create(&dir, &DEFAULT_MEASUREMENT_TUNABLES).unwrap();
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo.path).unwrap();
        writer
            .write_all(b"42 PASS1_END\n42 @nodes=-7\nPASS1_END\n43 @nodes\n")
            .unwrap();
        drop(writer);

        let mut capture = FifoCapture::default();
        fifo.drain(
            &mut capture,
            None,
            DEFAULT_MEASUREMENT_TUNABLES.max_fifo_line_bytes,
            TICK_DRAIN_BUDGET,
        );

        let [Record::Marker(marker), Record::Counter(counter)] = capture.records.as_slice() else {
            panic!("{:?}", capture.records);
        };
        assert_eq!(marker.marker_idx, 0);
        assert_eq!(marker.timestamp_us, 42);
        assert_eq!(marker.name, "PASS1_END");
        assert_eq!(counter.timestamp_us, 42);
        assert_eq!(counter.name, "nodes");
        assert_eq!(counter.value, -7);
        assert_eq!(capture.next_marker_idx, 1);
        assert_eq!(
            capture.loss.rejected_lines, 2,
            "the two malformed lines mark the capture damaged"
        );
        assert!(capture.loss.stream_lost());
    }

    #[test]
    fn tagged_sealing_and_terminal_records_parse_and_untagged_lines_do_not_change() {
        assert_eq!(
            parse_fifo_record("9 %a @@writer_blocks=-3"),
            FifoRecord::Sealing {
                ts: 9,
                call: "a",
                name: "writer_blocks",
                value: -3
            }
        );
        assert_eq!(
            parse_fifo_record("9 %a !sealed drops=0"),
            FifoRecord::Terminal {
                ts: 9,
                call: "a",
                end: SinkEnd::Sealed,
                drops: 0
            }
        );
        assert_eq!(
            parse_fifo_record("9 %a !failed drops=18446744073709551615"),
            FifoRecord::Terminal {
                ts: 9,
                call: "a",
                end: SinkEnd::Failed,
                drops: u64::MAX
            }
        );
        // The process stream keeps its old reading.
        assert_eq!(
            parse_fifo_record("9 @@x=1"),
            FifoRecord::Counter {
                ts: 9,
                call: None,
                name: "@x",
                value: 1
            }
        );
        assert_eq!(
            parse_fifo_record("9 !sealed drops=0"),
            FifoRecord::Marker {
                ts: 9,
                call: None,
                name: "!sealed drops=0"
            }
        );
        let long = format!("9 %a @@{}=1", "n".repeat(MAX_SEALING_NAME_BYTES + 1));
        let edge = format!("9 %a @@{}=1", "n".repeat(MAX_SEALING_NAME_BYTES));
        assert!(matches!(
            parse_fifo_record(&edge),
            FifoRecord::Sealing { .. }
        ));
        for bad in [
            "9 %a @@x",
            "9 %a @@=1",
            "9 %a @@x=y",
            "9 %a !sealed",
            "9 %a !sealed drops=",
            "9 %a !sealed drops=01",
            "9 %a !sealed drops=+1",
            "9 %a !sealed drops=-1",
            "9 %a !sealed drops=18446744073709551616",
            "9 %a !sealed  drops=0",
            "9 %a !open drops=0",
            "9 %a !done",
            long.as_str(),
        ] {
            assert_eq!(parse_fifo_record(bad), FifoRecord::Rejected, "{bad}");
        }
    }

    #[test]
    fn a_capture_keeps_one_arrival_order_and_counts_every_call_it_saw() {
        use std::io::Write;
        let dir = crate::test_scratch::scratch("sidecar", "fifo_arrival");
        let mut fifo = SidecarFifo::create(&dir, &DEFAULT_MEASUREMENT_TUNABLES).unwrap();
        fifo.limit_call_census(2);
        fifo.counters_remaining = 1;
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo.path).unwrap();
        writer
            .write_all(b"5 %a @n=1\n4 %a GO\n6 %a @@n=1\n7 %a !sealed drops=0\n8 %b @m=2\n9 %c X\n")
            .unwrap();
        drop(writer);
        let mut capture = FifoCapture::default();
        fifo.drain(&mut capture, None, 1024, TICK_DRAIN_BUDGET);
        let wire: Vec<String> = capture.records.iter().map(Record::wire).collect();
        assert_eq!(
            wire,
            ["5 %a @n=1", "4 %a GO", "7 %a !sealed drops=0", "9 %c X"],
            "the sealing emission and %b's counter went past the counter budget"
        );
        assert!(capture.loss.records_capped);
        assert_eq!(
            capture.calls_seen.ids.iter().collect::<Vec<_>>(),
            ["a", "b"],
            "a dropped record's call is still counted"
        );
        assert!(capture.calls_seen.overflowed, "a third id passed the bound");
    }

    /// A read error ends the drain and marks records possibly unread.
    #[test]
    fn a_fifo_read_error_marks_the_capture_incomplete() {
        let dir = crate::test_scratch::scratch("sidecar", "fifo_read_error");
        let mut fifo = SidecarFifo::create(&dir, &DEFAULT_MEASUREMENT_TUNABLES).unwrap();
        // Reading a directory fails with EISDIR.
        fifo.reader = BufReader::new(File::open(&dir).unwrap());
        let mut capture = FifoCapture::default();
        let (_, stop) = fifo.drain(&mut capture, None, 1024, TICK_DRAIN_BUDGET);
        assert_eq!(stop, DrainStop::Failed);
        assert!(capture.loss.fifo_unread && capture.loss.stream_lost());
    }

    #[test]
    fn fifo_records_parse_by_stream() {
        assert_eq!(
            parse_fifo_record("7 %sort-1 PASS1_END\n"),
            FifoRecord::Marker {
                ts: 7,
                call: Some("sort-1"),
                name: "PASS1_END"
            }
        );
        assert_eq!(
            parse_fifo_record("7 %a.b_c @n=-3"),
            FifoRecord::Counter {
                ts: 7,
                call: Some("a.b_c"),
                name: "n",
                value: -3
            }
        );
        assert_eq!(
            parse_fifo_record("7 PASS1 END"),
            FifoRecord::Marker {
                ts: 7,
                call: None,
                name: "PASS1 END"
            }
        );
        // A malformed tagged record is rejected, never an untagged marker.
        for bad in [
            "7 %",
            "7 %sort",
            "7 % NAME",
            "7 %so/rt NAME",
            "7 %sort @n=x",
            "7 @=1",
            "x NAME",
        ] {
            assert_eq!(parse_fifo_record(bad), FifoRecord::Rejected, "{bad}");
        }
        let long = format!("7 %{} NAME", "a".repeat(MAX_CALL_ID_BYTES + 1));
        assert_eq!(parse_fifo_record(&long), FifoRecord::Rejected);
        assert_eq!(parse_fifo_record("  \n"), FifoRecord::Blank);
    }

    #[test]
    fn fifo_discards_a_fragment_left_at_writer_close() {
        use std::io::Write;
        let dir = crate::test_scratch::scratch("sidecar", "fifo_fragment");
        let mut fifo = SidecarFifo::create(&dir, &DEFAULT_MEASUREMENT_TUNABLES).unwrap();
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo.path).unwrap();
        writer.write_all(b"1 %a START\n2 %a ST").unwrap();
        drop(writer);

        let mut capture = FifoCapture::default();
        fifo.drain(
            &mut capture,
            None,
            DEFAULT_MEASUREMENT_TUNABLES.max_fifo_line_bytes,
            TICK_DRAIN_BUDGET,
        );
        assert_eq!(capture.records.len(), 1);
        assert_eq!(capture.records[0].call(), Some("a"));
        assert_eq!(capture.loss.rejected_lines, 1);
    }

    fn marker_names(capture: FifoCapture) -> Vec<String> {
        capture
            .records
            .into_iter()
            .filter_map(|r| match r {
                Record::Marker(m) => Some(m.name),
                _ => None,
            })
            .collect()
    }

    /// One budgeted drain: the marker names it parsed, why it stopped, and
    /// whether it marked a loss.
    fn drain_names(
        fifo: &mut SidecarFifo,
        budget: usize,
        max_line: usize,
    ) -> (Vec<String>, DrainStop, bool) {
        let mut capture = FifoCapture::default();
        let (_, stop) = fifo.drain(&mut capture, None, max_line, budget);
        let lost = capture.loss.stream_lost();
        (marker_names(capture), stop, lost)
    }

    /// The last drain stops at its budget with records left behind it, or
    /// with a line pending while a writer stays open, and marks the capture
    /// incomplete; one that empties the FIFO does not.
    #[test]
    fn the_last_drain_is_bounded_and_marks_what_it_left_unread() {
        use std::io::Write;
        let dir = crate::test_scratch::scratch("sidecar", "fifo_final_budget");
        let mut fifo = SidecarFifo::create(&dir, &DEFAULT_MEASUREMENT_TUNABLES).unwrap();
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo.path).unwrap();
        writer.write_all(b"1 A\n2 B\n").unwrap();
        let mut capture = FifoCapture::default();
        fifo.drain_final(&mut capture, 1024, 4);
        assert_eq!(capture.records.len(), 1);
        assert!(
            capture.loss.fifo_unread,
            "records left past the budget may be unread"
        );

        // A line left without its newline while a writer stays open is
        // never read either.
        writer.write_all(b"3 C").unwrap();
        let mut capture = FifoCapture::default();
        fifo.drain_after_exit(&mut capture, 1024);
        assert_eq!(capture.records.len(), 1, "B is read");
        assert!(capture.loss.fifo_unread, "the pending 3 C is lost");

        drop(writer);
        fifo.reopen().unwrap();
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo.path).unwrap();
        writer.write_all(b"4 D\n").unwrap();
        drop(writer);
        let mut capture = FifoCapture::default();
        fifo.drain_after_exit(&mut capture, 1024);
        assert_eq!(capture.records.len(), 1);
        assert!(!capture.loss.any(), "an emptied FIFO is complete");
    }

    /// The budget counts every byte consumed and stops the drain at once,
    /// even right after a completed line; a line it cuts resumes intact on
    /// the next drain, and a newline past the budget does not complete it.
    #[test]
    fn a_drain_stops_at_its_byte_budget_and_resumes_the_cut_line() {
        use std::io::Write;
        let dir = crate::test_scratch::scratch("sidecar", "fifo_budget");
        let mut fifo = SidecarFifo::create(&dir, &DEFAULT_MEASUREMENT_TUNABLES).unwrap();
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo.path).unwrap();
        // "1 A\n" is 4 bytes, "2 BB\n" 5, "3 CCC\n" 6.
        writer.write_all(b"1 A\n2 BB\n3 CCC\n").unwrap();
        let line = 1024;

        // Budget ending on the first newline: one line, then stop.
        assert_eq!(
            drain_names(&mut fifo, 4, line),
            (vec!["A".to_owned()], DrainStop::Budget, false)
        );
        // Budget ending just before the next newline: nothing completes.
        assert_eq!(
            drain_names(&mut fifo, 4, line),
            (vec![], DrainStop::Budget, false)
        );
        // The cut line resumes; a budget past its newline completes it and
        // the next, and stops inside the third.
        assert_eq!(
            drain_names(&mut fifo, 3, line),
            (vec!["BB".to_owned()], DrainStop::Budget, false)
        );
        assert_eq!(
            drain_names(&mut fifo, 100, line),
            (vec!["CCC".to_owned()], DrainStop::Drained, false)
        );

        // A partial line pending at WouldBlock is kept, not a fragment.
        writer.write_all(b"4 DD").unwrap();
        assert_eq!(
            drain_names(&mut fifo, 100, line),
            (vec![], DrainStop::Drained, false)
        );
        writer.write_all(b"DD\n").unwrap();
        assert_eq!(
            drain_names(&mut fifo, 100, line),
            (vec!["DDDD".to_owned()], DrainStop::Drained, false)
        );

        // An overlong line spanning several budgets is discarded whole.
        writer
            .write_all(format!("5 {}\n6 E\n", "x".repeat(40)).as_bytes())
            .unwrap();
        assert_eq!(
            drain_names(&mut fifo, 10, 16),
            (vec![], DrainStop::Budget, false)
        );
        assert_eq!(
            drain_names(&mut fifo, 10, 16),
            (vec![], DrainStop::Budget, false)
        );
        let (names, stop, truncated) = drain_names(&mut fifo, 100, 16);
        assert_eq!((names, stop), (vec!["E".to_owned()], DrainStop::Drained));
        assert!(truncated, "the overlong line marks the capture damaged");

        drop(writer);
        assert_eq!(
            drain_names(&mut fifo, 100, line),
            (vec![], DrainStop::Closed, false)
        );
    }

    fn marker(idx: i32, ts: i64, name: &str) -> Marker {
        Marker {
            marker_idx: idx,
            timestamp_us: ts,
            name: name.to_owned(),
            run_idx: 0,
            call: None,
        }
    }

    #[test]
    fn timeline_orders_by_time_and_leaves_a_call_end_open() {
        // Arrival order 0, 1, 2 but time order B, A, C.
        let markers = [marker(0, 20, "A"), marker(1, 10, "B"), marker(2, 30, "C")];
        let open = Timeline::open(&markers);
        let segments = open.segments();
        let names: Vec<_> = segments.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["B", "A", "C"]);
        assert_eq!(segments[0].end_us, Some(20));
        assert_eq!(segments[2].end_us, None, "no call boundary is known");
        assert_eq!(open.resolve_phase_range("C"), Some((30, None)));
        assert_eq!(open.resolve_phase_range("B"), Some((10, Some(20))));
    }

    #[test]
    fn read_proc_io_tolerates_extra_lines() {
        let io = parse_proc_io(concat!(
            "rchar: 12\n",
            "wchar: 34\n",
            "unexpected line\n",
            "read_bytes: 56\n",
            "write_bytes: invalid\n",
            "cancelled_write_bytes: 7\n",
            "syscr: 8\n",
            "syscw: 9\n",
            "future_counter: 100\n",
        ))
        .expect("unexpected lines are ignored");
        assert_eq!(io.rchar, 12);
        assert_eq!(io.wchar, 34);
        assert_eq!(io.read_bytes, 56);
        assert_eq!(io.write_bytes, 0, "invalid values leave the default");
        assert_eq!(io.cancelled_write_bytes, 7);
        assert_eq!(io.syscr, 8);
        assert_eq!(io.syscw, 9);
    }
}
