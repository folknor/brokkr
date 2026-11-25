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
    /// Where the run sits in a `--rounds` invocation; absent for any other
    /// run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<super::schedule::Schedule>,
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
    /// The newest row of this user's command history when the run started:
    /// the latest brokkr command to complete, as far as that file knows.
    /// Absent when not recorded or the history held none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_command: Option<PreviousCommand>,
    /// When systemd's `fstrim.timer` last triggered, from its stamp file's
    /// mtime, in unix milliseconds: a timer firing, not a completed trim of
    /// any particular filesystem. Absent when there is no stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fstrim_timer_unix_ms: Option<i64>,
}

/// One command history row ([`Conditions::previous_command`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviousCommand {
    pub command: String,
    /// When its row was written, at its end, to the second.
    pub ended_unix_s: i64,
    pub elapsed_ms: Option<i64>,
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
    /// Tracked files of the `--commit` worktree left as Git LFS pointers,
    /// observed when the worktree was admitted. Absent when not recorded (a
    /// live-tree run, or a document from before the field); empty when the
    /// worktree was scanned and held none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lfs_pointers: Option<Vec<LfsPointer>>,
    /// The build root's `Cargo.lock`: where it came from and its bytes after
    /// the build. Absent when not recorded (a document from before the
    /// field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile: Option<Lockfile>,
    /// The bench's `rustflags` recipe as this build resolved it, and how it
    /// reached cargo. Absent for a bench without the key and for a document
    /// from before the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rustflags: Option<Rustflags>,
}

/// What a bench's `rustflags` added to a build ([`Build::rustflags`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rustflags {
    /// The resolved tokens brokkr asked cargo to add, in order.
    pub additions: Vec<String>,
    /// How they were injected. Absent when the build stopped before an
    /// injection was chosen, including a placement brokkr could not prove
    /// live, which fails the build rather than risk inert flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
}

/// How a build's [`Rustflags::additions`] reached cargo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Placement {
    /// An empty recipe: nothing injected, cargo's own flags as they are.
    None,
    /// Appended to the live environment source: the variable and the whole
    /// value cargo received, inherited flags included.
    Env { key: String, value: String },
    /// Cargo's `--config` arguments at the live config layer.
    Config { args: Vec<String> },
}

impl Placement {
    /// Whether this injection hands `additions` to cargo: an env source cargo
    /// reads whose value ends with them, or one `--config` entry at a
    /// rustflags table whose array is exactly them.
    pub fn carries(&self, additions: &[String]) -> bool {
        match self {
            Self::None => additions.is_empty(),
            Self::Env { key, value } => {
                let tokens: Vec<&str> = match key.as_str() {
                    "CARGO_ENCODED_RUSTFLAGS" => {
                        value.split('\u{1f}').filter(|t| !t.is_empty()).collect()
                    }
                    "RUSTFLAGS" => value.split_whitespace().collect(),
                    _ => return false,
                };
                tokens.len() >= additions.len()
                    && tokens[tokens.len() - additions.len()..]
                        .iter()
                        .zip(additions)
                        .all(|(t, a)| t == a)
            }
            Self::Config { args } => {
                let [flag, entry] = args.as_slice() else {
                    return false;
                };
                let Some((key, list)) = entry.split_once('=') else {
                    return false;
                };
                if flag != "--config"
                    || !matches!(key, "build.rustflags" | "target.\"cfg(all())\".rustflags")
                {
                    return false;
                }
                toml::from_str::<toml::Table>(&format!("v = {list}"))
                    .ok()
                    .and_then(|t| t.get("v").cloned())
                    .and_then(|v| v.try_into::<Vec<String>>().ok())
                    .is_some_and(|tokens| tokens == additions)
            }
        }
    }

    /// One line for a reader.
    pub fn describe(&self) -> String {
        match self {
            Self::None => "nothing injected".to_owned(),
            Self::Env { key, value } => format!("{key}={value:?}"),
            Self::Config { args } => args.join(" "),
        }
    }
}

