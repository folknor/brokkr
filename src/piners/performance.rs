//! The performance ratchet: a probe's `strategy.equity` per bar and its
//! report scalars (max drawdown and max run-up, in cash and in percent),
//! checked by the harness against the record, ratcheted like the label
//! census (`gate.rs`) and apart from the trade-list tier.
//!
//! The tier keeps meaning "the trades match". Equity can drift while every
//! trade matches, and a drift that is legitimate pins its count, so folding
//! a mismatch into the tier would either hide which signal moved or demote a
//! probe for good. Each leg is its own ratchet instead: a pinned count that
//! moves either way fails, until a bless adopts it.
//!
//! The wire field is `performance`, present only on a line the harness
//! evaluated it for, each leg optional on its own:
//!
//! ```json
//! "performance": {
//!   "equity":  { "compared": 2637, "mismatched": 0, "tolerance": "exact" },
//!   "scalars": { "compared": 4,    "mismatched": 0, "tolerance": "f32_ulp_of_excursion" }
//! }
//! ```
//!
//! `tolerance` is piners' own label for what counts as a mismatch. brokkr
//! stores it and shows it when a count moves, and never interprets it: the
//! float comparison is the harness's, the ratchet sees counts only.
//!
//! The pin is `performance = { equity = N, scalars = M }` in `pins.toml`,
//! each leg optional. Only a pinned leg is gated; a leg the line carries but
//! the pin lacks leaves the probe counted as unpinned. A pinned leg the line
//! does not carry fails, as a pinned label census that went missing does.

use crate::piners::registry::PerformancePin;

/// One performance leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PerfLeg {
    Equity,
    Scalars,
}

impl PerfLeg {
    pub const ALL: [PerfLeg; 2] = [PerfLeg::Equity, PerfLeg::Scalars];

    pub const fn as_str(self) -> &'static str {
        match self {
            PerfLeg::Equity => "equity",
            PerfLeg::Scalars => "scalars",
        }
    }

    /// The leg's pinned mismatch count, if pinned.
    pub const fn pinned(self, pin: &PerformancePin) -> Option<u64> {
        match self {
            PerfLeg::Equity => pin.equity,
            PerfLeg::Scalars => pin.scalars,
        }
    }
}

/// One leg of the census a line carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfLegCensus {
    pub compared: u64,
    pub mismatched: u64,
    pub tolerance: String,
}

/// The `performance` census. At least one leg is present: the field is
/// emitted only for an evaluated probe, so an empty one is a defect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PerformanceCensus {
    pub equity: Option<PerfLegCensus>,
    pub scalars: Option<PerfLegCensus>,
}

impl PerformanceCensus {
    pub const fn leg(&self, leg: PerfLeg) -> Option<&PerfLegCensus> {
        match leg {
            PerfLeg::Equity => self.equity.as_ref(),
            PerfLeg::Scalars => self.scalars.as_ref(),
        }
    }
}

/// Read a `performance` value strictly, as the label census is read: it is
/// gate evidence, so nothing in a present leg defaults. A leg key that is
/// present must be an object (an explicit null included), with `compared`
/// and `mismatched` non-negative integers, `mismatched <= compared`, and
/// `tolerance` a nonempty single-line string. Unknown keys are tolerated.
pub fn parse_census(value: &serde_json::Value) -> Result<PerformanceCensus, String> {
    let Some(obj) = value.as_object() else {
        return Err("performance is not an object".to_owned());
    };
    let leg = |leg: PerfLeg| -> Result<Option<PerfLegCensus>, String> {
        let name = leg.as_str();
        let Some(v) = obj.get(name) else {
            return Ok(None);
        };
        let leg_obj = v
            .as_object()
            .ok_or_else(|| format!("performance.{name} is not an object"))?;
        let count = |key: &str| {
            leg_obj
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| format!("performance.{name}.{key} is not a non-negative integer"))
        };
        let compared = count("compared")?;
        let mismatched = count("mismatched")?;
        if mismatched > compared {
            return Err(format!(
                "performance.{name} counts {mismatched} mismatched of {compared} compared"
            ));
        }
        let tolerance = leg_obj
            .get("tolerance")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.is_empty() && !t.contains(['\n', '\r']))
            .ok_or_else(|| {
                format!("performance.{name}.tolerance is not a nonempty single-line string")
            })?
            .to_owned();
        Ok(Some(PerfLegCensus {
            compared,
            mismatched,
            tolerance,
        }))
    };
    let census = PerformanceCensus {
        equity: leg(PerfLeg::Equity)?,
        scalars: leg(PerfLeg::Scalars)?,
    };
    if census.equity.is_none() && census.scalars.is_none() {
        return Err("performance carries neither an equity nor a scalars leg".to_owned());
    }
    Ok(census)
}

