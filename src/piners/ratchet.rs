//! The mismatch-count ratchets: the per-pair agreement censuses piners puts
//! on a disposition line beside its tier, each pinned per leg in `pins.toml`
//! and held there by the gate.
//!
//! Three censuses share one rule, so they share one implementation:
//!
//! - `labels`: displayed-label agreement over the matched pairs, legs
//!   `entry` and `exit`, both always present on a parity line;
//! - `performance`: the record's per-bar `strategy.equity` and its report
//!   scalars (max drawdown and max run-up, in cash and in percent), legs
//!   `equity` and `scalars`, each present only when evaluated;
//! - `excursions`: per-trade run-up and drawdown agreement over the matched
//!   pairs, legs `run_up` and `drawdown`, each present only when the oracle
//!   states it in comparable money.
//!
//! None of them moves the tier, which keeps meaning "the trades match": a
//! label, an equity value or an excursion can differ while every trade
//! matches, and a legitimate difference pins its count, so folding a mismatch
//! into the tier would either hide which signal moved or demote a probe for
//! good. The rule, per kind and per leg:
//!
//! - bless stamps each carried leg's `mismatched` count; a non-parity line
//!   clears the pin; a pinned leg the parity line does not carry refuses the
//!   bless, so a harness that lost a leg cannot pass as an authorized removal
//!   of its ratchet ([`stamp`]);
//! - the gate fails a pinned leg whose count moved either way, a fall as an
//!   improvement to re-bless, so the ratchet tightens rather than leaving the
//!   old allowance for a later regression; a pinned leg the line lacks fails
//!   as missing evidence unless the disposition already deviates; an
//!   unpinned leg is never gated ([`check`]);
//! - `ambiguous`, where a census counts it, moves with the pairing and is
//!   shown, never gated; `tolerance`, where a leg carries one, is piners' own
//!   label for what counts as a mismatch, stored and shown, never
//!   interpreted;
//! - a present census is gate evidence and is read strictly ([`parse`]).
//!
//! What differs between the kinds is the shape of the evidence, and that is
//! all a [`RatchetKind`] declares: its legs, which counts a leg carries,
//! whether it carries a tolerance, and whether every leg is always present.

use std::collections::BTreeMap;

use serde::Deserialize;

/// One census and its ratchet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RatchetKind {
    Labels,
    Performance,
    Excursions,
}

impl RatchetKind {
    pub const ALL: [RatchetKind; 3] = [
        RatchetKind::Labels,
        RatchetKind::Performance,
        RatchetKind::Excursions,
    ];

    /// The wire key on the disposition line, which is also the pin key in
    /// `pins.toml`.
    pub const fn key(self) -> &'static str {
        match self {
            RatchetKind::Labels => "labels",
            RatchetKind::Performance => "performance",
            RatchetKind::Excursions => "excursions",
        }
    }

    /// The legs, in wire and pin order.
    pub const fn legs(self) -> [&'static str; 2] {
        match self {
            RatchetKind::Labels => ["entry", "exit"],
            RatchetKind::Performance => ["equity", "scalars"],
            RatchetKind::Excursions => ["run_up", "drawdown"],
        }
    }

    /// Whether a leg counts the comparisons the pairing could not settle
    /// (`ambiguous`) and the ones it had nothing to compare (`unavailable`).
    pub const fn tallies_ambiguity(self) -> bool {
        matches!(self, RatchetKind::Labels | RatchetKind::Excursions)
    }

    /// Whether a leg carries piners' `tolerance` label.
    pub const fn has_tolerance(self) -> bool {
        matches!(self, RatchetKind::Performance | RatchetKind::Excursions)
    }

    /// Whether every leg is always present, in the census and in the pin.
    /// Only the label census is: piners sends it whole on every parity line.
    pub const fn whole(self) -> bool {
        matches!(self, RatchetKind::Labels)
    }

    /// The count keys a leg carries, in projection order.
    pub const fn counts(self) -> &'static [&'static str] {
        if self.tallies_ambiguity() {
            &["compared", "mismatched", "ambiguous", "unavailable"]
        } else {
            &["compared", "mismatched"]
        }
    }

    /// The physical `disposition` columns storing the pin and the verdict.
    pub const fn columns(self) -> (&'static str, &'static str) {
        match self {
            RatchetKind::Labels => ("expected_labels", "label_gate"),
            RatchetKind::Performance => ("expected_performance", "performance_gate"),
            RatchetKind::Excursions => ("expected_excursions", "excursion_gate"),
        }
    }

    /// The prefix of the census projections, `<prefix>_<leg>_<count>`.
    pub const fn projection_prefix(self) -> &'static str {
        match self {
            RatchetKind::Labels => "label",
            RatchetKind::Performance => "perf",
            RatchetKind::Excursions => "exc",
        }
    }

    /// The run-detail column header for the stored verdict.
    pub const fn header(self) -> &'static str {
        match self {
            RatchetKind::Labels => "labels",
            RatchetKind::Performance => "perf",
            RatchetKind::Excursions => "exc",
        }
    }

    fn leg_index(self, name: &str) -> Option<usize> {
        self.legs().iter().position(|l| *l == name)
    }
}

