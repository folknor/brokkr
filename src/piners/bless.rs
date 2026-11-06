//! `brokkr corpus --bless`: stamp each selected probe's *current*
//! disposition into its `expected` field in `pins.toml`, and each ratchet
//! census's per-leg mismatch counts into `labels`, `performance` and
//! `excursions` (`ratchet.rs`): every leg a parity line carries is stamped,
//! a non-parity line clears the counts, and a parity line without a leg
//! leaves an unpinned leg unpinned but refuses the bless where it would drop
//! a count already pinned.
//!
//! Bless is the sibling of `--reseed`: reseed adopts new corpus *content*
//! (re-hashing the pinned probe files), bless adopts new *dispositions*. Both are
//! deliberate human acts whose review surface is `git diff pins.toml`. Bless
//! records reality - including `compile_fail`/`runtime_fail`/`harness_abort`
//! outcomes - so a probe known to exercise an unimplemented feature can pin
//! `expected = "compile_fail"` and the gate will later catch it silently
//! starting to compile. The one outcome it refuses is `unevaluated`: an
//! experiment that was not conducted records nothing about the engine. A
//! `compile_fail` or `runtime_fail` is stamped with its cause. A
//! `harness_abort` is stamped with its death and needs a human attribution
//! (`--attribute`) wherever the pin does not already hold that very death.
//!
//! Unlike reseed, bless runs the harness first (the run pipeline is shared
//! with [`crate::piners::cmd`]); it then merges dispositions for the
//! selected probes into the already-loaded pin universe and rewrites the
//! file in place via [`crate::piners::pins_write`] (hand-written comments
//! survive), leaving unselected probes untouched.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::DevError;
use crate::output;
use crate::piners::pins_write;
use crate::piners::ratchet::{self, LegPin, RatchetKind};
use crate::piners::registry::{Pin, Registry, Verdict, valid_attribution};
use crate::piners::registry_io;
use crate::piners::report::{HarnessReport, ProbeLine, ProbeOutcome};

/// What a bless stamps into one probe's pin: its disposition and each
/// ratchet kind's counts.
struct Stamp {
    verdict: Verdict,
    /// An abort's attribution; `None` on every other verdict.
    attribution: Option<String>,
    ratchets: BTreeMap<RatchetKind, Option<LegPin>>,
}

/// Every reason a bless is refused, each naming its probes.
#[derive(Default)]
struct Refusals<'a> {
    missing: Vec<&'a str>,
    rejected: Vec<String>,
    unevaluated: Vec<String>,
    unpinned: Vec<&'a str>,
    /// Per ratchet kind, each probe whose parity line does not carry a leg
    /// its pin holds, with those legs.
    lost: BTreeMap<RatchetKind, Vec<String>>,
    /// Aborts that are new to their pin or moved death, with no
    /// `--attribute`.
    unattributed: Vec<String>,
    /// `--attribute` flags that cannot apply: an unselected probe, a line
    /// that is not an abort, or a text that is not one nonblank line.
    misattributed: Vec<String>,
}

/// Why a bless is refused, naming every probe per reason; `None` when it
/// may proceed.
fn refusal(r: &Refusals<'_>) -> Option<String> {
    let Refusals {
        missing,
        rejected,
        unevaluated,
        unpinned,
        lost,
        unattributed,
        misattributed,
    } = r;
    let mut why = Vec::new();
    if !missing.is_empty() {
        why.push(format!("no disposition for {}", missing.join(", ")));
    }
    if !rejected.is_empty() {
        why.push(format!(
            "unstampable disposition for {}",
            rejected.join(", ")
        ));
    }
    if !unevaluated.is_empty() {
        why.push(format!(
            "unevaluated, which no pin can hold, for {}",
            unevaluated.join(", ")
        ));
    }
    if !unpinned.is_empty() {
        why.push(format!("internal: not pinned: {}", unpinned.join(", ")));
    }
    for (kind, probes) in lost {
        if !probes.is_empty() {
            why.push(format!(
                "a pinned {} leg the parity line does not carry for {}",
                kind.key(),
                probes.join(", ")
            ));
        }
    }
    if !unattributed.is_empty() {
        why.push(format!(
            "a harness_abort new to its pin or on a moved death needs \
             `--attribute PROBE=TEXT` saying what died, for {}",
            unattributed.join(", ")
        ));
    }
    if !misattributed.is_empty() {
        why.push(format!(
            "an --attribute that cannot apply: {}",
            misattributed.join(", ")
        ));
    }
    (!why.is_empty()).then(|| why.join("; "))
}

