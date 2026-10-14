//! Resolve `brokkr lint-corpus` selection flags to a concrete probe-id set.
//!
//! Uses the shared selector engine in `src/piners/selector.rs` over the lint
//! universe in `lints.toml`; lint has no pending-probe state, and
//! `--all`/`--verify-only` take the whole universe. A bare invocation is an
//! error, so the full pass never runs by accident.

use std::collections::BTreeSet;

use crate::error::DevError;
use crate::piners::lint::registry::LintRegistry;
use crate::piners::selector::{self, SelectionArgs, SelectionPolicy};

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
    let pending = BTreeSet::new();
    selector::resolve_selection(
        &registry.pins,
        &registry.keywords,
        &pending,
        &SelectionArgs {
            keywords: &args.keywords,
            probes: &args.probe,
            all: args.all,
            whole_universe: args.all_universe,
        },
        &SelectionPolicy {
            label: "piners lint",
            registry_file: "lints.toml",
            whole_universe_named_error: None,
            pending_hint: None,
        },
        |_| {},
    )
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
        LintRegistry {
            pins,
            keywords,
            lints_text: String::new(),
        }
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
