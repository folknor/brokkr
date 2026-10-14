// Plan types, the persisted lane wire form, and plan-domain predicates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use crate::profile::PlannedSweep;
use super::super::{lane_kind, selector_kind_of, DeclaredShape, Resolution, ShapeId, TestBinary};

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

/// Values whose identity is their position in the invocation's sweep list.
/// Construction always walks that source list, and callers address entries
/// with `LaneIdx` rather than a raw integer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lanes<T> {
    entries: Vec<T>,
}

impl<T> Lanes<T> {
    pub(crate) fn from_sweeps<S>(sweeps: &[S], mut make: impl FnMut(LaneIdx, &S) -> T) -> Self {
        Self {
            entries: sweeps
                .iter()
                .enumerate()
                .map(|(index, sweep)| make(LaneIdx::new(index), sweep))
                .collect(),
        }
    }

    pub(crate) fn try_from_sweeps<S, E>(
        sweeps: &[S],
        mut make: impl FnMut(LaneIdx, &S) -> Result<T, E>,
    ) -> Result<Self, E> {
        let mut entries = Vec::with_capacity(sweeps.len());
        for (index, sweep) in sweeps.iter().enumerate() {
            entries.push(make(LaneIdx::new(index), sweep)?);
        }
        Ok(Self { entries })
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn get(&self, lane: LaneIdx) -> Option<&T> {
        self.entries.get(lane.get())
    }

    pub(crate) fn get_mut(&mut self, lane: LaneIdx) -> Option<&mut T> {
        self.entries.get_mut(lane.get())
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries.iter()
    }

    pub(crate) fn iter_with<'a, 'b: 'a, S>(
        &'a self,
        sweeps: &'b [S],
    ) -> impl Iterator<Item = (LaneIdx, &'b S, &'a T)> + 'a {
        assert_eq!(self.entries.len(), sweeps.len(), "lane data must match its sweep list");
        sweeps
            .iter()
            .zip(&self.entries)
            .enumerate()
            .map(|(i, (sweep, entry))| (LaneIdx::new(i), sweep, entry))
    }
}

/// xxh3-64 content fingerprint. Its serde form remains the same hexadecimal
/// string used by the existing plan record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct ContentHash(String);

impl ContentHash {
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str { &self.0 }
}

/// Executable path, kept separate from its content fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct ExecutablePath(String);

impl Display for ExecutablePath {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

/// A libtest name carried in a plan pair.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct TestName(String);

impl From<String> for TestName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TestName {
    fn from(value: &str) -> Self { Self(value.to_owned()) }
}

impl TestName {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl PartialEq<&str> for TestName {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

impl Display for TestName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

/// A binary's stable display id in inventory diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct BinaryId(String);

impl From<String> for BinaryId {
    fn from(value: String) -> Self { Self(value) }
}

impl From<&str> for BinaryId {
    fn from(value: &str) -> Self { Self(value.to_owned()) }
}

impl BinaryId {
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str { &self.0 }
}

impl Display for BinaryId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

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

/// Typed artifact fingerprint used by drift comparison.
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

impl PlannedArtifact {
    pub(crate) fn new(
        resolution: Option<&str>,
        unit: BinaryUnit,
        executable: String,
        hash: String,
    ) -> Self {
        Self {
            resolution: Resolution::from_option(resolution),
            unit,
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
pub(crate) fn shape_id(sweep: &PlannedSweep) -> ShapeId {
    DeclaredShape::of(sweep).id()
}

/// The unit policy works on: one test, in one binary, of one cargo resolution
/// of one build shape. No conditional keying: the package-keyed and
/// binary-id-keyed pairs of the old audit were the same thing at two
/// granularities, and the coarse one merged two binaries' same-named tests.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct PairId {
    pub(crate) shape: ShapeId,
    pub(crate) resolution: Resolution,
    pub(crate) unit: BinaryUnit,
    pub(crate) test: TestName,
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
    pub(crate) resolution: Resolution,
    pub(crate) unit: BinaryUnit,
    pub(crate) executable: ExecutablePath,
    /// xxh3-64 of the file, hex.
    pub(crate) hash: ContentHash,
}

/// One shape resolution's universe.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShapeRecord {
    pub(crate) shape: ShapeId,
    pub(crate) resolution: Resolution,
    pub(crate) label: String,
    /// Every lane of this shape is `curated = true`.
    pub(crate) curated: bool,
    /// The universe was enumerated and every lane of the shape prepared, so
    /// its classification is a finding rather than a guess.
    pub(crate) complete: bool,
    pub(crate) universe: Vec<(BinaryUnit, TestName)>,
    pub(crate) ignored: Vec<(BinaryUnit, TestName)>,
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
    pub(crate) lane: LaneIdx,
    pub(crate) label: String,
    pub(crate) kind: LaneKind,
    pub(crate) shape: ShapeId,
    /// One disposition owns all inventory state, so prepared, skipped,
    /// unavailable and partial listing cannot overlap.
    pub(crate) inventory: Inventory,
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
}

impl LaneRecord {
    /// A lane with nothing planned yet.
    pub(crate) fn empty(lane: LaneIdx, label: String, kind: LaneKind, shape: ShapeId) -> Self {
        Self {
            lane,
            label,
            kind,
            shape,
            inventory: if kind == LaneKind::DocOnly {
                Inventory::DocTests
            } else {
                Inventory::Unavailable(UnavailableReason::Stopped)
            },
            include_ignored: false,
            doc_carrier: false,
            doc_streams_required: false,
        }
    }
}

/// What preparation established about one lane's expected tests.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Inventory {
    Prepared {
        executions: Vec<PairId>,
        ignored_selected: Vec<PairId>,
        outside_claim: Vec<PairId>,
        artifacts: Vec<PlannedArtifact>,
        unlisted: Vec<(BinaryId, String)>,
    },
    Skipped(String),
    Unavailable(UnavailableReason),
    DocTests,
}

impl Inventory {
    pub(crate) fn is_known(&self) -> bool {
        matches!(self, Self::Prepared { .. } | Self::Skipped(_) | Self::DocTests)
    }