/// The root `Cargo.lock` a build resolved against ([`Build::lockfile`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    /// Where the file came from, observed before the build.
    pub source: LockSource,
    /// The file's xxh3-128 digest, read once the build succeeded. Absent
    /// when there was no file, when the build did not finish, or for an
    /// [`LockSource::Unmanaged`] layout, whose root file cargo may not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// Where a build root's `Cargo.lock` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockSource {
    /// The live tree's own.
    Live,
    /// The `--commit` worktree's commit tracks it.
    Tracked,
    /// Copied from the live tree's at worktree admission.
    Seeded,
    /// Untracked and not seeded (the live tree has none): the one the slot
    /// already held.
    Retained,
    /// Untracked, not seeded, and absent: cargo resolved afresh.
    Fresh,
    /// Not the conventional root-lock layout on one side: not managed, not
    /// digested.
    Unmanaged,
}

impl LockSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Tracked => "tracked",
            Self::Seeded => "seeded",
            Self::Retained => "retained",
            Self::Fresh => "fresh",
            Self::Unmanaged => "unmanaged",
        }
    }
}

/// One file left as a Git LFS pointer ([`Build::lfs_pointers`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LfsPointer {
    /// Repository-relative path.
    pub path: String,
    /// The object the pointer names, `sha256:<hex>`.
    pub oid: String,
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
    /// The host's memory writeback and scratch-device state around and
    /// through the iteration. Absent means not observed: an iteration that
    /// was never admitted, and every iteration stored before the field.
    /// Omitted when absent, so such a document encodes as it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostTrace>,
    /// The managed device reset run for this iteration before its trace and
    /// `bench()`: present for every iteration of an arm whose `fstrim()`
    /// answered true that got as far as attempting it. Absent means no
    /// managed reset was recorded, which is not evidence of an untrimmed
    /// device. Omitted when absent, so an earlier document encodes as it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<Reset>,
}

/// The procedure a [`Reset`] ran: `syncfs` then `FITRIM` on the scratch
/// filesystem through the `brokkr-fstrim` helper.
pub const RESET_TRIM_V1: &str = "trim-v1";

/// One managed reset, as the helper reported it. Every time is
/// `CLOCK_BOOTTIME` in microseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reset {
    /// [`RESET_TRIM_V1`].
    pub procedure: String,
    /// The directory trimmed through: the iteration's own.
    pub path: String,
    /// `MAJOR:MINOR` of the filesystem as the helper read it; absent when it
    /// never reported one.
    pub target: Option<String>,
    /// The filesystem's `f_fsid` in hex, evidence beside the device.
    pub fsid: Option<String>,
    /// Whether the helper saw the brokkr flock on its descriptor; a
    /// diagnostic only.
    pub lock_seen: bool,
    pub sync: ResetStage,
    pub trim: ResetStage,
    /// The length `FITRIM` returned: what the filesystem reports it passed
    /// on for discard, not what the device discarded.
    pub reported_bytes: Option<u64>,
    /// A refusal before the stages, or brokkr's own reason the reset could
    /// not run.
    pub refused: Option<String>,
    /// The first helper line out of order or form.
    pub malformed: Option<String>,
    /// The helper's output was cut or truncated, so lines may be missing.
    pub output_cut: bool,
    /// brokkr could not confirm the helper exited: it may have run on past
    /// the record, so the run stopped there.
    pub reap_unconfirmed: bool,
    /// How the helper ended, in words (`exit 0`, `signal 9`, `interrupted`).
    pub ending: String,
    /// Whether the procedure completed: every stage in order and
    /// successful, the capture whole, exit 0.
    pub completed: bool,
    pub started_us: Option<i64>,
    pub ended_us: Option<i64>,
}

