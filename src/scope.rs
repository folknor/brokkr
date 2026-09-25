//! Scope helpers for `brokkr check`'s diagnostic output.
//!
//! Every diagnostic a phase reports is an error, and every one of them is
//! shown - there is no cap and no flag to raise one. Errors in files with
//! unstaged changes - the files being edited right now - come first, then the
//! rest, which is the whole of what this module decides.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Files with unstaged changes: modified in the working tree relative to the
/// index, plus untracked files git does not ignore (a new file is unstaged
/// too). Paths are relative to `project_root`, the directory cargo and the
/// scanners report paths against.
///
/// Staged and committed changes do not count: the priority is the edit in
/// progress, and a file staged for commit is one the author has already
/// signed off on.
///
/// `None` when git cannot be asked (not a repository); callers then treat
/// every error as a candidate.
pub fn unstaged_files(project_root: &Path) -> Option<HashSet<PathBuf>> {
    let modified = git_paths(project_root, &["diff", "--name-only", "--relative", "-z"])?;
    let untracked = git_paths(project_root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Some(modified.into_iter().chain(untracked).collect())
}

/// Run a git command that prints NUL-separated paths.
fn git_paths(project_root: &Path, args: &[&str]) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(
        output
            .stdout
            .split(|b| *b == 0)
            .filter(|raw| !raw.is_empty())
            .filter_map(|raw| std::str::from_utf8(raw).ok())
            .map(PathBuf::from)
            .collect(),
    )
}

/// What kind of work is uncommitted in the working tree.
///
/// Drives `check`'s prose-only shortcut: editing documentation the build never
/// reads cannot break it, so a tree whose only uncommitted change is such
/// markdown does not need the clippy and test phases to prove it still
/// compiles - the last full run already did, on the same code.
///
/// "The build never reads" is the load-bearing qualifier. Markdown a Rust
/// source pulls in with `include_str!`/`include_bytes!` - brokkr's own `man`
/// pages, a crate's `#![doc = include_str!("README.md")]` with its doctests -
/// is compiled input, and an edit to it is [`Dirt::Code`]. See
/// [`included_files`] for how those are found and where the scan stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirt {
    /// Not a git repo, or git could not be asked. Never shortens a run: the
    /// shortcut is an inference from evidence, and there is none here.
    Unknown,
    /// Nothing uncommitted. A full run - a clean tree is exactly the state a
    /// complete check is *for*, and shortening it would make the common
    /// pre-commit invocation prove nothing.
    Clean,
    /// Every uncommitted path is markdown no Rust source includes.
    ProseOnly,
    /// At least one uncommitted path is not markdown, or is markdown a Rust
    /// source includes (or the includes could not be established).
    Code,
}

/// Extensions the prose-only shortcut treats as documentation.
const PROSE_EXTENSIONS: [&str; 2] = ["md", "markdown"];

/// Classify the working tree. Staged, unstaged and untracked-not-ignored paths
/// all count: what matters is whether anything not yet committed could change
/// how the code builds, and an untracked `.rs` file certainly can.
pub fn dirt(project_root: &Path) -> Dirt {
    let Some(output) = Command::new("git")
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return Dirt::Unknown;
    };
    let by_extension = classify_status(&output.stdout);
    if by_extension != Dirt::ProseOnly {
        return by_extension;
    }
    // Every changed path is markdown. Whether that is prose or compiled input
    // depends on what the Rust sources include - asked only now, since it
    // reads every source file and a code-dirty tree needs no answer.
    let (Some(paths), Some(included)) = (status_paths(&output.stdout), included_files(project_root))
    else {
        return Dirt::Code;
    };
    if paths.iter().any(|p| included.contains(Path::new(p))) {
        Dirt::Code
    } else {
        Dirt::ProseOnly
    }
}

