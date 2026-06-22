//! Resolve `brokkr lint-corpus` selection flags to a concrete probe-id set.
//!
//! Mirrors `src/piners/select.rs`: selection is over the pinned universe in
//! `lints.toml`; `--keyword` unions groupings, `--probe` picks ids directly,
//! `--all`/`--verify-only` take the whole universe. A bare invocation is an
//! error, so the full pass never runs by accident.

use crate::error::DevError;
use crate::piners::lint::registry::LintRegistry;

/// Flags that drive selection, lifted off the CLI command.
#[derive(Debug, Default)]
pub struct SelectArgs {
    pub keywords: Vec<String>,
    pub probe: Vec<String>,
    pub all: bool,
    /// `--verify-only` (and `--reanchor --all`) select the whole universe.
    pub all_universe: bool,
}

/// Resolve `args` against `registry` to an ordered, de-duplicated id list.
pub fn resolve(registry: &LintRegistry, args: &SelectArgs) -> Result<Vec<String>, DevError> {
    if args.all_universe || args.all {
        if registry.pins.is_empty() {
            return Err(DevError::Config(
                "piners lint: lints.toml is empty - nothing to select".into(),
            ));
        }
        return Ok(registry.pins.keys().cloned().collect());
    }

    let mut selected: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for keyword in &args.keywords {
        let ids = registry.keywords.get(keyword).ok_or_else(|| {
            DevError::Config(format!(
                "piners lint: unknown keyword '{keyword}' (available: {})",
                available(registry)
            ))
        })?;
        for id in ids {
            if seen.insert(id.clone()) {
                selected.push(id.clone());
            }
        }
    }

    for probe in &args.probe {
        if !registry.pins.contains_key(probe) {
            return Err(DevError::Config(format!(
                "piners lint: unknown probe '{probe}' - not pinned in lints.toml"
            )));
        }
        if seen.insert(probe.clone()) {
            selected.push(probe.clone());
        }
    }

    if selected.is_empty() {
        return Err(DevError::Config(format!(
            "piners lint: no probes selected. Pass --keyword <k> (repeatable), \
             --probe <id>, or --all. Available keywords: {}",
            available(registry)
        )));
    }

    Ok(selected)
}

fn available(registry: &LintRegistry) -> String {
    let names = registry.keyword_names();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::lint::registry::LintPin;
    use crate::piners::registry::FilePin;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn pin() -> LintPin {
        LintPin::new(FilePin {
            path: PathBuf::from("lint/p.pine"),
            xxh128: "00".into(),
        })
    }

    fn registry() -> LintRegistry {
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin());
        pins.insert("b".to_owned(), pin());
        pins.insert("c".to_owned(), pin());
        let mut keywords = BTreeMap::new();
        keywords.insert("x".to_owned(), vec!["a".to_owned(), "b".to_owned()]);
        keywords.insert("y".to_owned(), vec!["b".to_owned(), "c".to_owned()]);
        LintRegistry { pins, keywords }
    }

    #[test]
    fn keyword_union_dedups_preserving_order() {
        let args = SelectArgs {
            keywords: vec!["x".to_owned(), "y".to_owned()],
            ..Default::default()
        };
        assert_eq!(resolve(&registry(), &args).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn probes_union_with_keywords_dedup() {
        let args = SelectArgs {
            keywords: vec!["x".to_owned()],
            probe: vec!["a".to_owned(), "c".to_owned()],
            ..Default::default()
        };
        assert_eq!(resolve(&registry(), &args).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn all_selects_whole_universe() {
        let args = SelectArgs {
            all: true,
            ..Default::default()
        };
        assert_eq!(resolve(&registry(), &args).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn unknown_probe_errors() {
        let args = SelectArgs {
            probe: vec!["zzz".to_owned()],
            ..Default::default()
        };
        assert!(resolve(&registry(), &args).is_err());
    }

    #[test]
    fn no_selection_is_an_error() {
        assert!(resolve(&registry(), &SelectArgs::default()).is_err());
    }
}
