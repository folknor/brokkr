//! What one file says under one resolution context: the module and include
//! edges it declares, the byte spans its attributes prove absent from every
//! non-test build, the constructs that would put the classification outside
//! its source model, and the evidence the tripwire and the bare-`#[test]`
//! rule need.

use std::ops::Range;
use std::path::{Path, PathBuf};

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ImplItem, Item, ItemMod, Meta, Stmt, TraitItem};

use super::cfg;

/// Where a file's module declarations resolve, after rustc's directory
/// ownership: a directory, plus the pending stem a non-mod-rs file (`a.rs`
/// loaded by `mod a;`) pushes before its ordinary children (`a/child.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Entry {
    pub(super) dir: PathBuf,
    pub(super) stem: Option<String>,
}

impl Entry {
    /// A file that owns its directory: a crate root, a `mod.rs`, a
    /// `#[path]` target or an `include!` splice. Children resolve beside it.
    pub(super) fn owning(file: &Path) -> Self {
        Self {
            dir: file.parent().map(Path::to_path_buf).unwrap_or_default(),
            stem: None,
        }
    }
}

/// The file an edge names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    /// A `#[path]` target or a literal `include!`; the file owns its directory.
    Explicit(PathBuf),
    /// An ordinary `mod name;`: `dir/name.rs` or `dir/name/mod.rs`.
    Ordinary { dir: PathBuf, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Edge {
    pub(super) target: Target,
    /// Only test builds follow this edge.
    pub(super) test: bool,
    /// An `include!` splice rather than a module: parsed as items or as one
    /// expression.
    pub(super) splice: bool,
    /// Declared inside a bare-`#[test]` function: test-only exactly when the
    /// graph decides nothing shadows the builtin attribute.
    pub(super) bare: bool,
}

/// Production macro input that names a file rather than an identifier: the
/// tripwire's only evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Evidence {
    /// A string literal that is path-shaped: contains `/` or ends in `.rs`.
    PathLiteral { value: String, at: String },
    /// The identifier directly after `mod` in one token group.
    ModName { name: String, at: String },
}

#[derive(Debug, Default)]
pub(super) struct Facts {
    pub(super) edges: Vec<Edge>,
    /// Byte spans (file coordinates) proven absent without `test`, outside
    /// spans already inherited as test.
    pub(super) masks: Vec<Range<usize>>,
    /// Spans of free functions carrying a bare `#[test]`. Absent without
    /// `test` only if nothing can shadow the builtin attribute, which the
    /// graph decides across every reachable source.
    pub(super) bare_tests: Vec<Range<usize>>,
    /// Sites that could make a bare `#[test]` name something other than the
    /// builtin: a `use` binding `test`, a `#[macro_use] extern crate` that
    /// does not exclude it.
    pub(super) test_shadows: Vec<String>,
    /// Production-context constructs outside the source model.
    pub(super) refusals: Vec<String>,
    /// The same, inside a bare-`#[test]` function: they stand only when the
    /// graph cannot honor the attribute.
    pub(super) bare_refusals: Vec<String>,
    pub(super) evidence: Vec<Evidence>,
    /// Tripwire evidence inside bare-`#[test]` functions.
    pub(super) bare_evidence: Vec<Evidence>,
}

/// Bytes `syn::parse_file` would strip before parsing: a BOM, then a shebang
/// line that is not an inner attribute.
fn origin(src: &str) -> usize {
    let mut at = if src.starts_with('\u{feff}') { 3 } else { 0 };
    let rest = &src[at..];
    if rest.starts_with("#!") && !rest[2..].trim_start().starts_with('[') {
        at += rest.find('\n').unwrap_or(rest.len());
    }
    at
}