/// The `--attribute PROBE=TEXT` flags by probe, split at the first `=` (an
/// attribution may hold another). Refuses a flag with no `=` or an empty
/// probe, and a probe named twice; whether each applies is
/// [`plan_stamps`]'s, once the run has reported.
pub fn parse_attributions(flags: &[String]) -> Result<BTreeMap<String, String>, DevError> {
    let mut out = BTreeMap::new();
    for flag in flags {
        let Some((probe, text)) = flag.split_once('=').filter(|(p, _)| !p.is_empty()) else {
            return Err(DevError::Config(format!(
                "corpus --attribute {flag}: expected PROBE=TEXT"
            )));
        };
        if out.insert(probe.to_owned(), text.to_owned()).is_some() {
            return Err(DevError::Config(format!(
                "corpus --attribute: {probe} is attributed twice"
            )));
        }
    }
    Ok(out)
}

/// Every selected probe's stamp, validated before anything is mutated, or
/// the refusal naming every probe that has none.
fn plan_stamps<'a>(
    registry: &Registry,
    report: &'a HarnessReport,
    scope_ids: &'a [String],
    attributions: &BTreeMap<String, String>,
) -> Result<BTreeMap<&'a str, Stamp>, DevError> {
    // Each line, with its derived label (for naming a refusal), its
    // validated outcome (the parse-time verdict integrity and the gate
    // share), and its censuses, already held valid by the record check.
    let lines: BTreeMap<&str, &ProbeLine> = report
        .probes
        .iter()
        .map(|p| (p.probe.as_str(), p))
        .collect();
    let mut r = Refusals::default();
    for (id, text) in attributions {
        if !scope_ids.contains(id) {
            r.misattributed.push(format!("{id} is not selected"));
        } else if !valid_attribution(text) {
            r.misattributed
                .push(format!("{id}'s text is not one nonblank line"));
        } else if !matches!(
            lines.get(id.as_str()).and_then(|l| l.outcome()),
            Some(ProbeOutcome::Evaluated(Verdict::HarnessAbort(_)))
        ) {
            r.misattributed
                .push(format!("{id} did not report a harness_abort"));
        }
    }
    let mut stamps: BTreeMap<&str, Stamp> = BTreeMap::new();
    for id in scope_ids {
        let Some(line) = lines.get(id.as_str()) else {
            r.missing.push(id);
            continue;
        };
        match line.outcome() {
            None => r.rejected.push(format!("{id} ({})", line.disposition())),
            Some(ProbeOutcome::Unevaluated(refused)) => {
                r.unevaluated
                    .push(format!("{id} ({})", refused.reason.label()));
            }
            Some(_) if !registry.pins.contains_key(id) => r.unpinned.push(id),
            Some(ProbeOutcome::Evaluated(verdict)) => {
                let pin = registry.pins.get(id);
                // An abort keeps its pinned attribution only where the pin
                // already holds this very verdict; a new abort or a moved
                // death needs a fresh statement.
                let attribution = match verdict {
                    Verdict::HarnessAbort(_) => match attributions.get(id) {
                        Some(text) => Some(text.clone()),
                        None if pin.and_then(Pin::expectation) == Some(*verdict) => {
                            pin.and_then(|p| p.attribution.clone())
                        }
                        None => {
                            r.unattributed.push(format!("{id} ({verdict})"));
                            continue;
                        }
                    },
                    _ => None,
                };
                let mut ratchets = BTreeMap::new();
                let mut stampable = true;
                for kind in RatchetKind::ALL {
                    match ratchet::stamp(
                        kind,
                        verdict.label().is_parity(),
                        line.census(kind),
                        pin.and_then(|p| p.ratchet(kind)),
                    ) {
                        Ok(counts) => {
                            ratchets.insert(kind, counts);
                        }
                        Err(legs) => {
                            stampable = false;
                            r.lost
                                .entry(kind)
                                .or_default()
                                .push(format!("{id} ({})", legs.join(", ")));
                        }
                    }
                }
                if stampable {
                    stamps.insert(
                        id,
                        Stamp {
                            verdict: *verdict,
                            attribution,
                            ratchets,
                        },
                    );
                }
            }
        }
    }
    match refusal(&r) {
        Some(why) => Err(DevError::Config(format!(
            "corpus --bless: nothing blessed, pins.toml untouched - every selected probe needs \
             a stampable disposition ({why})"
        ))),
        None => Ok(stamps),
    }
}