/// One leg of a census, as the parser normalized it for its kind: the
/// ambiguity counts are present exactly when the kind tallies them, the
/// tolerance exactly when the kind carries one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegCensus {
    pub compared: u64,
    pub mismatched: u64,
    pub ambiguous: Option<u64>,
    pub unavailable: Option<u64>,
    pub tolerance: Option<String>,
}

/// A census a line carries, with the kind it was read as. Built only by
/// [`parse`], so a census always has its kind's shape: at least one leg,
/// every leg for a whole kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Census {
    kind: RatchetKind,
    legs: [Option<LegCensus>; 2],
}

impl Census {
    pub const fn kind(&self) -> RatchetKind {
        self.kind
    }

    /// The leg at `index` into the kind's [`RatchetKind::legs`].
    pub fn leg(&self, index: usize) -> Option<&LegCensus> {
        self.legs.get(index).and_then(Option::as_ref)
    }

    /// The ambiguous counts over every carried leg.
    pub fn ambiguous(&self) -> u64 {
        self.legs
            .iter()
            .flatten()
            .filter_map(|l| l.ambiguous)
            .fold(0, u64::saturating_add)
    }
}

/// The metric spellings piners gives a non-finite value (`docs/json.md`).
const METRIC_SPELLINGS: [&str; 3] = ["inf", "-inf", "nan"];

/// Read a census of `kind` strictly: it is gate evidence, so nothing in a
/// present leg defaults. The value must be an object; a leg key that is
/// present must be an object (an explicit null included); every count the
/// kind carries a non-negative integer, `mismatched <= compared`, and where
/// ambiguity is tallied `mismatched + ambiguous <= compared` (they are
/// disjoint subsets of `compared`); a tolerance, where the kind carries one,
/// a nonempty single-line string. A whole kind needs every leg, any other at
/// least one. Unknown keys are tolerated.
///
/// The performance `scalars` leg's `pairs`, the figures its count is derived
/// from, are checked too when present ([`check_scalar_pairs`]).
pub fn parse(kind: RatchetKind, value: &serde_json::Value) -> Result<Census, String> {
    let key = kind.key();
    let Some(obj) = value.as_object() else {
        return Err(format!("{key} is not an object"));
    };
    let mut legs: [Option<LegCensus>; 2] = [None, None];
    for (slot, name) in legs.iter_mut().zip(kind.legs()) {
        let Some(v) = obj.get(name) else {
            if kind.whole() {
                return Err(format!("{key} carries no {name} leg"));
            }
            continue;
        };
        let leg = v
            .as_object()
            .ok_or_else(|| format!("{key}.{name} is not an object"))?;
        let count = |k: &str| {
            leg.get(k)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| format!("{key}.{name}.{k} is not a non-negative integer"))
        };
        let compared = count("compared")?;
        let mismatched = count("mismatched")?;
        let (ambiguous, unavailable) = if kind.tallies_ambiguity() {
            (Some(count("ambiguous")?), Some(count("unavailable")?))
        } else {
            (None, None)
        };
        if mismatched > compared || ambiguous.is_some_and(|a| a > compared - mismatched) {
            return Err(match ambiguous {
                Some(a) => format!(
                    "{key}.{name} counts {mismatched} mismatched and {a} ambiguous of {compared} \
                     compared"
                ),
                None => {
                    format!("{key}.{name} counts {mismatched} mismatched of {compared} compared")
                }
            });
        }
        let tolerance = if kind.has_tolerance() {
            Some(
                leg.get("tolerance")
                    .and_then(serde_json::Value::as_str)
                    .filter(|t| !t.is_empty() && !t.contains(['\n', '\r']))
                    .ok_or_else(|| {
                        format!("{key}.{name}.tolerance is not a nonempty single-line string")
                    })?
                    .to_owned(),
            )
        } else {
            None
        };
        if kind == RatchetKind::Performance
            && name == "scalars"
            && let Some(pairs) = leg.get("pairs")
        {
            check_scalar_pairs(pairs, compared, mismatched)?;
        }
        *slot = Some(LegCensus {
            compared,
            mismatched,
            ambiguous,
            unavailable,
            tolerance,
        });
    }
    if legs.iter().all(Option::is_none) {
        let [a, b] = kind.legs();
        return Err(format!("{key} carries neither a {a} nor a {b} leg"));
    }
    Ok(Census { kind, legs })
}