    pub(crate) fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    #[cfg(test)]
    pub(crate) fn unavailable_reason(&self) -> Option<&UnavailableReason> {
        match self { Self::Unavailable(reason) => Some(reason), _ => None }
    }

    #[cfg(test)]
    pub(crate) fn skipped_reason(&self) -> Option<&str> {
        match self { Self::Skipped(reason) => Some(reason), _ => None }
    }

    pub(crate) fn unlisted(&self) -> &[(BinaryId, String)] {
        match self { Self::Prepared { unlisted, .. } => unlisted, _ => &[] }
    }

    pub(crate) fn executions(&self) -> &[PairId] {
        match self { Self::Prepared { executions, .. } => executions, _ => &[] }
    }

    #[cfg(test)]
    pub(crate) fn ignored_selected(&self) -> &[PairId] {
        match self { Self::Prepared { ignored_selected, .. } => ignored_selected, _ => &[] }
    }

    #[cfg(test)]
    pub(crate) fn artifacts(&self) -> &[PlannedArtifact] {
        match self { Self::Prepared { artifacts, .. } => artifacts, _ => &[] }
    }
}

/// Why a lane has no enumerable binary inventory.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnavailableReason {
    CargoThreaded,
    NoShim(String),
    PrepareFailed(String),
    Stopped,
}

impl Display for UnavailableReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::CargoThreaded => f.write_str(
                "cargo-mediated parallelism (`test_threads` other than 1) shares one output stream across tests and harnesses, so no test can be attributed to a binary",
            ),
            Self::NoShim(reason) => write!(
                f,
                "harness isolation is unavailable ({reason}), so every harness shares cargo's one output stream and no test can be attributed to a binary",
            ),
            Self::PrepareFailed(message) => write!(f, "preparation failed: {message}"),
            Self::Stopped => f.write_str("preparation was stopped before this lane"),
        }
    }
}

/// One binary's tests of a lane, as persisted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PairGroup {
    resolution: Resolution,
    unit: BinaryUnit,
    tests: Vec<TestName>,
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

fn ungroup_pairs(shape: &ShapeId, groups: Vec<PairGroup>) -> Vec<PairId> {
    groups
        .into_iter()
        .flat_map(|g| {
            let PairGroup { resolution, unit, tests } = g;
            tests.into_iter().map(move |test| PairId {
                shape: shape.clone(),
                resolution: resolution.clone(),
                unit: unit.clone(),
                test,
            })
        })
        .collect()
}

/// Compact persisted form of an inventory. Every pair in the containing lane
/// shares its shape, so grouping drops that repeated field from the wire.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum InventoryWire {
    Prepared {
        executions: Vec<PairGroup>,
        ignored_selected: Vec<PairGroup>,
        outside_claim: Vec<PairGroup>,
        artifacts: Vec<PlannedArtifact>,
        unlisted: Vec<(BinaryId, String)>,
    },
    Skipped(String),
    Unavailable(UnavailableReason),
    DocTests,
}

impl From<Inventory> for InventoryWire {
    fn from(inventory: Inventory) -> Self {
        match inventory {
            Inventory::Prepared { executions, ignored_selected, outside_claim, artifacts, unlisted } => Self::Prepared {
                executions: group_pairs(&executions),
                ignored_selected: group_pairs(&ignored_selected),
                outside_claim: group_pairs(&outside_claim),
                artifacts,
                unlisted,
            },
            Inventory::Skipped(reason) => Self::Skipped(reason),
            Inventory::Unavailable(reason) => Self::Unavailable(reason),
            Inventory::DocTests => Self::DocTests,
        }
    }
}

