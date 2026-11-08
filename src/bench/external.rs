//! External programs: what a `binary_with({program = NAME})` call runs
//! instead of the built snapshot.
//!
//! A name without a `/` is looked up on the child's own `PATH` (the call's
//! `env` override, else brokkr's), each entry relative to the child's working
//! directory when it is not absolute, an empty entry meaning that directory,
//! as a shell's lookup goes; a name with a `/` is a path, relative to the
//! working directory. The first regular file this process may execute wins,
//! and the child is launched by that absolute path through
//! `crate::process::spawn::spawn_exact`, which calls `execve` itself, so what
//! is recorded is what is executed: no second lookup, and no fallback to
//! another program or to the `/bin/sh` libc hands a file in no executable
//! format.
//!
//! Its identity is observed once resolved: the file's xxh3-128 digest as read
//! then, or why it could not be read. Opened without blocking and refused
//! unless a regular file, so a FIFO or a device in its place can never hang
//! the call. Evidence of what was there, not a pin: a file replaced between
//! the digest and the exec is not seen.

use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use super::store::ExecutableIdentity;

/// Why an external program could not be started: what was being done, and
/// what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartError {
    pub(crate) operation: &'static str,
    pub(crate) reason: String,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.operation, self.reason)
    }
}

/// Resolve `name` for a child running in `cwd` with `path` as its `PATH`
/// (`None` when it has none): the absolute path to execute.
pub(crate) fn resolve(name: &str, cwd: &Path, path: Option<&str>) -> Result<PathBuf, StartError> {
    resolve_spelled(name, cwd, path).map(|found| without_dots(&found))
}

/// `path` without its `.` components, so one file reached as `./tool` or
/// `bin/./tool` is recorded, and compared, as one path. `..` stays: through
/// a link it need not undo the component before it.
fn without_dots(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| *c != std::path::Component::CurDir)
        .collect()
}

fn resolve_spelled(name: &str, cwd: &Path, path: Option<&str>) -> Result<PathBuf, StartError> {
    if name.contains('/') {
        let candidate = cwd.join(name);
        return match executable(&candidate) {
            Ok(()) => Ok(candidate),
            Err(why) => Err(StartError {
                operation: "resolve",
                reason: format!("{}: {why}", candidate.display()),
            }),
        };
    }
    let Some(path) = path else {
        return Err(StartError {
            operation: "resolve",
            reason: format!("{name} cannot be looked up: the child has no PATH"),
        });
    };
    let mut refused: Vec<String> = Vec::new();
    for entry in path.split(':') {
        let dir = if entry.is_empty() {
            cwd.to_path_buf()
        } else {
            cwd.join(entry)
        };
        let candidate = dir.join(name);
        match executable(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(Refusal::Absent) => {}
            Err(why) => refused.push(format!("{} {why}", candidate.display())),
        }
    }
    let mut reason = format!("{name} is not on PATH ({path})");
    if !refused.is_empty() {
        reason.push_str(&format!("; found but not runnable: {}", refused.join(", ")));
    }
    Err(StartError {
        operation: "resolve",
        reason,
    })
}

/// Why a candidate is not one to execute.
enum Refusal {
    Absent,
    NotRegular,
    NotExecutable,
    Unreadable(std::io::Error),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => f.write_str("does not exist"),
            Self::NotRegular => f.write_str("is not a regular file"),
            Self::NotExecutable => f.write_str("is not executable by this user"),
            Self::Unreadable(e) => write!(f, "cannot be inspected: {e}"),
        }
    }
}

/// Whether `candidate` is a regular file this process may execute, through
/// any link, as exec would follow it. Whether its format runs is the exec's
/// to say (`crate::process::spawn::spawn_exact`).
fn executable(candidate: &Path) -> Result<(), Refusal> {
    let meta = match std::fs::metadata(candidate) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Refusal::Absent),
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => return Err(Refusal::Absent),
        Err(e) => return Err(Refusal::Unreadable(e)),
    };
    if !meta.is_file() {
        return Err(Refusal::NotRegular);
    }
    let Ok(c_path) = std::ffi::CString::new(candidate.as_os_str().as_bytes()) else {
        return Err(Refusal::Absent);
    };
    // SAFETY: a valid C string for the call's duration.
    if unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } != 0 {
        return Err(Refusal::NotExecutable);
    }
    Ok(())
}

/// The file at `path` as observed now: its digest, or why it could not be
/// read. Never blocks on a FIFO and never reads past a non-regular file.
pub(crate) fn observe(path: &Path) -> ExecutableIdentity {
    match digest(path) {
        Ok(digest) => ExecutableIdentity::Digest { digest },
        Err(reason) => ExecutableIdentity::Unread { reason },
    }
}

fn digest(path: &Path) -> Result<String, String> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("cannot open it: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot inspect it: {e}"))?;
    if !meta.is_file() {
        return Err("it is not a regular file".to_owned());
    }
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read it: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:032x}", hasher.digest128()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn script(path: &Path, mode: u32) {
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_name_is_looked_up_on_the_childs_path_and_a_path_is_taken_as_given() {
        let root = crate::test_scratch::scratch("bench_external", "resolve");
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        script(&first.join("tool"), 0o644);
        script(&second.join("tool"), 0o755);
        std::fs::create_dir_all(first.join("dir")).unwrap();

        let path = format!("{}:second", first.display());
        assert_eq!(
            resolve("tool", &root, Some(&path)).unwrap(),
            root.join("second").join("tool"),
            "a file that is not executable is passed over, a relative entry is the cwd's"
        );
        let missing = resolve("absent", &root, Some(&path)).unwrap_err();
        assert_eq!(missing.operation, "resolve");
        assert!(missing.reason.contains("not on PATH"), "{missing}");
        let only = resolve("tool", &root, Some(&first.display().to_string())).unwrap_err();
        assert!(
            only.reason.contains("not executable"),
            "a refusal names what was found: {only}"
        );
        assert!(resolve("tool", &root, None).is_err());
        assert_eq!(
            resolve("second/tool", &root, None).unwrap(),
            root.join("second/tool")
        );
        assert_eq!(
            resolve("./second/./tool", &root, None).unwrap(),
            root.join("second/tool"),
            "one file, one recorded path"
        );
        assert!(
            resolve("first/dir", &root, None)
                .unwrap_err()
                .reason
                .contains("not a regular file")
        );
    }

    #[test]
    fn an_observation_digests_a_regular_file_and_never_reads_a_fifo() {
        let root = crate::test_scratch::scratch("bench_external", "observe");
        let file = root.join("tool");
        script(&file, 0o755);
        let ExecutableIdentity::Digest { digest } = observe(&file) else {
            panic!("a regular file is digested");
        };
        assert_eq!(digest.len(), 32);
        assert_eq!(observe(&file), ExecutableIdentity::Digest { digest });

        let fifo = root.join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid C string for the call's duration.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let ExecutableIdentity::Unread { reason } = observe(&fifo) else {
            panic!("a FIFO is never digested");
        };
        assert!(reason.contains("not a regular file"), "{reason}");
        assert!(matches!(
            observe(&root.join("gone")),
            ExecutableIdentity::Unread { .. }
        ));
    }
}