/// Hold the scalars leg's `pairs` to its count: an array of objects, each a
/// nonempty `figure` no other pair names, `ours` and `theirs` each a JSON
/// number or a metric spelling (`inf`, `-inf`, `nan`), and a boolean `match`;
/// one pair per comparison (`compared`), and `mismatched` exactly the pairs
/// whose `match` is false. This is structural and aggregate consistency
/// only: which figures piners compares and whether a pair should match are
/// its policy, not checked here. Absent `pairs` (a harness predating them) is
/// valid.
fn check_scalar_pairs(
    pairs: &serde_json::Value,
    compared: u64,
    mismatched: u64,
) -> Result<(), String> {
    let at = "performance.scalars.pairs";
    let Some(items) = pairs.as_array() else {
        return Err(format!("{at} is not an array"));
    };
    let value_ok = |v: Option<&serde_json::Value>| match v {
        Some(serde_json::Value::Number(_)) => true,
        Some(serde_json::Value::String(s)) => METRIC_SPELLINGS.contains(&s.as_str()),
        _ => false,
    };
    let mut figures = std::collections::BTreeSet::new();
    let mut missed = 0u64;
    for (i, item) in items.iter().enumerate() {
        let pair = item
            .as_object()
            .ok_or_else(|| format!("{at}[{i}] is not an object"))?;
        let figure = pair
            .get("figure")
            .and_then(serde_json::Value::as_str)
            .filter(|f| !f.is_empty())
            .ok_or_else(|| format!("{at}[{i}].figure is not a nonempty string"))?;
        if !figures.insert(figure) {
            return Err(format!("{at} names {figure} twice"));
        }
        for side in ["ours", "theirs"] {
            if !value_ok(pair.get(side)) {
                return Err(format!(
                    "{at}[{i}].{side} is neither a number nor one of {}",
                    METRIC_SPELLINGS.join(", ")
                ));
            }
        }
        match pair.get("match") {
            Some(serde_json::Value::Bool(true)) => {}
            Some(serde_json::Value::Bool(false)) => missed += 1,
            _ => return Err(format!("{at}[{i}].match is not a boolean")),
        }
    }
    let len = u64::try_from(items.len()).unwrap_or(u64::MAX);
    if len != compared || missed != mismatched {
        return Err(format!(
            "{at} holds {len} pairs, {missed} of them missed, against {mismatched} mismatched \
             of {compared} compared"
        ));
    }
    Ok(())
}

/// A probe's blessed mismatch counts for one kind, by leg name. Which legs
/// it may hold is the kind's, checked when `pins.toml` loads
/// ([`check_pin`]): an unknown leg, an empty table and, for a whole kind, a
/// missing leg are refused.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct LegPin(BTreeMap<String, u64>);

impl LegPin {
    /// The pinned count of leg `name`, if pinned.
    pub fn leg(&self, name: &str) -> Option<u64> {
        self.0.get(name).copied()
    }

