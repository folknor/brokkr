//! The grid a `--rounds` invocation plans: from the bound arms of its
//! benches, which competitor and which workload each arm is, every
//! (workload, competitor) slot holding exactly one recipe.

use std::collections::{BTreeMap, BTreeSet};

use super::super::Frozen;
use crate::bench::{params, store};
use crate::config::bench::Bench;
use crate::error::DevError;

/// One bench of the invocation, bound and validated.
pub(super) struct Entry<'a> {
    pub(super) bench: &'a Bench,
    pub(super) frozen: Frozen,
    pub(super) arms: Vec<params::Bound>,
    pub(super) labels: Vec<String>,
    pub(super) given: params::Given,
    /// Each arm's `validate()` modules.
    pub(super) validated: Vec<BTreeSet<String>>,
    /// Each arm's `fstrim()` answer.
    pub(super) trims: Vec<bool>,
}

/// One planned cell's recipe: which bench, which of its arms, which commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Recipe {
    pub(super) bench: usize,
    pub(super) arm: usize,
    pub(super) commit: usize,
}

/// A bound arm's params as a matching key: each param's name, shape and
/// keys in bound order, over the params `keep` admits.
fn key_of(arm: &params::Bound, keep: impl Fn(&str) -> bool) -> Vec<(String, bool, Vec<String>)> {
    arm.document()
        .into_iter()
        .filter(|(name, _)| keep(name))
        .map(|(name, bound)| {
            (
                name,
                bound.many,
                bound.values.into_iter().map(|v| v.key).collect(),
            )
        })
        .collect()
}

type Key = Vec<(String, bool, Vec<String>)>;

