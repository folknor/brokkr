//! Reachability from Cargo's target roots, and the per-file verdict.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use crate::error::DevError;

use super::facts::{self, Context, Edge, Entry, Evidence, Facts, Target};

/// What `test_code = "skip"` may skip in one file, in file byte coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileMask {
    /// Every occurrence of the file is test-only.
    Test,
    /// The file has a production occurrence (or none at all); these spans are
    /// test-only in every production occurrence.
    Production(Vec<Range<usize>>),
}

/// The verdict for every Rust file a scan asked about.
#[derive(Debug, Default)]
pub(crate) struct Classification {
    files: HashMap<PathBuf, FileMask>,
    /// What `test_code = "only"` selects: bytes the skip mask covers that
    /// some occurrence does not prove absent from test builds. Disjoint
    /// ascending spans; a span may run past the end of the file.
    tests: HashMap<PathBuf, Vec<Range<usize>>>,
    /// Files whose test selection depends on what a bare `#[test]` names,
    /// which a shadowing site leaves unresolved.
    unresolved: HashSet<PathBuf>,
    /// The shadowing sites behind `unresolved`.
    pub(crate) shadows: Vec<String>,
    /// Operator-facing notes: tripwire vetoes, and why bare `#[test]`
    /// functions are not being skipped when something could shadow it.
    pub(crate) notes: Vec<String>,
}

impl Classification {
    pub(crate) fn mask(&self, rel: &Path) -> Option<&FileMask> {
        self.files.get(rel)
    }

    pub(crate) fn test_selection(&self, rel: &Path) -> Option<&[Range<usize>]> {
        self.tests.get(rel).map(Vec::as_slice)
    }

    pub(crate) fn is_unresolved(&self, rel: &Path) -> bool {
        self.unresolved.contains(rel)
    }
}

/// One crate target's root file, from `cargo metadata`.
#[derive(Debug, Clone)]
pub(crate) struct Root {
    pub(crate) path: PathBuf,
    /// An integration-test target: everything under it is test code.
    pub(crate) test: bool,
}

/// Every workspace target as a root: `test` targets are test roots, every
/// other kind (unknown ones included) a production root.
pub(crate) fn workspace_roots(root: &Path) -> Result<Vec<Root>, DevError> {
    let metadata = crate::cargo::metadata::load(
        root,
        crate::cargo::metadata::Query::workspace(),
        &[],
        Some(crate::cargo_env::Phase::Textlint),
    )?;
    Ok(metadata
        .packages
        .iter()
        .filter(|p| metadata.workspace_members.contains(&p.id))
        .flat_map(|p| &p.targets)
        .filter(|t| !t.src_path.is_empty())
        .map(|t| Root {
            path: PathBuf::from(&t.src_path),
            test: !t.kind.is_empty() && t.kind.iter().all(|k| k == "test"),
        })
        .collect())
}

/// Source text for an absolute path, from the same snapshot the scan reads.
/// `Err` says why there is none (an I/O error, a missing file, invalid
/// UTF-8), phrased to follow the path in a refusal.
pub(crate) type Reader<'a> = dyn Fn(&Path) -> Result<String, String> + 'a;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VisitKey {
    canon: PathBuf,
    entry: Entry,
    context: Context,
    splice: bool,
}

/// One production occurrence's masks: proven cfg spans, and bare-`#[test]`
/// spans that count only when nothing shadows the builtin.
struct ProductionMasks {
    cfg: Vec<Range<usize>>,
    bare_tests: Vec<Range<usize>>,
}

#[derive(Default)]
struct Occurrences {
    shown: String,
    production: Vec<ProductionMasks>,
    test: bool,
    /// One entry per occurrence not proven absent from test builds (in any
    /// context): the spans that occurrence proves absent from them.
    live: Vec<Vec<Range<usize>>>,
}

struct Pending {
    path: PathBuf,
    entry: Entry,
    context: Context,
    splice: bool,
}

#[derive(Default)]
struct Walk {
    /// Follow edges declared inside bare-`#[test]` functions as test edges.
    /// Only set on the second walk, once nothing can shadow the attribute.
    bare_is_test: bool,
    /// Whether any edge or refusal came from inside a bare-`#[test]`
    /// function, so a second walk would differ.
    saw_bare: bool,
    /// Whether any production occurrence holds a bare-`#[test]` function.
    bare_candidates: bool,
    seen: HashSet<VisitKey>,
    occurrences: HashMap<PathBuf, Occurrences>,
    refusals: Vec<String>,
    /// Refusals inside bare-`#[test]` functions: they stand unless the
    /// attribute is honored.
    bare_refusals: Vec<String>,
    /// Tripwire evidence with the production file that spelled it.
    evidence: Vec<(String, Evidence)>,
    /// The same, from inside bare-`#[test]` functions.
    bare_evidence: Vec<(String, Evidence)>,
    test_shadows: Vec<String>,
    queue: Vec<Pending>,
}

