//! What a live hold says about the work it is doing, held in memory and served
//! by its control socket: the activities the holder has open, the launch
//! cohorts its children carry in `BROKKR_EXEC`, the last sidecar marker, and
//! the root narration (the latest status line no activity claimed).
//!
//! One [`Observed`] exists per acquisition, created with the control service
//! and shared by the `LockInner`, the service and the collector. Nothing here
//! is ever written to the lock file: the lock file is the auth carrier and the
//! `kill --hard` target list, and this state is descriptive. Everything dies
//! with the process, so there is no staleness to undo after a crash - a dead
//! holder has no socket to ask.
//!
//! Every value is an observation with a time attached. An activity is open
//! from its guard's creation to its drop; a launch record maps a cohort
//! sequence number to the activity that was innermost on the spawning thread
//! at launch, captured as text then, so a later reader never attaches a
//! process to whatever step happens to be running now.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::AcqId;

/// The inherited environment variable naming a child's launch cohort:
/// `<acquisition id>.<sequence>`.
pub(crate) const EXEC_ENV: &str = "BROKKR_EXEC";

/// The most activities kept open at once. A runaway would otherwise grow the
/// table without bound; past it a new activity is simply not recorded.
const MAX_ACTIVITIES: usize = 512;
/// The most launch records kept. Older cohorts lose their label (rendered as
/// "older launch, label not kept"), never their membership.
const MAX_LAUNCHES: usize = 4096;
/// Longest label or narration kept, in chars.
const MAX_TEXT: usize = 200;

/// One open activity.
#[derive(Clone)]
pub(crate) struct ActivityRecord {
    pub(crate) id: u64,
    pub(crate) parent: Option<u64>,
    pub(crate) label: String,
    pub(crate) started: Instant,
    pub(crate) progress: Option<(u64, u64)>,
    pub(crate) narration: Option<String>,
}

/// One launch cohort: what was spawned and under which activity.
#[derive(Clone)]
pub(crate) struct LaunchRecord {
    pub(crate) seq: u64,
    pub(crate) label: String,
    pub(crate) under: Option<String>,
}

#[derive(Default)]
struct Activities {
    next_id: u64,
    open: Vec<ActivityRecord>,
}

/// The per-acquisition observation state.
pub(crate) struct Observed {
    acq: AcqId,
    activities: Mutex<Activities>,
    launches: Mutex<VecDeque<LaunchRecord>>,
    next_seq: AtomicU64,
    marker: Mutex<Option<(String, Instant)>>,
    narration: Mutex<Option<(String, Instant)>>,
    cgroup: OnceLock<super::cgroup::CgroupView>,
}

impl Observed {
    pub(crate) fn new(acq: AcqId) -> Self {
        Self {
            acq,
            activities: Mutex::new(Activities::default()),
            launches: Mutex::new(VecDeque::new()),
            next_seq: AtomicU64::new(1),
            marker: Mutex::new(None),
            narration: Mutex::new(None),
            cgroup: OnceLock::new(),
        }
    }

    pub(crate) fn acq(&self) -> &AcqId {
        &self.acq
    }

    /// Open an activity. `None` when the table is full.
    pub(crate) fn open(&self, parent: Option<u64>, label: &str) -> Option<u64> {
        let mut table = self.activities.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if table.open.len() >= MAX_ACTIVITIES {
            return None;
        }
        table.next_id += 1;
        let id = table.next_id;
        table.open.push(ActivityRecord {
            id,
            parent,
            label: clip(label),
            started: Instant::now(),
            progress: None,
            narration: None,
        });
        Some(id)
    }

    pub(crate) fn close(&self, id: u64) {
        let mut table = self.activities.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        table.open.retain(|a| a.id != id);
    }

    pub(crate) fn set_progress(&self, id: u64, done: u64, total: u64) {
        self.edit(id, |a| a.progress = Some((done, total)));
    }

    pub(crate) fn narrate_activity(&self, id: u64, text: &str) {
        let text = clip(text);
        self.edit(id, |a| a.narration = Some(text));
    }

    fn edit(&self, id: u64, f: impl FnOnce(&mut ActivityRecord)) {
        let mut table = self.activities.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = table.open.iter_mut().find(|a| a.id == id) {
            f(record);
        }
    }

    /// The label of `id` and of each ancestor still open, outermost first.
    pub(crate) fn label_path(&self, id: u64) -> Option<String> {
        let table = self.activities.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut parts = Vec::new();
        let mut cursor = Some(id);
        while let Some(current) = cursor {
            let Some(record) = table.open.iter().find(|a| a.id == current) else {
                break;
            };
            parts.push(record.label.clone());
            cursor = record.parent;
            if parts.len() > 16 {
                break;
            }
        }
        if parts.is_empty() {
            return None;
        }
        parts.reverse();
        Some(parts.join(" > "))
    }