/// The paths named by `git status --porcelain=v1 -z` output, relative to the
/// repository root. `None` when a record cannot be read - a path we cannot
/// read is a path we cannot vouch for.
fn status_paths(stdout: &[u8]) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    // A rename/copy record is followed by its origin path as a bare field.
    // That path is part of the change too: `git mv notes.md src/lib.rs` is not
    // a documentation edit.
    let mut expect_origin = false;
    for field in stdout.split(|b| *b == 0) {
        if field.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(field).ok()?;
        let path = if expect_origin {
            expect_origin = false;
            text
        } else {
            // `XY <path>`: two status columns, a space, then the path.
            let rest = text.get(3..)?;
            expect_origin = text.starts_with('R') || text.starts_with('C');
            rest
        };
        out.push(path);
    }
    Some(out)
}

/// Classify the bytes of `git status --porcelain=v1 -z` by extension alone.
/// Split out so the record format - including the second path a rename record
/// carries - is testable without a repository.
fn classify_status(stdout: &[u8]) -> Dirt {
    let Some(paths) = status_paths(stdout) else {
        return Dirt::Code;
    };
    let is_prose = |path: &str| {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| PROSE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
    };
    if paths.is_empty() {
        Dirt::Clean
    } else if paths.iter().all(|p| is_prose(p)) {
        Dirt::ProseOnly
    } else {
        Dirt::Code
    }
}

