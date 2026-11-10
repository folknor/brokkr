//! The run document: one attempted script-bench run, whole, as the store
//! takes it and gives it back.
//!
//! Every field is submitted evidence. What the store derives (the winning
//! iteration, the pairing key, a record's arrival position, a sample's or
//! marker's index) is not in the document, so a replayed document compares
//! equal to the stored one whatever the store computed from it. Every struct
//! refuses unknown fields and the document carries its format, so a document
//! a newer writer extended is refused rather than stored without its new
//! fields.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::StoreError;

/// The document format this store reads and writes.
pub const DOCUMENT_FORMAT: u32 = 1;

/// One attempted run: its identity, how it was built, what it was bound to,
/// its outcome, and every iteration that ran with every child beneath it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunDocument {
    pub format: u32,
    /// Client-generated, 32 lowercase hex digits. Also the publication's
    /// request id: republishing it is answered from the stored run.
    pub uuid: String,
    pub project: String,
    pub bench: String,
    /// The checkout the run was made from, as data: clones of one project
    /// share its history.
    pub checkout: String,
    pub hostname: String,
    /// The commit under test, full lowercase hex.
    pub commit: String,
    pub dirty: bool,
    pub subject: Option<String>,
    pub started_unix_ms: i64,
    pub brokkr_version: String,
    /// brokkr's own argv, credentials already redacted.
    pub argv: Vec<String>,
    pub conditions: Conditions,
    pub build: Build,
    /// What the operator asked to measure.
    pub measurement: Measurement,
    /// Bound params by name. An unbound param is absent.
    pub params: BTreeMap<String, BoundParam>,
    /// The resolved brokkr.toml as the writer serialized it; stored once per
    /// distinct text.
    pub config: String,
    pub iterations_requested: u32,
    pub outcome: RunOutcome,
    /// Every iteration that ran, in execution order. One that never started
    /// is not here.
    pub iterations: Vec<Iteration>,
}

/// The host's state when the run started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conditions {
    pub kernel: Option<String>,
    pub cpu_governor: Option<String>,
    pub avail_memory_mb: Option<i64>,
    /// The captured environment, credential values already redacted.
    pub captured_env: BTreeMap<String, String>,
}

/// The build: what was asked for, always, and what resolution and the build
/// produced, as far as they got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// `None` is the project's default bin.
    pub target: Option<String>,
    pub profile: String,
    /// Requested features, host features folded in.
    pub features: Vec<String>,
    pub default_features: bool,
    /// Absent when resolution did not finish.
    pub resolved: Option<ResolvedBuild>,
    /// The built artifact's xxh3-128 digest; absent when the build did not
    /// finish.
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedBuild {
    pub bin: String,
    /// The features cargo resolved for the package.
    pub features: Vec<String>,
}

/// What a run was asked to measure: `brokkr bench` plain, `brokkr bench
/// --hotpath`, built with the bench's `hotpath_features`, or `brokkr bench
/// --alloc`, built with its `alloc_features`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Measurement {
    Timing,
    Hotpath,
    Alloc,
}

impl Measurement {
    /// The `runs` columns it is stored as, `measurement` and `alloc`: the
    /// first column's CHECK predates alloc and cannot be widened in place,
    /// so an alloc run is a hotpath run flagged alloc (schema V5).
    pub(super) fn columns(self) -> (&'static str, bool) {
        match self {
            Self::Timing => ("timing", false),
            Self::Hotpath => ("hotpath", false),
            Self::Alloc => ("hotpath", true),
        }
    }

    pub(super) fn from_columns(name: &str, alloc: bool) -> Option<Self> {
        match (name, alloc) {
            ("timing", false) => Some(Self::Timing),
            ("hotpath", false) => Some(Self::Hotpath),
            ("hotpath", true) => Some(Self::Alloc),
            _ => None,
        }
    }

    /// A profiled run: every iteration that completed must capture every
    /// call it marked, and mark at least one.
    pub fn profiled(self) -> bool {
        self != Self::Timing
    }

    /// The `brokkr bench` flag that asks for it, `None` for timing.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Self::Timing => None,
            Self::Hotpath => Some("--hotpath"),
            Self::Alloc => Some("--alloc"),
        }
    }
}

