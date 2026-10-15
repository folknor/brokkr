//! The strict module walk behind the fixture inventory: which source files
//! the fixture's selected target compiles UNCONDITIONALLY.
//!
//! A `clean` fixture file claims "compiled, and produced nothing". Listing a
//! file proves neither half - `#[cfg(feature = "never")] mod probe;` keeps a
//! listed file out of the build entirely - so every listed file must be
//! reached from the target root through plain `mod` declarations. This is
//! deliberately NOT textlint's test-code classifier (`crate::test_code`):
//! that one over-approximates (an unknown cfg atom counts as possibly
//! present, an unreached file is classified as production), which is the
//! right direction for masking and the wrong one for an inclusion proof.
//!
//! The rules, each a refusal rather than a guess:
//!
//! - Traversal only through module scopes: a file root and inline `mod`
//!   bodies. A `mod` inside a function, impl, or macro input is not followed,
//!   so its file - if listed - fails as unreached.
//! - Any `cfg` or `cfg_attr` on a route node (an inner attribute of a reached
//!   file, an inline module, a `mod` declaration) is refused: the route is
//!   conditional, so reaching through it proves nothing.
//! - `include!` is never followed; an included file is listed-but-unreached.
//! - `#[path]` is followed only on an out-of-line `mod` at file scope, and is
//!   resolved against the declaring file's directory.
//! - A missing module file, or both `x.rs` and `x/mod.rs`, is refused.
//!
//! Item-level `cfg` inside a reached file (a `#[cfg(any())]` function holding
//! a sealed call) is outside the walk: a documented fixture constraint.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The canonical files reached, and every refusal met on the way.
#[derive(Debug, Default)]
pub(super) struct Walk {
    pub(super) reached: BTreeSet<PathBuf>,
    pub(super) refusals: Vec<String>,
    /// (file, module directory) pairs already visited. One file can be
    /// loaded twice with different child resolution (`mod a;` beside
    /// `#[path = "a.rs"] mod b;`), so the file alone is not the visit key.
    visited: BTreeSet<(PathBuf, PathBuf)>,
}

/// Walk from the target's root source file.
pub(super) fn walk(root: &Path) -> Walk {
    let mut out = Walk::default();
    // The crate root is a mod-rs file: its children resolve in its directory.
    let dir = root.parent().map(Path::to_path_buf).unwrap_or_default();
    visit_file(root, &dir, &mut out);
    out
}

fn is_conditional(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"))
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<Result<String, ()>> {
    let attr = attrs.iter().find(|a| a.path().is_ident("path"))?;
    let syn::Meta::NameValue(nv) = &attr.meta else {
        return Some(Err(()));
    };
    match &nv.value {
        syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => Some(Ok(s.value())),
        _ => Some(Err(())),
    }
}

/// Visit one file. `module_dir` is where its child modules resolve.
fn visit_file(file: &Path, module_dir: &Path, out: &mut Walk) {
    let canonical = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    if !out.visited.insert((canonical.clone(), module_dir.to_path_buf())) {
        return;
    }
    let source = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            out.refusals.push(format!("{}: unreadable: {e}", file.display()));
            return;
        }
    };
    let parsed = match syn::parse_file(&source) {
        Ok(f) => f,
        Err(e) => {
            out.refusals.push(format!("{}: does not parse: {e}", file.display()));
            return;
        }
    };
    if is_conditional(&parsed.attrs) {
        out.refusals.push(format!(
            "{}: an inner cfg/cfg_attr makes the whole file conditional",
            file.display()
        ));
        return;
    }
    // Reached only once its own inner attributes are known unconditional.
    out.reached.insert(canonical);
    let file_dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    visit_items(&parsed.items, file, module_dir, Some(&file_dir), out);
}

/// Visit a module scope's items. `path_base` is `Some` only at file scope,
/// where a `#[path]` resolves against the declaring file's directory.
fn visit_items(
    items: &[syn::Item],
    file: &Path,
    module_dir: &Path,
    path_base: Option<&Path>,
    out: &mut Walk,
) {
    for item in items {
        let syn::Item::Mod(m) = item else {
            continue;
        };
        let name = m.ident.to_string();
        if is_conditional(&m.attrs) {
            out.refusals.push(format!(
                "{}: `mod {name}` carries cfg/cfg_attr - a conditional route proves nothing",
                file.display()
            ));
            continue;
        }
        if let Some((_, items)) = &m.content {
            if path_attr(&m.attrs).is_some() {
                out.refusals.push(format!(
                    "{}: #[path] on inline `mod {name}` is not followed",
                    file.display()
                ));
                continue;
            }
            visit_items(items, file, &module_dir.join(&name), None, out);
            continue;
        }
        let target = match path_attr(&m.attrs) {
            Some(Ok(p)) => match path_base {
                // A #[path] file behaves as a mod-rs file: its own children
                // resolve in its directory.
                Some(base) => {
                    let f = base.join(p);
                    let dir = f.parent().map(Path::to_path_buf).unwrap_or_default();
                    Some((f, dir))
                }
                None => {
                    out.refusals.push(format!(
                        "{}: #[path] on `mod {name}` inside an inline module is not followed",
                        file.display()
                    ));
                    None
                }
            },
            Some(Err(())) => {
                out.refusals.push(format!(
                    "{}: `mod {name}` has a #[path] that is not a string literal",
                    file.display()
                ));
                None
            }
            None => resolve_module(file, module_dir, &name, out),
        };
        if let Some((f, dir)) = target {
            if f.is_file() {
                visit_file(&f, &dir, out);
            } else {
                out.refusals.push(format!(
                    "{}: `mod {name}` resolves to {}, which does not exist",
                    file.display(),
                    f.display()
                ));
            }
        }
    }
}

