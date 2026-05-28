//! Resolve `brokkr corpus` selection flags to a concrete probe-id set.
//!
//! Selection is over the pinned universe in `pins.toml`. Keyword files
//! are pure groupings: `--keyword` unions the ids they list, `--probe`
//! picks one id directly, `--all` takes the whole universe. There is no
//! implicit "run everything" - a bare invocation is an error, so the slow
//! full-corpus pass is never triggered by accident.

use crate::error::DevError;
use crate::piners::registry::Registry;

/// Flags that drive selection, lifted off the CLI command.
#[derive(Debug, Default)]
pub struct SelectArgs {
    /// `--keyword` (repeatable): union of the listed groupings.
    pub keywords: Vec<String>,
    /// `--probe <id>`: a single pinned probe.
    pub probe: Option<String>,
    /// `--all`: the whole pinned universe (slow characterization path).
    pub all: bool,
    /// `--verify-only`: walk and verify the whole universe, run nothing.
    pub verify_only: bool,
}

/// Resolve `args` against `registry` to an ordered, de-duplicated list of
/// probe ids.
///
/// `--verify-only` selects the entire pinned universe (verification walks
/// everything). Otherwise the union of `--keyword`/`--probe`/`--all`
/// applies. A selection that resolves to nothing - including a bare
/// invocation with no flags - is a hard error listing the available
/// keywords, mirroring the dataset-resolver error style.
pub fn resolve(registry: &Registry, args: &SelectArgs) -> Result<Vec<String>, DevError> {
    if args.verify_only || args.all {
        if registry.pins.is_empty() {
            return Err(DevError::Config(
                "piners: pins.toml is empty - nothing to select".into(),
            ));
        }
        return Ok(registry.pins.keys().cloned().collect());
    }

    let mut selected: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for keyword in &args.keywords {
        let ids = registry.keywords.get(keyword).ok_or_else(|| {
            DevError::Config(format!(
                "piners: unknown keyword '{keyword}' (available: {})",
                available(registry)
            ))
        })?;
        for id in ids {
            if seen.insert(id.clone()) {
                selected.push(id.clone());
            }
        }
    }

    if let Some(probe) = &args.probe {
        if !registry.pins.contains_key(probe) {
            return Err(DevError::Config(format!(
                "piners: unknown probe '{probe}' - not pinned in pins.toml"
            )));
        }
        if seen.insert(probe.clone()) {
            selected.push(probe.clone());
        }
    }

    if selected.is_empty() {
        return Err(DevError::Config(format!(
            "piners: no probes selected. Pass --keyword <k> (repeatable), \
             --probe <id>, or --all. Available keywords: {}",
            available(registry)
        )));
    }

    Ok(selected)
}

fn available(registry: &Registry) -> String {
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
    use crate::piners::registry::{FilePin, Pin};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn pin() -> Pin {
        Pin {
            expected: None,
            pine: FilePin {
                path: PathBuf::from("p.pine"),
                xxh128: "00".into(),
            },
            csv: FilePin {
                path: PathBuf::from("p.csv"),
                xxh128: "11".into(),
            },
        }
    }

    fn registry() -> Registry {
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin());
        pins.insert("b".to_owned(), pin());
        pins.insert("c".to_owned(), pin());
        let mut keywords = BTreeMap::new();
        keywords.insert("x".to_owned(), vec!["a".to_owned(), "b".to_owned()]);
        keywords.insert("y".to_owned(), vec!["b".to_owned(), "c".to_owned()]);
        Registry { pins, keywords }
    }

    #[test]
    fn keyword_union_dedups_preserving_order() {
        let args = SelectArgs {
            keywords: vec!["x".to_owned(), "y".to_owned()],
            ..Default::default()
        };
        let got = resolve(&registry(), &args).unwrap();
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn unknown_keyword_errors() {
        let args = SelectArgs {
            keywords: vec!["nope".to_owned()],
            ..Default::default()
        };
        assert!(resolve(&registry(), &args).is_err());
    }

    #[test]
    fn probe_selects_one_even_if_unkeyworded() {
        let args = SelectArgs {
            probe: Some("c".to_owned()),
            ..Default::default()
        };
        assert_eq!(resolve(&registry(), &args).unwrap(), vec!["c"]);
    }

    #[test]
    fn unknown_probe_errors() {
        let args = SelectArgs {
            probe: Some("zzz".to_owned()),
            ..Default::default()
        };
        assert!(resolve(&registry(), &args).is_err());
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
    fn verify_only_selects_whole_universe() {
        let args = SelectArgs {
            verify_only: true,
            ..Default::default()
        };
        assert_eq!(resolve(&registry(), &args).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn no_selection_is_an_error() {
        assert!(resolve(&registry(), &SelectArgs::default()).is_err());
    }
}
