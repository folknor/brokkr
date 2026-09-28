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
use crate::piners::registry::{self, Registry};
use crate::piners::registry_io;
use crate::piners::report::HarnessReport;

/// Stamp dispositions for `scope_ids` into the registry's pins, then
/// rewrite `pins_path` (the whole file - `[feeds]`/`[probe_config]`
/// round-trip untouched).
///
/// Only the selected ids' `expected` is updated. The pin universe stamped
/// into is re-read from `pins_path` here rather than taken from the
/// `registry` the run loaded: the run loaded it before taking the lock, so a
/// reseed that landed in between would otherwise be silently reverted by
/// this write. The caller holds the lock across this call. `registry` is
/// refreshed to match what was written.
///
/// A selected probe the harness emitted no line for is skipped with a
/// warning (nothing to bless). A disposition that is not a known label (a
/// malformed `parity` line with no tier) is refused for that probe rather
/// than written, since it would fail [`Registry::lint`] on the next load.
/// The caller is responsible for refusing to bless a run whose harness
/// failed (see `cmd.rs`); this function stamps what it is given.
pub fn apply(
    pins_path: &Path,
    registry: &mut Registry,
    report: &HarnessReport,
    scope_ids: &[String],
) -> Result<(), DevError> {
    // Edit the existing file in place so hand-written comments survive, and
    // stamp into its current contents (see above).
    let existing = if pins_path.exists() {
        Some(std::fs::read_to_string(pins_path).map_err(DevError::Io)?)
    } else {
        None
    };
    if let Some(text) = existing.as_deref() {
        let fresh = registry::parse_pins(text, pins_path)?;
        registry.pins = fresh.probes;
        registry.feeds = fresh.feeds;
        registry.probe_config = fresh.probe_config;
    }

    let actual: BTreeMap<&str, String> = report
        .probes
        .iter()
        .map(|p| (p.probe.as_str(), p.disposition()))
        .collect();

    let mut blessed = 0usize;
    let mut changed = 0usize;
    let mut missing: Vec<String> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    let mut vanished: Vec<String> = Vec::new();

    for id in scope_ids {
        let Some(disp) = actual.get(id.as_str()) else {
            missing.push(id.clone());
            continue;
        };
        if !registry::is_disposition(disp) {
            rejected.push(format!("{id} ({disp})"));
            continue;
        }
        let Some(pin) = registry.pins.get_mut(id) else {
            // The probe was selected from the pins the run loaded, but a
            // reseed since then dropped it from the file: nothing to stamp.
            vanished.push(id.clone());
            continue;
        };
        blessed += 1;
        if pin.expected.as_deref() != Some(disp.as_str()) {
            changed += 1;
            pin.expected = Some(disp.clone());
        }
    }

    registry_io::write_atomic(
        pins_path,
        &pins_write::render_pins(existing.as_deref(), &registry.feeds, &registry.pins)?,
    )?;
    output::corpus_msg(&format!(
        "blessed {blessed} (changed {changed}) -> {}",
        pins_path.display()
    ));
    if !missing.is_empty() {
        output::corpus_msg(&format!(
            "warning: {} selected probe(s) emitted no disposition, not blessed: {}",
            missing.len(),
            missing.join(", ")
        ));
    }
    if !rejected.is_empty() {
        output::corpus_msg(&format!(
            "warning: {} probe(s) had an unstampable disposition, not blessed: {}",
            rejected.len(),
            rejected.join(", ")
        ));
    }
    if !vanished.is_empty() {
        output::corpus_msg(&format!(
            "warning: {} probe(s) left pins.toml during the run, not blessed: {}",
            vanished.len(),
            vanished.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::registry::{FilePin, Pin};

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
    fn skips_probe_with_no_emitted_disposition() {
        let dir = crate::test_scratch::scratch("piners_bless", "skips_missing");
        let pins_path = dir.join("pins.toml");
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(Some("accepted")));

        let mut reg = registry_of(pins);
        apply(&pins_path, &mut reg, &report(""), &["a".to_owned()]).unwrap();

        // unchanged: no disposition emitted, nothing to bless
        assert_eq!(reg.pins["a"].expected.as_deref(), Some("accepted"));
    }

    #[test]
    fn stamps_into_the_file_on_disk_not_the_stale_loaded_pins() {
        // The run loaded `a` with hash "00"; a reseed then re-stamped the file
        // to "ff" before bless wrote. Bless must keep the reseed's hash and
        // only change `expected`.
        let dir = crate::test_scratch::scratch("piners_bless", "fresh_reread");
        let pins_path = dir.join("pins.toml");
        std::fs::write(
            &pins_path,
            "# keep me\n[probes.a]\npine = { path = \"p/strategy.pine\", xxh128 = \"ff\" }\n\
             csv = { path = \"p/tv_trades.csv\", xxh128 = \"ff\" }\n",
        )
        .unwrap();

        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(None)); // stale: hashes "00"/"11"
        let mut reg = registry_of(pins);
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#);
        apply(&pins_path, &mut reg, &rep, &["a".to_owned()]).unwrap();

        let written = std::fs::read_to_string(&pins_path).unwrap();
        assert!(written.contains("# keep me"));
        let data = registry::parse_pins(&written, &pins_path).unwrap();
        assert_eq!(data.probes["a"].pine.xxh128, "ff");
        assert_eq!(data.probes["a"].expected.as_deref(), Some("accepted"));
    }
}