/// Every file a Rust source in the repository compiles in through
/// `include_str!`/`include_bytes!`, as paths relative to the repository root
/// (the frame `git status` reports in).
///
/// Scans tracked and untracked-not-ignored `.rs` files, skipping `//` comment
/// lines. Two argument shapes resolve: a string literal (relative to the
/// including file, as rustc resolves it) and
/// `concat!(env!("CARGO_MANIFEST_DIR"), "<lit>")` (relative to the nearest
/// `Cargo.toml` above the including file). Any other argument - another
/// macro, a `const` - cannot be resolved without compiling, so the answer is
/// `None` and the caller runs everything: the shortcut is an inference from
/// evidence, and an include it cannot follow is missing evidence.
///
/// What this does not see: a `build.rs` or a test that reads markdown at run
/// time with `std::fs`. `--force-rust` is the answer there.
pub fn included_files(project_root: &Path) -> Option<HashSet<PathBuf>> {
    let top = git_stdout(project_root, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(top);
    let sources = git_paths(
        &top,
        &["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "*.rs"],
    )?;
    let mut out = HashSet::new();
    for rel in sources {
        let abs = top.join(&rel);
        let text = match std::fs::read_to_string(&abs) {
            Ok(t) => t,
            // Listed by the index but deleted in the working tree: it
            // includes nothing any more.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let dir = abs.parent()?;
        for arg in include_args(&text) {
            let target = match arg {
                IncludeArg::Relative(lit) => dir.join(lit),
                IncludeArg::ManifestRelative(lit) => {
                    manifest_dir(dir, &top)?.join(lit.trim_start_matches('/'))
                }
                IncludeArg::Unresolved => return None,
            };
            if let Ok(inside) = normalize(&target).strip_prefix(&top) {
                out.insert(inside.to_path_buf());
            }
        }
    }
    Some(out)
}

/// One `include_str!`/`include_bytes!` argument, as far as a text scan can
/// take it.
#[derive(Debug, PartialEq, Eq)]
enum IncludeArg {
    /// A string literal: relative to the including file's directory.
    Relative(String),
    /// `concat!(env!("CARGO_MANIFEST_DIR"), lit)`: relative to the package.
    ManifestRelative(String),
    /// Anything else.
    Unresolved,
}

/// The macros whose argument is a file compiled into the crate.
const INCLUDE_MACROS: [&str; 2] = ["include_str", "include_bytes"];

/// Every include invocation in `source`. A macro name not followed by an
/// opening delimiter (prose mentioning it, a string holding its name) is not
/// an invocation and is skipped; `//` comment lines are blanked first, since
/// doc comments routinely show `#![doc = ...]` examples.
fn include_args(source: &str) -> Vec<IncludeArg> {
    let code: Vec<&str> = source
        .lines()
        .map(|l| if l.trim_start().starts_with("//") { "" } else { l })
        .collect();
    let code = code.join("\n");
    let mut out = Vec::new();
    for mac in INCLUDE_MACROS {
        let needle = format!("{mac}!");
        let mut from = 0;
        while let Some(pos) = code[from..].find(&needle) {
            let start = from + pos;
            from = start + needle.len();
            // `my_include_str!` is some other macro.
            if code[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                continue;
            }
            // Rust allows comments in these gaps (`include_str! /* x */ (..)`);
            // missing such an include would wrongly allow the shortcut.
            let Some(rest) = skip_trivia(&code[from..]).strip_prefix(['(', '[', '{']) else {
                continue;
            };
            let rest = skip_trivia(rest);
            // An escaped quote: the invocation is text inside a string
            // literal (a test fixture, say), not code.
            if rest.starts_with('\\') {
                continue;
            }
            out.push(parse_include_arg(rest));
        }
    }
    out
}

/// `s` with leading whitespace and comments (`// ...` to end of line, and
/// `/* ... */`, nesting as Rust's do) removed. An unterminated block comment
/// swallows the rest.
fn skip_trivia(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if let Some(after) = s.strip_prefix("//") {
            s = after.find('\n').map_or("", |i| &after[i..]);
        } else if s.starts_with("/*") {
            let mut depth = 0usize;
            let mut i = 0;
            let bytes = s.as_bytes();
            while i < bytes.len() {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            s = s.get(i..).unwrap_or("");
        } else {
            return s;
        }
    }
}

fn parse_include_arg(s: &str) -> IncludeArg {
    if let Some((lit, _)) = string_literal(s) {
        return IncludeArg::Relative(lit);
    }
    if let Some(rest) = s.strip_prefix("concat!") {
        return manifest_concat(rest).map_or(IncludeArg::Unresolved, IncludeArg::ManifestRelative);
    }
    IncludeArg::Unresolved
}

/// The literal of `(env!("CARGO_MANIFEST_DIR"), "<lit>")` (the text after
/// `concat!`), and nothing looser: a third part would make the literal only a
/// piece of the path.
fn manifest_concat(s: &str) -> Option<String> {
    let s = s.trim_start().strip_prefix(['(', '[', '{'])?.trim_start();
    let s = s.strip_prefix("env!")?.trim_start();
    let s = s.strip_prefix(['(', '[', '{'])?.trim_start();
    let (var, s) = string_literal(s)?;
    if var != "CARGO_MANIFEST_DIR" {
        return None;
    }
    let s = s.trim_start().strip_prefix([')', ']', '}'])?.trim_start();
    let s = s.strip_prefix(',')?.trim_start();
    let (lit, s) = string_literal(s)?;
    let s = s.trim_start();
    let s = s.strip_prefix(',').map_or(s, str::trim_start);
    s.starts_with([')', ']', '}']).then_some(lit)
}

/// A leading Rust string literal (plain or raw) and the text after it. Plain
/// literals accept only the `\\` and `\"` escapes - nothing else belongs in a
/// path - so anything fancier reads as unresolvable rather than misread.
fn string_literal(s: &str) -> Option<(String, &str)> {
    if let Some(body) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = body.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => return Some((out, &body[i + 1..])),
                '\\' => match chars.next()?.1 {
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    _ => return None,
                },
                c => out.push(c),
            }
        }
        return None;
    }
    let after_r = s.strip_prefix('r')?;
    let hashes = after_r.len() - after_r.trim_start_matches('#').len();
    let body = after_r[hashes..].strip_prefix('"')?;
    let close = format!("\"{}", "#".repeat(hashes));
    let end = body.find(&close)?;
    Some((body[..end].to_owned(), &body[end + close.len()..]))
}