impl InventoryWire {
    fn into_inventory(self, shape: &ShapeId) -> Inventory {
        match self {
            Self::Prepared { executions, ignored_selected, outside_claim, artifacts, unlisted } => Inventory::Prepared {
                executions: ungroup_pairs(shape, executions),
                ignored_selected: ungroup_pairs(shape, ignored_selected),
                outside_claim: ungroup_pairs(shape, outside_claim),
                artifacts,
                unlisted,
            },
            Self::Skipped(reason) => Inventory::Skipped(reason),
            Self::Unavailable(reason) => Inventory::Unavailable(reason),
            Self::DocTests => Inventory::DocTests,
        }
    }
}

/// [`LaneRecord`] as it sits on disk. Pair groups omit the shared shape, and
/// a lane's inventory disposition contains its selected execution data.
#[derive(serde::Serialize, serde::Deserialize)]
struct LaneRecordWire {
    lane: LaneIdx,
    label: String,
    kind: LaneKind,
    shape: ShapeId,
    inventory: InventoryWire,
    include_ignored: bool,
    doc_carrier: bool,
    #[serde(default)]
    doc_streams_required: bool,
}

impl From<LaneRecord> for LaneRecordWire {
    fn from(l: LaneRecord) -> Self {
        Self {
            lane: l.lane,
            label: l.label,
            kind: l.kind,
            shape: l.shape,
            inventory: l.inventory.into(),
            include_ignored: l.include_ignored,
            doc_carrier: l.doc_carrier,
            doc_streams_required: l.doc_streams_required,
        }
    }
}

impl From<LaneRecordWire> for LaneRecord {
    fn from(w: LaneRecordWire) -> Self {
        let shape = w.shape;
        let inventory = w.inventory.into_inventory(&shape);
        Self {
            lane: w.lane,
            label: w.label,
            kind: w.kind,
            shape,
            inventory,
            include_ignored: w.include_ignored,
            doc_carrier: w.doc_carrier,
            doc_streams_required: w.doc_streams_required,
        }
    }
}

/// A dead `skip`/`only` filter found during preparation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DeadFilterRecord {
    pub(crate) sweeps: SweepNames,
    pub(crate) origin: FilterOriginName,
    pub(crate) label: FilterLabel,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct SweepNames(String);

impl From<String> for SweepNames { fn from(value: String) -> Self { Self(value) } }
impl Display for SweepNames { fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result { f.write_str(&self.0) } }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct FilterOriginName(String);

impl From<String> for FilterOriginName { fn from(value: String) -> Self { Self(value) } }
impl Display for FilterOriginName { fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result { f.write_str(&self.0) } }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct FilterLabel(String);

impl From<String> for FilterLabel { fn from(value: String) -> Self { Self(value) } }
impl Display for FilterLabel { fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result { f.write_str(&self.0) } }

/// A diagnostic plan incompleteness reason. It retains its existing text on
/// disk and in reports while staying distinct from arbitrary strings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct PlanIssue(String);

impl From<String> for PlanIssue { fn from(value: String) -> Self { Self(value) } }
impl From<&str> for PlanIssue { fn from(value: &str) -> Self { Self(value.to_owned()) } }

impl PlanIssue {
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str { &self.0 }
}

impl Display for PlanIssue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
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
    pub(crate) incomplete: Vec<PlanIssue>,
    pub(crate) shapes: Vec<ShapeRecord>,
    pub(crate) lanes: Vec<LaneRecord>,
    pub(crate) dead_filters: Vec<DeadFilterRecord>,
}

impl AccountingPlan {
    pub(crate) fn lane(&self, lane: LaneIdx) -> Option<&LaneRecord> {
        self.lanes.iter().find(|l| l.lane == lane)
    }
}

/// Whether a lane will run doctests - the one definition the plan's carrier
/// flag and the strict shim's rustdoc admission share. A `doc_only` lane does;
/// a serial libtest lane does under `[test] doctests = true` unless its
/// selection names a target (`run_one_test_sweep` then leaves cargo's doctest
/// pass off, and `--tests` is not appended either); no other lane runs any.
pub(crate) fn lane_runs_doctests(sweep: &PlannedSweep, doctests: bool) -> bool {
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
    current: &[ArtifactFingerprint],
) -> Vec<String> {
    let mut out = Vec::new();
    let now: BTreeMap<(&Resolution, &ExecutablePath), &ContentHash> = current
        .iter()
        .map(|artifact| ((&artifact.resolution, &artifact.executable), &artifact.hash))
        .collect();
    for p in planned {
        match now.get(&(&p.resolution, &p.executable)) {
            None => out.push(format!("{} ({}) is gone", p.unit.id(), p.executable)),
            Some(hash) if hash.0.as_str() != p.hash.0.as_str() => out.push(format!(
                "{} ({}) was rebuilt with different content",
                p.unit.id(),
                p.executable
            )),
            Some(_) => {}
        }
    }
    let known: BTreeSet<(Resolution, ExecutablePath)> = planned
        .iter()
        .map(|p| (p.resolution.clone(), p.executable.clone()))
        .collect();
    for artifact in current {
        if !known.contains(&(artifact.resolution.clone(), artifact.executable.clone())) {
            out.push(format!("{} was not in the plan", artifact.executable.0));
        }
    }
    out
}