/// Analyze `src` under `entry`. `test` is the inherited status: a file only
/// reached as test contributes test edges and no masks or refusals of its own.
/// An `Err` is a parse failure, described with its position.
pub(super) fn analyze(src: &str, entry: &Entry, test: bool, splice: bool) -> Result<Facts, String> {
    let origin = origin(src);
    let body = &src[origin..];
    let mut collector = Collector {
        origin,
        test,
        file_dir: entry.dir.clone(),
        dir: entry.dir.clone(),
        stem: entry.stem.clone(),
        bare: false,
        facts: Facts::default(),
    };
    let result = match syn::parse_str::<syn::File>(body) {
        Ok(file) => {
            collector.file(&file, src.len());
            Ok(())
        }
        Err(file_error) => match splice.then(|| syn::parse_str::<Expr>(body)) {
            Some(Ok(expr)) => {
                collector.visit_expr(&expr);
                Ok(())
            }
            _ => {
                let at = file_error.span().start();
                Err(format!("{}:{}: {file_error}", at.line, at.column + 1))
            }
        },
    };
    // The fallback source map grows with every parse on this thread; every
    // span above has been resolved to bytes already.
    proc_macro2::extra::invalidate_current_thread_spans();
    result.map(|()| collector.facts)
}

struct Collector {
    origin: usize,
    test: bool,
    /// The analyzed file's own directory: where `include!` resolves.
    file_dir: PathBuf,
    /// The current module directory (inline modules extend it).
    dir: PathBuf,
    /// The non-mod-rs stem still pending at this point.
    stem: Option<String>,
    /// Inside a bare-`#[test]` function reached in production context.
    bare: bool,
    facts: Facts,
}

fn position(span: Span) -> String {
    let at = span.start();
    format!("{}:{}", at.line, at.column + 1)
}

impl Collector {
    fn bytes(&self, span: Span) -> Range<usize> {
        let r = span.byte_range();
        r.start + self.origin..r.end + self.origin
    }

    fn refuse(&mut self, span: Span, what: &str) {
        if self.test {
            return;
        }
        let site = format!("{}: {what}", position(span));
        if self.bare {
            self.facts.bare_refusals.push(site);
        } else {
            self.facts.refusals.push(site);
        }
    }

    fn edge(&mut self, target: Target, test: bool, splice: bool) {
        self.facts.edges.push(Edge {
            target,
            test,
            splice,
            bare: self.bare,
        });
    }

    fn file(&mut self, file: &syn::File, len: usize) {
        if cfg::absent_without_test(&file.attrs) && !self.test {
            self.facts.masks.push(self.origin..len);
            self.test = true;
        }
        for item in &file.items {
            self.visit_item(item);
        }
    }

    /// Run `f` over a node whose `attrs` may prove it test-only, masking its
    /// whole span (attributes included) when they do.
    fn attributed(&mut self, attrs: &[Attribute], span: Span, f: impl FnOnce(&mut Self)) {
        let absent = cfg::absent_without_test(attrs);
        if absent && !self.test {
            let bytes = self.bytes(span);
            self.facts.masks.push(bytes);
        }
        let saved = self.test;
        self.test |= absent;
        f(self);
        self.test = saved;
    }

    /// `dir`, extended by the pending non-mod-rs stem.
    fn ordinary_dir(&self) -> PathBuf {
        match &self.stem {
            Some(stem) => self.dir.join(stem),
            None => self.dir.clone(),
        }
    }

    fn module(&mut self, m: &ItemMod) {
        let span = m.span();
        self.attributed(&m.attrs, span, |s| {
            let name = m.ident.unraw().to_string();
            let path = cfg::path_attr(&m.attrs);
            let conditional = cfg::conditional_paths(&m.attrs);
            if let Some((_, items)) = &m.content {
                if !conditional.is_empty() {
                    s.refuse(span, "an inline module with a cfg_attr path is outside the source model");
                }
                // An explicit path replaces the pending stem; an ordinary
                // inline module consumes it and nests its own name.
                let child = path.map_or_else(|| s.ordinary_dir().join(&name), |p| s.dir.join(p));
                let saved_dir = std::mem::replace(&mut s.dir, child);
                let saved_stem = s.stem.take();
                for item in items {
                    s.visit_item(item);
                }
                s.dir = saved_dir;
                s.stem = saved_stem;
                return;
            }
            let target = match path {
                Some(p) => Target::Explicit(s.dir.join(p)),
                None => Target::Ordinary {
                    dir: s.ordinary_dir(),
                    name,
                },
            };
            s.edge(target, s.test, false);
            for alt in conditional {
                s.edge(Target::Explicit(s.dir.join(alt.path)), s.test || alt.test_only, false);
            }
        });
    }