impl Reset {
    /// Whether the record itself shows a completed procedure: both stages
    /// ok, a target, its fsid and the returned length, no refusal or
    /// malformed line, a whole capture, a confirmed exit 0.
    pub fn evidences_completion(&self) -> bool {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let device = self
            .target
            .as_deref()
            .and_then(|t| t.split_once(':'))
            .is_some_and(|(major, minor)| digits(major) && digits(minor));
        let fsid = self.fsid.as_deref().is_some_and(|f| {
            f.len() == 16 && f.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        matches!(self.sync, ResetStage::Ok { .. })
            && matches!(self.trim, ResetStage::Ok { .. })
            && device
            && fsid
            && self.reported_bytes.is_some()
            && self.refused.is_none()
            && self.malformed.is_none()
            && !self.output_cut
            && !self.reap_unconfirmed
            && self.ending == "exit 0"
    }
}

/// One stage of a [`Reset`]: finished in `ns`, failed with an errno, or
/// never reported (not reached, or stopped before it said).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum ResetStage {
    Ok { ns: u64 },
    Failed { errno: i32 },
    Unreported,
}

/// One iteration's host trace. Every time is `CLOCK_BOOTTIME` in
/// microseconds, read just after the observation it stamps, so readings of
/// one boot order and subtract across iterations and runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostTrace {
    /// `/proc/sys/kernel/random/boot_id`; absent when unreadable.
    pub boot_id: Option<String>,
    /// The block device sampled for the iteration's scratch; absent when it
    /// could not be resolved, and then no reading carries device counters.
    pub device: Option<Device>,
    /// The cell cursor as this iteration found it before writing its own.
    pub previous: Option<Cursor>,
    /// Whether this iteration's in-progress cursor was written. When not,
    /// a later cell that names an earlier one as its predecessor may be
    /// missing this one between them.
    pub cursor_written: bool,
    /// When `bench()` was called; absent when it never was. Child
    /// `origin_offset_us` counts from here.
    pub block_start_us: Option<i64>,
    /// Read before `bench()` was called, and after it returned.
    pub start: Option<Reading>,
    pub end: Option<Reading>,
    /// Read after the iteration's directory was released, the cursor's end.
    pub released: Option<Reading>,
    /// Whether the release completed: when not, the directory was left to a
    /// later scan, so cleanup I/O may still follow `released`.
    pub release_completed: bool,
    /// The periodic readings in between, in order; read with children only.
    pub readings: Vec<Reading>,
    /// The run's reading budget ran out: periodic readings were dropped.
    pub capped: bool,
}

/// A sampled block device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    /// The kernel name, as under `/sys/class/block`.
    pub name: String,
    pub major: u32,
    pub minor: u32,
    pub layer: DeviceLayer,
    /// `/sys/class/block/NAME/diskseq` where the kernel has it: the device
    /// instance, distinct across re-creation at one number.
    pub diskseq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceLayer {
    /// A whole disk: the filesystem's own, or the parent of its partition,
    /// so its counters include every sibling partition.
    Disk,
    /// A mapped node (dm, md, loop): its own counters, not the physical
    /// drive's.
    Mapped,
}

impl DeviceLayer {
    pub fn name(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Mapped => "mapped",
        }
    }
}

/// One host reading. Each part is absent when its read failed, never zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    pub at_us: i64,
    /// `/proc/meminfo` `Dirty` and `Writeback`.
    pub dirty_kb: Option<i64>,
    pub writeback_kb: Option<i64>,
    pub device: Option<DeviceCounters>,
}

/// A device's cumulative `/sys/class/block/NAME/stat` counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCounters {
    /// 512-byte sectors, credited on completion.
    pub sectors_written: i64,
    pub writes_completed: i64,
    /// Milliseconds requests spent waiting and in service, summed.
    pub write_ticks_ms: i64,
    /// Reads and writes in flight.
    pub in_flight: i64,
    /// Absent on a kernel without discard statistics.
    pub sectors_discarded: Option<i64>,
}

/// The bench cell cursor: the last script-bench iteration this user's
/// brokkr started or ended on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Cursor {
    /// Started and never ended: its end, and what it did, are unknown.
    InProgress { cell: Cell },
    Ended {
        cell: Cell,
        device: Option<Device>,
        /// Its `released` reading.
        reading: Reading,
        release_completed: bool,
    },
}

/// Which iteration a cursor names. Its run may never have been published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub run: String,
    pub project: String,
    pub bench: String,
    /// From 0.
    pub iteration: u32,
    pub boot_id: Option<String>,
}

