// Plan types, the persisted lane wire form, and plan-domain predicates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use crate::profile::ResolvedSweep;
use super::super::{lane_kind, selector_kind_of, DeclaredShape, Resolution, TestBinary};

/// Index of one lane in the invocation's ordered plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct LaneIdx(usize);

impl LaneIdx {
    pub(crate) fn new(value: usize) -> Self {
        Self(value)
    }

    pub(crate) fn get(self) -> usize {
        self.0
    }
}

impl From<LaneIdx> for usize {
    fn from(value: LaneIdx) -> Self {
        value.get()
    }
}

/// xxh3-64 content fingerprint. Its serde form remains the same hexadecimal
/// string used by the existing plan record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct ContentHash(String);

/// Executable path, kept separate from its content fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ExecutablePath(String);

/// Cargo target kind after brokkr's selector normalization (`lib`, `bin`,
/// `test`, and the other cargo target kinds).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct TargetKind(String);

impl From<String> for TargetKind {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TargetKind {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl PartialEq<&str> for TargetKind {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

impl Display for TargetKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Typed artifact fingerprint used by drift comparison. The plan and prepare
/// boundary still speak their existing wire primitives.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ArtifactFingerprint {
    pub(crate) resolution: Resolution,
    pub(crate) executable: ExecutablePath,
    pub(crate) hash: ContentHash,
}

impl ArtifactFingerprint {
    pub(crate) fn new(resolution: Option<&str>, executable: String, hash: String) -> Self {
        Self {
            resolution: Resolution::from_option(resolution),
            executable: ExecutablePath(executable),
            hash: ContentHash(hash),
        }
    }
}

/// What one test binary IS: its package, its normalized target kind and its
/// target name. The executable path is launch metadata, not identity - the
/// same binary rebuilt is the same unit, and two binaries of one package are
/// two units however their paths compare.
///
/// The kind is normalized the way cargo's selectors read it (`lib` for every
/// library flavour, proc-macro included), which is also nextest's id rule:
/// `rlib`/`cdylib`/`proc-macro` harnesses are the one library harness of
/// their package, and an id built from the raw kind used to orphan every lib
/// pair only the engine lane ran.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct BinaryUnit {
    pub(crate) package_id: String,
    pub(crate) package: String,
    pub(crate) kind: TargetKind,
    pub(crate) target: String,
}

impl BinaryUnit {
    pub(crate) fn of(binary: &TestBinary) -> Self {
        Self {
            package_id: binary.package_id.clone(),
            package: binary.package.clone(),
            kind: selector_kind_of(&binary.kind).to_owned().into(),
            target: binary.target.clone(),
        }
    }

    /// nextest's `RustBinaryId` rendering - the display form, and the key the
    /// engine's events are mapped back onto units by.
    pub(crate) fn id(&self) -> String {
        nextest_metadata::RustBinaryId::from_parts(
            &self.package,
            &nextest_metadata::RustTestBinaryKind::new(self.kind.0.clone()),
            &self.target,
        )
        .to_string()
    }
}

/// A sweep's declared policy-shape digest, used to group policy universes.
/// The identity is minted from the named, explicitly encoded `DeclaredShape`;
/// two lanes with one declared shape share it even when their support
/// pre-builds differ.
pub(crate) fn shape_id(sweep: &ResolvedSweep) -> String {
    DeclaredShape::of(sweep).id().to_string()
}

/// The unit policy works on: one test, in one binary, of one cargo resolution
/// of one build shape. No conditional keying: the package-keyed and
/// binary-id-keyed pairs of the old audit were the same thing at two
/// granularities, and the coarse one merged two binaries' same-named tests.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct PairId {
    pub(crate) shape: String,
    pub(crate) resolution: Option<String>,
    pub(crate) unit: BinaryUnit,
    pub(crate) test: String,
}

/// The unit accounting works on: one expected execution of a pair, by one
/// lane. The same pair selected by two lanes is two executions, each of which
/// must be accounted for on its own. `attempt` is always 1 - no lane retries -
/// and is carried so a retry could never be silently folded into a first run.
///
/// The harness invocation is not part of the key; it is ENFORCED instead.
/// Within one lane a pair must run in exactly one harness process (its
/// binary's, or its own on the isolated lane, or the engine's one stream), and
/// reconciliation holds every execution to the first stream that reported it:
/// a start or terminal for the same execution on a second stream is a
/// `duplicate_execution` anomaly, never a second chance to pass or fail.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExecutionId {
    pub(crate) lane: LaneIdx,
    pub(crate) attempt: u32,
    pub(crate) pair: PairId,
}

