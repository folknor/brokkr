//! `brokkr corpus --bless`: stamp each selected probe's *current*
//! disposition into its `expected` field in `pins.toml`, and its per-leg
//! displayed-label mismatch counts into `labels` (the label ratchet,
//! `gate.rs`): a parity line's census is stamped, a non-parity line clears
//! the counts, and a parity line without a census leaves an unpinned probe
//! unpinned but refuses the bless where it would drop counts already pinned.
//!
//! Bless is the sibling of `--reseed`: reseed adopts new corpus *content*
//! (re-hashing the pinned probe files), bless adopts new *dispositions*. Both are
//! deliberate human acts whose review surface is `git diff pins.toml`. Bless
//! records reality - including `compile_fail`/`runtime_fail`/`harness_abort`
//! outcomes - so a probe known to exercise an unimplemented feature can pin
//! `expected = "compile_fail"` and the gate will later catch it silently
//! starting to compile. The one outcome it refuses is `unevaluated`: an
//! experiment that was not conducted records nothing about the engine.
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
use crate::piners::registry::{LabelPin, Registry};
use crate::piners::registry_io;
use crate::piners::report::{HarnessReport, LabelCensus};

/// The label counts a bless stamps for one probe with a valid line, or
/// `None` when it must refuse. A non-parity line has no labels to pin; a
/// parity line pins its census. A parity line without one keeps an unpinned
/// probe unpinned (a harness predating the census still blesses), but cannot
/// replace counts the pin already holds: that would turn a harness that lost
/// the census into an authorized-looking removal of the ratchet.
fn label_stamp(
    parity: bool,
    census: Option<LabelCensus>,
    pinned: Option<LabelPin>,
) -> Option<Option<LabelPin>> {
    match (parity, census, pinned) {
        (false, _, _) => Some(None),
        (true, Some(c), _) => Some(Some(LabelPin {
            entry: c.entry.mismatched,
            exit: c.exit.mismatched,
        })),
        (true, None, None) => Some(None),
        (true, None, Some(_)) => None,
    }
}

