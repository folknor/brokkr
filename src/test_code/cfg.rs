//! Proof that a `cfg` predicate is false in every non-test build, or in
//! every test build.
//!
//! The predicate is evaluated in strong Kleene logic with the bare `test`
//! atom fixed (False for non-test builds, True for test builds) and every
//! other atom Unknown. A result of False holds under every assignment of the
//! unknown atoms, so it is a proof of absence. It is not a decision
//! procedure: `all(feature = "x", not(feature = "x"))` is impossible but
//! evaluates Unknown, which keeps the node in play. Anything that does not
//! parse as a well-formed predicate proves nothing.

use syn::ext::IdentExt;
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::{Attribute, Ident, Lit, LitBool, Meta, Token};

/// Whether `path` is the single identifier `name`. Rust drops the `r#` of a
/// raw identifier, so `r#path` and `r#test` name what `path` and `test` do;
/// `Path::is_ident` compares the spelling and would miss them.
pub(super) fn is_named(path: &syn::Path, name: &str) -> bool {
    path.leading_colon.is_none()
        && path.segments.len() == 1
        && path.segments[0].arguments.is_none()
        && path.segments[0].ident.unraw() == name
}

/// Strong Kleene truth value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }
}

/// One parsed `cfg` predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pred {
    /// The bare `test` atom.
    Test,
    /// `true` / `false` (cfg boolean literals).
    Bool(bool),
    /// Any other atom or key-value pair, `test = "x"` included.
    Other,
    Any(Vec<Pred>),
    All(Vec<Pred>),
    Not(Box<Pred>),
}

impl Pred {
    /// Truth with the bare `test` atom fixed to `test`.
    fn eval(&self, test: bool) -> Truth {
        match self {
            Self::Test if test => Truth::True,
            Self::Test => Truth::False,
            Self::Bool(true) => Truth::True,
            Self::Bool(false) => Truth::False,
            Self::Other => Truth::Unknown,
            Self::Any(preds) => {
                let values: Vec<Truth> = preds.iter().map(|p| p.eval(test)).collect();
                if values.contains(&Truth::True) {
                    Truth::True
                } else if values.iter().all(|v| *v == Truth::False) {
                    Truth::False
                } else {
                    Truth::Unknown
                }
            }
            Self::All(preds) => {
                let values: Vec<Truth> = preds.iter().map(|p| p.eval(test)).collect();
                if values.contains(&Truth::False) {
                    Truth::False
                } else if values.iter().all(|v| *v == Truth::True) {
                    Truth::True
                } else {
                    Truth::Unknown
                }
            }
            Self::Not(pred) => pred.eval(test).not(),
        }
    }
}

fn parse_pred(input: ParseStream<'_>) -> syn::Result<Pred> {
    if input.peek(LitBool) {
        let lit: LitBool = input.parse()?;
        return Ok(Pred::Bool(lit.value));
    }
    let ident = Ident::parse_any(input)?;
    if input.peek(syn::token::Paren) {
        let content;
        syn::parenthesized!(content in input);
        let args = Punctuated::<Pred, Token![,]>::parse_terminated_with(&content, parse_pred)?;
        let args: Vec<Pred> = args.into_iter().collect();
        return match ident.unraw().to_string().as_str() {
            "any" => Ok(Pred::Any(args)),
            "all" => Ok(Pred::All(args)),
            "not" if args.len() == 1 => Ok(Pred::Not(Box::new(
                args.into_iter().next().unwrap_or(Pred::Other),
            ))),
            _ => Err(syn::Error::new(ident.span(), "not a cfg predicate")),
        };
    }
    if input.peek(Token![=]) {
        input.parse::<Token![=]>()?;
        // A cfg value is a string literal; anything else is malformed and
        // must prove nothing.
        let _value: syn::LitStr = input.parse()?;
        return Ok(Pred::Other);
    }
    Ok(if ident.unraw() == "test" {
        Pred::Test
    } else {
        Pred::Other
    })
}

fn parse_whole(input: ParseStream<'_>) -> syn::Result<Pred> {
    let pred = parse_pred(input)?;
    // A single trailing comma is legal in attribute arguments.
    if input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
    }
    if !input.is_empty() {
        return Err(input.error("trailing tokens after cfg predicate"));
    }
    Ok(pred)
}

/// The predicate of a `cfg(...)` meta, or `None` for a meta that is not
/// `cfg` or does not parse as a predicate.
fn cfg_pred(meta: &Meta) -> Option<Pred> {
    if !is_named(meta.path(), "cfg") {
        return None;
    }
    let Meta::List(list) = meta else {
        return None;
    };
    parse_whole.parse2(list.tokens.clone()).ok()
}