/// The nearest directory at or above `dir`, and inside `top`, holding a
/// `Cargo.toml` - what `CARGO_MANIFEST_DIR` is for a source file in it.
fn manifest_dir<'a>(dir: &'a Path, top: &Path) -> Option<&'a Path> {
    dir.ancestors()
        .take_while(|d| d.starts_with(top))
        .find(|d| d.join("Cargo.toml").is_file())
}

/// Resolve `.` and `..` lexically, the way a path joined onto a directory
/// reads - no filesystem access, so a not-yet-created target still resolves.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A git command's trimmed stdout, or `None` when it cannot be run or fails.
fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Order a diagnostic list for display.
///
/// Items whose path is in `unstaged` come first - an error in the file being
/// edited is the one to fix first - then every other item; each group keeps
/// its input order. `unstaged = None` (no git) leaves the order unchanged.
/// Nothing is ever dropped.
pub fn prioritize<T, F>(
    items: Vec<T>,
    get_path: F,
    unstaged: Option<&HashSet<PathBuf>>,
) -> Vec<T>
where
    F: Fn(&T) -> &Path,
{
    let (in_unstaged, elsewhere): (Vec<T>, Vec<T>) = match unstaged {
        Some(set) => items.into_iter().partition(|item| set.contains(get_path(item))),
        None => (Vec::new(), items),
    };
    in_unstaged.into_iter().chain(elsewhere).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn item(path: &str) -> (PathBuf, &str) {
        (p(path), path)
    }

    fn shown<'a>(ordered: &[(PathBuf, &'a str)]) -> Vec<&'a str> {
        ordered.iter().map(|t| t.1).collect()
    }

    /// The rule: an error in the file being edited comes first, the rest
    /// follow, and none of them is dropped.
    #[test]
    fn unstaged_errors_come_first() {
        let unstaged: HashSet<PathBuf> = ["b", "d"].iter().map(|s| p(s)).collect();
        let items = vec![item("a"), item("b"), item("c"), item("d"), item("e")];
        let ordered = prioritize(items, |t| t.0.as_path(), Some(&unstaged));
        assert_eq!(shown(&ordered), vec!["b", "d", "a", "c", "e"]);
    }

    /// An unstaged set that matches nothing leaves the input order alone.
    #[test]
    fn no_unstaged_match_keeps_the_input_order() {
        let unstaged: HashSet<PathBuf> = [p("z")].into_iter().collect();
        let items = vec![item("a"), item("b"), item("c"), item("d")];
        let ordered = prioritize(items, |t| t.0.as_path(), Some(&unstaged));
        assert_eq!(shown(&ordered), vec!["a", "b", "c", "d"]);
    }

    /// No git means no scope information, so the order is left untouched.
    #[test]
    fn no_git_keeps_the_input_order() {
        let items = vec![item("a"), item("b"), item("c")];
        let ordered = prioritize(items, |t| t.0.as_path(), None);
        assert_eq!(shown(&ordered), vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_status_is_a_clean_tree() {
        assert_eq!(classify_status(b""), Dirt::Clean);
    }

    #[test]
    fn markdown_only_changes_are_prose() {
        let status = b" M docs/guide.md\0?? notes/todo.markdown\0A  README.MD\0";
        assert_eq!(classify_status(status), Dirt::ProseOnly);
    }

    #[test]
    fn one_code_file_is_enough_to_make_it_code() {
        let status = b" M docs/guide.md\0 M src/lib.rs\0";
        assert_eq!(classify_status(status), Dirt::Code);
    }

    /// A path with no extension (`Makefile`, `justfile`) is not prose.
    #[test]
    fn extensionless_paths_are_code() {
        assert_eq!(classify_status(b" M Makefile\0"), Dirt::Code);
    }

    /// A rename record carries a second path, and it counts: moving a note on
    /// top of a source file is not a documentation edit.
    #[test]
    fn a_rename_weighs_both_of_its_paths() {
        let renamed = b"R  docs/b.md\0docs/a.md\0";
        assert_eq!(classify_status(renamed), Dirt::ProseOnly);

        let out_of_prose = b"R  src/lib.rs\0notes.md\0";
        assert_eq!(classify_status(out_of_prose), Dirt::Code);

        let into_prose = b"R  docs/a.md\0src/lib.rs\0";
        assert_eq!(classify_status(into_prose), Dirt::Code);
    }

    /// An invocation of `mac` with `arg`, built at run time so this file's own
    /// text never holds one - `included_files` scans it like any other source.
    fn inv(mac: &str, arg: &str) -> String {
        format!("{mac}!({arg})")
    }

    #[test]
    fn a_literal_include_resolves_relative_to_the_file() {
        let src = format!("const T: &str = {};\n", inv("include_str", "\"../docs/a.md\""));
        assert_eq!(include_args(&src), vec![IncludeArg::Relative("../docs/a.md".into())]);
        let raw = format!("#![doc = {}]\n", inv("include_bytes", "r#\"README.md\"#"));
        assert_eq!(include_args(&raw), vec![IncludeArg::Relative("README.md".into())]);
    }

    #[test]
    fn a_manifest_dir_concat_resolves_relative_to_the_package() {
        let arg = "concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/README.md\")";
        let src = inv("include_str", arg);
        assert_eq!(include_args(&src), vec![IncludeArg::ManifestRelative("/README.md".into())]);
        // A third part makes the literal only a fragment of the path.
        let three = inv("include_str", "concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/a\", \"/b.md\")");
        assert_eq!(include_args(&three), vec![IncludeArg::Unresolved]);
    }

    #[test]
    fn comments_between_macro_and_argument_are_skipped() {
        let m = "include_str";
        let block = format!("{m}! /* a /* nested */ note */ (\"a.md\")");
        assert_eq!(include_args(&block), vec![IncludeArg::Relative("a.md".into())]);
        let line = format!("{m}! // why\n(\"b.md\")");
        assert_eq!(include_args(&line), vec![IncludeArg::Relative("b.md".into())]);
        let inside = inv("include_bytes", "/* x */ \"c.md\"");
        assert_eq!(include_args(&inside), vec![IncludeArg::Relative("c.md".into())]);
    }

    #[test]
    fn an_argument_the_scan_cannot_follow_is_unresolved() {
        let src = inv("include_str", "some_path!()");
        assert_eq!(include_args(&src), vec![IncludeArg::Unresolved]);
    }

    #[test]
    fn comments_prose_and_other_macros_are_not_invocations() {
        let commented = format!("// {}\n", inv("include_str", "x"));
        assert!(include_args(&commented).is_empty());
        let prose = "the include_str! macro reads a file\n";
        assert!(include_args(prose).is_empty());
        let other = inv("my_include_str", "\"a.md\"");
        assert!(include_args(&other).is_empty());
        // Escaped quotes: the invocation is text inside a string literal.
        let quoted = format!("let s = \"{}\";", inv("include_str", "\\\"a.md\\\""));
        assert!(include_args(&quoted).is_empty());
    }

    #[test]
    fn normalize_resolves_parent_components_lexically() {
        assert_eq!(normalize(Path::new("/r/src/../docs/./a.md")), p("/r/docs/a.md"));
    }

    /// An origin path that happens to look like a status record must be read
    /// as a path, not re-parsed - otherwise its first three bytes vanish.
    #[test]
    fn an_origin_path_is_not_reparsed_as_a_record() {
        // Re-parsing would strip "xy." and leave an extensionless "md".
        let status = b"R  docs/b.md\0xy.md\0";
        assert_eq!(classify_status(status), Dirt::ProseOnly);
    }
}