    /// The pin as a `{ leg = N, ... }` TOML inline table in the kind's leg
    /// order. A name the kind does not have is written too, after its legs:
    /// a writer that dropped it would turn a misspelled leg in a pin it never
    /// meant to touch into a silent loss of that count, where writing it
    /// lets the loader's check on the render refuse it.
    pub fn toml(&self, kind: RatchetKind) -> String {
        let own = kind
            .legs()
            .into_iter()
            .filter_map(|leg| self.leg(leg).map(|n| format!("{leg} = {n}")));
        let foreign = self
            .0
            .iter()
            .filter(|(l, _)| kind.leg_index(l).is_none())
            .map(|(l, n)| format!("{} = {n}", crate::piners::toml_writer::toml_string(l)));
        let legs: Vec<String> = own.chain(foreign).collect();
        format!("{{ {} }}", legs.join(", "))
    }

    fn json(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.0
                .iter()
                .map(|(leg, n)| (leg.clone(), (*n).into()))
                .collect(),
        )
    }
}

impl<const N: usize> From<[(&str, u64); N]> for LegPin {
    fn from(legs: [(&str, u64); N]) -> Self {
        Self(legs.into_iter().map(|(l, n)| (l.to_owned(), n)).collect())
    }
}

/// Why a probe's `kind` pin cannot load, if it cannot.
pub fn check_pin(kind: RatchetKind, pin: &LegPin) -> Option<String> {
    let key = kind.key();
    let [a, b] = kind.legs();
    if let Some(unknown) = pin.0.keys().find(|l| kind.leg_index(l).is_none()) {
        return Some(format!(
            "`{key}` pins an unknown leg `{unknown}` - its legs are `{a}` and `{b}`"
        ));
    }
    if pin.0.is_empty() {
        return Some(format!(
            "`{key}` pins no leg - pin `{a}`, `{b}`, or both, or drop the key"
        ));
    }
    if kind.whole()
        && let Some(missing) = kind.legs().into_iter().find(|l| pin.leg(l).is_none())
    {
        return Some(format!(
            "`{key}` pins no `{missing}` - both `{a}` and `{b}` are required"
        ));
    }
    None
}

/// One way a probe's census fails its ratchet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deviation {
    /// A pinned leg's mismatch count moved, either way.
    Moved {
        kind: RatchetKind,
        leg: &'static str,
        pinned: u64,
        census: LegCensus,
    },
    /// A pinned leg the line does not carry, on a line whose disposition
    /// matches: a ratchet cannot pass on missing evidence.
    Missing {
        kind: RatchetKind,
        leg: &'static str,
        pinned: u64,
    },
}

impl Deviation {
    fn to_json(&self) -> serde_json::Value {
        match self {
            Deviation::Moved {
                leg,
                pinned,
                census,
                ..
            } => {
                let mut obj = serde_json::Map::new();
                obj.insert("leg".to_owned(), (*leg).into());
                obj.insert("pinned".to_owned(), (*pinned).into());
                obj.insert("actual".to_owned(), census.mismatched.into());
                obj.insert("compared".to_owned(), census.compared.into());
                if let Some(a) = census.ambiguous {
                    obj.insert("ambiguous".to_owned(), a.into());
                }
                if let Some(t) = &census.tolerance {
                    obj.insert("tolerance".to_owned(), t.clone().into());
                }
                serde_json::Value::Object(obj)
            }
            Deviation::Missing { leg, pinned, .. } => serde_json::json!({
                "leg": leg, "missing": true, "pinned": pinned
            }),
        }
    }

    pub fn render(&self) -> String {
        match self {
            Deviation::Moved {
                kind,
                leg,
                pinned,
                census,
            } => {
                let actual = census.mismatched;
                let way = if actual > *pinned {
                    "rose"
                } else {
                    "fell (improvement - re-bless to adopt)"
                };
                let mut line = format!(
                    "{} {leg} mismatches {way} {pinned} -> {actual} of {} compared",
                    kind.key(),
                    census.compared
                );
                if let Some(a) = census.ambiguous {
                    line.push_str(&format!(", {a} ambiguous (not gated)"));
                }
                if let Some(t) = &census.tolerance {
                    line.push_str(&format!(" (tolerance {t})"));
                }
                line
            }
            Deviation::Missing { kind, leg, pinned } => format!(
                "{} {leg} pinned at {pinned} mismatches, but the line carries no {} {leg} census",
                kind.key(),
                kind.key()
            ),
        }
    }
}