/// Whether the attributes prove the node absent from every non-test build:
/// several `cfg` attributes conjoin, so one False suffices.
pub(super) fn absent_without_test(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| cfg_pred(&a.meta).is_some_and(|p| p.eval(false) == Truth::False))
}

/// Whether the attributes prove the node absent from every test build. Unlike
/// [`absent_without_test`] this also reads the `cfg`s a `cfg_attr` introduces:
/// `cfg_attr(G, cfg(P))` removes the node whenever `G` holds and `P` does not,
/// so it is a proof when `all(G, not(P))` is True. Used only to select test
/// code, never to widen what production scanning skips.
pub(super) fn absent_with_test(attrs: &[Attribute]) -> bool {
    let direct = attrs
        .iter()
        .any(|a| cfg_pred(&a.meta).is_some_and(|p| p.eval(true) == Truth::False));
    direct
        || conditional_metas(attrs).into_iter().any(|(meta, guards)| {
            cfg_pred(&meta).is_some_and(|p| {
                let mut all = guards;
                all.push(Pred::Not(Box::new(p)));
                Pred::All(all).eval(true) == Truth::True
            })
        })
}

/// One `path = "..."` alternative a `cfg_attr` may select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConditionalPath {
    pub(super) path: String,
    /// The alternative is only ever selected in test builds.
    pub(super) test_only: bool,
    /// The alternative is never selected in a test build.
    pub(super) never_in_test: bool,
    /// The alternative is selected in every test build, so the declaration's
    /// default path never is.
    pub(super) always_in_test: bool,
}

/// Every attribute a (possibly nested) `cfg_attr` may introduce, with the
/// guards that must all hold for it to apply. A guard that fails to parse
/// but leaves the input at its separating comma (`sanitize("x")`, say) is
/// kept as an unknown guard. One whose failure stops before that comma
/// (`a::b`, `feature = 7`) drops the whole `cfg_attr`; rustc rejects every
/// such guard, so no compiling source loses an attribute that way.
fn conditional_metas(attrs: &[Attribute]) -> Vec<(Meta, Vec<Pred>)> {
    let mut out = Vec::new();
    for attr in attrs {
        if is_named(attr.path(), "cfg_attr")
            && let Meta::List(list) = &attr.meta
        {
            collect_cfg_attr(list.tokens.clone(), &[], &mut out);
        }
    }
    out
}

fn collect_cfg_attr(
    tokens: proc_macro2::TokenStream,
    outer: &[Pred],
    out: &mut Vec<(Meta, Vec<Pred>)>,
) {
    let parsed = (|input: ParseStream<'_>| {
        let guard = parse_pred(input).unwrap_or(Pred::Other);
        input.parse::<Token![,]>()?;
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        Ok((guard, metas))
    })
    .parse2(tokens);
    let Ok((guard, metas)) = parsed else {
        return;
    };
    let mut guards = outer.to_vec();
    guards.push(guard);
    for meta in metas {
        match meta {
            Meta::List(list) if is_named(&list.path, "cfg_attr") => {
                collect_cfg_attr(list.tokens, &guards, out);
            }
            other => out.push((other, guards.clone())),
        }
    }
}