impl HostTrace {
    /// What publication refuses of a trace; a writer checks its own first,
    /// so a bad cursor file never costs it the run.
    pub fn validate(&self) -> Result<(), String> {
        let bounds = |r: &Reading| {
            r.at_us >= 0
                && r.dirty_kb.is_none_or(|v| v >= 0)
                && r.writeback_kb.is_none_or(|v| v >= 0)
                && r.device.is_none_or(|d| {
                    d.sectors_written >= 0
                        && d.writes_completed >= 0
                        && d.write_ticks_ms >= 0
                        && d.in_flight >= 0
                        && d.sectors_discarded.is_none_or(|v| v >= 0)
                })
        };
        let ordered: Vec<&Reading> = self
            .start
            .iter()
            .chain(&self.readings)
            .chain(&self.end)
            .chain(&self.released)
            .collect();
        if ordered.iter().any(|r| !bounds(r)) {
            return Err("a host reading out of range".to_owned());
        }
        if ordered.windows(2).any(|w| w[0].at_us > w[1].at_us) {
            return Err("host readings out of order".to_owned());
        }
        if self.device.is_none() && ordered.iter().any(|r| r.device.is_some()) {
            return Err("device counters without a sampled device".to_owned());
        }
        if self.block_start_us.is_some_and(|b| b < 0) {
            return Err("a negative block start".to_owned());
        }
        if let Some(Cursor::Ended {
            cell,
            device,
            reading,
            ..
        }) = &self.previous
        {
            if !bounds(reading) {
                return Err("a previous cell reading out of range".to_owned());
            }
            if device.is_none() && reading.device.is_some() {
                return Err("a previous cell's counters without its device".to_owned());
            }
            if cell.boot_id.is_some()
                && cell.boot_id == self.boot_id
                && self.start.as_ref().is_some_and(|s| reading.at_us > s.at_us)
            {
                return Err("a previous cell of this boot ending after this start".to_owned());
            }
        }
        Ok(())
    }
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
    /// The page-cache preparation brokkr made of the call's inputs before
    /// launching it (`cold` and `warm`). Absent means no brokkr preparation
    /// was recorded, never a residency: a call that asked none, and every
    /// child stored before the field existed. Omitted when absent, so such a
    /// document encodes as it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CachePreparation>,
    /// The child's interval on its own clock, the boot clock: where it sits
    /// on the host trace and where its timeline ends. Absent for a child
    /// stored before the field existed, whose readers fall back to its wall
    /// and origin offset. Omitted when absent, so such a document encodes as
    /// it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<ChildBootInterval>,
}

/// A child's interval on the boot clock (`CLOCK_BOOTTIME`, microseconds):
/// `launch_us` its child clock's zero, read just before the spawn, and
/// `interval_us` the span to the end observation its wall also stamps,
/// absent when the boot clock could not be read there. The span differs from
/// the wall by any suspend, and by the skew of reading two clocks in turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildBootInterval {
    pub launch_us: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_us: Option<i64>,
}

/// The page-cache procedure a preparation followed, as stored accounts name
/// it.
pub const CACHE_PROCEDURE: &str = "page-cache-v1";

/// One call's page-cache preparation, established before its launch: every
/// file prepared, then every file scanned. The counts are what brokkr
/// accumulated while it queried every page of each file during the scan
/// interval, not one simultaneous snapshot, and a page under read is
/// reported not resident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePreparation {
    pub procedure: String,
    /// When the scan began and ended, from the start of the iteration's
    /// block, as `origin_offset_us` is.
    pub scan_from_us: i64,
    pub scan_to_us: i64,
    /// Each prepared file, one per inode, in the order the call listed them.
    pub files: Vec<CacheFile>,
}

/// One prepared file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheFile {
    /// Every input path the call listed for this inode.
    pub paths: Vec<String>,
    pub mode: CacheMode,
    /// The file's size when it was prepared.
    pub bytes: u64,
    /// The bytes of it the scan found resident, clipped at its end.
    pub resident: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    /// Advised out of the page cache: none of it resident.
    Cold,
    /// Read through: all of it resident.
    Warm,
}

impl CacheMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
        }
    }
}