/// How a lane executes its tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaneKind {
    /// `cargo test`, every harness isolated by the shim.
    Serial,
    /// Prebuilt binaries executed concurrently.
    Parallel,
    /// One process per test.
    Isolated,
    /// The nextest engine.
    Nextest,
    /// `cargo test --doc`: doctests only, no binary executions.
    DocOnly,
}

/// One planned test executable: where it is and what it was, so a lane can
/// prove before it runs that it is about to run what was enumerated.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PlannedArtifact {
    pub(crate) resolution: Option<String>,
    pub(crate) unit: BinaryUnit,
    pub(crate) executable: String,
    /// xxh3-64 of the file, hex.
    pub(crate) hash: String,
}

/// One shape resolution's universe.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShapeRecord {
    pub(crate) shape: String,
    pub(crate) resolution: Option<String>,
    pub(crate) label: String,
    /// Every lane of this shape is `curated = true`.
    pub(crate) curated: bool,
    /// The universe was enumerated and every lane of the shape prepared, so
    /// its classification is a finding rather than a guess.
    pub(crate) complete: bool,
    pub(crate) universe: Vec<(BinaryUnit, String)>,
    pub(crate) ignored: Vec<(BinaryUnit, String)>,
}

/// One lane's selection and the executions it implies.
///
/// Persisted through [`LaneRecordWire`]: a lane's pairs all share its shape,
/// and a pair repeats its whole binary unit (a package id URL, a kind, a
/// target) for every test, so the flat form of a workspace's plan was
/// megabytes per lane, per run - and every run, certifying or not, now keeps
/// one. The wire form groups the pairs by (resolution, binary) and drops the
/// shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(from = "LaneRecordWire", into = "LaneRecordWire")]
pub(crate) struct LaneRecord {
    pub(crate) lane: usize,
    pub(crate) label: String,
    pub(crate) kind: LaneKind,
    pub(crate) shape: String,
    /// The lane's inventory was prepared: its executions are known. False
    /// for a lane that could not be prepared and for one whose streams
    /// cannot be attributed to binaries ([`Self::unavailable`] says which).
    pub(crate) prepared: bool,
    /// Why this lane has no inventory, when it has none. Set for a lane a
    /// certifying claim would have refused (cargo-mediated parallelism, a
    /// serial lane with no harness shim) and for one whose preparation
    /// failed under a claim that does not require it: the lane still runs,
    /// and what it observed is reported as such, never as a made-up
    /// selection.
    pub(crate) unavailable: Option<String>,
    /// The lane did not run in this invocation by choice (a CLI `-p` its
    /// config rules out), so it expects nothing.
    pub(crate) skipped: Option<String>,
    /// Binaries of the lane that answer no libtest listing, so the tests
    /// they hold are outside the inventory: `(binary id, why)`. Never present
    /// on a certifying lane, which refuses them.
    pub(crate) unlisted: Vec<(String, String)>,
    pub(crate) include_ignored: bool,
    /// Will run doctests: a `doc_only` lane, or a serial lane under `[test]
    /// doctests = true` whose selection carries no target selector (one
    /// turns cargo's doctest pass off). See [`lane_runs_doctests`]. Only a
    /// carrier's lane may present a rustdoc stream on a strict session.
    pub(crate) doc_carrier: bool,
    /// The carrier must present at least one rustdoc stream to have
    /// completed: a `doc_only` lane always (cargo refuses `--doc` with no
    /// library), a serial carrier when its selection reaches a member whose
    /// cargo metadata marks a target doctested (`DocFacts::obliges` - never
    /// inferred from a library harness, which `test`/`doctest` set
    /// independently). The count beyond one is not knowable - doctests have no
    /// inventory - so one is the floor that tells "ran" from "never ran".
    pub(crate) doc_streams_required: bool,
    /// Every pair this lane is expected to execute.
    pub(crate) executions: Vec<PairId>,
    /// Pairs the lane's filters select that run as `ignored` (the lane does not
    /// lift `#[ignore]`): libtest still reports them, and that report is not an
    /// unplanned test.
    pub(crate) ignored_selected: Vec<PairId>,
    /// `#[bench]` functions the lane's filters admit: outside the claim, but a
    /// `cargo test` run executes each once in test mode and reports it like a
    /// test, so its records are neither accounted nor unplanned.
    pub(crate) outside_claim: Vec<PairId>,
    pub(crate) artifacts: Vec<PlannedArtifact>,
}