/// One way a probe's performance fails its ratchet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerformanceDeviation {
    /// A pinned leg's mismatch count moved. A fall fails too, so the ratchet
    /// tightens rather than leaving the old allowance for a regression.
    Moved {
        leg: PerfLeg,
        pinned: u64,
        actual: u64,
        compared: u64,
        tolerance: String,
    },
    /// A pinned leg the line does not carry, on a line whose disposition
    /// matches: a ratchet cannot pass on missing evidence.
    Missing { leg: PerfLeg, pinned: u64 },
}

impl PerformanceDeviation {
    fn to_json(&self) -> serde_json::Value {
        match self {
            PerformanceDeviation::Moved {
                leg,
                pinned,
                actual,
                compared,
                tolerance,
            } => serde_json::json!({
                "leg": leg.as_str(), "pinned": pinned, "actual": actual,
                "compared": compared, "tolerance": tolerance
            }),
            PerformanceDeviation::Missing { leg, pinned } => serde_json::json!({
                "leg": leg.as_str(), "missing": true, "pinned": pinned
            }),
        }
    }

    pub fn render(&self) -> String {
        match self {
            PerformanceDeviation::Moved {
                leg,
                pinned,
                actual,
                compared,
                tolerance,
            } => {
                let way = if actual > pinned {
                    "rose"
                } else {
                    "fell (improvement - re-bless to adopt)"
                };
                format!(
                    "{} mismatches {way} {pinned} -> {actual} of {compared} compared \
                     (tolerance {tolerance})",
                    leg.as_str()
                )
            }
            PerformanceDeviation::Missing { leg, pinned } => format!(
                "{} pinned at {pinned} mismatches, but the line carries no {} census",
                leg.as_str(),
                leg.as_str()
            ),
        }
    }
}

/// The performance ratchet's view of one selected probe, stored with its run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PerformanceAssessment {
    /// The pin at run time; `None` = no leg stamped.
    pub pinned: Option<PerformancePin>,
    /// The deviations found, `None` when the probe was not checked (no valid
    /// untainted line).
    pub deviations: Option<Vec<PerformanceDeviation>>,
}

impl PerformanceAssessment {
    /// The pinned counts as stored JSON, unpinned legs left out.
    pub fn pinned_json(&self) -> Option<String> {
        self.pinned.map(|p| pin_json(&p).to_string())
    }

    /// The deviations as stored JSON: `[]` when checked and clean.
    pub fn deviations_json(&self) -> Option<String> {
        self.deviations.as_ref().map(|ds| {
            serde_json::Value::Array(ds.iter().map(PerformanceDeviation::to_json).collect())
                .to_string()
        })
    }
}

fn pin_json(pin: &PerformancePin) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for leg in PerfLeg::ALL {
        if let Some(n) = leg.pinned(pin) {
            obj.insert(leg.as_str().to_owned(), n.into());
        }
    }
    serde_json::Value::Object(obj)
}

/// How many checked probes have a pinned leg, and how many carry a leg their
/// pin does not hold yet.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PerformanceCoverage {
    pub ratcheted: usize,
    pub unpinned: usize,
}