/// One ratchet's view of one selected probe, stored with its run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assessment {
    /// The pin at run time; `None` = never stamped.
    pub pinned: Option<LegPin>,
    /// The deviations found, `None` when the probe was not checked (no valid
    /// untainted line).
    pub deviations: Option<Vec<Deviation>>,
}

impl Assessment {
    /// The pinned counts as stored JSON, unpinned legs left out.
    pub fn pinned_json(&self) -> Option<String> {
        self.pinned.as_ref().map(|p| p.json().to_string())
    }

    /// The deviations as stored JSON: `[]` when checked and clean.
    pub fn deviations_json(&self) -> Option<String> {
        self.deviations.as_ref().map(|ds| {
            serde_json::Value::Array(ds.iter().map(Deviation::to_json).collect()).to_string()
        })
    }
}

/// How many checked probes one ratchet holds, how many await a stamp, and
/// the ambiguous counts across every checked census (shown, never gated).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    pub ratcheted: usize,
    pub unpinned: usize,
    pub ambiguous: u64,
}

impl Coverage {
    /// Count one checked probe. It is ratcheted when it has a pinned leg its
    /// line carries, and awaits a stamp when its line carries a leg its pin
    /// lacks - for the label census, which every parity line carries, also
    /// when an unpinned parity line carries none (a harness predating it).
    /// A pinned leg the line lacks is missing evidence ([`check`]), in
    /// neither count on its own account; a probe whose line carries no
    /// census and whose pin holds none is in neither.
    pub fn record(
        &mut self,
        kind: RatchetKind,
        parity: bool,
        pinned: Option<&LegPin>,
        census: Option<&Census>,
    ) {
        let pinned_count = |leg: &str| pinned.and_then(|p| p.leg(leg));
        let carried = |i: usize| census.and_then(|c| c.leg(i)).is_some();
        let legs = kind.legs();
        if (0..legs.len()).any(|i| carried(i) && pinned_count(legs[i]).is_some()) {
            self.ratcheted += 1;
        }
        let awaiting = (0..legs.len()).any(|i| carried(i) && pinned_count(legs[i]).is_none())
            || (kind.whole() && parity && pinned.is_none() && census.is_none());
        if awaiting {
            self.unpinned += 1;
        }
        if let Some(c) = census {
            self.ambiguous = self.ambiguous.saturating_add(c.ambiguous());
        }
    }

    /// The coverage line, when any probe was ratcheted or awaits a stamp.
    pub fn line(&self, kind: RatchetKind) -> Option<String> {
        (self.ratcheted + self.unpinned > 0).then(|| {
            let mut line = format!(
                "{}: {} ratcheted",
                kind.key(),
                crate::output::count(self.ratcheted, "probe")
            );
            if self.unpinned > 0 {
                line.push_str(&format!(
                    ", {} with an unpinned leg (ungated until blessed)",
                    self.unpinned
                ));
            }
            if kind.tallies_ambiguity() {
                line.push_str(&format!(", {} ambiguous (not gated)", self.ambiguous));
            }
            line
        })
    }
}

/// One ratchet's check for one probe with a valid untainted line. Each
/// pinned leg is checked on its own; an unpinned leg is never gated.
/// `disposition_deviates` is the disposition check's verdict alone, never
/// another ratchet's: it excuses only a missing leg, never a moved one.
pub fn check(
    kind: RatchetKind,
    pinned: Option<&LegPin>,
    census: Option<&Census>,
    disposition_deviates: bool,
) -> Vec<Deviation> {
    debug_assert!(
        census.is_none_or(|c| c.kind() == kind),
        "a census read as another kind"
    );
    let Some(pin) = pinned else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, leg) in kind.legs().into_iter().enumerate() {
        let Some(pinned) = pin.leg(leg) else {
            continue;
        };
        match census.and_then(|c| c.leg(i)) {
            Some(c) if c.mismatched != pinned => out.push(Deviation::Moved {
                kind,
                leg,
                pinned,
                census: c.clone(),
            }),
            Some(_) => {}
            // A disposition that already deviates says enough.
            None if disposition_deviates => {}
            None => out.push(Deviation::Missing { kind, leg, pinned }),
        }
    }
    out
}