/// Stamp dispositions for `scope_ids` into the registry's pins, then
/// rewrite `pins_path` (the whole file - `[feeds]`/`[harness_files]`/
/// `[probe_config]` round-trip untouched).
///
/// Only the selected ids' `expected` is updated. The run loaded `registry`
/// under the lock it still holds, so no other brokkr writer can have touched
/// `pins.toml` since; the text it was parsed from ([`Registry::pins_text`])
/// is what gets edited. If the file on disk no longer matches that text, a
/// hand edit landed during the run: the bless is refused rather than revert
/// it, or stamp dispositions measured against pins that are no longer the
/// file's.
///
/// All or nothing: if any selected probe has no disposition line, or one
/// that is not a known label (a malformed `parity` line with no tier, which
/// would fail [`Registry::lint`] on the next load), or one reporting
/// `unevaluated`, which no pin can hold, the whole bless is
/// refused before a single pin is touched, naming every such probe. Blessing
/// the rest would leave the file half-adopted with nothing in it saying so.
/// The caller also refuses a run whose harness failed or broke protocol (see
/// `cmd.rs`); this is the last line, over exactly the stamps about to land.
pub fn apply(
    pins_path: &Path,
    registry: &mut Registry,
    report: &HarnessReport,
    scope_ids: &[String],
    attributions: &BTreeMap<String, String>,
) -> Result<(), DevError> {
    // Edit the text the run loaded, in place, so hand-written comments
    // survive - and only if it is still what is on disk (see above).
    let on_disk = match std::fs::read_to_string(pins_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(DevError::Io(e)),
    };
    if on_disk != registry.pins_text {
        return Err(DevError::Config(format!(
            "corpus --bless: {} changed during the run - nothing blessed (the run is \
             recorded; re-run the bless against the file as it is now)",
            pins_path.display()
        )));
    }

    let stamps = plan_stamps(registry, report, scope_ids, attributions)?;

    let mut blessed = 0usize;
    let mut changed = 0usize;
    for id in scope_ids {
        let (Some(pin), Some(stamp)) = (registry.pins.get_mut(id), stamps.get(id.as_str())) else {
            continue; // unreachable: validated above
        };
        blessed += 1;
        // The whole stamp, so an attribution-only correction is a change.
        let moved = pin.expectation() != Some(stamp.verdict)
            || pin.attribution != stamp.attribution
            || stamp
                .ratchets
                .iter()
                .any(|(kind, counts)| pin.ratchet(*kind) != counts.as_ref());
        if moved {
            changed += 1;
            pin.set_expectation(stamp.verdict, stamp.attribution.clone());
            for (kind, counts) in &stamp.ratchets {
                *pin.ratchet_mut(*kind) = counts.clone();
            }
        }
    }

    let text = pins_write::render_pins(
        Some(&registry.pins_text),
        &registry.feeds,
        &registry.harness_files,
        &registry.pending,
        &registry.pins,
    )?;
    registry_io::write_atomic(pins_path, &text)?;
    registry.pins_text = text;
    output::corpus_msg(&format!(
        "blessed {blessed} (changed {changed}) -> {}",
        output::path(pins_path)
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::registry::{self, FilePin, Pin};

    fn pin(expected: Option<&str>) -> Pin {
        let mut p = Pin::new(
            FilePin {
                path: "p/strategy.pine".into(),
                xxh128: "00".into(),
            },
            FilePin {
                path: "p/tv_trades.csv".into(),
                xxh128: "11".into(),
            },
        );
        p.expected = expected.map(|label| {
            registry::PinnedDisposition::from_label(label).expect("test disposition is recognized")
        });
        p
    }

    fn registry_of(pins: std::collections::BTreeMap<String, Pin>) -> Registry {
        Registry {
            pins,
            ..Registry::default()
        }
    }

    fn report(lines: &str) -> HarnessReport {
        crate::piners::report::parse(lines.as_bytes())
    }

    /// A bless with no `--attribute`.
    fn apply(
        pins_path: &Path,
        registry: &mut Registry,
        report: &HarnessReport,
        scope_ids: &[String],
    ) -> Result<(), DevError> {
        super::apply(pins_path, registry, report, scope_ids, &BTreeMap::new())
    }

    fn attributed(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const SEGV: &str =
        r#"{"probe":"a","outcome":"harness_abort","error":"signal 11","death":{"kind":"signal","number":11}}"#;
    const OOM: &str =
        r#"{"probe":"a","outcome":"harness_abort","error":"signal 9","death":{"kind":"signal","number":9}}"#;

    #[test]
    fn an_abort_needs_an_attribution_to_enter_or_move_and_keeps_it_otherwise() {
        let dir = crate::test_scratch::scratch("piners_bless", "abort_attribution");
        let pins_path = dir.join("pins.toml");
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(Some("accepted")));
        let mut reg = registry_of(pins);
        let ids = ["a".to_owned()];
        // A new abort without --attribute: refused, nothing written.
        let err = apply(&pins_path, &mut reg, &report(SEGV), &ids).unwrap_err();
        assert!(
            err.to_string()
                .contains("needs `--attribute PROBE=TEXT` saying what died, for a (harness_abort (signal 11))"),
            "{err}"
        );
        assert!(!pins_path.exists());
        // With one: death and attribution stamped together.
        let flag = attributed(&[("a", "engine recursion overflows the stack")]);
        super::apply(&pins_path, &mut reg, &report(SEGV), &ids, &flag).unwrap();
        let written = std::fs::read_to_string(&pins_path).unwrap();
        assert!(
            written.contains("death = { kind = \"signal\", number = 11 }"),
            "{written}"
        );
        let on_disk = registry::parse_pins(&written, &pins_path).unwrap();
        assert_eq!(
            on_disk.probes["a"].expectation(),
            Some(Verdict::HarnessAbort(registry::Death::Signal(11)))
        );
        // The same death again keeps the attribution without a flag.
        apply(&pins_path, &mut reg, &report(SEGV), &ids).unwrap();
        assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), written);
        // An attribution-only correction is a change.
        let fixed = attributed(&[("a", "deep recursion in ta.pivothigh")]);
        super::apply(&pins_path, &mut reg, &report(SEGV), &ids, &fixed).unwrap();
        assert!(
            std::fs::read_to_string(&pins_path)
                .unwrap()
                .contains("deep recursion in ta.pivothigh")
        );
        // A moved death needs a fresh statement.
        let err = apply(&pins_path, &mut reg, &report(OOM), &ids).unwrap_err();
        assert!(err.to_string().contains("for a (harness_abort (signal 9))"), "{err}");
        // Recovery clears both.
        let ok = report(r#"{"probe":"a","outcome":"runtime_fail","cause":"script","error":"x"}"#);
        apply(&pins_path, &mut reg, &ok, &ids).unwrap();
        let written = std::fs::read_to_string(&pins_path).unwrap();
        assert!(!written.contains("death"), "{written}");
        assert!(!written.contains("attribution"), "{written}");
    }

    /// A failure pin blessed before piners sent a cause still loads, reads
    /// as unblessed, and a bless adopts the line's cause, so the corpus is
    /// never locked out of the bless that repairs it.
    #[test]
    fn a_causeless_failure_pin_loads_and_bless_adopts_the_line_cause() {
        let dir = crate::test_scratch::scratch("piners_bless", "causeless_failure");
        let pins_path = dir.join("pins.toml");
        let old = "[probes.a]\nexpected = \"runtime_fail\"\n\
                   pine = { path = \"p/strategy.pine\", xxh128 = \"00\" }\n\
                   csv = { path = \"p/tv_trades.csv\", xxh128 = \"11\" }\n";
        let loaded = registry::parse_pins(old, &pins_path).unwrap();
        assert_eq!(loaded.probes["a"].expectation(), None);
        let mut reg = registry_of(loaded.probes);
        let ids = ["a".to_owned()];
        let line = report(r#"{"probe":"a","outcome":"runtime_fail","cause":"capability","error":"x"}"#);
        apply(&pins_path, &mut reg, &line, &ids).unwrap();
        let written = std::fs::read_to_string(&pins_path).unwrap();
        assert!(written.contains("cause = \"capability\""), "{written}");
        assert_eq!(
            registry::parse_pins(&written, &pins_path).unwrap().probes["a"].expectation(),
            Some(Verdict::RuntimeFail(registry::Cause::Capability))
        );
    }

    #[test]
    fn attribute_flags_split_at_the_first_equals_and_name_a_probe_once() {
        let flags = |f: &[&str]| parse_attributions(&f.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(
            flags(&["a=x = y", "b=z"]).unwrap(),
            attributed(&[("a", "x = y"), ("b", "z")])
        );
        assert!(flags(&["a"]).unwrap_err().to_string().contains("expected PROBE=TEXT"));
        assert!(flags(&["=x"]).unwrap_err().to_string().contains("expected PROBE=TEXT"));
        assert!(
            flags(&["a=x", "a=y"])
                .unwrap_err()
                .to_string()
                .contains("a is attributed twice")
        );
    }

    #[test]
    fn refuses_an_attribute_that_cannot_apply() {
        let dir = crate::test_scratch::scratch("piners_bless", "misattributed");
        let pins_path = dir.join("pins.toml");
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(None));
        let mut reg = registry_of(pins);
        let ids = ["a".to_owned()];
        let parity = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#);
        let segv = report(SEGV);
        for (flags, why) in [
            (attributed(&[("a", "x")]), "a did not report a harness_abort"),
            (attributed(&[("zz", "x")]), "zz is not selected"),
            (attributed(&[("a", "  ")]), "a's text is not one nonblank line"),
        ] {
            let rep = if why.starts_with("a did") { &parity } else { &segv };
            let err = super::apply(&pins_path, &mut reg, rep, &ids, &flags).unwrap_err();
            assert!(err.to_string().contains(why), "{err}");
        }
        assert!(!pins_path.exists());
    }

    #[test]
    fn stamps_current_dispositions_including_fails() {
        let dir = crate::test_scratch::scratch("piners_bless", "stamps_current");
        let pins_path = dir.join("pins.toml");

        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(Some("accepted"))); // will change
        pins.insert("b".to_owned(), pin(None)); // fresh, will bless to a fail
        pins.insert("untouched".to_owned(), pin(Some("byte_exact"))); // out of scope

        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"count_divergent\"}}\n{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\"}",
        );

        let mut reg = registry_of(pins);
        apply(
            &pins_path,
            &mut reg,
            &rep,
            &["a".to_owned(), "b".to_owned()],
        )
        .unwrap();

        assert_eq!(
            reg.pins["a"].expected,
            Some(registry::PinnedDisposition::CountDivergent)
        );
        assert_eq!(
            reg.pins["b"].expected,
            Some(registry::PinnedDisposition::CompileFail)
        );
        assert_eq!(
            reg.pins["untouched"].expected,
            Some(registry::PinnedDisposition::ByteExact)
        );

        let written = std::fs::read_to_string(&pins_path).unwrap();
        let on_disk = registry::parse_pins(&written, &pins_path).unwrap();
        assert_eq!(
            on_disk.probes["a"].expected,
            Some(registry::PinnedDisposition::CountDivergent)
        );
        assert_eq!(
            on_disk.probes["b"].expected,
            Some(registry::PinnedDisposition::CompileFail)
        );
        assert_eq!(
            on_disk.probes["untouched"].expected,
            Some(registry::PinnedDisposition::ByteExact)
        );
    }

    const CENSUS: &str = r#""labels":{"entry":{"compared":4,"mismatched":1,"ambiguous":0,"unavailable":0},"exit":{"compared":4,"mismatched":3,"ambiguous":1,"unavailable":0}}"#;

    #[test]
    fn stamps_the_census_clears_on_a_break_and_keeps_an_unpinned_probe_unpinned() {
        let dir = crate::test_scratch::scratch("piners_bless", "stamps_labels");
        let pins_path = dir.join("pins.toml");
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(Some("accepted"))); // gains labels
        let mut broke = pin(Some("accepted"));
        broke.labels = Some(LegPin::from([("entry", 0), ("exit", 0)]));
        pins.insert("b".to_owned(), broke); // now compile_fail: labels clear
        pins.insert("c".to_owned(), pin(None)); // old harness: stays unpinned
        let rep = report(&format!(
            "{{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"accepted\"}},{CENSUS}}}\n\
             {{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\"}}\n\
             {{\"probe\":\"c\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"accepted\"}}}}"
        ));
        let mut reg = registry_of(pins);
        let ids = ["a".to_owned(), "b".to_owned(), "c".to_owned()];
        apply(&pins_path, &mut reg, &rep, &ids).unwrap();
        let on_disk =
            registry::parse_pins(&std::fs::read_to_string(&pins_path).unwrap(), &pins_path)
                .unwrap();
        assert_eq!(
            on_disk.probes["a"].labels,
            Some(LegPin::from([("entry", 1), ("exit", 3)]))
        );
        assert_eq!(on_disk.probes["b"].labels, None);
        assert_eq!(on_disk.probes["c"].labels, None);
    }

    #[test]
    fn refuses_to_drop_pinned_labels_for_a_parity_line_without_a_census() {
        let dir = crate::test_scratch::scratch("piners_bless", "refuses_lost_labels");
        let pins_path = dir.join("pins.toml");
        std::fs::write(&pins_path, LOADED).unwrap();
        let mut pinned = pin(Some("accepted"));
        pinned.labels = Some(LegPin::from([("entry", 0), ("exit", 2)]));
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pinned);
        let mut reg = registry_of(pins);
        reg.pins_text = LOADED.to_owned();
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"byte_exact"}}"#);
        let err = apply(&pins_path, &mut reg, &rep, &["a".to_owned()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("a pinned labels leg the parity line does not carry for a (entry, exit)"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), LOADED);
        assert_eq!(
            reg.pins["a"].labels,
            Some(LegPin::from([("entry", 0), ("exit", 2)]))
        );
    }

    #[test]
    fn stamps_performance_legs_and_refuses_to_drop_a_pinned_one() {
        let dir = crate::test_scratch::scratch("piners_bless", "stamps_performance");
        let pins_path = dir.join("pins.toml");
        let line = |perf: &str| {
            report(&format!(
                "{{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"accepted\"}},\
                 \"performance\":{{{perf}}}}}"
            ))
        };
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(Some("accepted")));
        let mut reg = registry_of(pins);
        let both = line(
            "\"equity\":{\"compared\":9,\"mismatched\":1,\"tolerance\":\"exact\"},\
             \"scalars\":{\"compared\":4,\"mismatched\":0,\"tolerance\":\"exact\"}",
        );
        apply(&pins_path, &mut reg, &both, &["a".to_owned()]).unwrap();
        let written = std::fs::read_to_string(&pins_path).unwrap();
        assert!(
            written.contains("performance = { equity = 1, scalars = 0 }"),
            "{written}"
        );
        let on_disk = registry::parse_pins(&written, &pins_path).unwrap();
        assert_eq!(
            on_disk.probes["a"].performance,
            Some(LegPin::from([("equity", 1), ("scalars", 0)]))
        );
        // The equity leg went missing: refused, nothing written.
        let scalars_only =
            line("\"scalars\":{\"compared\":4,\"mismatched\":0,\"tolerance\":\"exact\"}");
        let err = apply(&pins_path, &mut reg, &scalars_only, &["a".to_owned()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("pinned performance leg the parity line does not carry for a (equity)"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), written);
    }

    #[test]
    fn stamps_excursion_legs_in_leg_order_and_clears_them_on_a_break() {
        let dir = crate::test_scratch::scratch("piners_bless", "stamps_excursions");
        let pins_path = dir.join("pins.toml");
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(Some("accepted")));
        let mut reg = registry_of(pins);
        let leg = |n: u64| {
            format!(
                "{{\"compared\":9,\"mismatched\":{n},\"ambiguous\":0,\"unavailable\":1,\
                 \"tolerance\":\"t\"}}"
            )
        };
        let rep = report(&format!(
            "{{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"accepted\"}},\
             \"excursions\":{{\"drawdown\":{},\"run_up\":{}}}}}",
            leg(1),
            leg(4)
        ));
        apply(&pins_path, &mut reg, &rep, &["a".to_owned()]).unwrap();
        let written = std::fs::read_to_string(&pins_path).unwrap();
        assert!(
            written.contains("excursions = { run_up = 4, drawdown = 1 }"),
            "{written}"
        );
        let broke = report(r#"{"probe":"a","outcome":"runtime_fail","cause":"script","error":"x"}"#);
        apply(&pins_path, &mut reg, &broke, &["a".to_owned()]).unwrap();
        let written = std::fs::read_to_string(&pins_path).unwrap();
        assert!(!written.contains("excursions"), "{written}");
    }

    #[test]
    fn refuses_the_whole_bless_when_a_selected_probe_has_no_disposition() {
        let dir = crate::test_scratch::scratch("piners_bless", "refuses_missing");
        let pins_path = dir.join("pins.toml");
        std::fs::write(&pins_path, LOADED).unwrap();
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(None));
        pins.insert("b".to_owned(), pin(Some("accepted")));
        let mut reg = registry_of(pins);
        reg.pins_text = LOADED.to_owned();
        // `a` reported, `b` did not: nothing may be stamped, `a` included.
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"byte_exact"}}"#);
        let err = apply(
            &pins_path,
            &mut reg,
            &rep,
            &["a".to_owned(), "b".to_owned()],
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nothing blessed"), "{msg}");
        assert!(msg.contains("no disposition for b"), "{msg}");
        assert_eq!(reg.pins["a"].expected, None, "in-memory pins mutated");
        assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), LOADED);
    }

    #[test]
    fn refuses_the_whole_bless_on_an_unstampable_disposition() {
        let dir = crate::test_scratch::scratch("piners_bless", "refuses_unstampable");
        let pins_path = dir.join("pins.toml");
        std::fs::write(&pins_path, LOADED).unwrap();
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(None));
        pins.insert("b".to_owned(), pin(None));
        let mut reg = registry_of(pins);
        reg.pins_text = LOADED.to_owned();
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\"}\n{\"probe\":\"b\",\"outcome\":\"parity\"}",
        );
        let err = apply(
            &pins_path,
            &mut reg,
            &rep,
            &["a".to_owned(), "b".to_owned()],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unstampable disposition for b (parity)"),
            "{err}"
        );
        assert_eq!(reg.pins["a"].expected, None);
        // A tier posing as an outcome derives a pinnable-looking label and is
        // still refused.
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"accepted\"}\n{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\"}",
        );
        let err = apply(
            &pins_path,
            &mut reg,
            &rep,
            &["a".to_owned(), "b".to_owned()],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unstampable disposition for a (accepted)"),
            "{err}"
        );
        // A record whose own disposition field contradicts it is unstampable too.
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\",\"disposition\":\"runtime_fail\"}\n{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\"}",
        );
        let err = apply(
            &pins_path,
            &mut reg,
            &rep,
            &["a".to_owned(), "b".to_owned()],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unstampable disposition for a (compile_fail)"),
            "{err}"
        );
        assert_eq!(reg.pins["a"].expected, None);
        // A retired outcome on a new line is unstampable as well.
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"no_tv_data\"}\n{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\"}",
        );
        let err = apply(
            &pins_path,
            &mut reg,
            &rep,
            &["a".to_owned(), "b".to_owned()],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unstampable disposition for a (no_tv_data)"),
            "{err}"
        );
        assert_eq!(reg.pins["a"].expected, None);
        assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), LOADED);
    }

    #[test]
    fn refuses_the_whole_bless_when_any_probe_is_unevaluated() {
        let dir = crate::test_scratch::scratch("piners_bless", "refuses_unevaluated");
        let pins_path = dir.join("pins.toml");
        std::fs::write(&pins_path, LOADED).unwrap();
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(None));
        pins.insert("b".to_owned(), pin(None));
        let mut reg = registry_of(pins);
        reg.pins_text = LOADED.to_owned();
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"unevaluated\",\"disposition\":\"unevaluated\",\
             \"reason\":\"no_overlap\",\"error\":\"disjoint\"}\n\
             {\"probe\":\"b\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\"}",
        );
        assert!(rep.invalid.is_empty(), "{:?}", rep.invalid);
        let err = apply(
            &pins_path,
            &mut reg,
            &rep,
            &["a".to_owned(), "b".to_owned()],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unevaluated, which no pin can hold, for a"),
            "{err}"
        );
        // Nothing stamped, not even the stampable sibling.
        assert_eq!(reg.pins["b"].expected, None);
        assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), LOADED);
    }

    const LOADED: &str = "# keep me\n[probes.a]\npine = { path = \"p/strategy.pine\", xxh128 = \"00\" }\n\
                          csv = { path = \"p/tv_trades.csv\", xxh128 = \"11\" }\n";

    #[test]
    fn edits_the_loaded_text_in_place() {
        let dir = crate::test_scratch::scratch("piners_bless", "loaded_text");
        let pins_path = dir.join("pins.toml");
        std::fs::write(&pins_path, LOADED).unwrap();
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(None));
        let mut reg = registry_of(pins);
        reg.pins_text = LOADED.to_owned();
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#);
        apply(&pins_path, &mut reg, &rep, &["a".to_owned()]).unwrap();

        let written = std::fs::read_to_string(&pins_path).unwrap();
        assert!(written.contains("# keep me"));
        let data = registry::parse_pins(&written, &pins_path).unwrap();
        assert_eq!(
            data.probes["a"].expected,
            Some(registry::PinnedDisposition::Accepted)
        );
        assert_eq!(reg.pins_text, written);
    }

    #[test]
    fn refuses_when_the_file_changed_during_the_run() {
        // A hand edit landed between the run's load and the bless: writing
        // the loaded state would revert it.
        let dir = crate::test_scratch::scratch("piners_bless", "changed_underneath");
        let pins_path = dir.join("pins.toml");
        let edited = LOADED.replace("# keep me", "# edited by hand");
        std::fs::write(&pins_path, &edited).unwrap();
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(None));
        let mut reg = registry_of(pins);
        reg.pins_text = LOADED.to_owned();
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#);
        let err = apply(&pins_path, &mut reg, &rep, &["a".to_owned()]).unwrap_err();
        assert!(format!("{err:?}").contains("changed during the run"));
        assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), edited);
    }
}