impl PerformanceCoverage {
    /// Count one checked probe. Only a line that carries a census counts as
    /// unpinned: most records carry no plots or scalars, and a probe the
    /// harness never evaluates performance for awaits no stamp.
    pub fn record(&mut self, pinned: Option<PerformancePin>, census: Option<&PerformanceCensus>) {
        let pinned_leg = |leg: PerfLeg| pinned.as_ref().and_then(|p| leg.pinned(p)).is_some();
        if PerfLeg::ALL.into_iter().any(pinned_leg) {
            self.ratcheted += 1;
        }
        let awaiting = census.is_some_and(|c| {
            PerfLeg::ALL
                .into_iter()
                .any(|leg| c.leg(leg).is_some() && !pinned_leg(leg))
        });
        if awaiting {
            self.unpinned += 1;
        }
    }

    /// The coverage line, when any probe was ratcheted or awaits a stamp.
    pub fn line(&self) -> Option<String> {
        (self.ratcheted + self.unpinned > 0).then(|| {
            let mut line = format!(
                "performance: {} ratcheted",
                crate::output::count(self.ratcheted, "probe")
            );
            if self.unpinned > 0 {
                line.push_str(&format!(
                    ", {} with an unpinned leg (ungated until blessed)",
                    self.unpinned
                ));
            }
            line
        })
    }
}

/// The performance check for one probe with a valid untainted line. Each
/// pinned leg is checked on its own; an unpinned leg is never gated.
pub fn check(
    pinned: Option<PerformancePin>,
    census: Option<&PerformanceCensus>,
    disposition_deviates: bool,
) -> Vec<PerformanceDeviation> {
    let Some(pin) = pinned else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for leg in PerfLeg::ALL {
        let Some(pinned) = leg.pinned(&pin) else {
            continue;
        };
        match census.and_then(|c| c.leg(leg)) {
            Some(c) if c.mismatched != pinned => out.push(PerformanceDeviation::Moved {
                leg,
                pinned,
                actual: c.mismatched,
                compared: c.compared,
                tolerance: c.tolerance.clone(),
            }),
            Some(_) => {}
            // A disposition that already deviates says enough.
            None if disposition_deviates => {}
            None => out.push(PerformanceDeviation::Missing { leg, pinned }),
        }
    }
    out
}