impl LaneRecord {
    /// A lane with nothing planned yet.
    pub(crate) fn empty(lane: usize, label: String, kind: LaneKind, shape: String) -> Self {
        Self {
            lane,
            label,
            kind,
            shape,
            prepared: false,
            unavailable: None,
            skipped: None,
            unlisted: Vec::new(),
            include_ignored: false,
            doc_carrier: false,
            doc_streams_required: false,
            executions: Vec::new(),
            ignored_selected: Vec::new(),
            outside_claim: Vec::new(),
            artifacts: Vec::new(),
        }
    }
}

/// One binary's tests of a lane, as persisted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PairGroup {
    resolution: Option<String>,
    unit: BinaryUnit,
    tests: Vec<String>,
}

/// Group pairs by (resolution, binary) in first-seen order.
fn group_pairs(pairs: &[PairId]) -> Vec<PairGroup> {
    let mut out: Vec<PairGroup> = Vec::new();
    for p in pairs {
        match out.iter_mut().rev().find(|g| g.resolution == p.resolution && g.unit == p.unit) {
            Some(g) => g.tests.push(p.test.clone()),
            None => out.push(PairGroup {
                resolution: p.resolution.clone(),
                unit: p.unit.clone(),
                tests: vec![p.test.clone()],
            }),
        }
    }
    out
}

fn ungroup_pairs(shape: &str, groups: Vec<PairGroup>) -> Vec<PairId> {
    groups
        .into_iter()
        .flat_map(|g| {
            let PairGroup { resolution, unit, tests } = g;
            tests.into_iter().map(move |test| PairId {
                shape: shape.to_owned(),
                resolution: resolution.clone(),
                unit: unit.clone(),
                test,
            })
        })
        .collect()
}

/// [`LaneRecord`] as it sits on disk. The defaulted fields are the ones a
/// reader of an older record may not find.
#[derive(serde::Serialize, serde::Deserialize)]
struct LaneRecordWire {
    lane: usize,
    label: String,
    kind: LaneKind,
    shape: String,
    prepared: bool,
    #[serde(default)]
    unavailable: Option<String>,
    #[serde(default)]
    skipped: Option<String>,
    #[serde(default)]
    unlisted: Vec<(String, String)>,
    include_ignored: bool,
    doc_carrier: bool,
    #[serde(default)]
    doc_streams_required: bool,
    executions: Vec<PairGroup>,
    ignored_selected: Vec<PairGroup>,
    #[serde(default)]
    outside_claim: Vec<PairGroup>,
    artifacts: Vec<PlannedArtifact>,
}

impl From<LaneRecord> for LaneRecordWire {
    fn from(l: LaneRecord) -> Self {
        Self {
            executions: group_pairs(&l.executions),
            ignored_selected: group_pairs(&l.ignored_selected),
            outside_claim: group_pairs(&l.outside_claim),
            lane: l.lane,
            label: l.label,
            kind: l.kind,
            shape: l.shape,
            prepared: l.prepared,
            unavailable: l.unavailable,
            skipped: l.skipped,
            unlisted: l.unlisted,
            include_ignored: l.include_ignored,
            doc_carrier: l.doc_carrier,
            doc_streams_required: l.doc_streams_required,
            artifacts: l.artifacts,
        }
    }
}

impl From<LaneRecordWire> for LaneRecord {
    fn from(w: LaneRecordWire) -> Self {
        Self {
            executions: ungroup_pairs(&w.shape, w.executions),
            ignored_selected: ungroup_pairs(&w.shape, w.ignored_selected),
            outside_claim: ungroup_pairs(&w.shape, w.outside_claim),
            lane: w.lane,
            label: w.label,
            kind: w.kind,
            shape: w.shape,
            prepared: w.prepared,
            unavailable: w.unavailable,
            skipped: w.skipped,
            unlisted: w.unlisted,
            include_ignored: w.include_ignored,
            doc_carrier: w.doc_carrier,
            doc_streams_required: w.doc_streams_required,
            artifacts: w.artifacts,
        }
    }
}

/// A dead `skip`/`only` filter found during preparation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DeadFilterRecord {
    pub(crate) sweeps: String,
    pub(crate) origin: String,
    pub(crate) label: String,
}