    /// The narration no activity claimed: the latest status line written on a
    /// thread with no open activity.
    pub(crate) fn narrate_root(&self, text: &str) {
        *self.narration.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((clip(text), Instant::now()));
    }

    /// Allocate a launch cohort and return the `BROKKR_EXEC` value for it.
    pub(crate) fn launch(&self, label: &str, under: Option<String>) -> String {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut launches = self.launches.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if launches.len() >= MAX_LAUNCHES {
            launches.pop_front();
        }
        launches.push_back(LaunchRecord {
            seq,
            label: clip(label),
            under,
        });
        format!("{}.{seq}", self.acq.as_str())
    }

    pub(crate) fn launch_record(&self, seq: u64) -> Option<LaunchRecord> {
        let launches = self.launches.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        launches.iter().find(|l| l.seq == seq).cloned()
    }

    pub(crate) fn note_marker(&self, name: Option<&str>) {
        *self.marker.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            name.map(|n| (clip(n), Instant::now()));
    }

    pub(crate) fn set_cgroup(&self, view: super::cgroup::CgroupView) {
        self.cgroup.set(view).ok();
    }

    pub(crate) fn cgroup(&self) -> Option<&super::cgroup::CgroupView> {
        self.cgroup.get()
    }

    /// A consistent copy of the descriptive state, or `None` for whichever part
    /// was contended at the instant of asking (the service never waits).
    pub(crate) fn snapshot(&self) -> ObservedSnapshot {
        let activities = self.activities.try_lock().ok().map(|t| t.open.clone());
        let marker = self.marker.try_lock().ok().and_then(|m| m.clone());
        let narration = self.narration.try_lock().ok().and_then(|n| n.clone());
        ObservedSnapshot {
            activities,
            marker,
            narration,
        }
    }
}

pub(crate) struct ObservedSnapshot {
    pub(crate) activities: Option<Vec<ActivityRecord>>,
    pub(crate) marker: Option<(String, Instant)>,
    pub(crate) narration: Option<(String, Instant)>,
}

/// Keep at most [`MAX_TEXT`] chars, control characters replaced by spaces.
pub(crate) fn clip(text: &str) -> String {
    sanitize(text).chars().take(MAX_TEXT).collect()
}

/// Replace every control character (tab and newline included) with a space,
/// so a value is safe both on the line-oriented wire and on a terminal.
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Parse a `BROKKR_EXEC` value into its acquisition id and sequence.
pub(crate) fn parse_exec(value: &str) -> Option<(&str, u64)> {
    let (acq, seq) = value.split_once('.')?;
    Some((acq, seq.parse().ok()?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn observed() -> Observed {
        Observed::new(AcqId::parse("0123456789abcdef0123456789abcdef").unwrap())
    }

    #[test]
    fn activities_nest_and_close() {
        let o = observed();
        let outer = o.open(None, "verify merge").unwrap();
        let inner = o.open(Some(outer), "osmium\tapply-changes").unwrap();
        assert_eq!(o.label_path(inner).unwrap(), "verify merge > osmium apply-changes");
        o.close(inner);
        assert!(o.label_path(inner).is_none());
        assert_eq!(o.snapshot().activities.unwrap().len(), 1);
    }

    #[test]
    fn launch_tokens_carry_the_acquisition_and_resolve_back() {
        let o = observed();
        let token = o.launch("pbfhogg apply-changes", Some("verify merge".into()));
        let (acq, seq) = parse_exec(&token).unwrap();
        assert_eq!(acq, o.acq().as_str());
        let record = o.launch_record(seq).unwrap();
        assert_eq!(record.label, "pbfhogg apply-changes");
        assert_eq!(record.under.as_deref(), Some("verify merge"));
    }

    #[test]
    fn launch_table_is_bounded() {
        let o = observed();
        for _ in 0..MAX_LAUNCHES + 5 {
            o.launch("probe", None);
        }
        assert!(o.launch_record(1).is_none(), "the oldest records are dropped");
        assert!(o.launch_record(u64::try_from(MAX_LAUNCHES).unwrap() + 5).is_some());
    }

    #[test]
    fn sanitize_strips_controls() {
        assert_eq!(sanitize("a\tb\nc\u{1b}[2K"), "a b c [2K");
    }
}