/// The pin a bless stamps for one probe with a valid line, or the pinned
/// legs it would drop when it must refuse. A non-parity line clears the
/// pin, as it clears label counts. A parity line stamps every leg it
/// carries; a pinned leg it does not carry refuses the bless, so a harness
/// that lost a leg cannot pass as an authorized removal of its ratchet.
pub fn stamp(
    parity: bool,
    census: Option<&PerformanceCensus>,
    pinned: Option<PerformancePin>,
) -> Result<Option<PerformancePin>, Vec<PerfLeg>> {
    if !parity {
        return Ok(None);
    }
    let mut lost = Vec::new();
    let mut leg_stamp = |leg: PerfLeg| match census.and_then(|c| c.leg(leg)) {
        Some(c) => Some(c.mismatched),
        None => {
            if pinned.as_ref().and_then(|p| leg.pinned(p)).is_some() {
                lost.push(leg);
            }
            None
        }
    };
    let pin = PerformancePin {
        equity: leg_stamp(PerfLeg::Equity),
        scalars: leg_stamp(PerfLeg::Scalars),
    };
    if !lost.is_empty() {
        return Err(lost);
    }
    Ok((pin.equity.is_some() || pin.scalars.is_some()).then_some(pin))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn census(json: &str) -> Result<PerformanceCensus, String> {
        serde_json::from_str(json)
            .map_err(|e| e.to_string())
            .and_then(|v| parse_census(&v))
    }

    fn leg(mismatched: u64) -> PerfLegCensus {
        PerfLegCensus {
            compared: 10,
            mismatched,
            tolerance: "exact".to_owned(),
        }
    }

    #[test]
    fn a_census_is_read_strictly_and_each_leg_is_optional() {
        let c = census(
            r#"{"equity":{"compared":2637,"mismatched":0,"tolerance":"exact"},"extra":1}"#,
        )
        .unwrap();
        assert_eq!(c.equity.unwrap().compared, 2637);
        assert_eq!(c.scalars, None);
        for bad in [
            "null",
            "{}",
            r#"{"equity":null}"#,
            r#"{"equity":{"compared":3,"mismatched":4,"tolerance":"exact"}}"#,
            r#"{"equity":{"compared":3,"mismatched":-1,"tolerance":"exact"}}"#,
            r#"{"scalars":{"compared":4,"mismatched":0}}"#,
            r#"{"scalars":{"compared":4,"mismatched":0,"tolerance":""}}"#,
            r#"{"scalars":{"compared":4,"mismatched":0,"tolerance":"a\nb"}}"#,
        ] {
            assert!(census(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_pinned_legs_are_gated_and_a_pinned_leg_gone_missing_fails() {
        let both = PerformanceCensus {
            equity: Some(leg(2)),
            scalars: Some(leg(1)),
        };
        let equity_only = PerformancePin {
            equity: Some(2),
            scalars: None,
        };
        // The scalars leg moves freely under an equity-only pin.
        assert!(check(Some(equity_only), Some(&both), false).is_empty());
        let moved = check(
            Some(PerformancePin {
                equity: Some(0),
                scalars: Some(1),
            }),
            Some(&both),
            false,
        );
        assert_eq!(moved.len(), 1);
        assert!(moved[0].render().contains("equity mismatches rose 0 -> 2"));
        assert!(moved[0].render().contains("tolerance exact"));
        // A pinned leg the line lacks.
        let scalars_only = PerformanceCensus {
            equity: None,
            scalars: Some(leg(1)),
        };
        let missing = check(Some(equity_only), Some(&scalars_only), false);
        assert_eq!(
            missing,
            vec![PerformanceDeviation::Missing {
                leg: PerfLeg::Equity,
                pinned: 2
            }]
        );
        assert!(check(Some(equity_only), None, true).is_empty());
        assert!(check(None, Some(&both), false).is_empty());
    }

    #[test]
    fn coverage_counts_a_carried_unpinned_leg_and_ignores_a_probe_without_a_census() {
        let mut c = PerformanceCoverage::default();
        let both = PerformanceCensus {
            equity: Some(leg(0)),
            scalars: Some(leg(0)),
        };
        let equity_only = PerformancePin {
            equity: Some(0),
            scalars: None,
        };
        c.record(Some(equity_only), Some(&both)); // ratcheted, scalars awaiting
        c.record(None, None); // no performance at all: neither
        c.record(None, Some(&both)); // awaiting
        assert_eq!(
            c,
            PerformanceCoverage {
                ratcheted: 1,
                unpinned: 2
            }
        );
        assert!(c.line().unwrap().contains("2 with an unpinned leg"));
        assert_eq!(PerformanceCoverage::default().line(), None);
    }

    #[test]
    fn bless_stamps_carried_legs_clears_on_a_break_and_refuses_to_drop_a_pinned_leg() {
        let scalars_only = PerformanceCensus {
            equity: None,
            scalars: Some(leg(3)),
        };
        assert_eq!(
            stamp(true, Some(&scalars_only), None),
            Ok(Some(PerformancePin {
                equity: None,
                scalars: Some(3)
            }))
        );
        assert_eq!(stamp(true, None, None), Ok(None));
        let pinned = PerformancePin {
            equity: Some(0),
            scalars: None,
        };
        assert_eq!(stamp(false, None, Some(pinned)), Ok(None));
        assert_eq!(
            stamp(true, Some(&scalars_only), Some(pinned)),
            Err(vec![PerfLeg::Equity])
        );
        let a = PerformanceAssessment {
            pinned: Some(PerformancePin {
                equity: None,
                scalars: Some(3),
            }),
            deviations: Some(Vec::new()),
        };
        assert_eq!(a.pinned_json().as_deref(), Some(r#"{"scalars":3}"#));
        assert_eq!(a.deviations_json().as_deref(), Some("[]"));
    }
}
