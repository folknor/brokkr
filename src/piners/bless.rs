//! `brokkr corpus --bless`: stamp each selected probe's *current*
//! disposition into its `expected` field in `pins.toml`.
//!
//! Bless is the sibling of `--reseed`: reseed adopts new corpus *content*
//! (re-hashing `pine`/`csv`), bless adopts new *dispositions*. Both are
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
use crate::piners::report::HarnessReport;

/// Stamp dispositions for `scope_ids` into the registry's pins, then
/// rewrite `pins_path` (the whole file - `[feeds]`/`[roots]` round-trip
/// untouched).
///
/// The registry holds the whole loaded universe; only the selected ids are
/// updated. A selected probe the harness emitted no line for is skipped
/// with a warning (nothing to bless). A disposition that is not a known
/// label (a malformed `parity` line with no tier) is refused for that probe
/// rather than written, since it would fail [`Registry::lint`] on the next
/// load.
pub fn apply(
    pins_path: &Path,
    registry: &mut Registry,
    report: &HarnessReport,
    scope_ids: &[String],
) -> Result<(), DevError> {
    let actual: BTreeMap<&str, String> = report
        .probes
        .iter()
        .map(|p| (p.probe.as_str(), p.disposition()))
        .collect();

    let mut blessed = 0usize;
    let mut changed = 0usize;
    let mut missing: Vec<String> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();

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
            continue; // selection came from this map; cannot happen
        };
        blessed += 1;
        if pin.expected.as_deref() != Some(disp.as_str()) {
            changed += 1;
            pin.expected = Some(disp.clone());
        }
    }

    // Edit the existing file in place so hand-written comments survive.
    let existing = if pins_path.exists() {
        Some(std::fs::read_to_string(pins_path).map_err(DevError::Io)?)
    } else {
        None
    };
    std::fs::write(
        pins_path,
        pins_write::render_pins(
            existing.as_deref(),
            &registry.feeds,
            &registry.roots,
            &registry.pins,
        )?,
    )
    .map_err(DevError::Io)?;
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
                path: "p.pine".into(),
                xxh128: "00".into(),
            },
            FilePin {
                path: "p.csv".into(),
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
        let dir = std::env::temp_dir().join(format!("brokkr_bless_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(reg.pins["a"].expected.as_deref(), Some("count_divergent"));
        assert_eq!(reg.pins["b"].expected.as_deref(), Some("compile_fail"));
        assert_eq!(reg.pins["untouched"].expected.as_deref(), Some("byte_exact"));
    }

    #[test]
    fn skips_probe_with_no_emitted_disposition() {
        let dir = std::env::temp_dir().join(format!("brokkr_bless_skip_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pins_path = dir.join("pins.toml");
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin(Some("accepted")));

        let mut reg = registry_of(pins);
        apply(&pins_path, &mut reg, &report(""), &["a".to_owned()]).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // unchanged: no disposition emitted, nothing to bless
        assert_eq!(reg.pins["a"].expected.as_deref(), Some("accepted"));
    }
}