/// Why a bless is refused, naming every probe per reason; `None` when it
/// may proceed.
fn refusal(
    missing: &[&str],
    rejected: &[String],
    unevaluated: &[String],
    unpinned: &[&str],
    lost_labels: &[&str],
) -> Option<String> {
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
    if !lost_labels.is_empty() {
        why.push(format!(
            "pinned label counts but no label census on the parity line for {}",
            lost_labels.join(", ")
        ));
    }
    (!why.is_empty()).then(|| why.join("; "))
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

    // Each line's derived label (for naming a refusal) and its validated one
    // (`report::valid_label`, the validator integrity and the gate share).
    let actual: BTreeMap<&str, (String, Option<crate::piners::registry::CorpusDisposition>)> =
        report
            .probes
            .iter()
            .map(|p| (p.probe.as_str(), (p.disposition(), p.valid_disposition())))
            .collect();

    // Each line's label census, already held valid by the record check.
    let census: BTreeMap<&str, Option<LabelCensus>> = report
        .probes
        .iter()
        .map(|p| (p.probe.as_str(), p.labels().ok().flatten()))
        .collect();

    // Validate every stamp before mutating anything.
    let mut missing: Vec<&str> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    let mut unevaluated: Vec<String> = Vec::new();
    let mut unpinned: Vec<&str> = Vec::new();
    let mut lost_labels: Vec<&str> = Vec::new();
    let mut label_stamps: BTreeMap<&str, Option<LabelPin>> = BTreeMap::new();
    for id in scope_ids {
        match actual.get(id.as_str()) {
            None => missing.push(id),
            Some((derived, None)) => rejected.push(format!("{id} ({derived})")),
            Some((_, Some(disp))) if !disp.is_pinnable() => unevaluated.push(id.clone()),
            Some(_) if !registry.pins.contains_key(id) => unpinned.push(id),
            Some((_, Some(disp))) => {
                let pinned = registry.pins.get(id).and_then(|p| p.labels);
                let line = census.get(id.as_str()).copied().flatten();
                match label_stamp(disp.is_parity(), line, pinned) {
                    Some(stamp) => {
                        label_stamps.insert(id, stamp);
                    }
                    None => lost_labels.push(id),
                }
            }
        }
    }
    if let Some(why) = refusal(&missing, &rejected, &unevaluated, &unpinned, &lost_labels) {
        return Err(DevError::Config(format!(
            "corpus --bless: nothing blessed, pins.toml untouched - every selected probe needs \
             a stampable disposition ({why})"
        )));
    }

    let mut blessed = 0usize;
    let mut changed = 0usize;
    for id in scope_ids {
        let (Some((_, Some(disp))), Some(pin), Some(&labels)) = (
            actual.get(id.as_str()),
            registry.pins.get_mut(id),
            label_stamps.get(id.as_str()),
        ) else {
            continue; // unreachable: validated above
        };
        blessed += 1;
        if pin.expected != Some(*disp) || pin.labels != labels {
            changed += 1;
            pin.expected = Some(*disp);
            pin.labels = labels;
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
            registry::CorpusDisposition::from_label(label).expect("test disposition is recognized")
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

    #[test]
    fn stamps_current_dispositions_including_fails() {
        let dir = crate::test_scratch::scratch("piners_bless", "stamps_current");
        let pins_path = dir.join("pins.toml");

        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(Some("accepted"))); // will change
        pins.insert("b".to_owned(), pin(None)); // fresh, will bless to a fail
        pins.insert("untouched".to_owned(), pin(Some("byte_exact"))); // out of scope

        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"count_divergent\"}}\n{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"error\":\"x\"}",
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
            Some(registry::CorpusDisposition::CountDivergent)
        );
        assert_eq!(
            reg.pins["b"].expected,
            Some(registry::CorpusDisposition::CompileFail)
        );
        assert_eq!(
            reg.pins["untouched"].expected,
            Some(registry::CorpusDisposition::ByteExact)
        );

        let written = std::fs::read_to_string(&pins_path).unwrap();
        let on_disk = registry::parse_pins(&written, &pins_path).unwrap();
        assert_eq!(
            on_disk.probes["a"].expected,
            Some(registry::CorpusDisposition::CountDivergent)
        );
        assert_eq!(
            on_disk.probes["b"].expected,
            Some(registry::CorpusDisposition::CompileFail)
        );
        assert_eq!(
            on_disk.probes["untouched"].expected,
            Some(registry::CorpusDisposition::ByteExact)
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
        broke.labels = Some(LabelPin { entry: 0, exit: 0 });
        pins.insert("b".to_owned(), broke); // now compile_fail: labels clear
        pins.insert("c".to_owned(), pin(None)); // old harness: stays unpinned
        let rep = report(&format!(
            "{{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"accepted\"}},{CENSUS}}}\n\
             {{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"error\":\"x\"}}\n\
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
            Some(LabelPin { entry: 1, exit: 3 })
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
        pinned.labels = Some(LabelPin { entry: 0, exit: 2 });
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pinned);
        let mut reg = registry_of(pins);
        reg.pins_text = LOADED.to_owned();
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"byte_exact"}}"#);
        let err = apply(&pins_path, &mut reg, &rep, &["a".to_owned()]).unwrap_err();
        assert!(err.to_string().contains("no label census"));
        assert_eq!(std::fs::read_to_string(&pins_path).unwrap(), LOADED);
        assert_eq!(reg.pins["a"].labels, Some(LabelPin { entry: 0, exit: 2 }));
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
            "{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"error\":\"x\"}\n{\"probe\":\"b\",\"outcome\":\"parity\"}",
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
            "{\"probe\":\"a\",\"outcome\":\"accepted\"}\n{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"error\":\"x\"}",
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
            "{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"error\":\"x\",\"disposition\":\"runtime_fail\"}\n{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"error\":\"x\"}",
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
            "{\"probe\":\"a\",\"outcome\":\"no_tv_data\"}\n{\"probe\":\"b\",\"outcome\":\"compile_fail\",\"error\":\"x\"}",
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
             {\"probe\":\"b\",\"outcome\":\"compile_fail\",\"error\":\"x\"}",
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
            Some(registry::CorpusDisposition::Accepted)
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
