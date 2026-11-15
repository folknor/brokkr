//! The bench-script registry check: `bench_scripts` is the directory every
//! bench script lives under, judged both ways from one resolved root
//! (`docs/brokkr.toml.bench.md`, `bench_scripts`). Every bench names a
//! `script_file` under it, never an inline `script`; and every `.lua` entry
//! under it must be a script some `[bench.NAME]` `script_file` names, a
//! module `bench_lib` loads, or covered by `bench_scripts_exempt`, so a new
//! script cannot exist unregistered. A contract-test file under
//! `bench_tests` needs no bench: the registry excuses exactly the files
//! [`discover_tests`] found, the same walk deciding both. Exemptions, tests
//! and the library excuse an entry from registration only, never a bench
//! from placement.
//!
//! A bench's script is judged by its [`Location`] at check time, the one
//! identity both directions and [`super::test_overlaps`] compare: the
//! loaded `script_file` keeps its joined spelling when its directory did not
//! resolve at load, and that directory may exist by the time the check runs.
//!
//! An entry is its location, the canonical directory it sits in plus its
//! name as spelled, never what it links to: a link `alias.lua` to a
//! registered script is an entry of its own and needs its own bench, and an
//! exemption covers what sits under it, not what links into it. The walk
//! holds each directory open and descends only real directories, opened
//! following no link; every other entry is judged by its name and type
//! alone and never opened, so a FIFO or a link named `x.lua` cannot stall or
//! redirect it. A linked directory is not descended, so a script reached
//! only through one is not discovered. Every failure to list or inspect an
//! entry is a finding: nothing is skipped because it could not be read.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use super::{FindingOwner, ScriptFinding, library};
use crate::config::bench::{Bench, BenchScript, Registry};

/// What the walk found at one entry, by its no-follow type.
enum Kind {
    Directory,
    Regular,
    Other,
}

/// Where a bench's `script_file` is now: its directory resolved on disk,
/// its name kept as spelled, the location identity the walk gives an entry.
pub(super) struct Location {
    pub(super) path: PathBuf,
    /// Whether the directory resolved. When it did not, `path` is the
    /// loaded path unchanged and its placement is not judged: a file whose
    /// directory cannot be resolved cannot be opened, so the same phase's
    /// unreadable-script finding already fails it.
    pub(super) resolved: bool,
}

pub(super) fn location(path: &Path) -> Location {
    let unresolved = || Location {
        path: path.to_path_buf(),
        resolved: false,
    };
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return unresolved();
    };
    match std::fs::canonicalize(dir) {
        Ok(dir) => Location {
            path: dir.join(name),
            resolved: true,
        },
        Err(_) => unresolved(),
    }
}

/// The contract-test files under `dir` (`bench_tests`): every regular
/// `.lua` file below it, found by the walk the registry uses, and every
/// `.lua` entry that is not a regular file, or that the walk could not
/// judge, as a finding. The one discovery both running the tests and the
/// registry's accounting of them use.
pub(super) fn discover_tests(dir: &Path) -> (Vec<PathBuf>, Vec<ScriptFinding>) {
    let Walk {
        entries,
        mut findings,
        ..
    } = walk(dir, &FindingOwner::Test);
    let mut tests = Vec::new();
    for (path, kind) in entries {
        if !path.as_os_str().as_bytes().ends_with(b".lua") {
            continue;
        }
        match kind {
            Kind::Regular => tests.push(path),
            _ => findings.push(ScriptFinding {
                owner: FindingOwner::Test,
                file: path,
                problem: "is not a regular file: a test file is one, never a link".into(),
            }),
        }
    }
    tests.sort();
    (tests, findings)
}

/// What [`walk`] found below one directory.
struct Walk {
    /// The directory resolved fully, the prefix of every entry's path;
    /// `None` when it could not be resolved, which `findings` then says.
    root: Option<PathBuf>,
    /// Every entry that is not a directory, by its location and no-follow
    /// type.
    entries: Vec<(PathBuf, Kind)>,
    findings: Vec<ScriptFinding>,
}