/// The class a run pairs in: what was asked, and what was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingClass {
    /// Asked for timing and no child captured a profile.
    Timing,
    /// Asked for a hotpath profile.
    Hotpath,
    /// Asked for an allocation profile.
    Alloc,
    /// Asked for timing, but some child captured a profile: the build was
    /// instrumented by other means.
    Incidental,
}

/// One bound param. A scalar has exactly one value; a collection keeps its
/// values in bound order, which is part of pairing, and may be empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundParam {
    pub many: bool,
    /// The `from` path the param selected under, `{param}` references
    /// resolved; `None` for a plain string param.
    pub from: Option<String>,
    pub values: Vec<ParamValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamValue {
    pub key: String,
    /// A file's digest; `None` for a container or a plain string, which
    /// pair on the key alone.
    pub digest: Option<String>,
}

// The enums with unit variants are externally tagged: an internally tagged
// unit variant ignores any other field of its map, so a field a newer writer
// added there would be dropped rather than refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum RunOutcome {
    /// Every requested iteration completed with one verdict: the run has a
    /// result.
    Completed,
    Failed {
        reason: String,
    },
    Interrupted,
}

/// One iteration: one fresh Lua state and one call of `bench()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Iteration {
    pub outcome: IterationOutcome,
    /// The block's wall, from the call of `bench()` to its end; `None` when
    /// `bench()` was never called.
    pub wall_us: Option<i64>,
    /// `_G.record` as extracted at the end of the sample; `None` when it
    /// never was.
    pub record: Option<serde_json::Map<String, serde_json::Value>>,
    /// Every child, in launch order.
    pub children: Vec<Child>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum IterationOutcome {
    /// `bench()` returned; `None` is a nil verdict, distinct from `""`.
    Completed {
        verdict: Option<String>,
    },
    Failed {
        reason: String,
    },
    Interrupted,
}

/// One child process an iteration launched. Its endings are evidence only:
/// the script decides what an exit means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Child {
    pub program: String,
    /// Credentials already redacted.
    pub argv: Vec<String>,
    /// What the call added to the child's environment, credentials already
    /// redacted.
    pub env: BTreeMap<String, String>,
    pub pid: Option<u32>,
    /// When the child started, from the start of its iteration's block. The
    /// child's own stamps stay on its own clock and are never rebased.
    pub origin_offset_us: i64,
    pub wall_us: i64,
    pub ending: ChildEnding,
    pub evidence: Evidence,
    /// A call marked `hotpath = STAGE`: what became of its report. Absent
    /// for an unmarked call.
    pub hotpath: Option<HotpathCapture>,
    /// The executable a `program` call ran, as observed at its launch.
    /// Absent means no separate observation: the snapshot child, whose
    /// identity is the build's artifact, and every child stored before the
    /// field existed. Omitted when absent, so such a document encodes as
    /// it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<Executable>,
    /// The child's stderr as brokkr spooled it: its size and a bounded
    /// tail. Absent means not observed: a child stored before the field
    /// existed. Omitted when absent, so such a document encodes as it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<StderrCapture>,
    /// A call marked `measured`: this child's wall is its iteration's
    /// result. Omitted when unmarked, so such a document encodes as it did.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub measured: bool,
}

/// The most raw stderr bytes one child's stored tail holds.
pub const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// The most raw stderr bytes the tails of one run hold together.
pub const STDERR_RUN_BYTES: usize = 16 * 1024 * 1024;

/// A tail's text may be longer than its raw bytes, never shorter: each
/// invalid sequence, one to three bytes, becomes U+FFFD, three bytes.
const STDERR_TEXT_EXPANSION: u64 = 3;

/// A child's stderr, spooled in full to a file in its iteration while it ran
/// and snapshotted once its call ended: evidence of the end of what it wrote,
/// never the whole log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StderrCapture {
    /// Whether the call's borrow was released before the snapshot. Without
    /// it something the child started may still have been writing, so the
    /// size and tail are what the spool held then, not its final contents.
    pub released: bool,
    /// The spool's size as observed for the snapshot; absent when it could
    /// not be observed.
    pub bytes: Option<u64>,
    pub tail: StderrTail,
}

/// What was kept of a child's stderr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum StderrTail {
    /// The last `raw` bytes, [`STDERR_TAIL_BYTES`] at most, as UTF-8 with
    /// each invalid sequence replaced; `cut` when bytes before them were
    /// not kept, so the text may open mid-line.
    Kept { text: String, raw: u64, cut: bool },
    /// Past what was left of the run's [`STDERR_RUN_BYTES`]: not kept.
    OverBudget,
    /// The spool could not be read.
    Unreadable { reason: String },
}