/// `name.rs` or `name/mod.rs` under `module_dir`; both, or neither, is a
/// refusal. Returns the file and the directory its children resolve in.
fn resolve_module(
    file: &Path,
    module_dir: &Path,
    name: &str,
    out: &mut Walk,
) -> Option<(PathBuf, PathBuf)> {
    let flat = module_dir.join(format!("{name}.rs"));
    let nested = module_dir.join(name).join("mod.rs");
    match (flat.is_file(), nested.is_file()) {
        (true, false) => Some((flat, module_dir.join(name))),
        (false, true) => Some((nested, module_dir.join(name))),
        (true, true) => {
            out.refusals.push(format!(
                "{}: `mod {name}` is ambiguous - both {} and {} exist",
                file.display(),
                flat.display(),
                nested.display()
            ));
            None
        }
        (false, false) => {
            out.refusals.push(format!(
                "{}: `mod {name}` has no file ({} or {})",
                file.display(),
                flat.display(),
                nested.display()
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = crate::test_scratch::scratch(module_path!(), name);
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    fn rel(dir: &Path, w: &Walk) -> Vec<String> {
        let dir = dir.canonicalize().unwrap();
        w.reached
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn plain_modules_are_reached_through_both_layouts_and_inline_scopes() {
        let dir = tree(
            "plain",
            &[
                ("src/lib.rs", "mod a;\nmod b;\nmod inline { mod deep; }\n"),
                ("src/a.rs", "mod child;\n"),
                ("src/a/child.rs", ""),
                ("src/b/mod.rs", ""),
                ("src/inline/deep.rs", ""),
            ],
        );
        let w = walk(&dir.join("src/lib.rs"));
        assert!(w.refusals.is_empty(), "{:?}", w.refusals);
        assert_eq!(
            rel(&dir, &w),
            // Path order is per component: `src/a/...` sorts before `src/a.rs`.
            ["src/a/child.rs", "src/a.rs", "src/b/mod.rs", "src/inline/deep.rs", "src/lib.rs"]
        );
    }

    // The counterexamples the inclusion proof exists for: a conditional
    // route, a conditional ancestor, an inner cfg, and routes the compiler
    // may never take (a mod in a fn body, an include! in a macro).
    #[test]
    fn conditional_or_speculative_routes_never_reach_their_file() {
        let dir = tree(
            "conditional",
            &[
                (
                    "src/lib.rs",
                    "#[cfg(feature = \"never\")]\nmod gated;\n\
                     #[cfg(any())]\nmod outer { mod inner; }\n\
                     fn f() { mod hidden; }\n\
                     macro_rules! m { () => { include!(\"inc.rs\"); }; }\n\
                     mod inner_cfg;\n",
                ),
                ("src/gated.rs", ""),
                ("src/outer/inner.rs", ""),
                ("src/hidden.rs", ""),
                ("src/inc.rs", ""),
                ("src/inner_cfg.rs", "#![cfg(test)]\n"),
            ],
        );
        let w = walk(&dir.join("src/lib.rs"));
        assert_eq!(rel(&dir, &w), ["src/lib.rs"]);
        assert_eq!(w.refusals.len(), 3, "{:?}", w.refusals);
    }

    #[test]
    fn ambiguous_and_missing_module_files_are_refused() {
        let dir = tree(
            "ambiguous",
            &[
                ("src/lib.rs", "mod both;\nmod none;\n"),
                ("src/both.rs", ""),
                ("src/both/mod.rs", ""),
            ],
        );
        let w = walk(&dir.join("src/lib.rs"));
        assert_eq!(w.refusals.len(), 2, "{:?}", w.refusals);
        assert!(w.refusals[0].contains("ambiguous") && w.refusals[1].contains("no file"));
    }

    // One file loaded twice resolves its children twice, in two places.
    #[test]
    fn a_file_loaded_twice_resolves_its_children_in_each_context() {
        let dir = tree(
            "twice",
            &[
                ("src/lib.rs", "mod a;\n#[path = \"a.rs\"]\nmod b;\n"),
                ("src/a.rs", "mod child;\n"),
                ("src/a/child.rs", ""),
                ("src/child.rs", ""),
            ],
        );
        let w = walk(&dir.join("src/lib.rs"));
        assert!(w.refusals.is_empty(), "{:?}", w.refusals);
        assert_eq!(rel(&dir, &w), ["src/a/child.rs", "src/a.rs", "src/child.rs", "src/lib.rs"]);
    }

    #[test]
    fn a_file_scope_path_attribute_is_followed() {
        let dir = tree(
            "path_attr",
            &[("src/lib.rs", "#[path = \"elsewhere/x.rs\"]\nmod x;\n"), ("src/elsewhere/x.rs", "")],
        );
        let w = walk(&dir.join("src/lib.rs"));
        assert!(w.refusals.is_empty(), "{:?}", w.refusals);
        assert_eq!(rel(&dir, &w), ["src/elsewhere/x.rs", "src/lib.rs"]);
    }
}