impl CachePreparation {
    /// The account publication admits, and reading checks again, so a
    /// store holding anything else never presents it as established.
    pub(super) fn validate(&self, origin_offset_us: i64) -> Result<(), String> {
        if self.procedure.is_empty() {
            return Err("a cache preparation with an empty procedure".to_owned());
        }
        if self.scan_from_us < 0
            || self.scan_from_us > self.scan_to_us
            || self.scan_to_us > origin_offset_us
        {
            return Err(format!(
                "a cache scan from {} to {} does not end by the launch at {origin_offset_us}",
                self.scan_from_us, self.scan_to_us
            ));
        }
        if self.files.is_empty() {
            return Err("a cache preparation of no file".to_owned());
        }
        for file in &self.files {
            if file.paths.is_empty() || file.paths.iter().any(|p| !p.starts_with('/')) {
                return Err("a prepared file without an absolute path".to_owned());
            }
            if i64::try_from(file.bytes).is_err() {
                return Err("a prepared file's size past the store's range".to_owned());
            }
            let established = match file.mode {
                CacheMode::Cold => file.resident == 0,
                CacheMode::Warm => file.resident == file.bytes,
            };
            if !established {
                return Err(format!(
                    "a {} file of {} bytes with {} resident",
                    file.mode.name(),
                    file.bytes,
                    file.resident
                ));
            }
        }
        Ok(())
    }
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
        if let Some(schedule) = &self.schedule {
            schedule
                .validate()
                .map_err(|e| refuse(format!("schedule: {e}")))?;
            if matches!(schedule.phase, super::schedule::Phase::Build { .. })
                && (!self.iterations.is_empty() || matches!(self.outcome, RunOutcome::Completed))
            {
                return Err(refuse(
                    "a build-phase run ran iterations or completed: it records a build that \
                     failed or was interrupted",
                ));
            }
        }
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
        if let Some(lock) = &self.build.lockfile
            && let Some(digest) = &lock.digest
        {
            if lock.source == LockSource::Unmanaged {
                return Err(refuse("an unmanaged lockfile with a digest"));
            }
            if !is_lower_hex(digest, &[32]) {
                return Err(refuse(format!(
                    "lockfile digest {digest:?} is not 32 lowercase hex digits"
                )));
            }
        }
        if let Some(flags) = &self.build.rustflags {
            // The rule `rustflags` tokens load under: none empty, none
            // holding a NUL or cargo's 0x1f separator.
            if let Some(token) = flags
                .additions
                .iter()
                .find(|t| t.is_empty() || t.contains(['\0', '\u{1f}']))
            {
                return Err(refuse(format!(
                    "rustflags token {token:?} is empty or holds a NUL or 0x1f"
                )));
            }
            match (&flags.placement, flags.additions.is_empty()) {
                (Some(Placement::None), false) => {
                    return Err(refuse("rustflags additions placed as nothing injected"));
                }
                (Some(Placement::Env { .. } | Placement::Config { .. }), true) => {
                    return Err(refuse("an empty rustflags recipe with an injection"));
                }
                (Some(placement), false) if !placement.carries(&flags.additions) => {
                    return Err(refuse(
                        "a rustflags placement that does not carry its additions",
                    ));
                }
                _ => {}
            }
            if self.build.artifact.is_some() && flags.placement.is_none() {
                return Err(refuse("a built artifact whose rustflags were never placed"));
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
                let reset = self.iterations.iter().map(|i| i.reset.is_some());
                if reset.clone().any(|r| r) && !reset.clone().all(|r| r) {
                    return Err(refuse(
                        "a completed run whose iterations disagree on a managed reset",
                    ));
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
    /// A run whose iterations ran a managed reset is `bench-pair-v4` with
    /// the procedure around that key, so it pairs only with runs reset the
    /// same way.
    pub fn pairing_key(&self) -> String {
        let inner = self.unreset_pairing_key();
        match self.reset_procedure() {
            None => inner,
            Some(procedure) => serde_json::json!(["bench-pair-v4", procedure, inner]).to_string(),
        }
    }

    /// The procedure the run's iterations reset with, from the first that
    /// recorded one; a completed run's iterations all agree (`validate`).
    pub fn reset_procedure(&self) -> Option<&str> {
        self.iterations
            .iter()
            .find_map(|i| i.reset.as_ref())
            .map(|r| r.procedure.as_str())
    }

    fn unreset_pairing_key(&self) -> String {
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
        if let Some(reset) = &self.reset {
            if reset.procedure != RESET_TRIM_V1 {
                return Err(format!("unknown reset procedure {:?}", reset.procedure));
            }
            if let (Some(a), Some(b)) = (reset.started_us, reset.ended_us)
                && (a < 0 || b < a)
            {
                return Err("a reset that ends before it starts".to_owned());
            }
            if matches!(self.outcome, IterationOutcome::Completed { .. }) && !reset.completed {
                return Err("a completed iteration whose reset did not complete".to_owned());
            }
            if reset.completed && !reset.evidences_completion() {
                return Err(
                    "a reset claimed completed whose stages, ending or capture say otherwise"
                        .to_owned(),
                );
            }
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
        if let Some(host) = &self.host {
            host.validate().map_err(|e| format!("host: {e}"))?;
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
        if let Some(boot) = self.boot
            && (boot.launch_us < 0 || boot.interval_us.is_some_and(|us| us < 0))
        {
            return Err("negative boot launch or interval".to_owned());
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
        if let Some(cache) = &self.cache {
            cache.validate(self.origin_offset_us)?;
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