    /// An `include!` argument: a string literal is an edge, exactly
    /// `concat!(env!("OUT_DIR"), "...", ...)` is generated source outside the
    /// model by contract, anything else in a production context is refused.
    fn include(&mut self, tokens: &TokenStream, span: Span) {
        let literal = syn::parse::Parser::parse2(
            |input: syn::parse::ParseStream<'_>| {
                let lit: syn::LitStr = input.parse()?;
                let _: Option<syn::Token![,]> = input.parse()?;
                Ok(lit)
            },
            tokens.clone(),
        );
        match literal {
            Ok(lit) => self.edge(Target::Explicit(self.file_dir.join(lit.value())), self.test, true),
            Err(_) if is_out_dir_concat(tokens) => {}
            Err(_) => self.refuse(span, "include! with a non-literal path is outside the source model"),
        }
    }

    /// Macro input is opaque: nothing inside it is masked, but literal
    /// `include!`s anywhere in it are followed, and in production context the
    /// spellings that name a file feed the tripwire.
    fn macro_tokens(&mut self, tokens: TokenStream) {
        self.macro_tokens_in(tokens, false);
    }

    /// The token walk behind [`Self::macro_tokens`]. `in_use` is set inside
    /// a `use` tree (up to its `;`), carried into nested groups so
    /// `use a::{b, test};` is seen.
    fn macro_tokens_in(&mut self, tokens: TokenStream, mut in_use: bool) {
        let trees: Vec<TokenTree> = tokens.into_iter().collect();
        // The argument group of an `include!` already handled as an edge (or
        // refusal): its literal is not further evidence.
        let mut consumed: Option<usize> = None;
        for (i, tree) in trees.iter().enumerate() {
            // Shadowing imports hidden in macro input (`cfg_if!` and the
            // like). Conservative: a false positive only stops bare
            // `#[test]` from being honored, the loud direction.
            match tree {
                TokenTree::Ident(ident) if ident == "use" => in_use = true,
                TokenTree::Punct(p) if p.as_char() == ';' => in_use = false,
                TokenTree::Ident(ident) if ident == "as" => {
                    if let Some(TokenTree::Ident(name)) = trees.get(i + 1)
                        && name.unraw() == "test"
                    {
                        self.macro_shadow(name.span(), "renames to test in macro input");
                    }
                }
                TokenTree::Ident(ident)
                    if in_use
                        && ident.unraw() == "test"
                        && match trees.get(i + 1) {
                            None => true,
                            Some(TokenTree::Punct(p)) => matches!(p.as_char(), ';' | ','),
                            Some(_) => false,
                        }
                        && !matches!(trees.get(i.wrapping_sub(1)), Some(TokenTree::Ident(a)) if a == "as") =>
                {
                    self.macro_shadow(ident.span(), "use binds test in macro input");
                }
                _ => {}
            }
            match tree {
                TokenTree::Ident(ident) => match (trees.get(i + 1), trees.get(i + 2)) {
                    (Some(TokenTree::Punct(bang)), Some(TokenTree::Group(group)))
                        if ident.unraw() == "include" && bang.as_char() == '!' =>
                    {
                        self.include(&group.stream(), ident.span());
                        consumed = Some(i + 2);
                    }
                    (Some(TokenTree::Ident(name)), _) if ident == "mod" => {
                        self.evidence(Evidence::ModName {
                            name: name.unraw().to_string(),
                            at: position(name.span()),
                        });
                    }
                    _ => {}
                },
                TokenTree::Literal(lit) => {
                    if let Ok(s) = syn::parse2::<syn::LitStr>(TokenTree::Literal(lit.clone()).into()) {
                        let value = s.value();
                        if value.contains('/') || value.ends_with(".rs") {
                            self.evidence(Evidence::PathLiteral {
                                value,
                                at: position(lit.span()),
                            });
                        }
                    }
                }
                TokenTree::Group(group) if consumed != Some(i) => self.macro_tokens_in(group.stream(), in_use),
                TokenTree::Group(_) | TokenTree::Punct(_) => {}
            }
        }
    }

    /// Record tripwire evidence from production macro input. Inside a
    /// bare-`#[test]` function it is kept apart: it stands only when the
    /// graph cannot honor the attribute.
    fn evidence(&mut self, evidence: Evidence) {
        if self.test {
            return;
        }
        if self.bare {
            self.facts.bare_evidence.push(evidence);
        } else {
            self.facts.evidence.push(evidence);
        }
    }

    /// A shadowing site spelled in macro input. Like evidence, only
    /// production context counts.
    fn macro_shadow(&mut self, span: Span, what: &str) {
        if self.test {
            return;
        }
        self.facts.test_shadows.push(format!("{}: {what}", position(span)));
    }

    fn function(&mut self, item: &Item, f: &syn::ItemFn) {
        let bare = !self.test && is_bare_test(&f.attrs) && !cfg::absent_without_test(&f.attrs);
        if bare {
            let bytes = self.bytes(item.span());
            self.facts.bare_tests.push(bytes);
        }
        let saved = self.bare;
        self.bare |= bare;
        self.attributed(&f.attrs, item.span(), |s| visit::visit_item(s, item));
        self.bare = saved;
    }

    fn use_tree(&mut self, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Path(p) => self.use_tree(&p.tree),
            syn::UseTree::Name(n) if n.ident.unraw() == "test" => {
                self.facts
                    .test_shadows
                    .push(format!("{}: use binds test", position(n.ident.span())));
            }
            syn::UseTree::Rename(r) if r.rename.unraw() == "test" => {
                self.facts
                    .test_shadows
                    .push(format!("{}: use renames to test", position(r.rename.span())));
            }
            syn::UseTree::Group(g) => g.items.iter().for_each(|t| self.use_tree(t)),
            // A glob is not recorded: a glob-imported attribute named `test`
            // colliding with the prelude builtin is an ambiguity error
            // (E0659), so it cannot make a compiling bare `#[test]` name
            // something else.
            _ => {}
        }
    }

    fn extern_crate(&mut self, e: &syn::ItemExternCrate) {
        for meta in cfg::macro_use_metas(&e.attrs) {
            let shadows = match &meta {
                Meta::List(list) => list
                    .tokens
                    .clone()
                    .into_iter()
                    .any(|t| matches!(t, TokenTree::Ident(i) if i.unraw() == "test")),
                _ => true,
            };
            if shadows {
                self.facts.test_shadows.push(format!(
                    "{}: #[macro_use] extern crate {} may import an attribute named test",
                    position(e.span()),
                    e.ident
                ));
            }
        }
    }
}