impl Walk {
    fn run(&mut self, root: &Path, read: &Reader<'_>) {
        while let Some(p) = self.queue.pop() {
            // A missing target is skipped, not refused: rustc fails the
            // build when an active edge names a missing file, and an inactive
            // conditional alternative (a `cfg_attr` path) legitimately may not
            // exist.
            let Ok(canon) = std::fs::canonicalize(&p.path) else {
                continue;
            };
            let key = VisitKey {
                canon: canon.clone(),
                entry: p.entry.clone(),
                context: p.context,
                splice: p.splice,
            };
            if !self.seen.insert(key) {
                continue;
            }
            let shown = display(root, &p.path);
            let src = match read(&p.path) {
                Ok(src) => src,
                Err(why) => {
                    if !p.context.test {
                        self.refusals.push(format!("{shown}: {why}"));
                    }
                    continue;
                }
            };
            match facts::analyze(&src, &p.entry, p.context, p.splice) {
                Err(_) if p.context.test => {
                    // Unparsed test code proves nothing dead: all of it is
                    // live unless the edge itself was dead.
                    let o = self.occurrence(canon, &shown);
                    o.test = true;
                    if !p.context.dead {
                        o.live.push(Vec::new());
                    }
                }
                Err(error) => self
                    .refusals
                    .push(format!("{shown}: does not parse: {error}")),
                Ok(found) => self.record(root, canon, &shown, p.context, found),
            }
        }
    }

    fn occurrence(&mut self, canon: PathBuf, shown: &str) -> &mut Occurrences {
        let o = self.occurrences.entry(canon).or_default();
        if o.shown.is_empty() {
            shown.clone_into(&mut o.shown);
        }
        o
    }

    fn record(&mut self, root: &Path, canon: PathBuf, shown: &str, context: Context, found: Facts) {
        if !context.test {
            self.bare_candidates |= !found.bare_tests.is_empty();
        }
        let o = self.occurrence(canon, shown);
        if !context.dead {
            o.live.push(normalize(found.dead));
        }
        if context.test {
            o.test = true;
        } else {
            o.production.push(ProductionMasks {
                cfg: found.masks,
                bare_tests: found.bare_tests,
            });
        }
        self.refusals
            .extend(found.refusals.into_iter().map(|r| format!("{shown}:{r}")));
        self.saw_bare |= !found.bare_refusals.is_empty() || found.edges.iter().any(|e| e.bare);
        self.bare_refusals.extend(
            found
                .bare_refusals
                .into_iter()
                .map(|r| format!("{shown}:{r}")),
        );
        self.test_shadows.extend(
            found
                .test_shadows
                .into_iter()
                .map(|s| format!("{shown}:{s}")),
        );
        self.evidence
            .extend(found.evidence.into_iter().map(|e| (shown.to_owned(), e)));
        self.bare_evidence.extend(
            found
                .bare_evidence
                .into_iter()
                .map(|e| (shown.to_owned(), e)),
        );
        for edge in found.edges {
            self.follow(root, shown, edge);
        }
    }

    fn follow(&mut self, root: &Path, from: &str, mut edge: Edge) {
        edge.test |= edge.bare && self.bare_is_test;
        let context = Context {
            test: edge.test,
            dead: edge.dead,
        };
        match edge.target {
            Target::Explicit(path) => {
                let entry = Entry::owning(&path);
                self.queue.push(Pending {
                    path,
                    entry,
                    context,
                    splice: edge.splice,
                });
            }
            Target::Ordinary { dir, name } => {
                let file = dir.join(format!("{name}.rs"));
                let nested = dir.join(&name).join("mod.rs");
                if file.is_file() && nested.is_file() && !edge.test {
                    self.refusals.push(format!(
                        "{from}: module {name} is ambiguous: both {} and {} exist",
                        display(root, &file),
                        display(root, &nested)
                    ));
                    return;
                }
                if file.is_file() {
                    self.queue.push(Pending {
                        path: file,
                        entry: Entry {
                            dir: dir.clone(),
                            stem: Some(name.clone()),
                        },
                        context,
                        splice: false,
                    });
                }
                if nested.is_file() {
                    let entry = Entry::owning(&nested);
                    self.queue.push(Pending {
                        path: nested,
                        entry,
                        context,
                        splice: false,
                    });
                }
            }
        }
    }
}