/// Every entry below `dir` that is not a directory, and every failure to
/// resolve, list or inspect one as a finding owned by `owner`.
fn walk(dir: &Path, owner: &FindingOwner) -> Walk {
    let mut findings = Vec::new();
    let mut found = Vec::new();
    let finding = |file: &Path, problem: String| ScriptFinding {
        owner: owner.clone(),
        file: file.to_path_buf(),
        problem,
    };
    // The root itself is resolved fully: the walk's paths start from it.
    let root = match std::fs::canonicalize(dir) {
        Ok(root) => root,
        Err(e) => {
            return Walk {
                root: None,
                entries: found,
                findings: vec![finding(dir, format!("cannot be resolved: {e}"))],
            };
        }
    };
    let held = match open_dir(None, root.as_os_str()) {
        Ok(Some(held)) => held,
        Ok(None) => {
            let findings = vec![finding(&root, "is not a directory".into())];
            return Walk {
                root: Some(root),
                entries: found,
                findings,
            };
        }
        Err(e) => {
            let findings = vec![finding(
                &root,
                format!("cannot be read as a directory: {e}"),
            )];
            return Walk {
                root: Some(root),
                entries: found,
                findings,
            };
        }
    };
    let mut stack = vec![(held, root.clone())];
    while let Some((dir, shown)) = stack.pop() {
        let (names, failures) = list(&dir);
        findings.extend(failures.into_iter().map(|problem| finding(&shown, problem)));
        for name in names {
            let path = shown.join(&name);
            let kind = match kind_at(&dir, &name) {
                Ok(Some(kind)) => kind,
                // Gone since the listing: the walk saw something it cannot
                // judge, so it says so rather than passing it.
                Ok(None) => {
                    findings.push(finding(
                        &path,
                        "vanished while the directory was read".into(),
                    ));
                    continue;
                }
                Err(e) => {
                    findings.push(finding(&path, format!("cannot be inspected: {e}")));
                    continue;
                }
            };
            if matches!(kind, Kind::Directory) {
                match open_dir(Some(&dir), &name) {
                    Ok(Some(child)) => stack.push((child, path)),
                    // Inspected as a directory, now a link, something else
                    // or gone: replaced under the walk.
                    Ok(None) => findings.push(finding(
                        &path,
                        "was replaced while the directory was read".into(),
                    )),
                    Err(e) => findings.push(finding(
                        &path,
                        format!("cannot be read as a directory: {e}"),
                    )),
                }
                continue;
            }
            found.push((path, kind));
        }
    }
    Walk {
        root: Some(root),
        entries: found,
        findings,
    }
}