/// `#[test]` exactly: single segment, no arguments.
fn is_bare_test(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| matches!(&a.meta, Meta::Path(p) if cfg::is_named(p, "test")))
}

/// Whether `tokens` is exactly `concat!(env!("OUT_DIR"), "lit", ...)`.
fn is_out_dir_concat(tokens: &TokenStream) -> bool {
    let Ok(Expr::Macro(concat)) = syn::parse2::<Expr>(tokens.clone()) else {
        return false;
    };
    if !cfg::is_named(&concat.mac.path, "concat") {
        return false;
    }
    let Ok(args) = concat
        .mac
        .parse_body_with(syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated)
    else {
        return false;
    };
    let mut args = args.iter();
    let env_out_dir = matches!(args.next(), Some(Expr::Macro(env))
        if cfg::is_named(&env.mac.path, "env")
            && env.mac.parse_body::<syn::LitStr>().is_ok_and(|l| l.value() == "OUT_DIR"));
    env_out_dir
        && args.all(|a| {
            matches!(
                a,
                Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(_),
                    ..
                })
            )
        })
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(i) => &i.attrs,
        ImplItem::Fn(i) => &i.attrs,
        ImplItem::Type(i) => &i.attrs,
        ImplItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(i) => &i.attrs,
        TraitItem::Fn(i) => &i.attrs,
        TraitItem::Type(i) => &i.attrs,
        TraitItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn foreign_item_attrs(item: &syn::ForeignItem) -> &[Attribute] {
    match item {
        syn::ForeignItem::Fn(i) => &i.attrs,
        syn::ForeignItem::Static(i) => &i.attrs,
        syn::ForeignItem::Type(i) => &i.attrs,
        syn::ForeignItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn expr_attrs(expr: &Expr) -> &[Attribute] {
    match expr {
        Expr::Array(e) => &e.attrs,
        Expr::Assign(e) => &e.attrs,
        Expr::Async(e) => &e.attrs,
        Expr::Await(e) => &e.attrs,
        Expr::Binary(e) => &e.attrs,
        Expr::Block(e) => &e.attrs,
        Expr::Break(e) => &e.attrs,
        Expr::Call(e) => &e.attrs,
        Expr::Cast(e) => &e.attrs,
        Expr::Closure(e) => &e.attrs,
        Expr::Const(e) => &e.attrs,
        Expr::Continue(e) => &e.attrs,
        Expr::Field(e) => &e.attrs,
        Expr::ForLoop(e) => &e.attrs,
        Expr::Group(e) => &e.attrs,
        Expr::If(e) => &e.attrs,
        Expr::Index(e) => &e.attrs,
        Expr::Infer(e) => &e.attrs,
        Expr::Let(e) => &e.attrs,
        Expr::Lit(e) => &e.attrs,
        Expr::Loop(e) => &e.attrs,
        Expr::Macro(e) => &e.attrs,
        Expr::Match(e) => &e.attrs,
        Expr::MethodCall(e) => &e.attrs,
        Expr::Paren(e) => &e.attrs,
        Expr::Path(e) => &e.attrs,
        Expr::Range(e) => &e.attrs,
        Expr::RawAddr(e) => &e.attrs,
        Expr::Reference(e) => &e.attrs,
        Expr::Repeat(e) => &e.attrs,
        Expr::Return(e) => &e.attrs,
        Expr::Struct(e) => &e.attrs,
        Expr::Try(e) => &e.attrs,
        Expr::TryBlock(e) => &e.attrs,
        Expr::Tuple(e) => &e.attrs,
        Expr::Unary(e) => &e.attrs,
        Expr::Unsafe(e) => &e.attrs,
        Expr::While(e) => &e.attrs,
        Expr::Yield(e) => &e.attrs,
        _ => &[],
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item(&mut self, item: &'ast Item) {
        match item {
            Item::Mod(m) => self.module(m),
            Item::Fn(f) => self.function(item, f),
            Item::Verbatim(tokens) => {
                self.refuse(tokens.span(), "syntax syn cannot model (a verbatim item) is outside the source model");
            }
            _ => {
                if let Item::Use(u) = item {
                    self.use_tree(&u.tree);
                }
                if let Item::ExternCrate(e) = item {
                    self.extern_crate(e);
                }
                self.attributed(item_attrs(item), item.span(), |s| visit::visit_item(s, item));
            }
        }
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        if let ImplItem::Verbatim(tokens) = item {
            self.refuse(tokens.span(), "syntax syn cannot model (a verbatim impl item) is outside the source model");
            return;
        }
        self.attributed(impl_item_attrs(item), item.span(), |s| visit::visit_impl_item(s, item));
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        if let TraitItem::Verbatim(tokens) = item {
            self.refuse(tokens.span(), "syntax syn cannot model (a verbatim trait item) is outside the source model");
            return;
        }
        self.attributed(trait_item_attrs(item), item.span(), |s| visit::visit_trait_item(s, item));
    }

    fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
        if let syn::ForeignItem::Verbatim(tokens) = item {
            self.refuse(tokens.span(), "syntax syn cannot model (a verbatim foreign item) is outside the source model");
            return;
        }
        self.attributed(foreign_item_attrs(item), item.span(), |s| visit::visit_foreign_item(s, item));
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Verbatim(tokens) = expr {
            self.refuse(tokens.span(), "syntax syn cannot model (a verbatim expression) is outside the source model");
            return;
        }
        self.attributed(expr_attrs(expr), expr.span(), |s| visit::visit_expr(s, expr));
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::Local(local) => self.attributed(&local.attrs, stmt.span(), |s| visit::visit_stmt(s, stmt)),
            Stmt::Macro(mac) => self.attributed(&mac.attrs, stmt.span(), |s| visit::visit_stmt(s, stmt)),
            // Masked with its trailing `;`, like `Local` and `Macro`.
            Stmt::Expr(expr, _) => self.attributed(expr_attrs(expr), stmt.span(), |s| visit::visit_stmt(s, stmt)),
            Stmt::Item(_) => visit::visit_stmt(self, stmt),
        }
    }

    fn visit_field(&mut self, field: &'ast syn::Field) {
        self.attributed(&field.attrs, field.span(), |s| visit::visit_field(s, field));
    }

    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        self.attributed(&variant.attrs, variant.span(), |s| visit::visit_variant(s, variant));
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        self.attributed(&arm.attrs, arm.span(), |s| visit::visit_arm(s, arm));
    }

    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        self.attributed(&field.attrs, field.span(), |s| visit::visit_field_value(s, field));
    }

    fn visit_field_pat(&mut self, field: &'ast syn::FieldPat) {
        self.attributed(&field.attrs, field.span(), |s| visit::visit_field_pat(s, field));
    }

    fn visit_generic_param(&mut self, param: &'ast syn::GenericParam) {
        let attrs = match param {
            syn::GenericParam::Lifetime(p) => &p.attrs,
            syn::GenericParam::Type(p) => &p.attrs,
            syn::GenericParam::Const(p) => &p.attrs,
        };
        self.attributed(attrs, param.span(), |s| visit::visit_generic_param(s, param));
    }

    fn visit_fn_arg(&mut self, arg: &'ast syn::FnArg) {
        let attrs = match arg {
            syn::FnArg::Receiver(r) => &r.attrs,
            syn::FnArg::Typed(t) => &t.attrs,
        };
        self.attributed(attrs, arg.span(), |s| visit::visit_fn_arg(s, arg));
    }

    fn visit_type(&mut self, ty: &'ast syn::Type) {
        if let syn::Type::Verbatim(tokens) = ty {
            self.refuse(tokens.span(), "syntax syn cannot model (a verbatim type) is outside the source model");
            return;
        }
        visit::visit_type(self, ty);
    }

    fn visit_pat(&mut self, pat: &'ast syn::Pat) {
        if let syn::Pat::Verbatim(tokens) = pat {
            self.refuse(tokens.span(), "syntax syn cannot model (a verbatim pattern) is outside the source model");
            return;
        }
        visit::visit_pat(self, pat);
    }

    fn visit_type_param_bound(&mut self, bound: &'ast syn::TypeParamBound) {
        if let syn::TypeParamBound::Verbatim(tokens) = bound {
            self.refuse(tokens.span(), "syntax syn cannot model (a verbatim bound) is outside the source model");
            return;
        }
        visit::visit_type_param_bound(self, bound);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // An `include!`'s own argument is the edge, not evidence.
        if mac.path.segments.last().is_some_and(|seg| seg.ident.unraw() == "include") {
            self.include(&mac.tokens, mac.span());
            return;
        }
        self.macro_tokens(mac.tokens.clone());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn entry() -> Entry {
        Entry {
            dir: PathBuf::from("src"),
            stem: Some("foo".into()),
        }
    }

    fn masked<'a>(src: &'a str, spans: &[Range<usize>]) -> Vec<&'a str> {
        spans.iter().map(|r| &src[r.clone()]).collect()
    }

    #[test]
    fn cfg_test_item_is_masked_with_its_attribute() {
        let src = "fn a() {}\n#[cfg(test)]\nfn b() {}\nfn c() {}\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(masked(src, &facts.masks), vec!["#[cfg(test)]\nfn b() {}"]);
    }

    #[test]
    fn attribute_on_a_generic_param_masks_only_the_param() {
        let src = "fn production<#[cfg(test)] T>() { forbidden(); }\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(masked(src, &facts.masks), vec!["#[cfg(test)] T"]);
    }

    #[test]
    fn impl_with_generic_commas_and_use_tree_are_masked_whole() {
        let src = "#[cfg(test)]\nimpl<A, B> T<A, B> for X { fn f() {} }\n#[cfg(test)]\nuse crate::{a, b};\nfn p() {}\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(
            masked(src, &facts.masks),
            vec![
                "#[cfg(test)]\nimpl<A, B> T<A, B> for X { fn f() {} }",
                "#[cfg(test)]\nuse crate::{a, b};"
            ]
        );
    }

    #[test]
    fn statements_fields_and_arms_are_masked() {
        let src = "struct S { a: u8, #[cfg(test)] b: u8 }\nfn f(x: u8) { #[cfg(test)] let y = 1; match x { #[cfg(test)] 0 => {} _ => {} } }\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(
            masked(src, &facts.masks),
            vec!["#[cfg(test)] b: u8", "#[cfg(test)] let y = 1;", "#[cfg(test)] 0 => {}"]
        );
    }

    #[test]
    fn inner_cfg_test_masks_only_its_module() {
        let src = "mod fixtures {\n    #![cfg(test)]\n    fn f() {}\n}\nfn production() {}\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(masked(src, &facts.masks), vec!["mod fixtures {\n    #![cfg(test)]\n    fn f() {}\n}"]);
    }

    #[test]
    fn file_level_inner_cfg_masks_the_file_after_the_bom() {
        let src = "\u{feff}#![cfg(test)]\nfn f() {}\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(facts.masks, vec![3..src.len()]);
    }

    #[test]
    fn spans_are_shifted_past_a_bom_and_shebang() {
        let src = "\u{feff}#!/usr/bin/env run\nfn a() {}\n#[cfg(test)]\nfn b() {}\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(masked(src, &facts.masks), vec!["#[cfg(test)]\nfn b() {}"]);
    }

    #[test]
    fn module_edges_follow_rustc_directory_ownership() {
        // `entry()` is the non-mod-rs file `src/foo.rs`.
        let src = "mod a;\n#[cfg(test)]\nmod t;\n#[path = \"x/y.rs\"]\nmod p;\nmod inl {\n    mod b;\n    #[path = \"q.rs\"]\n    mod q;\n}\n#[path = \"other\"]\nmod moved { mod c; }\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        let ord = |dir: &str, name: &str| Target::Ordinary {
            dir: PathBuf::from(dir),
            name: name.into(),
        };
        let targets: Vec<(Target, bool)> = facts.edges.into_iter().map(|e| (e.target, e.test)).collect();
        assert_eq!(
            targets,
            vec![
                (ord("src/foo", "a"), false),
                (ord("src/foo", "t"), true),
                (Target::Explicit(PathBuf::from("src/x/y.rs")), false),
                (ord("src/foo/inl", "b"), false),
                (Target::Explicit(PathBuf::from("src/foo/inl/q.rs")), false),
                // An explicit path on the first inline module replaces the
                // pending stem: rustc looks in src/other, not src/foo/other.
                (ord("src/other", "c"), false),
            ]
        );
    }

    #[test]
    fn ungated_child_of_a_test_module_is_test() {
        let src = "#[cfg(test)]\nmod tests {\n    mod nested;\n}\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert!(facts.edges.iter().all(|e| e.test));
    }

    #[test]
    fn include_literals_resolve_from_the_file_even_inside_inline_modules() {
        let src = "emit! { include!(\"shared.rs\"); }\nmod inner { const X: u8 = include!(\"v.rs\"); }\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        let targets: Vec<Target> = facts.edges.into_iter().map(|e| e.target).collect();
        assert_eq!(
            targets,
            vec![
                Target::Explicit(PathBuf::from("src/shared.rs")),
                Target::Explicit(PathBuf::from("src/v.rs")),
            ]
        );
    }

    #[test]
    fn only_the_exact_out_dir_form_escapes_refusal() {
        let ok = "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n";
        assert!(analyze(ok, &entry(), false, false).unwrap().refusals.is_empty());
        for bad in [
            "include!(concat!(\"a\", \"/gen.rs\"));\n",
            "include!(env!(\"MY_OUT_DIR_SOURCE\"));\n",
            "include!(choose!(\"OUT_DIR\", \"production.rs\"));\n",
        ] {
            assert_eq!(analyze(bad, &entry(), false, false).unwrap().refusals.len(), 1, "{bad}");
            assert!(analyze(bad, &entry(), true, false).unwrap().refusals.is_empty(), "{bad}");
        }
    }

    #[test]
    fn cfg_inside_macro_input_proves_nothing() {
        let src = "rewrite! { #[cfg(test)] fn f() {} }\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert!(facts.masks.is_empty());
    }

    #[test]
    fn tripwire_evidence_names_files_not_identifiers() {
        let src = "fn p() { println!(\"tests {}\", state); }\nother!(mod fixtures);\nload!(\"data/shared.rs\");\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(
            facts.evidence,
            vec![
                Evidence::ModName {
                    name: "fixtures".into(),
                    at: "2:12".into()
                },
                Evidence::PathLiteral {
                    value: "data/shared.rs".into(),
                    at: "3:7".into()
                },
            ]
        );
    }

    #[test]
    fn bare_test_on_a_free_fn_is_recorded_separately() {
        let src = "#[test]\nfn t() {}\n#[tokio::test]\nfn u() {}\n#[test(x)]\nfn v() {}\nfn p() {}\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert!(facts.masks.is_empty());
        assert_eq!(masked(src, &facts.bare_tests), vec!["#[test]\nfn t() {}"]);
    }

    #[test]
    fn imports_and_macro_use_that_can_shadow_test_are_reported() {
        let src = "use a::test;\nuse b::{c as test};\nuse d::testing;\n#[macro_use]\nextern crate e;\n#[macro_use(f)]\nextern crate g;\n#[macro_use(test)]\nextern crate h;\n#[cfg_attr(feature = \"hooks\", macro_use)]\nextern crate i;\nuse j::k as r#test;\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(facts.test_shadows.len(), 6, "{:?}", facts.test_shadows);
    }

    #[test]
    fn shadowing_imports_inside_macro_input_are_reported() {
        let src = "cfg_if::cfg_if! {\n    if #[cfg(feature = \"x\")] {\n        use attrs::wrap as test;\n    } else {\n        use attrs::{other, test};\n    }\n}\nm! { use a::test; }\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(facts.test_shadows.len(), 3, "{:?}", facts.test_shadows);
        // Not imports: a `test` path segment mid-path, a call argument.
        let quiet = "m! { use a::test::b; }\nn!(test);\n";
        assert!(analyze(quiet, &entry(), false, false).unwrap().test_shadows.is_empty());
        // Test context does not count.
        let gated = "#[cfg(test)]\nm! { use a::test; }\n";
        assert!(analyze(gated, &entry(), false, false).unwrap().test_shadows.is_empty());
    }

    #[test]
    fn expression_statement_is_masked_with_its_semicolon() {
        let src = "fn f() { #[cfg(test)] setup(); run(); }\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert_eq!(masked(src, &facts.masks), vec!["#[cfg(test)] setup();"]);
    }

    #[test]
    fn edges_and_refusals_inside_a_bare_test_are_tagged() {
        let src = "#[test]\nfn fixture() {\n    include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/x.rs\"));\n    include!(\"y.rs\");\n}\n";
        let facts = analyze(src, &entry(), false, false).unwrap();
        assert!(facts.refusals.is_empty());
        assert_eq!(facts.bare_refusals.len(), 1);
        assert!(facts.edges.iter().all(|e| e.bare && !e.test));
    }

    #[test]
    fn verbatim_types_patterns_and_bounds_are_refused() {
        // Built directly: which source syn keeps verbatim changes between
        // syn releases, and the visitor must refuse every such position.
        let ty = syn::Type::Verbatim(quote_tokens("dyn Foo"));
        let mut c = collector();
        c.visit_type(&ty);
        let pat = syn::Pat::Verbatim(quote_tokens("x"));
        c.visit_pat(&pat);
        let bound = syn::TypeParamBound::Verbatim(quote_tokens("Foo"));
        c.visit_type_param_bound(&bound);
        assert_eq!(c.facts.refusals.len(), 3);
    }

    fn quote_tokens(src: &str) -> TokenStream {
        src.parse().unwrap()
    }

    fn collector() -> Collector {
        Collector {
            origin: 0,
            test: false,
            file_dir: PathBuf::from("src"),
            dir: PathBuf::from("src"),
            stem: None,
            bare: false,
            facts: Facts::default(),
        }
    }

    #[test]
    fn a_splice_may_be_one_expression() {
        let facts = analyze("[1u8, 2, 3]\n", &entry(), false, true).unwrap();
        assert!(facts.edges.is_empty());
        assert!(analyze("[1u8, 2, 3]\n", &entry(), false, false).is_err());
    }
}
