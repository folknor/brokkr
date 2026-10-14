//! Resolve `brokkr corpus` selection flags to a concrete probe-id set.
//!
//! Selection is over the pinned universe in `pins.toml`. Keyword files
//! are pure groupings: `--keyword` unions the ids they list, `--probe`
//! (repeatable) picks ids directly, `--all` takes the whole universe. There is no
//! implicit "run everything" - a bare invocation is an error, so the slow
//! full-corpus pass is never triggered by accident.

use std::collections::BTreeSet;

use crate::error::DevError;
use crate::piners::registry::Registry;
use crate::piners::selector::{self, SelectionArgs, SelectionPolicy};

/// Flags that drive selection, lifted off the CLI command.
#[derive(Debug, Default)]
pub struct SelectArgs {
    /// `--keyword` (repeatable): union of the listed groupings.
    pub keywords: Vec<String>,
    /// `--probe <id>` (repeatable): the union of the listed pinned probes.
    pub probe: Vec<String>,
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
    let pending: BTreeSet<String> = registry.pending.keys().cloned().collect();
    selector::resolve_selection(
        &registry.pins,
        &registry.keywords,
        &pending,
        &SelectionArgs {
            keywords: &args.keywords,
            probes: &args.probe,
            all: args.all,
            whole_universe: args.verify_only,
        },
        &SelectionPolicy {
            label: "piners",
            registry_file: "pins.toml",
            whole_universe_named_error: Some(
                "piners: --verify-only checks the whole pinned universe and cannot be combined with \
                 --keyword or --probe.",
            ),
            pending_hint: Some("brokkr corpus --reseed --probe "),
        },
        |ids| crate::output::corpus_msg(&pending_notice(ids)),
    )
}

/// The one-line notice for registered-but-unpinned probes left out of a
/// selection or a verification.
pub fn pending_notice(ids: &[String]) -> String {
    format!(
        "{} registered but not pinned, skipped: {} (pin each with `brokkr \
         corpus --reseed --probe <id>`)",
        crate::output::count(ids.len(), "[pending] probe"),
        ids.join(", ")
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::registry::{FilePin, Pin};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn pin() -> Pin {
        Pin::new(
            FilePin {
                path: PathBuf::from("p.pine"),
                xxh128: "00".into(),
            },
            FilePin {
                path: PathBuf::from("p.csv"),
                xxh128: "11".into(),
            },
        )
    }

    fn registry() -> Registry {
        let mut pins = BTreeMap::new();
        pins.insert("a".to_owned(), pin());
        pins.insert("b".to_owned(), pin());
        pins.insert("c".to_owned(), pin());
        let mut keywords = BTreeMap::new();
        keywords.insert("x".to_owned(), vec!["a".to_owned(), "b".to_owned()]);
        keywords.insert("y".to_owned(), vec!["b".to_owned(), "c".to_owned()]);
        Registry {
            pins,
            keywords,
            ..Registry::default()
        }
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
            probe: vec!["c".to_owned()],
            ..Default::default()
        };
        assert_eq!(resolve(&registry(), &args).unwrap(), vec!["c"]);
    }

    #[test]
    fn multiple_probes_union_dedup_preserving_order() {
        let args = SelectArgs {
            probe: vec!["c".to_owned(), "a".to_owned(), "c".to_owned()],
            ..Default::default()
        };
        assert_eq!(resolve(&registry(), &args).unwrap(), vec!["c", "a"]);
    }

    #[test]
    fn probes_union_with_keywords_dedup() {
        let args = SelectArgs {
            keywords: vec!["x".to_owned()],
            probe: vec!["a".to_owned(), "c".to_owned()],
            ..Default::default()
        };
        // keyword x -> [a, b]; probe c is new, probe a already seen.
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
    fn a_pending_probe_is_skipped_by_a_keyword_and_refused_by_name() {
        let mut reg = registry();
        reg.pending.insert("d".to_owned(), PathBuf::from("p/d"));
        reg.keywords
            .insert("z".to_owned(), vec!["a".to_owned(), "d".to_owned()]);
        reg.keywords.insert("only".to_owned(), vec!["d".to_owned()]);
        let kw = |k: &str| SelectArgs {
            keywords: vec![k.to_owned()],
            ..Default::default()
        };
        assert_eq!(resolve(&reg, &kw("z")).unwrap(), vec!["a"]);
        let err = resolve(&reg, &kw("only")).unwrap_err();
        assert!(format!("{err:?}").contains("nothing runnable"));
        let by_name = SelectArgs {
            probe: vec!["d".to_owned()],
            ..Default::default()
        };
        assert!(format!("{:?}", resolve(&reg, &by_name).unwrap_err()).contains("[pending]"));
        let all = SelectArgs {
            all: true,
            ..Default::default()
        };
        assert_eq!(resolve(&reg, &all).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn no_selection_is_an_error() {
        assert!(resolve(&registry(), &SelectArgs::default()).is_err());
    }
}