/// `tests` are the contract-test files discovery found, which need no bench.
/// Every bench in the composed `benches` is held to the registry's root:
/// an inline script, or a `script_file` located outside it, is a finding
/// owned by the bench.
pub(super) fn check(
    registry: &Registry,
    benches: &BTreeMap<String, Bench>,
    tests: &[PathBuf],
) -> Vec<ScriptFinding> {
    let Walk {
        root,
        entries,
        mut findings,
    } = walk(&registry.dir, &FindingOwner::Registry);
    let shown_root = root.as_deref().unwrap_or(&registry.dir);
    let mut registered: BTreeSet<PathBuf> = BTreeSet::new();
    for bench in benches.values() {
        let owned = |file: &Path, problem: String| ScriptFinding {
            owner: FindingOwner::Bench(bench.name.clone()),
            file: file.to_path_buf(),
            problem,
        };
        match &bench.script {
            // Refused whether or not the root resolved: no root makes an
            // inline script one under it.
            BenchScript::Inline { declared_in, .. } => findings.push(owned(
                declared_in,
                format!(
                    "script is inline, but bench_scripts is set: every bench script is a \
                     script_file under {}",
                    shown_root.display()
                ),
            )),
            BenchScript::File { path, declared_in } => {
                let here = location(path);
                if here.resolved
                    && root
                        .as_deref()
                        .is_some_and(|root| !here.path.starts_with(root))
                {
                    findings.push(owned(
                        &here.path,
                        format!(
                            "script_file (set in {}) lies outside bench_scripts {}: every bench \
                             script lives under it",
                            declared_in.display(),
                            shown_root.display()
                        ),
                    ));
                }
                registered.insert(here.path);
            }
        }
    }
    let library = registry
        .library
        .as_deref()
        .and_then(|dir| std::fs::canonicalize(dir).ok());
    let excused = |path: &Path, kind: &Kind| {
        registered.contains(path)
            || tests.iter().any(|test| test == path)
            || registry
                .exempt
                .iter()
                .any(|exempt| path.starts_with(exempt))
            || (matches!(kind, Kind::Regular)
                && library.as_deref().is_some_and(|lib| {
                    path.strip_prefix(lib)
                        .is_ok_and(|relative| library::module_name(relative).is_some())
                }))
    };
    for (path, kind) in entries {
        if !path.as_os_str().as_bytes().ends_with(b".lua") || excused(&path, &kind) {
            continue;
        }
        findings.push(ScriptFinding {
            owner: FindingOwner::Registry,
            file: path,
            problem: "no [bench.NAME] script_file names it: register it as a bench, list it in \
                      bench_scripts_exempt, or put it under bench_tests as a contract test"
                .into(),
        });
    }
    findings.sort_by(|a, b| a.file.cmp(&b.file));
    findings
}

/// The sorted names in the directory `dir` holds, whatever its path names
/// now, and every failure to list it or one of its entries.
fn list(dir: &std::fs::File) -> (Vec<OsString>, Vec<String>) {
    let listing = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    let entries = match std::fs::read_dir(&listing) {
        Ok(entries) => entries,
        Err(e) => return (Vec::new(), vec![format!("cannot be listed: {e}")]),
    };
    let mut names = Vec::new();
    let mut failures = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.file_name()),
            Err(e) => failures.push(format!("an entry cannot be listed: {e}")),
        }
    }
    names.sort();
    (names, failures)
}

/// Open the directory `name` (inside `parent`, or as a path) following no
/// link: `None` when it is a link, not a directory, or gone.
fn open_dir(
    parent: Option<&std::fs::File>,
    name: &OsStr,
) -> std::io::Result<Option<std::fs::File>> {
    let c_name = std::ffi::CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
    let at = parent.map_or(libc::AT_FDCWD, std::os::fd::AsRawFd::as_raw_fd);
    // SAFETY: a valid descriptor (or AT_FDCWD) and C string for the call.
    let fd = unsafe {
        libc::openat(
            at,
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ELOOP | libc::ENOENT | libc::ENOTDIR) => Ok(None),
            _ => Err(error),
        };
    }
    // SAFETY: openat just returned this descriptor, owned by nobody else.
    Ok(Some(unsafe { std::fs::File::from_raw_fd(fd) }))
}

