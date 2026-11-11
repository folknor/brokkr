//! The bench-script registry check: with `bench_scripts` set, every `.lua`
//! entry under that directory must be a script some `[bench.NAME]`
//! `script_file` names, a module `bench_lib` loads, or covered by
//! `bench_scripts_exempt`, so a new script cannot exist unregistered
//! (`docs/brokkr.toml.bench.md`, `bench_scripts`).
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

pub(super) fn check(registry: &Registry, benches: &BTreeMap<String, Bench>) -> Vec<ScriptFinding> {
    let mut findings = Vec::new();
    let finding = |file: &Path, problem: String| ScriptFinding {
        owner: FindingOwner::Registry,
        file: file.to_path_buf(),
        problem,
    };
    // The root itself is resolved fully: the walk's paths start from it.
    let root = match std::fs::canonicalize(&registry.dir) {
        Ok(root) => root,
        Err(e) => return vec![finding(&registry.dir, format!("cannot be resolved: {e}"))],
    };
    let held = match open_dir(None, root.as_os_str()) {
        Ok(Some(held)) => held,
        Ok(None) => return vec![finding(&root, "is not a directory".into())],
        Err(e) => {
            return vec![finding(
                &root,
                format!("cannot be read as a directory: {e}"),
            )];
        }
    };
    let registered: BTreeSet<&Path> = benches
        .values()
        .filter_map(|bench| match &bench.script {
            BenchScript::File { path, .. } => Some(path.as_path()),
            BenchScript::Inline { .. } => None,
        })
        .collect();
    let library = registry
        .library
        .as_deref()
        .and_then(|dir| std::fs::canonicalize(dir).ok());
    let excused = |path: &Path, kind: &Kind| {
        registered.contains(path)
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
    let mut stack = vec![(held, root)];
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
            if !name.as_bytes().ends_with(b".lua") || excused(&path, &kind) {
                continue;
            }
            findings.push(finding(
                &path,
                "no [bench.NAME] script_file names it: register it as a bench, or list it in \
                 bench_scripts_exempt"
                    .into(),
            ));
        }
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
        let found: Vec<String> = check(&registry, &benches)
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
            check(&registry, &benches)[0].to_string(),
            format!(
                "bench_scripts {}: no [bench.NAME] script_file names it: register it as a bench, \
                 or list it in bench_scripts_exempt",
                root.join("fam/alias.lua").display()
            )
        );
        let absent = Registry {
            dir: root.join("absent"),
            exempt: Vec::new(),
            library: None,
        };
        assert_eq!(
            check(&absent, &benches).len(),
            1,
            "a missing directory is a finding"
        );
    }
}
