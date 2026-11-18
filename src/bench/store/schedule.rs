//! Where a run sits in a `--rounds` invocation: the plan every run of one
//! invocation carries whole, and the run's own slot in it.
//!
//! The plan is in every run rather than in a record of its own, so one
//! stored run is enough to name every planned slot that never stored. What a
//! reader derives from the runs of one invocation (blocked, absent) is not
//! stored.

use serde::{Deserialize, Serialize};

/// The most rounds a plan holds: far past any comparison, and small enough
/// that a reader may walk every slot.
pub const MAX_ROUNDS: u32 = 10_000;

/// The most slots a plan holds, all its workloads, competitors and rounds
/// together, for the same reason.
pub const MAX_SLOTS: u64 = 100_000;

impl Plan {
    /// Its workloads, competitors and rounds multiplied; `None` past `u64`.
    pub fn slots(&self) -> Option<u64> {
        u64::try_from(self.workloads.len())
            .ok()?
            .checked_mul(u64::try_from(self.competitors.len()).ok()?)?
            .checked_mul(u64::from(self.rounds))
    }
}

/// A run's place in a `--rounds` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// The invocation's id, 32 lowercase hex digits, shared by its runs.
    pub invocation: String,
    pub plan: Plan,
    pub slot: Slot,
    pub phase: Phase,
}

/// What one invocation set out to run: every competitor in every workload,
/// `rounds` times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub axis: Axis,
    /// Each competitor's label, in plan order: at least two.
    pub competitors: Vec<String>,
    /// Each workload's label, in plan order: at least one, the lone one
    /// empty when nothing but the competitors fans.
    pub workloads: Vec<String>,
    pub rounds: u32,
    /// The build each competitor runs, by index into the invocation's
    /// builds: competitors sharing one build share its index.
    pub builds: Vec<u32>,
}

/// What the competitors differ in. Externally tagged, as the document's
/// other enums with unit variants are, so a field a newer writer added is
/// refused rather than dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Axis {
    /// The values of one param (`--versus NAME`).
    Param { name: String },
    /// The commit built (`--commit` given more than once).
    Build,
    /// The bench (NAME a list).
    Bench,
}

impl Axis {
    pub fn name(&self) -> String {
        match self {
            Self::Param { name } => format!("param {name}"),
            Self::Build => "build".to_owned(),
            Self::Bench => "bench".to_owned(),
        }
    }
}

/// One planned cell: a competitor in a workload in a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub workload: u32,
    pub competitor: u32,
    pub round: u32,
    /// Its position within the round, from 0: the competitor's own index in
    /// an even round, reversed in an odd one.
    pub position: u32,
}

/// Which step of the invocation the run records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The up-front build of [`Plan::builds`] index `build` failed or was
    /// interrupted, attributed to the first slot that runs it. No iteration
    /// ran; every slot of that build is blocked by this run.
    Build { build: u32 },
    /// The slot's own iterations.
    Measure,
}

/// The position competitor `competitor` of `count` takes in round `round`:
/// its own index in an even round, reversed in an odd one.
pub fn position(competitor: u32, count: u32, round: u32) -> u32 {
    if round % 2 == 0 {
        competitor
    } else {
        count - 1 - competitor
    }
}

impl Schedule {
    /// Refuse a schedule no invocation can have.
    pub fn validate(&self) -> Result<(), String> {
        if !super::document::is_lower_hex(&self.invocation, &[32]) {
            return Err(format!(
                "invocation {:?} is not 32 lowercase hex digits",
                self.invocation
            ));
        }
        let plan = &self.plan;
        let count = u32::try_from(plan.competitors.len()).map_err(|_| "too many competitors")?;
        if count < 2 {
            return Err("a plan has at least two competitors".into());
        }
        if plan.workloads.is_empty() {
            return Err("a plan has at least one workload".into());
        }
        if plan.rounds == 0 || plan.rounds > MAX_ROUNDS {
            return Err(format!(
                "a plan has 1 to {MAX_ROUNDS} rounds, not {}",
                plan.rounds
            ));
        }
        if plan.slots().is_none_or(|n| n > MAX_SLOTS) {
            return Err(format!(
                "a plan holds at most {MAX_SLOTS} slots (workloads, competitors and rounds \
                 multiplied)"
            ));
        }
        if plan.builds.len() != plan.competitors.len() {
            return Err(format!(
                "{} builds named for {} competitors",
                plan.builds.len(),
                plan.competitors.len()
            ));
        }
        // Numbered from 0 in the order the competitors first need them.
        let mut next = 0u32;
        for &build in &plan.builds {
            if build > next {
                return Err(format!(
                    "build {build} is named before build {next}: builds are numbered in the \
                     order competitors first need them"
                ));
            }
            if build == next {
                next += 1;
            }
        }
        if let Axis::Param { name } = &plan.axis
            && name.is_empty()
        {
            return Err("a param axis names no param".into());
        }
        let slot = self.slot;
        if slot.competitor >= count
            || usize::try_from(slot.workload).map_or(true, |w| w >= plan.workloads.len())
            || slot.round >= plan.rounds
        {
            return Err(format!("slot {slot:?} lies outside the plan"));
        }
        if slot.position != position(slot.competitor, count, slot.round) {
            return Err(format!(
                "slot {slot:?} is not where its round puts that competitor"
            ));
        }
        if let Phase::Build { build } = self.phase {
            let first = plan.builds.iter().position(|b| *b == build);
            if first != Some(slot.competitor as usize) || slot.workload != 0 || slot.round != 0 {
                return Err(format!(
                    "a build-phase run for build {build} is not attributed to the first slot \
                     it serves"
                ));
            }
        }
        Ok(())
    }
}