/// Every `path = "..."` reachable through (possibly nested) `cfg_attr`s.
/// An alternative whose guards prove False without `test` is test-only; one
/// whose guards prove False with `test` is never selected in a test build.
pub(super) fn conditional_paths(attrs: &[Attribute]) -> Vec<ConditionalPath> {
    conditional_metas(attrs)
        .into_iter()
        .filter_map(|(meta, guards)| match meta {
            Meta::NameValue(nv) if is_named(&nv.path, "path") => match nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: Lit::Str(s), ..
                }) => {
                    let all = Pred::All(guards);
                    Some(ConditionalPath {
                        path: s.value(),
                        test_only: all.eval(false) == Truth::False,
                        never_in_test: all.eval(true) == Truth::False,
                        always_in_test: all.eval(true) == Truth::True,
                    })
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Every `macro_use` attribute that may apply, directly or through any
/// `cfg_attr`. Guards are ignored: a shadowing import active in any build
/// is enough to distrust a bare `#[test]`.
pub(super) fn macro_use_metas(attrs: &[Attribute]) -> Vec<Meta> {
    attrs
        .iter()
        .map(|a| a.meta.clone())
        .chain(conditional_metas(attrs).into_iter().map(|(m, _)| m))
        .filter(|m| is_named(m.path(), "macro_use"))
        .collect()
}

/// The value of a plain `#[path = "..."]` attribute, if present.
pub(super) fn path_attr(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| match &attr.meta {
        Meta::NameValue(nv) if is_named(&nv.path, "path") => match &nv.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: Lit::Str(s), ..
            }) => Some(s.value()),
            _ => None,
        },
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn attrs(src: &str) -> Vec<Attribute> {
        let item: syn::ItemFn = syn::parse_str(&format!("{src} fn f() {{}}")).unwrap();
        item.attrs
    }

    #[test]
    fn bare_test_is_absent() {
        assert!(absent_without_test(&attrs("#[cfg(test)]")));
    }

    #[test]
    fn raw_identifiers_name_what_their_plain_spelling_does() {
        assert!(absent_without_test(&attrs("#[r#cfg(r#all(r#test))]")));
        assert_eq!(
            path_attr(&attrs("#[r#path = \"shared.rs\"]")),
            Some("shared.rs".into())
        );
    }

    #[test]
    fn any_with_a_feature_is_present() {
        assert!(!absent_without_test(&attrs(
            "#[cfg(any(test, feature = \"test-support\"))]"
        )));
    }

    #[test]
    fn all_with_test_is_absent() {
        assert!(absent_without_test(&attrs(
            "#[cfg(all(test, feature = \"x\"))]"
        )));
    }

    #[test]
    fn double_negation_is_absent_and_single_is_present() {
        assert!(absent_without_test(&attrs("#[cfg(not(not(test)))]")));
        assert!(!absent_without_test(&attrs("#[cfg(not(test))]")));
    }

    #[test]
    fn key_value_test_is_not_the_test_atom() {
        assert!(!absent_without_test(&attrs("#[cfg(test = \"x\")]")));
    }

    #[test]
    fn separate_cfg_attributes_conjoin() {
        assert!(absent_without_test(&attrs("#[cfg(unix)] #[cfg(test)]")));
    }

    #[test]
    fn empty_any_is_dead_and_empty_all_is_live() {
        assert!(absent_without_test(&attrs("#[cfg(any())]")));
        assert!(!absent_without_test(&attrs("#[cfg(all())]")));
    }

    #[test]
    fn malformed_predicate_proves_nothing() {
        assert!(!absent_without_test(&attrs("#[cfg(not(test, unix))]")));
        assert!(!absent_without_test(&attrs("#[cfg(test unix)]")));
        assert!(!absent_without_test(&attrs(
            "#[cfg(all(test, feature = 7))]"
        )));
    }

    #[test]
    fn macro_use_is_found_directly_and_under_cfg_attr() {
        let found = macro_use_metas(&attrs(
            "#[macro_use] #[cfg_attr(feature = \"hooks\", macro_use(test))] #[cfg_attr(unix, cfg_attr(test, macro_use))] #[allow(dead_code)]",
        ));
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn cfg_attr_paths_carry_their_guard() {
        let paths = conditional_paths(&attrs(
            "#[cfg_attr(test, path = \"a.rs\")] #[cfg_attr(unix, cfg_attr(test, path = \"b.rs\"), path = \"c.rs\")] #[cfg_attr(not(test), path = \"d.rs\")]",
        ));
        assert_eq!(
            paths,
            vec![
                ConditionalPath {
                    path: "a.rs".into(),
                    test_only: true,
                    never_in_test: false,
                    always_in_test: true,
                },
                ConditionalPath {
                    path: "b.rs".into(),
                    test_only: true,
                    never_in_test: false,
                    always_in_test: false,
                },
                ConditionalPath {
                    path: "c.rs".into(),
                    test_only: false,
                    never_in_test: false,
                    always_in_test: false,
                },
                ConditionalPath {
                    path: "d.rs".into(),
                    test_only: false,
                    never_in_test: true,
                    always_in_test: false,
                },
            ]
        );
    }

    #[test]
    fn absence_from_test_builds_reads_cfg_and_cfg_attr() {
        assert!(absent_with_test(&attrs("#[cfg(not(test))]")));
        assert!(absent_with_test(&attrs("#[cfg(any())]")));
        assert!(!absent_with_test(&attrs("#[cfg(test)]")));
        assert!(!absent_with_test(&attrs("#[cfg(feature = \"x\")]")));
        // cfg_attr(G, cfg(P)) removes the node when G holds and P does not.
        assert!(absent_with_test(&attrs("#[cfg_attr(test, cfg(not(test)))]")));
        assert!(absent_with_test(&attrs(
            "#[cfg_attr(all(), cfg_attr(test, cfg(any())))]"
        )));
        // An unknown guard proves nothing.
        assert!(!absent_with_test(&attrs("#[cfg_attr(unix, cfg(not(test)))]")));
        // Correlations between atoms are not tracked: this pair is
        // impossible, yet neither attribute alone is False.
        assert!(!absent_with_test(&attrs(
            "#[cfg(feature = \"x\")] #[cfg(not(feature = \"x\"))]"
        )));
    }
}