/// The type of `name` inside `dir`, not following a link: `None` when gone.
fn kind_at(dir: &std::fs::File, name: &OsStr) -> std::io::Result<Option<Kind>> {
    let c_name = std::ffi::CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a valid descriptor, C string and stat buffer for the call.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            _ => Err(error),
        };
    }
    // SAFETY: fstatat succeeded, so it filled the buffer.
    let mode = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
    Ok(Some(match mode {
        libc::S_IFDIR => Kind::Directory,
        libc::S_IFREG => Kind::Regular,
        _ => Kind::Other,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn file_bench(name: &str, path: PathBuf) -> (String, Bench) {
        let mut bench = super::super::tests::bench(BenchScript::File {
            path,
            declared_in: PathBuf::from("brokkr.toml"),
        });
        bench.name = name.to_owned();
        (name.to_owned(), bench)
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One tree, every way an entry is judged.
    fn every_unnamed_lua_entry_is_a_finding_and_nothing_else_is() {
        let scratch =
            std::fs::canonicalize(crate::test_scratch::scratch("bench_registry", "walk")).unwrap();
        let root = scratch.join("benches");
        std::fs::create_dir_all(scratch.join("outside")).unwrap();
        std::fs::write(scratch.join("outside/hidden.lua"), b"").unwrap();
        for dir in ["fam", "lib/io", "lib/bad-dir", "scratch"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "fam/named.lua",
            "fam/new.lua",
            "fam/README.md",
            "lib/util.lua",
            "lib/io/uring.lua",
            "lib/no-name.lua",
            "lib/bad-dir/x.lua",
            "scratch/try.lua",
        ] {
            std::fs::write(root.join(file), b"").unwrap();
        }
        std::os::unix::fs::symlink(root.join("fam/named.lua"), root.join("fam/alias.lua")).unwrap();
        std::os::unix::fs::symlink(root.join("fam/named.lua"), root.join("lib/linked.lua"))
            .unwrap();
        std::os::unix::fs::symlink(root.join("scratch/try.lua"), root.join("fam/via.lua")).unwrap();
        std::os::unix::fs::symlink(root.join("fam/named.lua"), root.join("fam/reg_link.lua"))
            .unwrap();
        // A linked directory is not descended.
        std::os::unix::fs::symlink(scratch.join("outside"), root.join("linkdir")).unwrap();

        let benches: BTreeMap<String, Bench> = [
            file_bench("named", root.join("fam/named.lua")),
            file_bench("reg_link", root.join("fam/reg_link.lua")),
        ]
        .into_iter()
        .collect();
        let registry = Registry {
            dir: root.clone(),
            exempt: vec![root.join("scratch")],
            library: Some(root.join("lib")),
        };
        let found: Vec<String> = check(&registry, &benches, &[root.join("scratch/try.lua")])
            .iter()
            .map(|f| f.file.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        assert_eq!(
            found,
            [
                "fam/alias.lua",
                "fam/new.lua",
                "fam/via.lua",
                "lib/bad-dir/x.lua",
                "lib/linked.lua",
                "lib/no-name.lua",
            ],
            "an entry is its location: links and loader-refused library files need a bench"
        );
        assert_eq!(
            check(&registry, &benches, &[])[0].to_string(),
            format!(
                "bench_scripts {}: no [bench.NAME] script_file names it: register it as a bench, \
                 list it in bench_scripts_exempt, or put it under bench_tests as a contract test",
                root.join("fam/alias.lua").display()
            )
        );
        // A test file discovery found needs no bench; one it did not find
        // (here the same name, unexempted) does.
        let unexempt = Registry {
            dir: root.clone(),
            exempt: Vec::new(),
            library: None,
        };
        let shown = |tests: &[PathBuf]| -> Vec<PathBuf> {
            check(&unexempt, &benches, tests)
                .into_iter()
                .map(|f| f.file)
                .collect()
        };
        assert!(shown(&[]).contains(&root.join("scratch/try.lua")));
        assert!(!shown(&[root.join("scratch/try.lua")]).contains(&root.join("scratch/try.lua")));
        let (tests, findings) = discover_tests(&root.join("fam"));
        assert_eq!(
            tests,
            [root.join("fam/named.lua"), root.join("fam/new.lua")],
            "only regular .lua files are tests"
        );
        let refused: Vec<&Path> = findings.iter().map(|f| f.file.as_path()).collect();
        assert_eq!(
            refused,
            [
                root.join("fam/alias.lua"),
                root.join("fam/reg_link.lua"),
                root.join("fam/via.lua")
            ],
            "a linked .lua under bench_tests is a finding, never a test"
        );
        let absent = Registry {
            dir: root.join("absent"),
            exempt: Vec::new(),
            library: None,
        };
        assert_eq!(
            check(&absent, &benches, &[]).len(),
            1,
            "a missing directory is a finding"
        );
    }

    /// The registry's findings owned by a bench, as (bench, file).
    fn placed(registry: &Registry, benches: &BTreeMap<String, Bench>) -> Vec<(String, PathBuf)> {
        check(registry, benches, &[])
            .into_iter()
            .filter_map(|f| match f.owner {
                FindingOwner::Bench(name) => Some((name, f.file)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_bench_script_is_a_script_file_under_bench_scripts() {
        let scratch =
            std::fs::canonicalize(crate::test_scratch::scratch("bench_registry", "placement"))
                .unwrap();
        let root = scratch.join("benches");
        std::fs::create_dir_all(root.join("fam")).unwrap();
        std::fs::create_dir_all(scratch.join("outside")).unwrap();
        for file in ["fam/inside.lua", "fam/dotted.lua"] {
            std::fs::write(root.join(file), b"").unwrap();
        }
        std::fs::write(scratch.join("outside/away.lua"), b"").unwrap();
        // A linked script named under the root is under it; a linked
        // directory leading out is not.
        std::os::unix::fs::symlink(scratch.join("outside/away.lua"), root.join("fam/link.lua"))
            .unwrap();
        std::os::unix::fs::symlink(scratch.join("outside"), root.join("linkdir")).unwrap();
        let mut inline = super::super::tests::bench(BenchScript::Inline {
            source: "function bench() end".into(),
            declared_in: scratch.join("brokkr.toml"),
        });
        inline.name = "inline".into();
        let benches: BTreeMap<String, Bench> = [
            file_bench("inside", root.join("fam/inside.lua")),
            // Through `..` and spelled unresolved, as a load whose directory
            // did not yet exist leaves it: judged where it is now.
            file_bench("dotted", root.join("fam/../fam/dotted.lua")),
            file_bench("link", root.join("fam/link.lua")),
            file_bench("away", scratch.join("outside/away.lua")),
            file_bench("via", root.join("linkdir/away.lua")),
            file_bench("missing", root.join("nowhere/x.lua")),
            ("inline".to_owned(), inline),
        ]
        .into_iter()
        .collect();
        // An exemption naming the outside directory excuses no bench.
        let registry = Registry {
            dir: root.clone(),
            exempt: vec![scratch.join("outside")],
            library: None,
        };
        let away = scratch.join("outside/away.lua");
        assert_eq!(
            placed(&registry, &benches),
            [
                ("inline".to_owned(), scratch.join("brokkr.toml")),
                ("away".to_owned(), away.clone()),
                ("via".to_owned(), away.clone()),
            ],
            "an inline script and a script_file located outside are the bench's findings; \
             an exemption excuses no bench, an unresolvable directory is left to the read"
        );
        assert!(
            check(&registry, &benches, &[])
                .iter()
                .all(|f| f.owner != FindingOwner::Registry),
            "a script_file spelled through `..` registers the entry it resolves to now"
        );
        let finding = check(&registry, &benches, &[])
            .into_iter()
            .find(|f| f.owner == FindingOwner::Bench("away".into()))
            .unwrap();
        assert_eq!(
            finding.to_string(),
            format!(
                "[bench.away] {}: script_file (set in brokkr.toml) lies outside bench_scripts {}: \
                 every bench script lives under it",
                away.display(),
                root.display()
            )
        );
        // With no root resolved an inline script is still refused, and no
        // script_file is judged against a root that is not there.
        let absent = Registry {
            dir: root.join("absent"),
            exempt: Vec::new(),
            library: None,
        };
        assert_eq!(
            placed(&absent, &benches),
            [("inline".to_owned(), scratch.join("brokkr.toml"))]
        );
        // The overlap of a test with a bench is judged by the same location.
        let overlaps =
            super::super::test_overlaps(&[root.join("fam/dotted.lua")], &benches, &BTreeSet::new());
        assert_eq!(
            overlaps,
            [ScriptFinding {
                owner: FindingOwner::Test,
                file: root.join("fam/dotted.lua"),
                problem: "is [bench.dotted]'s script_file: a test file is never a bench script"
                    .into(),
            }]
        );
    }
}