/// An external executable a child ran: the absolute path brokkr resolved
/// and executed, and its xxh3-128 digest as read at launch, or why it could
/// not be read. Evidence of what was there: a file replaced between the
/// digest and the exec is not seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Executable {
    pub path: String,
    pub identity: ExecutableIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum ExecutableIdentity {
    Digest { digest: String },
    Unread { reason: String },
}

/// One marked call's hotpath capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotpathCapture {
    /// The script's name for the stage.
    pub stage: String,
    /// The stage's marked calls so far in the iteration, this one included,
    /// whatever became of each: from 1.
    pub occurrence: u32,
    pub outcome: CaptureOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum CaptureOutcome {
    /// The report, as parsed: a JSON object.
    Captured { report: serde_json::Value },
    /// The report file was still empty: nothing wrote one.
    Missing,
    /// The report was read but is not one brokkr can take.
    Invalid { reason: String },
    /// The report could not be read.
    Unreadable { reason: String },
    /// Over the per-report or per-iteration budget: not read.
    OverBudget,
}

impl CaptureOutcome {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Captured { .. } => "captured",
            Self::Missing => "missing",
            Self::Invalid { .. } => "invalid",
            Self::Unreadable { .. } => "unreadable",
            Self::OverBudget => "over_budget",
        }
    }
}

/// How a child ended, as its supervisor assigned it, without loss: a
/// deadline is a whole `Duration`, seconds and nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum ChildEnding {
    Exited { code: i32 },
    Signalled { signal: i32 },
    StopMarker { marker: String },
    Deadline { secs: u64, nanos: u32 },
    Interrupted,
    WaitFailed { reason: String },
}

/// Everything the sidecar captured for one child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    /// `/proc` samples in order; a sample's index is its position.
    pub samples: Vec<Sample>,
    /// Every FIFO record kept, in arrival order; a record's position is its
    /// arrival index across every kind.
    pub records: Vec<Record>,
    pub loss: Loss,
    pub clock: Clock,
    /// Every call id a tagged record named, kept or not.
    pub calls_seen: BTreeSet<String>,
    /// More call ids arrived than the census holds.
    pub calls_overflowed: bool,
}

/// One FIFO record. Stamps are the child's own, and an off-contract stamp
/// (negative, or past the child's wall) is kept as it came.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum Record {
    Marker {
        timestamp_us: i64,
        call: Option<String>,
        name: String,
    },
    Counter {
        timestamp_us: i64,
        call: Option<String>,
        name: String,
        value: i64,
    },
    Sealing {
        timestamp_us: i64,
        call: String,
        name: String,
        value: i64,
    },
    Terminal {
        timestamp_us: i64,
        call: String,
        end: SinkEnd,
        drops: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkEnd {
    Sealed,
    Failed,
}

impl SinkEnd {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Sealed => "sealed",
            Self::Failed => "failed",
        }
    }

    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "sealed" => Some(Self::Sealed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// What the capture omitted, by cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Loss {
    pub rejected_lines: u64,
    pub fifo_unread: bool,
    pub records_capped: bool,
    pub samples_capped: bool,
    pub output_truncated: bool,
}

/// The clock check: retained records stamped outside the child's run
/// interval, and the smallest such stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clock {
    pub outliers: u64,
    pub min_us: Option<i64>,
}

/// Define [`Sample`] and its column list together, so the write and read
/// paths cannot disagree about either. The names are the frozen schema's
/// column names.
macro_rules! define_sample {
    ($($field:ident),+ $(,)?) => {
        /// One `/proc` sample on the child's clock.
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Sample {
            pub timestamp_us: i64,
            $(pub $field: i64,)+
        }

        impl Sample {
            /// Every value column, in the order [`Self::values`] gives them.
            pub const COLUMNS: &'static [&'static str] =
                &["timestamp_us", $(stringify!($field)),+];

            pub(super) fn values(&self) -> Vec<i64> {
                vec![self.timestamp_us, $(self.$field),+]
            }

            pub(super) fn from_row(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Self> {
                let mut column = first;
                let mut next = || {
                    column += 1;
                    row.get::<_, i64>(column - 1)
                };
                Ok(Self {
                    timestamp_us: next()?,
                    $($field: next()?,)+
                })
            }
        }
    };
}

