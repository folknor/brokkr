//! `brokkr corpus --bless`: stamp each selected probe's *current*
//! disposition into its `expected` field in `pins.toml`.
//!
//! Bless is the sibling of `--reseed`: reseed adopts new corpus *content*
//! (re-hashing the pinned probe files), bless adopts new *dispositions*. Both are
//! deliberate human acts whose review surface is `git diff pins.toml`. Bless
//! records reality - including `compile_fail`/`runtime_fail`/`no_tv_data`/
//! `no_overlap` outcomes - so a probe known to exercise an unimplemented
//! feature can pin `expected = "compile_fail"` and the gate will later catch
//! it silently starting to compile.
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
use crate::piners::registry::Registry;
use crate::piners::registry_io;
use crate::piners::report::HarnessReport;

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
/// would fail [`Registry::lint`] on the next load), the whole bless is
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
    let actual: BTreeMap<&str, (String, Option<&'static str>)> = report
        .probes
        .iter()
        .map(|p| (p.probe.as_str(), (p.disposition(), p.valid_disposition())))
        .collect();

    // Validate every stamp before mutating anything.
    let mut missing: Vec<&str> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    let mut unpinned: Vec<&str> = Vec::new();
    for id in scope_ids {
        match actual.get(id.as_str()) {
            None => missing.push(id),
            Some((derived, None)) => rejected.push(format!("{id} ({derived})")),
            Some(_) if !registry.pins.contains_key(id) => unpinned.push(id),
            Some(_) => {}
        }
    }
    if !missing.is_empty() || !rejected.is_empty() || !unpinned.is_empty() {
        let mut why = Vec::new();
        if !missing.is_empty() {
            why.push(format!("no disposition for {}", missing.join(", ")));
        }
        if !rejected.is_empty() {
            why.push(format!("unstampable disposition for {}", rejected.join(", ")));
        }
        if !unpinned.is_empty() {
            why.push(format!("internal: not pinned: {}", unpinned.join(", ")));
        }
        return Err(DevError::Config(format!(
            "corpus --bless: nothing blessed, pins.toml untouched - every selected probe needs \
             a stampable disposition ({})",
            why.join("; ")
        )));
    }

    let mut blessed = 0usize;
    let mut changed = 0usize;
    for id in scope_ids {
        let (Some((_, Some(disp))), Some(pin)) = (actual.get(id.as_str()), registry.pins.get_mut(id))
        else {
            continue; // unreachable: validated above
        };
        blessed += 1;
        if pin.expected.as_deref() != Some(*disp) {
            changed += 1;
            pin.expected = Some((*disp).to_owned());
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
        pins_path.display()
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
        p.expected = expected.map(str::to_owned);
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
        apply(&pins_path, &mut reg, &rep, &["a".to_owned(), "b".to_owned()]).unwrap();

        assert_eq!(reg.pins["a"].expected.as_deref(), Some("count_divergent"));
        assert_eq!(reg.pins["b"].expected.as_deref(), Some("compile_fail"));
        assert_eq!(reg.pins["untouched"].expected.as_deref(), Some("byte_exact"));
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
        let err = apply(&pins_path, &mut reg, &rep, &["a".to_owned(), "b".to_owned()]).unwrap_err();
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
            "{\"probe\":\"a\",\"outcome\":\"no_tv_data\"}\n{\"probe\":\"b\",\"outcome\":\"parity\"}",
        );
        let err = apply(&pins_path, &mut reg, &rep, &["a".to_owned(), "b".to_owned()]).unwrap_err();
        assert!(err.to_string().contains("unstampable disposition for b (parity)"), "{err}");
        assert_eq!(reg.pins["a"].expected, None);
        // A tier posing as an outcome derives a pinnable-looking label and is
        // still refused.
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"accepted\"}\n{\"probe\":\"b\",\"outcome\":\"no_tv_data\"}",
        );
        let err = apply(&pins_path, &mut reg, &rep, &["a".to_owned(), "b".to_owned()]).unwrap_err();
        assert!(err.to_string().contains("unstampable disposition for a (accepted)"), "{err}");
        assert_eq!(reg.pins["a"].expected, None);
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
        assert_eq!(data.probes["a"].expected.as_deref(), Some("accepted"));
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