/// The workloads' labels: `name=keys` of each param whose key differs
/// between workloads, space separated; empty for a lone workload.
fn workload_labels(keys: &[Key]) -> Vec<String> {
    let rendered: Vec<BTreeMap<&str, String>> = keys
        .iter()
        .map(|key| {
            key.iter()
                .map(|(name, _, keys)| (name.as_str(), keys.join(",")))
                .collect()
        })
        .collect();
    let names: BTreeSet<&str> = rendered.iter().flat_map(|m| m.keys().copied()).collect();
    let varying: Vec<&str> = names
        .into_iter()
        .filter(|name| {
            rendered
                .iter()
                .any(|m| m.get(name) != rendered.first().and_then(|f| f.get(name)))
        })
        .collect();
    rendered
        .iter()
        .map(|m| {
            varying
                .iter()
                .map(|name| format!("{name}={}", m.get(name).map_or("(unbound)", String::as_str)))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// What the invocation plans: the axis, each competitor's label, each
/// workload's label, and every slot's recipe by workload then competitor.
pub(super) struct Grid {
    pub(super) axis: store::Axis,
    pub(super) competitors: Vec<String>,
    pub(super) workloads: Vec<String>,
    pub(super) recipes: Vec<Vec<Recipe>>,
}

/// Index of `key` in `keys`, appended when new.
pub(super) fn intern<T: PartialEq>(keys: &mut Vec<T>, key: T) -> usize {
    if let Some(at) = keys.iter().position(|k| *k == key) {
        return at;
    }
    keys.push(key);
    keys.len() - 1
}

/// The plan for `--versus PARAM` over one bench's arms.
pub(super) fn by_param(entry: &Entry<'_>, name: &str, bench_name: &str) -> Result<Grid, DevError> {
    let Some(param) = entry.bench.params.get(name) else {
        return Err(DevError::Usage(format!(
            "bench {bench_name}: --versus {name}: the bench declares no param --{name}"
        )));
    };
    if param.many {
        return Err(DevError::Usage(format!(
            "bench {bench_name}: --versus {name}: --{name} is a collection, bound whole in one \
             arm; the competitors are a scalar's values"
        )));
    }
    if !entry.given.gives(name) {
        return Err(DevError::Usage(format!(
            "bench {bench_name}: --versus {name}: give the competing values as --{name} A,B"
        )));
    }
    let mut competitors: Vec<Vec<String>> = Vec::new();
    let mut workloads: Vec<Key> = Vec::new();
    let mut cells: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for (index, arm) in entry.arms.iter().enumerate() {
        let competitor = key_of(arm, |n| n == name)
            .pop()
            .map(|(_, _, keys)| keys)
            .unwrap_or_default();
        let c = intern(&mut competitors, competitor);
        let w = intern(&mut workloads, key_of(arm, |n| n != name));
        if cells.insert((w, c), index).is_some() {
            return Err(DevError::Usage(format!(
                "bench {bench_name}: --versus {name}: two arms bind competitor {} in one \
                 workload, so they cannot be told apart",
                competitors[c].join(",")
            )));
        }
    }
    if competitors.len() < 2 {
        return Err(DevError::Usage(format!(
            "bench {bench_name}: --versus {name}: --{name} binds one value; the competitors \
             need two or more"
        )));
    }
    grid(
        store::Axis::Param {
            name: name.to_owned(),
        },
        competitors.iter().map(|k| k.join(",")).collect(),
        &workloads,
        |w, c| {
            cells.get(&(w, c)).map(|&arm| Recipe {
                bench: 0,
                arm,
                commit: 0,
            })
        },
        "a workload some competitor does not bind (a param that depends on the competitor binds \
         different keys under it)",
    )
}

/// The plan for several `--commit`s over one bench's arms.
pub(super) fn by_build(entry: &Entry<'_>, commits: &[String]) -> Result<Grid, DevError> {
    let workloads: Vec<Key> = entry.arms.iter().map(|arm| key_of(arm, |_| true)).collect();
    grid(
        store::Axis::Build,
        commits.to_vec(),
        &workloads,
        |w, c| {
            Some(Recipe {
                bench: 0,
                arm: w,
                commit: c,
            })
        },
        "",
    )
}

/// The plan for a bench list: workloads keyed by the params the command line
/// gave, which every bench bound from the same tokens.
pub(super) fn by_bench(entries: &[Entry<'_>]) -> Result<Grid, DevError> {
    let mut workloads: Vec<Key> = Vec::new();
    let mut cells: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for (b, entry) in entries.iter().enumerate() {
        let given: BTreeSet<&str> = entry.given.names().collect();
        for (index, arm) in entry.arms.iter().enumerate() {
            let w = intern(&mut workloads, key_of(arm, |n| given.contains(n)));
            if let Some(other) = cells.insert((w, b), index) {
                return Err(DevError::Usage(format!(
                    "bench {}: arms {} and {} bind the same given params, so neither is the \
                     match: a param of its own fans out under them ({}; {}); give it one value",
                    entry.bench.name,
                    other + 1,
                    index + 1,
                    entry.labels[other],
                    entry.labels[index]
                )));
            }
        }
    }
    grid(
        store::Axis::Bench,
        entries.iter().map(|e| e.bench.name.clone()).collect(),
        &workloads,
        |w, b| {
            cells.get(&(w, b)).map(|&arm| Recipe {
                bench: b,
                arm,
                commit: 0,
            })
        },
        "a workload some bench does not bind",
    )
}

/// Every slot's recipe, refused when one is missing: every competitor must
/// run every workload, or the workload has nothing to match.
fn grid(
    axis: store::Axis,
    competitors: Vec<String>,
    workloads: &[Key],
    recipe: impl Fn(usize, usize) -> Option<Recipe>,
    why: &str,
) -> Result<Grid, DevError> {
    // Refused before the matrix is laid out: a plan of more cells than the
    // store takes is refused anyway, whatever its rounds.
    if workloads
        .len()
        .checked_mul(competitors.len())
        .is_none_or(|n| u64::try_from(n).map_or(true, |n| n > store::MAX_SLOTS))
    {
        return Err(DevError::Usage(format!(
            "--rounds: {} workloads of {} competitors make more than the {} cells an invocation \
             may plan",
            workloads.len(),
            competitors.len(),
            store::MAX_SLOTS
        )));
    }
    let labels = workload_labels(workloads);
    let mut recipes = Vec::with_capacity(workloads.len());
    for (w, label) in labels.iter().enumerate() {
        let mut row = Vec::with_capacity(competitors.len());
        for (c, competitor) in competitors.iter().enumerate() {
            row.push(recipe(w, c).ok_or_else(|| {
                DevError::Usage(format!(
                    "--rounds: competitor {competitor} has no cell in workload {}: {why}",
                    if label.is_empty() {
                        "(the only one)"
                    } else {
                        label
                    }
                ))
            })?);
        }
        recipes.push(row);
    }
    Ok(Grid {
        axis,
        competitors,
        workloads: labels,
        recipes,
    })
}