define_sample! {
    rss_kb, anon_kb, file_kb, shmem_kb, swap_kb, vsize_kb, vm_hwm_kb, utime, stime,
    num_threads, minflt, majflt, rchar, wchar, read_bytes, write_bytes,
    cancelled_write_bytes, syscr, syscw, vol_cs, nonvol_cs,
}

fn refuse(message: impl Into<String>) -> StoreError {
    StoreError::Refused(message.into())
}

pub(super) fn is_lower_hex(text: &str, lengths: &[usize]) -> bool {
    lengths.contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl RunDocument {
    /// Refuse a document the store must not hold: malformed identity, a
    /// shape no run can have, or a completed outcome its iterations do not
    /// support.
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.format != DOCUMENT_FORMAT {
            return Err(refuse(format!(
                "document format {} is not {DOCUMENT_FORMAT}, the one this store knows",
                self.format
            )));
        }
        if !is_lower_hex(&self.uuid, &[32]) {
            return Err(refuse(format!(
                "run uuid {:?} is not 32 lowercase hex digits",
                self.uuid
            )));
        }
        if !is_lower_hex(&self.commit, &[40, 64]) {
            return Err(refuse(format!(
                "commit {:?} is not a full lowercase hex sha",
                self.commit
            )));
        }
        for (field, value) in [
            ("project", &self.project),
            ("bench", &self.bench),
            ("checkout", &self.checkout),
            ("hostname", &self.hostname),
            ("brokkr_version", &self.brokkr_version),
            ("build.profile", &self.build.profile),
        ] {
            if value.is_empty() {
                return Err(refuse(format!("{field} is empty")));
            }
        }
        self.validate_build()?;
        self.validate_params()?;
        if self.iterations_requested == 0 {
            return Err(refuse("iterations_requested is 0"));
        }
        if self.iterations.len() > self.iterations_requested as usize {
            return Err(refuse(format!(
                "{} iterations stored where {} were requested",
                self.iterations.len(),
                self.iterations_requested
            )));
        }
        for (idx, iteration) in self.iterations.iter().enumerate() {
            iteration
                .validate()
                .map_err(|e| refuse(format!("iteration {idx}: {e}")))?;
        }
        let tails: u64 = self
            .iterations
            .iter()
            .flat_map(|i| &i.children)
            .filter_map(|c| c.stderr.as_ref())
            .map(StderrCapture::kept_raw)
            .fold(0, u64::saturating_add);
        if tails > STDERR_RUN_BYTES as u64 {
            return Err(refuse(format!(
                "stderr tails of {tails} raw bytes, past the {STDERR_RUN_BYTES} a run keeps"
            )));
        }
        self.validate_outcome()
    }

    fn validate_build(&self) -> Result<(), StoreError> {
        if let Some(resolved) = &self.build.resolved
            && resolved.bin.is_empty()
        {
            return Err(refuse("build.resolved.bin is empty"));
        }
        if let Some(artifact) = &self.build.artifact {
            if self.build.resolved.is_none() {
                return Err(refuse("a build artifact without a resolved build"));
            }
            if !is_lower_hex(artifact, &[32]) {
                return Err(refuse(format!(
                    "artifact digest {artifact:?} is not 32 lowercase hex digits"
                )));
            }
        }
        Ok(())
    }

    fn validate_params(&self) -> Result<(), StoreError> {
        for (name, param) in &self.params {
            if name.is_empty() {
                return Err(refuse("a param with an empty name"));
            }
            if !param.many && param.values.len() != 1 {
                return Err(refuse(format!(
                    "scalar param {name} has {} values",
                    param.values.len()
                )));
            }
            let mut keys = BTreeSet::new();
            for value in &param.values {
                if !keys.insert(value.key.as_str()) {
                    return Err(refuse(format!("param {name} binds {:?} twice", value.key)));
                }
                if param.from.is_none() && value.digest.is_some() {
                    return Err(refuse(format!(
                        "plain param {name} carries a digest; only a value selected by from can"
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_outcome(&self) -> Result<(), StoreError> {
        match &self.outcome {
            RunOutcome::Failed { reason } if reason.is_empty() => {
                Err(refuse("a failed run with an empty reason"))
            }
            RunOutcome::Failed { .. } | RunOutcome::Interrupted => Ok(()),
            RunOutcome::Completed => {
                if self.build.artifact.is_none() {
                    return Err(refuse("a completed run without a built artifact"));
                }
                if self.iterations.len() != self.iterations_requested as usize {
                    return Err(refuse(format!(
                        "a completed run holds {} of {} requested iterations",
                        self.iterations.len(),
                        self.iterations_requested
                    )));
                }
                let mut verdicts = self.iterations.iter().map(|i| match &i.outcome {
                    IterationOutcome::Completed { verdict } => Ok(verdict),
                    _ => Err(refuse(
                        "a completed run holds an iteration that did not complete",
                    )),
                });
                let first = verdicts.next().transpose()?;
                for verdict in verdicts {
                    if Some(verdict?) != first {
                        return Err(refuse(
                            "a completed run whose iterations disagree on the verdict",
                        ));
                    }
                }
                let measured = self.iterations.iter().map(|i| i.measured_child().is_some());
                if measured.clone().any(|m| m) && !measured.clone().all(|m| m) {
                    return Err(refuse(
                        "a completed run whose iterations disagree on a measured call",
                    ));
                }
                if self.measurement.profiled() {
                    for (idx, iteration) in self.iterations.iter().enumerate() {
                        let captures: Vec<&HotpathCapture> = iteration
                            .children
                            .iter()
                            .filter_map(|c| c.hotpath.as_ref())
                            .collect();
                        if captures.is_empty()
                            || captures
                                .iter()
                                .any(|c| !matches!(c.outcome, CaptureOutcome::Captured { .. }))
                        {
                            return Err(refuse(format!(
                                "a completed profiled run whose iteration {idx} did not capture \
                                 every marked call, or marked none"
                            )));
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// The result's iteration: the first fastest, by each iteration's
    /// [`Iteration::result_wall_us`]. `None` unless the run completed.
    pub fn best_iteration(&self) -> Option<usize> {
        if self.outcome != RunOutcome::Completed {
            return None;
        }
        let mut best: Option<(usize, i64)> = None;
        for (idx, iteration) in self.iterations.iter().enumerate() {
            let wall = iteration.result_wall_us()?;
            if best.is_none_or(|(_, fastest)| wall < fastest) {
                best = Some((idx, wall));
            }
        }
        best.map(|(idx, _)| idx)
    }

    /// Whether the run's result is a measured child's wall rather than the
    /// block's: some iteration marked a call `measured`.
    pub fn child_measured(&self) -> bool {
        self.iterations.iter().any(|i| i.measured_child().is_some())
    }

    /// The result's wall in microseconds: the best iteration's
    /// [`Iteration::result_wall_us`].
    pub fn result_wall_us(&self) -> Option<i64> {
        self.iterations
            .get(self.best_iteration()?)?
            .result_wall_us()
    }

    /// The class the run pairs in: [`Measurement::Hotpath`] and
    /// [`Measurement::Alloc`] as asked; a
    /// timing run any of whose children, in any iteration, captured a
    /// report is incidental; any other is timing.
    pub fn pairing_class(&self) -> PairingClass {
        let captured = self.iterations.iter().any(|iteration| {
            iteration.children.iter().any(|child| {
                child
                    .hotpath
                    .as_ref()
                    .is_some_and(|c| matches!(c.outcome, CaptureOutcome::Captured { .. }))
            })
        });
        match (self.measurement, captured) {
            (Measurement::Hotpath, _) => PairingClass::Hotpath,
            (Measurement::Alloc, _) => PairingClass::Alloc,
            (Measurement::Timing, true) => PairingClass::Incidental,
            (Measurement::Timing, false) => PairingClass::Timing,
        }
    }

    /// The identity runs pair on: project, bench, and each bound param's
    /// keys and digests in bound order, params by name. Versioned, so a
    /// later algorithm is a new projection and never a silent rewrite;
    /// rows compare only under one algorithm. A timing run's key is
    /// `bench-pair-v1`, unchanged; a profiled or incidental run's is
    /// `bench-pair-v2` with its class, so neither ever pairs with timing. A
    /// run whose result is a measured child's wall is `bench-pair-v3` with
    /// its class, so it never pairs with one ranked on the block's wall.
    pub fn pairing_key(&self) -> String {
        let params: Vec<serde_json::Value> = self
            .params
            .iter()
            .map(|(name, param)| {
                let values: Vec<serde_json::Value> = param
                    .values
                    .iter()
                    .map(|v| serde_json::json!([v.key, v.digest]))
                    .collect();
                serde_json::json!([name, param.many, values])
            })
            .collect();
        if self.child_measured() {
            let class = match self.pairing_class() {
                PairingClass::Timing => "timing",
                PairingClass::Hotpath => "hotpath",
                PairingClass::Alloc => "alloc",
                PairingClass::Incidental => "incidental",
            };
            return serde_json::json!([
                "bench-pair-v3",
                class,
                "measured-child",
                self.project,
                self.bench,
                params
            ])
            .to_string();
        }
        match self.pairing_class() {
            PairingClass::Timing => {
                serde_json::json!(["bench-pair-v1", self.project, self.bench, params])
            }
            PairingClass::Hotpath => {
                serde_json::json!(["bench-pair-v2", "hotpath", self.project, self.bench, params])
            }
            PairingClass::Alloc => {
                serde_json::json!(["bench-pair-v2", "alloc", self.project, self.bench, params])
            }
            PairingClass::Incidental => serde_json::json!([
                "bench-pair-v2",
                "incidental",
                self.project,
                self.bench,
                params
            ]),
        }
        .to_string()
    }
}

impl Iteration {
    /// The child marked `measured`, if any; at most one is valid.
    pub fn measured_child(&self) -> Option<&Child> {
        self.children.iter().find(|c| c.measured)
    }

    /// What the iteration is ranked on: its measured child's wall when it
    /// marked one, else the block's wall.
    pub fn result_wall_us(&self) -> Option<i64> {
        match self.measured_child() {
            Some(child) => Some(child.wall_us),
            None => self.wall_us,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.wall_us.is_some_and(|w| w < 0) {
            return Err("negative wall".to_owned());
        }
        if self.children.iter().filter(|c| c.measured).count() > 1 {
            return Err("more than one measured child".to_owned());
        }
        match &self.outcome {
            IterationOutcome::Completed { .. } => {
                if self.wall_us.is_none() {
                    return Err("completed without a wall".to_owned());
                }
                if self.record.is_none() {
                    return Err("completed without an extracted record".to_owned());
                }
            }
            IterationOutcome::Failed { reason } if reason.is_empty() => {
                return Err("failed with an empty reason".to_owned());
            }
            IterationOutcome::Failed { .. } | IterationOutcome::Interrupted => {}
        }
        if let Some(record) = &self.record
            && record_depth(record) > MAX_RECORD_DEPTH
        {
            return Err(format!("record nests deeper than {MAX_RECORD_DEPTH}"));
        }
        let mut occurrences: BTreeMap<&str, u32> = BTreeMap::new();
        for (idx, child) in self.children.iter().enumerate() {
            child.validate().map_err(|e| format!("child {idx}: {e}"))?;
            if let Some(capture) = &child.hotpath {
                let seen = occurrences.entry(capture.stage.as_str()).or_insert(0);
                *seen += 1;
                if capture.occurrence != *seen {
                    return Err(format!(
                        "child {idx}: stage {:?} occurrence {} where {} follows",
                        capture.stage, capture.occurrence, *seen
                    ));
                }
            }
        }
        Ok(())
    }
}

impl HotpathCapture {
    fn validate(&self) -> Result<(), String> {
        if self.stage.is_empty() {
            return Err("a hotpath capture with an empty stage".to_owned());
        }
        match &self.outcome {
            CaptureOutcome::Captured { report } => {
                if !report.is_object() {
                    return Err("a captured hotpath report that is not a JSON object".to_owned());
                }
                if depth(report) > MAX_RECORD_DEPTH {
                    return Err(format!(
                        "a hotpath report nests deeper than {MAX_RECORD_DEPTH}"
                    ));
                }
            }
            CaptureOutcome::Invalid { reason } | CaptureOutcome::Unreadable { reason }
                if reason.is_empty() =>
            {
                return Err("a hotpath capture failure with an empty reason".to_owned());
            }
            _ => {}
        }
        Ok(())
    }
}

impl Child {
    fn validate(&self) -> Result<(), String> {
        if self.program.is_empty() {
            return Err("empty program".to_owned());
        }
        if self.origin_offset_us < 0 {
            return Err("negative origin offset".to_owned());
        }
        if self.wall_us < 0 {
            return Err("negative wall".to_owned());
        }
        if let ChildEnding::Deadline { nanos, .. } = self.ending
            && nanos >= 1_000_000_000
        {
            return Err("deadline nanoseconds past a second".to_owned());
        }
        let evidence = &self.evidence;
        if (evidence.clock.outliers == 0) != evidence.clock.min_us.is_none() {
            return Err("clock outlier count and smallest stamp disagree".to_owned());
        }
        // Counts are stored as SQLite integers; a larger one would not read
        // back as it was submitted.
        if i64::try_from(evidence.clock.outliers).is_err()
            || i64::try_from(evidence.loss.rejected_lines).is_err()
        {
            return Err("a capture count past the store's range".to_owned());
        }
        if let Some(capture) = &self.hotpath {
            capture.validate()?;
        }
        if let Some(executable) = &self.executable {
            if !executable.path.starts_with('/') {
                return Err(format!(
                    "executable path {:?} is not absolute",
                    executable.path
                ));
            }
            match &executable.identity {
                ExecutableIdentity::Digest { digest } if !is_lower_hex(digest, &[32]) => {
                    return Err(format!(
                        "executable digest {digest:?} is not 32 lowercase hex digits"
                    ));
                }
                ExecutableIdentity::Unread { reason } if reason.is_empty() => {
                    return Err("an unread executable with an empty reason".to_owned());
                }
                _ => {}
            }
        }
        if let Some(stderr) = &self.stderr {
            stderr.validate()?;
        }
        Ok(())
    }
}

impl StderrCapture {
    fn validate(&self) -> Result<(), String> {
        if self.bytes.is_some_and(|b| i64::try_from(b).is_err()) {
            return Err("a stderr size past the store's range".to_owned());
        }
        match &self.tail {
            StderrTail::Kept { text, raw, cut } => {
                let Some(bytes) = self.bytes else {
                    return Err("a kept stderr tail with no observed size".to_owned());
                };
                if *raw > STDERR_TAIL_BYTES as u64 {
                    return Err(format!(
                        "a stderr tail of {raw} raw bytes, past the {STDERR_TAIL_BYTES} a tail keeps"
                    ));
                }
                if *raw > bytes || *cut != (*raw < bytes) {
                    return Err(format!(
                        "a stderr tail of {raw} raw bytes disagrees with its size {bytes} and cut {cut}"
                    ));
                }
                let len = text.len() as u64;
                if len < *raw || len > raw * STDERR_TEXT_EXPANSION {
                    return Err(format!(
                        "a stderr tail text of {len} bytes cannot hold {raw} raw bytes"
                    ));
                }
                Ok(())
            }
            StderrTail::Unreadable { reason } if reason.is_empty() => {
                Err("an unreadable stderr with an empty reason".to_owned())
            }
            StderrTail::OverBudget | StderrTail::Unreadable { .. } => Ok(()),
        }
    }

    /// The raw bytes the tail keeps, for the run's budget.
    fn kept_raw(&self) -> u64 {
        match &self.tail {
            StderrTail::Kept { raw, .. } => *raw,
            StderrTail::OverBudget | StderrTail::Unreadable { .. } => 0,
        }
    }
}

/// How deeply a record may nest: the reader's JSON parser refuses past 128,
/// and a record is refused at publication well before, so whatever is stored
/// reads back.
const MAX_RECORD_DEPTH: usize = 64;

/// The nesting depth of a JSON value: a scalar is 0.
fn depth(value: &serde_json::Value) -> usize {
    let mut deepest = 0;
    let mut stack: Vec<(&serde_json::Value, usize)> = vec![(value, 0)];
    while let Some((value, level)) = stack.pop() {
        let children: Box<dyn Iterator<Item = &serde_json::Value>> = match value {
            serde_json::Value::Array(items) => Box::new(items.iter()),
            serde_json::Value::Object(map) => Box::new(map.values()),
            _ => continue,
        };
        deepest = deepest.max(level + 1);
        stack.extend(children.map(|child| (child, level + 1)));
    }
    deepest
}

fn record_depth(record: &serde_json::Map<String, serde_json::Value>) -> usize {
    1 + record.values().map(depth).max().unwrap_or(0)
}