/// The pin a bless stamps for one kind on one probe with a valid line, or
/// the pinned legs it would drop when it must refuse. A non-parity line
/// clears the pin. A parity line stamps every leg it carries, and a pinned
/// leg it does not carry refuses the bless; a parity line carrying none
/// leaves an unpinned probe unpinned.
pub fn stamp(
    kind: RatchetKind,
    parity: bool,
    census: Option<&Census>,
    pinned: Option<&LegPin>,
) -> Result<Option<LegPin>, Vec<&'static str>> {
    debug_assert!(
        census.is_none_or(|c| c.kind() == kind),
        "a census read as another kind"
    );
    if !parity {
        return Ok(None);
    }
    let mut legs = BTreeMap::new();
    let mut lost = Vec::new();
    for (i, leg) in kind.legs().into_iter().enumerate() {
        match census.and_then(|c| c.leg(i)) {
            Some(c) => {
                legs.insert(leg.to_owned(), c.mismatched);
            }
            None if pinned.and_then(|p| p.leg(leg)).is_some() => lost.push(leg),
            None => {}
        }
    }
    if !lost.is_empty() {
        return Err(lost);
    }
    Ok((!legs.is_empty()).then_some(LegPin(legs)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    use RatchetKind::{Excursions, Labels, Performance};

    fn census(kind: RatchetKind, json: &str) -> Result<Census, String> {
        serde_json::from_str(json)
            .map_err(|e| e.to_string())
            .and_then(|v| parse(kind, &v))
    }

    const PERF_LEG: &str = r#"{"compared":10,"mismatched":2,"tolerance":"exact"}"#;
    const EXC_LEG: &str =
        r#"{"compared":10,"mismatched":2,"ambiguous":1,"unavailable":0,"tolerance":"t"}"#;

    #[test]
    fn each_kind_is_read_strictly_to_its_own_shape() {
        let c = census(
            Performance,
            r#"{"equity":{"compared":2637,"mismatched":0,"tolerance":"exact"},"extra":1}"#,
        )
        .unwrap();
        assert_eq!(c.leg(0).unwrap().compared, 2637);
        assert_eq!(c.leg(0).unwrap().ambiguous, None);
        assert_eq!(c.leg(1), None);
        let c = census(Excursions, &format!(r#"{{"drawdown":{EXC_LEG}}}"#)).unwrap();
        assert_eq!(c.leg(0), None);
        assert_eq!(c.leg(1).unwrap().ambiguous, Some(1));
        assert_eq!(c.ambiguous(), 1);
        let leg = r#"{"compared":3,"mismatched":1,"ambiguous":2,"unavailable":0}"#;
        let c = census(Labels, &format!(r#"{{"entry":{leg},"exit":{leg}}}"#)).unwrap();
        assert_eq!(c.leg(1).unwrap().tolerance, None);
        for (kind, bad) in [
            (Performance, "null".to_owned()),
            (Performance, "{}".to_owned()),
            (Performance, r#"{"equity":null}"#.to_owned()),
            (
                Performance,
                r#"{"equity":{"compared":3,"mismatched":4,"tolerance":"exact"}}"#.to_owned(),
            ),
            (
                Performance,
                r#"{"equity":{"compared":3,"mismatched":-1,"tolerance":"exact"}}"#.to_owned(),
            ),
            (
                Performance,
                r#"{"scalars":{"compared":4,"mismatched":0}}"#.to_owned(),
            ),
            (
                Performance,
                r#"{"scalars":{"compared":4,"mismatched":0,"tolerance":""}}"#.to_owned(),
            ),
            (
                Performance,
                r#"{"scalars":{"compared":4,"mismatched":0,"tolerance":"a\nb"}}"#.to_owned(),
            ),
            (Excursions, "{}".to_owned()),
            (Excursions, r#"{"run_up":null}"#.to_owned()),
            (Excursions, format!(r#"{{"run_up":{PERF_LEG}}}"#)),
            (
                Excursions,
                r#"{"run_up":{"compared":3,"mismatched":2,"ambiguous":2,"unavailable":0,"tolerance":"t"}}"#
                    .to_owned(),
            ),
            (
                Excursions,
                r#"{"run_up":{"compared":3,"mismatched":1,"ambiguous":0,"unavailable":0}}"#
                    .to_owned(),
            ),
            (Labels, format!(r#"{{"entry":{leg}}}"#)),
            (Labels, "null".to_owned()),
        ] {
            assert!(census(kind, &bad).is_err(), "{kind:?} {bad}");
        }
    }

    #[test]
    fn scalar_pairs_must_agree_with_their_leg_when_present() {
        let pair = |figure: &str, ours: &str, matched: bool| {
            format!(r#"{{"figure":"{figure}","ours":{ours},"theirs":1.5,"match":{matched}}}"#)
        };
        let leg = |pairs: &str, compared: u64, mismatched: u64| {
            format!(
                r#"{{"scalars":{{"compared":{compared},"mismatched":{mismatched},"tolerance":"t","pairs":{pairs}}}}}"#
            )
        };
        let good = format!(
            "[{},{}]",
            pair("max_drawdown", "1.5", true),
            pair("max_runup", "\"nan\"", false)
        );
        assert!(census(Performance, &leg(&good, 2, 1)).is_ok());
        // A present null is malformed; absent pairs (a harness predating
        // them) are valid.
        assert!(census(Performance, &leg("null", 2, 1)).is_err());
        assert!(
            census(
                Performance,
                r#"{"scalars":{"compared":4,"mismatched":0,"tolerance":"t"}}"#
            )
            .is_ok()
        );
        for (pairs, compared, mismatched) in [
            (good.clone(), 2, 0),
            (good.clone(), 3, 1),
            (r#"["bad"]"#.to_owned(), 1, 0),
            (
                format!(
                    "[{},{}]",
                    pair("max_runup", "1", true),
                    pair("max_runup", "1", true)
                ),
                2,
                0,
            ),
            (format!("[{}]", pair("x", "\"infinity\"", true)), 1, 0),
            (format!("[{}]", pair("x", "null", true)), 1, 0),
            (
                r#"[{"figure":"x","ours":1,"theirs":1,"match":0}]"#.to_owned(),
                1,
                0,
            ),
            (
                r#"[{"figure":"","ours":1,"theirs":1,"match":true}]"#.to_owned(),
                1,
                0,
            ),
        ] {
            assert!(
                census(Performance, &leg(&pairs, compared, mismatched)).is_err(),
                "{pairs} {compared} {mismatched}"
            );
        }
    }

    #[test]
    fn a_pin_holds_only_its_kinds_legs() {
        assert_eq!(check_pin(Excursions, &LegPin::from([("run_up", 3)])), None);
        assert!(
            check_pin(Excursions, &LegPin::from([("runup", 3)]))
                .unwrap()
                .contains("unknown leg `runup`")
        );
        assert!(
            check_pin(Performance, &LegPin::default())
                .unwrap()
                .contains("pins no leg")
        );
        assert!(
            check_pin(Labels, &LegPin::from([("entry", 0)]))
                .unwrap()
                .contains("pins no `exit`")
        );
        assert_eq!(
            LegPin::from([("drawdown", 1), ("run_up", 2)]).toml(Excursions),
            "{ run_up = 2, drawdown = 1 }"
        );
    }

    #[test]
    fn only_pinned_legs_are_gated_and_a_pinned_leg_gone_missing_fails() {
        let both = census(
            Excursions,
            &format!(r#"{{"run_up":{EXC_LEG},"drawdown":{EXC_LEG}}}"#),
        )
        .unwrap();
        let run_up_only = LegPin::from([("run_up", 2)]);
        // The drawdown leg moves freely under a run_up-only pin.
        assert!(check(Excursions, Some(&run_up_only), Some(&both), false).is_empty());
        let moved = check(
            Excursions,
            Some(&LegPin::from([("run_up", 0), ("drawdown", 2)])),
            Some(&both),
            false,
        );
        assert_eq!(moved.len(), 1);
        let line = moved[0].render();
        assert!(
            line.contains("excursions run_up mismatches rose 0 -> 2 of 10"),
            "{line}"
        );
        assert!(line.contains("1 ambiguous (not gated)"), "{line}");
        assert!(line.contains("(tolerance t)"), "{line}");
        assert_eq!(
            Assessment {
                pinned: None,
                deviations: Some(moved),
            }
            .deviations_json()
            .unwrap(),
            r#"[{"actual":2,"ambiguous":1,"compared":10,"leg":"run_up","pinned":0,"tolerance":"t"}]"#
        );
        // A pinned leg the line lacks.
        let drawdown_only = census(Excursions, &format!(r#"{{"drawdown":{EXC_LEG}}}"#)).unwrap();
        assert_eq!(
            check(Excursions, Some(&run_up_only), Some(&drawdown_only), false),
            vec![Deviation::Missing {
                kind: Excursions,
                leg: "run_up",
                pinned: 2
            }]
        );
        assert!(check(Excursions, Some(&run_up_only), None, true).is_empty());
        // A deviating disposition never excuses a moved count.
        assert_eq!(
            check(
                Excursions,
                Some(&LegPin::from([("run_up", 5)])),
                Some(&both),
                true
            )
            .len(),
            1
        );
        assert!(check(Excursions, None, Some(&both), false).is_empty());
    }

    #[test]
    fn coverage_counts_per_leg_and_sums_ambiguity_over_every_checked_census() {
        let both = census(
            Excursions,
            &format!(r#"{{"run_up":{EXC_LEG},"drawdown":{EXC_LEG}}}"#),
        )
        .unwrap();
        let drawdown_only = census(Excursions, &format!(r#"{{"drawdown":{EXC_LEG}}}"#)).unwrap();
        let run_up = LegPin::from([("run_up", 0)]);
        let mut c = Coverage::default();
        c.record(Excursions, true, Some(&run_up), Some(&both)); // ratcheted, drawdown awaiting
        c.record(Excursions, true, None, None); // no census at all: neither
        c.record(Excursions, true, None, Some(&both)); // awaiting
        c.record(Excursions, true, Some(&run_up), None); // missing: neither
        c.record(Excursions, true, Some(&run_up), Some(&drawdown_only)); // awaiting only
        assert_eq!(
            c,
            Coverage {
                ratcheted: 1,
                unpinned: 3,
                ambiguous: 5
            }
        );
        assert!(
            c.line(Excursions)
                .unwrap()
                .contains("3 with an unpinned leg")
        );
        assert_eq!(Coverage::default().line(Excursions), None);
        // The label census rides every parity line, so an unpinned parity
        // line without one still awaits a stamp; a break does not.
        let mut l = Coverage::default();
        l.record(Labels, true, None, None);
        l.record(Labels, false, None, None);
        assert_eq!(l.unpinned, 1);
        let mut p = Coverage::default();
        p.record(Performance, true, None, None);
        assert_eq!(p, Coverage::default());
    }

    #[test]
    fn bless_stamps_carried_legs_clears_on_a_break_and_refuses_to_drop_a_pinned_leg() {
        let drawdown_only = census(Excursions, &format!(r#"{{"drawdown":{EXC_LEG}}}"#)).unwrap();
        assert_eq!(
            stamp(Excursions, true, Some(&drawdown_only), None),
            Ok(Some(LegPin::from([("drawdown", 2)])))
        );
        assert_eq!(stamp(Excursions, true, None, None), Ok(None));
        let pinned = LegPin::from([("run_up", 0)]);
        assert_eq!(stamp(Excursions, false, None, Some(&pinned)), Ok(None));
        assert_eq!(
            stamp(Excursions, true, Some(&drawdown_only), Some(&pinned)),
            Err(vec!["run_up"])
        );
        let a = Assessment {
            pinned: Some(LegPin::from([("drawdown", 2)])),
            deviations: Some(Vec::new()),
        };
        assert_eq!(a.pinned_json().as_deref(), Some(r#"{"drawdown":2}"#));
        assert_eq!(a.deviations_json().as_deref(), Some("[]"));
    }
}
