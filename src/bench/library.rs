//! A project's bench library: the `bench_lib` directory a script's
//! `require(name)` loads modules from (`docs/commands/bench.md`, Modules).
//!
//! Read once when a run starts, every `.lua` file under the directory
//! compiled then, so an edit during a run changes nothing, as for the script
//! itself; a module that does not compile is kept as its error and fails
//! only the `require` that names it. A module's name is its path below the
//! directory without `.lua`, its directories joined by dots (`io/uring.lua`
//! is `io.uring`), every segment letters, digits and `_`, never a dot, so a
//! name names one file; a file whose path makes no such name, and every
//! symbolic link, is not a module. The scan holds each directory open and
//! opens what is inside it following no link, so nothing outside the
//! directory is ever read, even if a path is swapped while it runs.

use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use super::lua;
use crate::error::DevError;

/// The library as one run sees it.
pub(crate) struct Library {
    pub(crate) dir: PathBuf,
    pub(crate) modules: BTreeMap<String, Module>,
}

/// One module: its file, its exact source and what compiling it gave.
pub(crate) struct Module {
    pub(crate) path: PathBuf,
    pub(crate) source: Vec<u8>,
    pub(crate) compiled: Result<lua::Script, lua::CompileError>,
}

/// Whether `segment` is one segment of a module name, a directory or a
/// file stem under the library: letters, digits and `_`, never a dot, so a
/// name names exactly one file.
fn is_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether `name` is a module name: dot-separated segments.
pub(crate) fn is_module_name(name: &str) -> bool {
    name.split('.').all(is_segment)
}

impl Library {
    /// Read and compile every module under `dir`, through directory
    /// descriptors held open and opens that follow no link, so a path
    /// swapped for a link (or a FIFO) while the scan runs is never followed
    /// or read.
    pub(crate) fn read(dir: &Path) -> Result<Self, DevError> {
        let root = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(dir)
            .map_err(|e| {
                DevError::Config(format!(
                    "bench_lib {} cannot be read as a directory: {e}",
                    dir.display()
                ))
            })?;
        let mut modules = BTreeMap::new();
        collect(&root, dir, &mut Vec::new(), &mut modules)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            modules,
        })
    }
}

/// Open `name` inside the directory `parent` holds, following no link and
/// never blocking: `None` when it is a link or gone.
fn open_at(parent: &std::fs::File, name: &str) -> std::io::Result<Option<std::fs::File>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let c_name = std::ffi::CString::new(name).map_err(std::io::Error::other)?;
    // SAFETY: a valid descriptor and C string for the call's duration.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ELOOP | libc::ENOENT) => Ok(None),
            _ => Err(error),
        };
    }
    // SAFETY: openat just returned this descriptor, owned by nobody else.
    Ok(Some(unsafe { std::fs::File::from_raw_fd(fd) }))
}

fn collect(
    dir: &std::fs::File,
    shown: &Path,
    prefix: &mut Vec<String>,
    modules: &mut BTreeMap<String, Module>,
) -> Result<(), DevError> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;
    let unreadable = |path: &Path, e: std::io::Error| {
        DevError::Config(format!("bench_lib: {} cannot be read: {e}", path.display()))
    };
    // The names of the directory held open, whatever its path names now.
    let listing = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    let mut names: Vec<String> = std::fs::read_dir(&listing)
        .map_err(|e| unreadable(shown, e))?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        let path = shown.join(&name);
        let Some(file) = open_at(dir, &name).map_err(|e| unreadable(&path, e))? else {
            continue;
        };
        let meta = file.metadata().map_err(|e| unreadable(&path, e))?;
        if meta.is_dir() {
            if is_segment(&name) {
                prefix.push(name);
                collect(&file, &path, prefix, modules)?;
                prefix.pop();
            }
            continue;
        }
        let Some(stem) = name.strip_suffix(".lua") else {
            continue;
        };
        if !meta.is_file() || !is_segment(stem) {
            continue;
        }
        let mut source = Vec::new();
        (&file)
            .read_to_end(&mut source)
            .map_err(|e| unreadable(&path, e))?;
        let mut segments = prefix.clone();
        segments.push(stem.to_owned());
        let compiled = lua::load_script(&source, Some(&path.display().to_string()), None);
        modules.insert(
            segments.join("."),
            Module {
                path,
                source,
                compiled,
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn every_lua_file_with_a_module_name_is_a_module_and_nothing_else_is() {
        let root = crate::test_scratch::scratch("bench_library", "read");
        std::fs::create_dir_all(root.join("io")).unwrap();
        std::fs::create_dir_all(root.join("bad-dir")).unwrap();
        std::fs::create_dir_all(root.join("a.b")).unwrap();
        std::fs::write(root.join("a.b/c.lua"), b"return {}").unwrap();
        std::fs::write(root.join("x.y.lua"), b"return {}").unwrap();
        std::fs::write(root.join("preflight.lua"), b"return {}").unwrap();
        std::fs::write(root.join("io/uring.lua"), b"return {}").unwrap();
        std::fs::write(root.join("broken.lua"), b"return (").unwrap();
        std::fs::write(root.join("bad-dir/x.lua"), b"return {}").unwrap();
        std::fs::write(root.join("no-name.lua"), b"return {}").unwrap();
        std::fs::write(root.join("README.md"), b"no").unwrap();
        std::os::unix::fs::symlink(root.join("preflight.lua"), root.join("linked.lua")).unwrap();
        let library = Library::read(&root).unwrap();
        assert_eq!(
            library.modules.keys().cloned().collect::<Vec<_>>(),
            ["broken", "io.uring", "preflight"],
            "a dotted directory or stem would make a name two files could share"
        );
        assert!(library.modules["broken"].compiled.is_err());
        assert!(library.modules["io.uring"].compiled.is_ok());
        assert!(Library::read(&root.join("absent")).is_err());
        assert!(is_module_name("a.b_2") && !is_module_name("a..b") && !is_module_name("../x"));
    }
}
