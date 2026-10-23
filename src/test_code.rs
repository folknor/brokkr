//! Which bytes of a workspace's Rust source are test code: the classifier
//! behind `[[textlint]] test_code`.
//!
//! Two selections come out of it. The skip mask is the claim below. The test
//! selection is the skip mask less what every occurrence proves absent from
//! test builds too (dead code), so `test_code = "only"` sees code that exists
//! for tests and that some test build may contain.
//!
//! The claim is scoped to brokkr's *unexpanded source model*. A byte is
//! test-only when every occurrence of it the model can see is absent from
//! every non-test build:
//!
//! - **Reachability** starts at every workspace target's root from
//!   `cargo metadata` (integration-test targets are test roots, every other
//!   kind a production root) and follows `mod` declarations with rustc's
//!   directory-ownership rules, `#[path]` (plain and under `cfg_attr`) and
//!   literal `include!`s, including ones inside macro input. A file is
//!   test-only when it is reached, and only through test contexts. A file
//!   reached in several contexts keeps a byte masked only when every
//!   production occurrence masks it.
//! - **Gating** is a proof, not a guess: a `cfg` predicate evaluated in
//!   strong Kleene logic with `test` False and every other atom Unknown
//!   (`cfg::absent_without_test`). `cfg(any(test, feature = "x"))` stays
//!   production.
//! - **Spans** come from `syn`, so an attribute on a generic parameter masks
//!   that parameter, never the function body after it.
//!
//! Macro expansion and generated source are outside the model, for edges and
//! masks alike: a derive, an attribute macro or `include!(concat!(env!(
//! "OUT_DIR"), ...))` may create or rewrite code no source scan can see.
//! What the model cannot represent in a production context - a computed
//! `include!` other than the `OUT_DIR` form, a syntax node syn keeps
//! verbatim, an inline module with a `cfg_attr` path, a module with both
//! `x.rs` and `x/mod.rs`, a file that does not parse - refuses the whole
//! classification rather than guessing. A file the walk calls test-only whose
//! name production macro input spells is handed back to production (the
//! tripwire); that is detection, not proof. A file nothing reaches is scanned
//! as production, with only its own `cfg` masks: bare `#[test]` is not
//! honored there, since its crate root is out of sight.

mod cfg;
mod facts;
mod graph;

pub(crate) use graph::{Classification, FileMask, classify, workspace_roots};