/// The whole invocation, prepared: written to disk before the first test runs.
///
/// Two products live here and are kept apart. The EXECUTION INVENTORY - each
/// lane's selected executions, the binaries they live in and the artifacts'
/// identity - is prepared for every enumerable lane of every run and
/// journaled against, certifying or not.
/// The POLICY UNIVERSE (`shapes`, `dead_filters`) is prepared only under a
/// certifying claim, because only that claim is about what the profile does
/// not select.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct AccountingPlan {
    pub(crate) run_id: String,
    /// The run is held to a `certifies = "complete"` claim: its policy
    /// universe was prepared, its lanes were held to the attribution rules
    /// that claim needs, and its executions are held to the green invariant.
    /// A plan without it is an inventory and nothing more.
    #[serde(default)]
    pub(crate) certifying: bool,
    /// False when anything could not be prepared; `incomplete` says what.
    pub(crate) complete: bool,
    pub(crate) incomplete: Vec<String>,
    pub(crate) shapes: Vec<ShapeRecord>,
    pub(crate) lanes: Vec<LaneRecord>,
    pub(crate) dead_filters: Vec<DeadFilterRecord>,
}

impl AccountingPlan {
    pub(crate) fn lane(&self, lane: impl Into<usize>) -> Option<&LaneRecord> {
        let lane = lane.into();
        self.lanes.iter().find(|l| l.lane == lane)
    }
}

/// Whether a lane will run doctests - the one definition the plan's carrier
/// flag and the strict shim's rustdoc admission share. A `doc_only` lane does;
/// a serial libtest lane does under `[test] doctests = true` unless its
/// selection names a target (`run_one_test_sweep` then leaves cargo's doctest
/// pass off, and `--tests` is not appended either); no other lane runs any.
pub(crate) fn lane_runs_doctests(sweep: &ResolvedSweep, doctests: bool) -> bool {
    match lane_kind(sweep) {
        LaneKind::DocOnly => true,
        // The package selection, profile and feature flags never name a
        // target, so the lane's own target filters decide it, whatever
        // packages the lane selects.
        LaneKind::Serial => doctests && sweep.filters.targets.is_empty(),
        LaneKind::Parallel | LaneKind::Isolated | LaneKind::Nextest => false,
    }
}

/// Every way the artifacts a lane is about to run differ from the ones the
/// plan enumerated: a planned executable missing or rebuilt with other
/// content, or an executable the plan never saw. Path AND content, because a
/// shape whose difference cargo does not hash into the file name (an env var a
/// build script reads) rebuilds the same path in place - and the binary at
/// that path is then another shape's, which no path comparison can see.
pub(crate) fn artifact_drift(
    planned: &[PlannedArtifact],
    current: &[(Option<String>, String, String)],
) -> Vec<String> {
    let current: Vec<ArtifactFingerprint> = current
        .iter()
        .map(|(resolution, executable, hash)| {
            ArtifactFingerprint::new(resolution.as_deref(), executable.clone(), hash.clone())
        })
        .collect();
    artifact_drift_typed(planned, &current)
}

pub(crate) fn artifact_drift_typed(
    planned: &[PlannedArtifact],
    current: &[ArtifactFingerprint],
) -> Vec<String> {
    let mut out = Vec::new();
    let now: BTreeMap<(&Resolution, &ExecutablePath), &ContentHash> = current
        .iter()
        .map(|artifact| ((&artifact.resolution, &artifact.executable), &artifact.hash))
        .collect();
    for p in planned {
        let resolution = Resolution::from_option(p.resolution.as_deref());
        let executable = ExecutablePath(p.executable.clone());
        match now.get(&(&resolution, &executable)) {
            None => out.push(format!("{} ({}) is gone", p.unit.id(), p.executable)),
            Some(hash) if hash.0.as_str() != p.hash.as_str() => out.push(format!(
                "{} ({}) was rebuilt with different content",
                p.unit.id(),
                p.executable
            )),
            Some(_) => {}
        }
    }
    let known: BTreeSet<(Resolution, ExecutablePath)> = planned
        .iter()
        .map(|p| (Resolution::from_option(p.resolution.as_deref()), ExecutablePath(p.executable.clone())))
        .collect();
    for artifact in current {
        if !known.contains(&(artifact.resolution.clone(), artifact.executable.clone())) {
            out.push(format!("{} was not in the plan", artifact.executable.0));
        }
    }
    out
}