fn display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Sort and merge into disjoint ascending spans.
fn normalize(mut spans: Vec<Range<usize>>) -> Vec<Range<usize>> {
    spans.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match out.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => out.push(span),
        }
    }
    out
}

fn intersect(a: &[Range<usize>], b: &[Range<usize>]) -> Vec<Range<usize>> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        let start = a[i].start.max(b[j].start);
        let end = a[i].end.min(b[j].end);
        if start < end {
            out.push(start..end);
        }
        if a[i].end < b[j].end {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// The normal components of `path`, folded lexically: `.` and roots
/// dropped, `..` cancelling its predecessor (a leading `..` has none and is
/// dropped). What a suffix comparison can honestly match on.
fn components(path: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => out.extend(s.to_str().map(str::to_owned)),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

/// Whether tripwire evidence names `file` (absolute): a module name matching
/// `name.rs` or `name/mod.rs`, or a path literal that is a whole-component
/// suffix of the file's path.
fn names_file(evidence: &Evidence, file: &Path) -> bool {
    match evidence {
        Evidence::ModName { name, .. } => {
            let is_mod_rs = file.file_name().is_some_and(|n| n == "mod.rs");
            let module = if is_mod_rs {
                file.parent().and_then(Path::file_name)
            } else {
                file.file_stem()
            };
            module.is_some_and(|m| m == name.as_str())
        }
        Evidence::PathLiteral { value, .. } => {
            let wanted = components(Path::new(value));
            let have = components(file);
            !wanted.is_empty() && have.ends_with(&wanted)
        }
    }
}

fn evidence_site(from: &str, evidence: &Evidence) -> String {
    match evidence {
        Evidence::ModName { name, at } => format!("{from}:{at} declares mod {name} in macro input"),
        Evidence::PathLiteral { value, at } => {
            format!("{from}:{at} spells {value:?} in macro input")
        }
    }
}

/// `a` with every byte of `b` removed; both disjoint and ascending.
fn subtract(a: &[Range<usize>], b: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for span in a {
        let mut start = span.start;
        for cut in b {
            if cut.end <= start || cut.start >= span.end {
                continue;
            }
            if cut.start > start {
                out.push(start..cut.start);
            }
            start = start.max(cut.end);
        }
        if start < span.end {
            out.push(start..span.end);
        }
    }
    out
}

/// What the test selection keeps of `candidates`: bytes some live occurrence
/// does not prove absent from test builds. With no live occurrence, nothing.
fn live_part(candidates: &[Range<usize>], live: &[Vec<Range<usize>>]) -> Vec<Range<usize>> {
    let Some((first, rest)) = live.split_first() else {
        return Vec::new();
    };
    let dead_everywhere = rest.iter().fold(first.clone(), |acc, d| intersect(&acc, d));
    subtract(candidates, &dead_everywhere)
}

/// A file analyzed on its own, as a production crate root: for a file
/// nothing reaches, or one the tripwire vetoed. Returns its `cfg` skip masks
/// and its test selection (those masks less what its own attributes prove
/// absent from test builds). Bare `#[test]` is never honored here, because the
/// walk cannot see the file's crate root, where a `#[macro_use] extern crate`
/// could shadow the builtin. A parse failure masks and selects nothing.
fn standalone(path: &Path, read: &Reader<'_>) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let Some(found) = read(path)
        .ok()
        .and_then(|src| facts::analyze(&src, &Entry::owning(path), Context::default(), false).ok())
    else {
        return (Vec::new(), Vec::new());
    };
    let masks = normalize(found.masks);
    let tests = subtract(&masks, &normalize(found.dead));
    (masks, tests)
}

fn walk_from(root: &Path, roots: &[Root], read: &Reader<'_>, bare_is_test: bool) -> Walk {
    let mut walk = Walk {
        bare_is_test,
        ..Walk::default()
    };
    for r in roots {
        walk.queue.push(Pending {
            path: r.path.clone(),
            entry: Entry::owning(&r.path),
            context: Context {
                test: r.test,
                dead: false,
            },
            splice: false,
        });
    }
    walk.run(root, read);
    walk
}

/// Per-file verdicts of one walk.
#[derive(Default)]
struct Verdicts {
    masks: HashMap<PathBuf, FileMask>,
    tests: HashMap<PathBuf, Vec<Range<usize>>>,
    notes: Vec<String>,
}

/// The skip mask and test selection of every file in `rust_files`, with
/// bare-`#[test]` spans counted as test code when `honor_bare_tests`.
fn verdicts(
    root: &Path,
    walk: &Walk,
    honor_bare_tests: bool,
    rust_files: &[PathBuf],
    read: &Reader<'_>,
) -> Verdicts {
    let mut out = Verdicts::default();
    for rel in rust_files {
        let abs = root.join(rel);
        let occurrence = std::fs::canonicalize(&abs)
            .ok()
            .and_then(|canon| walk.occurrences.get(&canon));
        let (mask, tests) = match occurrence {
            Some(o) if !o.production.is_empty() => {
                let mut masks = o.production.iter().map(|m| {
                    let mut spans = m.cfg.clone();
                    if honor_bare_tests {
                        spans.extend(m.bare_tests.iter().cloned());
                    }
                    normalize(spans)
                });
                let first = masks.next().unwrap_or_default();
                let mask = masks.fold(first, |acc, m| intersect(&acc, &m));
                let tests = live_part(&mask, &o.live);
                (FileMask::Production(mask), tests)
            }
            Some(o) if o.test => match walk.evidence.iter().find(|(_, e)| names_file(e, &abs)) {
                Some((from, evidence)) => {
                    out.notes.push(format!(
                        "{}: reached only as test code, but {}; scanned as production",
                        o.shown,
                        evidence_site(from, evidence)
                    ));
                    let (mask, tests) = standalone(&abs, read);
                    (FileMask::Production(mask), tests)
                }
                None => {
                    let whole = 0..usize::MAX;
                    let tests = live_part(std::slice::from_ref(&whole), &o.live);
                    (FileMask::Test, tests)
                }
            },
            _ => {
                let (mask, tests) = standalone(&abs, read);
                (FileMask::Production(mask), tests)
            }
        };
        out.masks.insert(rel.clone(), mask);
        out.tests.insert(rel.clone(), tests);
    }
    out
}

/// Classify `rust_files` (relative to `root`) against `roots`, reading every
/// source through `read`. Refuses, naming every site, when a production
/// context holds a construct outside the source model. `select_tests` asks
/// for the unresolved-file set a `test_code = "only"` rule needs, which can
/// cost a further walk.
pub(crate) fn classify(
    root: &Path,
    roots: &[Root],
    rust_files: &[PathBuf],
    read: &Reader<'_>,
    select_tests: bool,
) -> Result<Classification, DevError> {
    // A bare `#[test]` is the builtin only if nothing in scope can shadow it.
    // Imports are module-scoped and `#[macro_use]` reaches a whole crate, so
    // one shadowing site anywhere (test contexts included) disables the rule
    // everywhere: coarse, and only ever in the loud direction. The first walk
    // treats bare-test bodies as production; when the attribute turns out to
    // be honored, a second walk follows their edges as test. Both walks visit
    // the same files, so they find the same shadowing sites.
    let mut walk = walk_from(root, roots, read, false);
    let honor_bare_tests = walk.test_shadows.is_empty();
    if honor_bare_tests && walk.saw_bare {
        walk = walk_from(root, roots, read, true);
    }
    if !honor_bare_tests {
        let bare = std::mem::take(&mut walk.bare_refusals);
        walk.refusals.extend(bare);
        let bare = std::mem::take(&mut walk.bare_evidence);
        walk.evidence.extend(bare);
    }
    if !walk.refusals.is_empty() {
        walk.refusals.sort();
        walk.refusals.dedup();
        return Err(DevError::Refused(format!(
            "test_code cannot classify this workspace's test code; each site is outside the source model:\n  {}",
            walk.refusals.join("\n  ")
        )));
    }
    let mut out = Classification::default();
    if !honor_bare_tests {
        walk.test_shadows.sort();
        walk.test_shadows.dedup();
        out.notes.push(format!(
            "bare #[test] functions are scanned as production, because something could shadow the builtin attribute:\n  {}",
            walk.test_shadows.join("\n  ")
        ));
    }
    let found = verdicts(root, &walk, honor_bare_tests, rust_files, read);
    out.files = found.masks;
    out.tests = found.tests;
    out.notes.extend(found.notes);
    // Unresolved bare `#[test]`: the test selection above treats each as
    // production. Classify again as if each were the builtin; a file whose
    // selection differs between the two readings cannot be test-scanned
    // honestly, while one an independent proof settles either way can.
    if !honor_bare_tests && select_tests && (walk.saw_bare || walk.bare_candidates) {
        let builtin = walk_from(root, roots, read, true);
        let alternative = verdicts(root, &builtin, true, rust_files, read);
        for rel in rust_files {
            let (now, then) = (out.tests.get(rel), alternative.tests.get(rel));
            if now == then {
                continue;
            }
            // Two spellings of one selection (a whole-file span against a
            // file-level `#![cfg(test)]` span, say) can differ past the end
            // of the file or over a BOM, which the scanner strips; compare
            // only the bytes it matches against. A shebang is matched, so a
            // selection that differs over one stays different.
            let body = read(&root.join(rel)).map_or(0..usize::MAX, |src| {
                let bom = if src.starts_with('\u{feff}') { 3 } else { 0 };
                bom..src.len()
            });
            let clip = |spans: Option<&Vec<Range<usize>>>| {
                spans.map_or_else(Vec::new, |s| {
                    normalize(intersect(s, std::slice::from_ref(&body)))
                })
            };
            if clip(now) != clip(then) {
                out.unresolved.insert(rel.clone());
            }
        }
        out.shadows.clone_from(&walk.test_shadows);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = crate::test_scratch::scratch("test_code_graph", name);
        for (rel, body) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dir
    }

    fn run(dir: &Path, roots: &[(&str, bool)], files: &[&str]) -> Result<Classification, DevError> {
        let roots: Vec<Root> = roots
            .iter()
            .map(|(p, test)| Root {
                path: dir.join(p),
                test: *test,
            })
            .collect();
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        classify(
            dir,
            &roots,
            &files,
            &|p: &Path| std::fs::read_to_string(p).map_err(|e| e.to_string()),
            true,
        )
    }

    fn selected(c: &Classification, dir: &Path, rel: &str) -> Vec<String> {
        let src = std::fs::read_to_string(dir.join(rel)).unwrap();
        c.test_selection(Path::new(rel))
            .unwrap()
            .iter()
            .map(|r| src[r.start..r.end.min(src.len())].to_owned())
            .collect()
    }

    #[test]
    fn test_selection_is_test_only_code_live_in_some_test_build() {
        let dir = tree(
            "select",
            &[
                (
                    "src/lib.rs",
                    "mod shared;\nfn p() {}\n#[cfg(test)]\nmod tests {\n    #[cfg(not(test))]\n    fn gone() {}\n    fn live() {}\n}\n#[cfg(test)]\n#[cfg(not(test))]\nmod dead;\n#[cfg(any())]\nfn never() {}\n",
                ),
                ("src/shared.rs", "fn s() {}\n"),
                ("src/dead.rs", "fn d() {}\n"),
                (
                    "tests/it.rs",
                    "#[path = \"../src/shared.rs\"]\nmod shared;\nfn t() {}\n",
                ),
            ],
        );
        let c = run(
            &dir,
            &[("src/lib.rs", false), ("tests/it.rs", true)],
            &["src/lib.rs", "src/shared.rs", "src/dead.rs", "tests/it.rs"],
        )
        .unwrap();
        // The test module less its dead item; the dead declaration and the
        // dead-everywhere function are not selected.
        assert_eq!(
            selected(&c, &dir, "src/lib.rs"),
            vec!["#[cfg(test)]\nmod tests {\n    ", "\n    fn live() {}\n}"]
        );
        // Shipping code an integration test also includes is not test code.
        assert!(selected(&c, &dir, "src/shared.rs").is_empty());
        // Reached only through an edge dead in test builds.
        assert!(selected(&c, &dir, "src/dead.rs").is_empty());
        assert_eq!(
            selected(&c, &dir, "tests/it.rs"),
            vec!["#[path = \"../src/shared.rs\"]\nmod shared;\nfn t() {}\n"]
        );
        // The skip mask is untouched by dead-code proofs.
        assert_eq!(c.mask(Path::new("src/dead.rs")), Some(&FileMask::Test));
    }

    #[test]
    fn a_file_dead_in_one_test_context_stays_selected_through_a_live_one() {
        let dir = tree(
            "select_two",
            &[
                (
                    "src/lib.rs",
                    "#[cfg(test)]\n#[cfg_attr(test, cfg(not(test)))]\n#[path = \"fixture.rs\"]\nmod dead_f;\n#[cfg(test)]\n#[path = \"fixture.rs\"]\nmod f;\n",
                ),
                ("src/fixture.rs", "fn f() {}\n"),
            ],
        );
        let c = run(&dir, &[("src/lib.rs", false)], &["src/fixture.rs"]).unwrap();
        assert_eq!(selected(&c, &dir, "src/fixture.rs"), vec!["fn f() {}\n"]);
        assert!(!c.is_unresolved(Path::new("src/fixture.rs")));
    }

    #[test]
    fn shadowed_bare_test_marks_only_files_whose_selection_it_decides() {
        let dir = tree(
            "select_shadowed",
            &[
                (
                    "src/lib.rs",
                    "use attrs::test;\nmod shared;\n#[test]\nfn example() {\n    #[path = \"shared.rs\"]\n    mod subject;\n}\n#[cfg(not(test))]\n#[test]\nfn impossible() {}\n",
                ),
                ("src/shared.rs", "fn s() {}\n"),
                ("src/other.rs", "#[cfg(test)]\nfn t() {}\n"),
            ],
        );
        let c = run(
            &dir,
            &[("src/lib.rs", false)],
            &["src/lib.rs", "src/shared.rs", "src/other.rs"],
        )
        .unwrap();
        // `example` is test code only if the attribute is the builtin.
        assert!(c.is_unresolved(Path::new("src/lib.rs")));
        // Reached by an ordinary production edge as well: production either way.
        assert!(!c.is_unresolved(Path::new("src/shared.rs")));
        // Unreachable, settled by its own cfg.
        assert!(!c.is_unresolved(Path::new("src/other.rs")));
        assert_eq!(c.shadows.len(), 1);
    }

    #[test]
    fn a_default_path_every_test_build_replaces_is_not_selected() {
        let dir = tree(
            "select_replaced",
            &[
                (
                    "src/lib.rs",
                    "#[cfg_attr(test, path = \"replacement.rs\")]\nmod helper;\n",
                ),
                ("src/helper.rs", "#[cfg(test)]\nfn forbidden() {}\n"),
                ("src/replacement.rs", "fn r() {}\n"),
            ],
        );
        let c = run(&dir, &[("src/lib.rs", false)], &["src/helper.rs"]).unwrap();
        assert!(selected(&c, &dir, "src/helper.rs").is_empty());
        // The skip mask still masks the cfg(test) item.
        assert_eq!(
            c.mask(Path::new("src/helper.rs")).cloned(),
            production(&[(0, 30)])
        );
    }

    #[test]
    fn equal_selections_spelled_differently_are_not_unresolved() {
        let dir = tree(
            "select_spelling",
            &[
                (
                    "src/lib.rs",
                    "use attrs::test;\n#[test]\nfn example() {\n    #[path = \"helper.rs\"]\n    mod helper;\n}\n",
                ),
                ("src/helper.rs", "\u{feff}#![cfg(test)]\nfn helper() {}\n"),
                (
                    "src/script.rs",
                    "#!/usr/bin/env x\n#![cfg(test)]\nfn s() {}\n",
                ),
            ],
        );
        let c = run(&dir, &[("src/lib.rs", false)], &["src/helper.rs"]).unwrap();
        assert!(!c.is_unresolved(Path::new("src/helper.rs")));
        // A shebang the scanner matches is a real difference between the
        // two readings.
        std::fs::write(
            dir.join("src/lib.rs"),
            "use attrs::test;\n#[test]\nfn example() {\n    #[path = \"script.rs\"]\n    mod helper;\n}\n",
        )
        .unwrap();
        let c = run(&dir, &[("src/lib.rs", false)], &["src/script.rs"]).unwrap();
        assert!(c.is_unresolved(Path::new("src/script.rs")));
    }

    #[test]
    fn a_dead_bare_test_is_not_unresolved() {
        let dir = tree(
            "select_dead_bare",
            &[(
                "src/lib.rs",
                "use attrs::test;\n#[cfg(not(test))]\n#[test]\nfn impossible() {}\n",
            )],
        );
        let c = run(&dir, &[("src/lib.rs", false)], &["src/lib.rs"]).unwrap();
        assert!(!c.is_unresolved(Path::new("src/lib.rs")));
    }

    fn production(spans: &[(usize, usize)]) -> Option<FileMask> {
        Some(FileMask::Production(
            spans.iter().map(|&(a, b)| a..b).collect(),
        ))
    }

    #[test]
    fn out_of_line_test_module_and_its_children_are_test() {
        let dir = tree(
            "out_of_line",
            &[
                ("src/lib.rs", "mod a;\n#[cfg(test)]\nmod tests;\n"),
                ("src/a.rs", "pub fn a() {}\n"),
                ("src/tests.rs", "mod helper;\n"),
                ("src/tests/helper.rs", "fn h() {}\n"),
            ],
        );
        let c = run(
            &dir,
            &[("src/lib.rs", false)],
            &["src/a.rs", "src/tests.rs", "src/tests/helper.rs"],
        )
        .unwrap();
        assert_eq!(c.mask(Path::new("src/a.rs")).cloned(), production(&[]));
        assert_eq!(c.mask(Path::new("src/tests.rs")), Some(&FileMask::Test));
        assert_eq!(
            c.mask(Path::new("src/tests/helper.rs")),
            Some(&FileMask::Test)
        );
    }

    #[test]
    fn a_file_with_any_production_occurrence_is_production() {
        let dir = tree(
            "shared",
            &[
                (
                    "src/lib.rs",
                    "#[cfg(test)]\n#[path = \"shared.rs\"]\nmod fixtures;\ninclude!(\"shared.rs\");\n",
                ),
                ("src/shared.rs", "fn s() {}\n"),
            ],
        );
        let c = run(&dir, &[("src/lib.rs", false)], &["src/shared.rs"]).unwrap();
        assert_eq!(c.mask(Path::new("src/shared.rs")).cloned(), production(&[]));
    }

    #[test]
    fn a_test_root_including_the_library_does_not_make_it_test() {
        let dir = tree(
            "test_root",
            &[
                ("src/lib.rs", "pub fn f() {}\n"),
                ("tests/it.rs", "#[path = \"../src/lib.rs\"]\nmod subject;\n"),
            ],
        );
        let c = run(
            &dir,
            &[("src/lib.rs", false), ("tests/it.rs", true)],
            &["src/lib.rs", "tests/it.rs"],
        )
        .unwrap();
        assert_eq!(c.mask(Path::new("src/lib.rs")).cloned(), production(&[]));
        assert_eq!(c.mask(Path::new("tests/it.rs")), Some(&FileMask::Test));
    }

    #[test]
    fn a_file_in_two_production_contexts_keeps_its_own_masks() {
        let body = "#[cfg(test)]\nfn t() {}\nfn p() {}\n";
        let dir = tree(
            "intersect",
            &[
                ("src/lib.rs", "mod body;\n"),
                ("src/main.rs", "#[path = \"body.rs\"]\nmod b;\n"),
                ("src/body.rs", body),
            ],
        );
        let c = run(
            &dir,
            &[("src/lib.rs", false), ("src/main.rs", false)],
            &["src/body.rs"],
        )
        .unwrap();
        assert_eq!(
            c.mask(Path::new("src/body.rs")).cloned(),
            production(&[(0, 22)])
        );
    }

    #[test]
    fn non_mod_rs_children_and_inline_paths_resolve_like_rustc() {
        let dir = tree(
            "non_mod_rs",
            &[
                (
                    "src/lib.rs",
                    "mod outer;\n#[cfg(test)]\n#[path = \"thread_files/child.rs\"]\nmod fixture;\n",
                ),
                (
                    "src/outer.rs",
                    "#[path = \"thread_files\"]\nmod thread {\n    mod child;\n}\n",
                ),
                ("src/thread_files/child.rs", "fn c() {}\n"),
            ],
        );
        let c = run(
            &dir,
            &[("src/lib.rs", false)],
            &["src/thread_files/child.rs"],
        )
        .unwrap();
        assert_eq!(
            c.mask(Path::new("src/thread_files/child.rs")).cloned(),
            production(&[])
        );
    }

    #[test]
    fn included_file_resolves_children_beside_itself() {
        let dir = tree(
            "include_entry",
            &[
                (
                    "src/lib.rs",
                    "include!(\"fragments/body.rs\");\n#[cfg(test)]\n#[path = \"fragments/child.rs\"]\nmod fixtures;\n",
                ),
                ("src/fragments/body.rs", "mod child;\n"),
                ("src/fragments/child.rs", "fn c() {}\n"),
            ],
        );
        let c = run(&dir, &[("src/lib.rs", false)], &["src/fragments/child.rs"]).unwrap();
        assert_eq!(
            c.mask(Path::new("src/fragments/child.rs")).cloned(),
            production(&[])
        );
    }

    #[test]
    fn unreachable_file_is_production_with_its_own_masks() {
        let dir = tree(
            "unreachable",
            &[
                ("src/lib.rs", ""),
                ("src/dead.rs", "#[cfg(test)]\nfn t() {}\n"),
            ],
        );
        let c = run(&dir, &[("src/lib.rs", false)], &["src/dead.rs"]).unwrap();
        assert_eq!(
            c.mask(Path::new("src/dead.rs")).cloned(),
            production(&[(0, 22)])
        );
    }

    #[test]
    fn macro_input_naming_the_file_vetoes_a_test_classification() {
        let files = [
            (
                "src/lib.rs",
                "declare!(mod shared);\n#[cfg(test)]\n#[path = \"shared.rs\"]\nmod fixtures;\n#[cfg(test)]\nmod state;\nfn f() { log!(\"state\"); }\n",
            ),
            ("src/shared.rs", "fn s() {}\n"),
            ("src/state.rs", "fn t() {}\n"),
        ];
        let dir = tree("veto", &files);
        let c = run(
            &dir,
            &[("src/lib.rs", false)],
            &["src/shared.rs", "src/state.rs"],
        )
        .unwrap();
        assert_eq!(c.mask(Path::new("src/shared.rs")).cloned(), production(&[]));
        // A bare identifier or word in a string is not evidence.
        assert_eq!(c.mask(Path::new("src/state.rs")), Some(&FileMask::Test));
        assert_eq!(c.notes.len(), 1);
    }

    #[test]
    fn path_literal_evidence_matches_whole_components_only() {
        let lit = |v: &str| Evidence::PathLiteral {
            value: v.into(),
            at: String::new(),
        };
        let file = Path::new("/w/src/state.rs");
        assert!(names_file(&lit("src/state.rs"), file));
        assert!(names_file(&lit("./state.rs"), file));
        assert!(!names_file(&lit("estate.rs"), file));
        assert!(!names_file(&lit("other/state.rs"), file));
        assert!(names_file(&lit("old/../src/state.rs"), file));
        assert!(!names_file(&lit("src/../old/state.rs"), file));
    }

    #[test]
    fn edges_inside_an_honored_bare_test_are_test_edges() {
        let dir = tree(
            "bare_edges",
            &[
                (
                    "src/main.rs",
                    "#[test]\nfn t() {\n    include!(\"fixture.rs\");\n    include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/x.rs\"));\n}\n",
                ),
                ("src/fixture.rs", "fn f() {}\n"),
            ],
        );
        let c = run(&dir, &[("src/main.rs", false)], &["src/fixture.rs"]).unwrap();
        assert_eq!(c.mask(Path::new("src/fixture.rs")), Some(&FileMask::Test));

        let dir = tree(
            "bare_edges_shadowed",
            &[(
                "src/main.rs",
                "use x::test;\n#[test]\nfn t() {\n    include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/x.rs\"));\n}\n",
            )],
        );
        assert!(run(&dir, &[("src/main.rs", false)], &[]).is_err());
    }

    #[test]
    fn an_unreachable_file_never_honors_bare_test() {
        // Nothing shadows `test` anywhere the walk can see, but the file's
        // own crate root is invisible, so its bare `#[test]` stays scanned.
        let body = "#[test]\nfn p() {}\n#[cfg(test)]\nfn t() {}\n";
        let dir = tree(
            "standalone_bare",
            &[("src/lib.rs", ""), ("src/dead.rs", body)],
        );
        let c = run(&dir, &[("src/lib.rs", false)], &["src/dead.rs"]).unwrap();
        assert_eq!(
            c.mask(Path::new("src/dead.rs")).cloned(),
            production(&[(18, 40)])
        );
    }

    #[test]
    fn bare_test_functions_are_skipped_unless_something_can_shadow_test() {
        let body = "#[test]\nfn t() {}\nfn p() {}\n";
        let dir = tree("bare", &[("src/main.rs", body)]);
        let c = run(&dir, &[("src/main.rs", false)], &["src/main.rs"]).unwrap();
        assert_eq!(
            c.mask(Path::new("src/main.rs")).cloned(),
            production(&[(0, 17)])
        );

        let dir = tree(
            "bare_shadowed",
            &[
                (
                    "src/main.rs",
                    "include!(\"imports.rs\");\n#[test]\nfn t() {}\n",
                ),
                ("src/imports.rs", "use attributes::identity as test;\n"),
            ],
        );
        let c = run(&dir, &[("src/main.rs", false)], &["src/main.rs"]).unwrap();
        assert_eq!(c.mask(Path::new("src/main.rs")).cloned(), production(&[]));
        assert_eq!(c.notes.len(), 1);
    }

    #[test]
    fn ambiguous_module_file_is_refused_in_production() {
        let dir = tree(
            "ambiguous",
            &[
                ("src/lib.rs", "mod x;\n"),
                ("src/x.rs", ""),
                ("src/x/mod.rs", ""),
            ],
        );
        assert!(run(&dir, &[("src/lib.rs", false)], &[]).is_err());
    }

    #[test]
    fn production_parse_failure_is_refused_and_test_parse_failure_is_not() {
        let dir = tree(
            "parse",
            &[
                ("src/lib.rs", "#[cfg(test)]\nmod t;\n"),
                ("src/t.rs", "fn (\n"),
            ],
        );
        let c = run(&dir, &[("src/lib.rs", false)], &["src/t.rs"]).unwrap();
        assert_eq!(c.mask(Path::new("src/t.rs")), Some(&FileMask::Test));
        let bad = tree(
            "parse_prod",
            &[("src/lib.rs", "mod t;\n"), ("src/t.rs", "fn (\n")],
        );
        assert!(run(&bad, &[("src/lib.rs", false)], &[]).is_err());
    }
}
