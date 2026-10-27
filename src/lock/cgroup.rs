//! The per-acquisition cgroup: an opportunistic accounting layer.
//!
//! Where the holder's own cgroup v2 directory is delegated to this user, a
//! fresh hold creates `brokkr-<acquisition id>` beneath it and moves the
//! holder in. Every process the holder forks from then on is born inside it,
//! wherever it later reparents, so `cpu.stat`'s `usage_usec` is the CPU of
//! everything born under the hold since entry - exited processes, double-forked
//! helpers and the holder itself included. That is what the process scan
//! cannot give: a scan sees live processes only.
//!
//! What the number is not: work done before entry, after the holder moved
//! out, or delegated elsewhere (a podman container placed by systemd) is not
//! in it, and a nested group's work is. `brokkr lock` labels it "cgroup CPU
//! since entry" and keeps it apart from the sampled-process figures. Memory is
//! `memory.current` when that file exists, labelled as cgroup-charged memory:
//! charges do not follow a migrated process, so it is not the holder's RSS.
//!
//! # When it is used
//!
//! Only on cgroup v2, only when `/proc/self/mountinfo` shows a cgroup2 mount
//! whose root is `/` (so the path in `/proc/self/cgroup` resolves under it
//! without a namespace offset), only when that directory is owned by this
//! user - systemd's delegation contract chowns a delegated subtree to its user,
//! and a directory merely writable by root is another manager's - and never
//! for root. A login session scope is not delegated, so on a plain ssh or tty
//! login this layer is simply absent; under the user manager (`systemd-run
//! --user`, a desktop app scope) it is present. Any failure before the
//! migration means no cgroup, silently.
//!
//! # Restoring
//!
//! Once the holder has moved, failure can no longer be silent: release writes
//! `0` to the parent's `cgroup.procs` (moving the whole process back) and
//! removes the group. If the holder has been moved elsewhere by someone else
//! it is left where it is. A failed restore warns once and sets a process-wide
//! flag; and a holder already inside a `brokkr-*` group never creates another,
//! so a stranded group cannot grow a nested chain of them.
//!
//! Every directory is reached through descriptors opened at setup, never by
//! re-resolving a saved path. Leftover `brokkr-*` groups from crashed holders
//! are removed bottom-up at the next setup in the same parent, and only groups
//! with brokkr's own name pattern are ever touched; `rmdir` alone decides
//! whether one is empty.
//!
//! The holder lives inside the group, so nothing may ever use the group's
//! `cgroup.kill` or `cgroup.freeze` to stop a run - it would stop the holder
//! before its cleanup.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::AcqId;

const PREFIX: &str = "brokkr-";
/// Set when a release could not move the holder back out of its group.
static STRANDED: AtomicBool = AtomicBool::new(false);

/// The hold's ownership of its group: restores the holder on drop.
pub(crate) struct HoldCgroup {
    parent: OwnedFd,
    name: CString,
    /// The holder's cgroup path inside the group, as `/proc/self/cgroup`
    /// prints it - how release tells "still ours" from "moved by someone else".
    inside: String,
}

/// Read-only access to the group's accounting files.
pub(crate) struct CgroupView {
    dir: OwnedFd,
    pub(crate) name: String,
    pub(crate) entered: Instant,
}

/// Create the group and move the holder into it, or `None` where the layer
/// does not apply.
pub(crate) fn enter(acq: &AcqId) -> Option<(HoldCgroup, CgroupView)> {
    if STRANDED.load(Ordering::Relaxed) || unsafe { libc::geteuid() } == 0 {
        return None;
    }
    let own_path = own_cgroup_path()?;
    if own_path
        .rsplit('/')
        .next()
        .is_some_and(|base| base.starts_with(PREFIX))
    {
        return None;
    }
    let mountpoint = cgroup2_mountpoint()?;
    let dir_path = format!("{}{}", mountpoint.trim_end_matches('/'), own_path);
    let parent = open_dir(None, &dir_path)?;
    if !owned_by_me(parent.as_raw_fd(), None)
        || !owned_by_me(parent.as_raw_fd(), Some("cgroup.procs"))
        || read_at(parent.as_raw_fd(), "cgroup.type")?.trim() != "domain"
    {
        return None;
    }
    sweep_leftovers(parent.as_raw_fd(), 0);

    let name_str = format!("{PREFIX}{}", acq.as_str());
    let name = CString::new(name_str.clone()).ok()?;
    // SAFETY: valid dirfd and NUL-terminated name.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
        return None;
    }
    let undo_mkdir = |parent: &OwnedFd| unsafe {
        libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
    };
    let Some(own) = open_dir(Some(parent.as_raw_fd()), &name_str) else {
        undo_mkdir(&parent);
        return None;
    };
    if read_at(own.as_raw_fd(), "cgroup.type").is_none_or(|t| t.trim() != "domain") {
        undo_mkdir(&parent);
        return None;
    }
    let inside = format!("{}/{name_str}", own_path.trim_end_matches('/'));
    if !write_at(own.as_raw_fd(), "cgroup.procs", b"0") {
        undo_mkdir(&parent);
        return None;
    }
    let hold = HoldCgroup {
        parent,
        name,
        inside: inside.clone(),
    };
    // Migrated: from here every exit path restores through `hold`'s drop.
    if own_cgroup_path().as_deref() != Some(inside.as_str()) {
        drop(hold);
        return None;
    }
    let view_fd = own.try_clone().ok()?;
    drop(own);
    Some((
        hold,
        CgroupView {
            dir: view_fd,
            name: name_str,
            entered: Instant::now(),
        },
    ))
}

impl Drop for HoldCgroup {
    fn drop(&mut self) {
        // A verified move elsewhere by someone else is not ours to undo. An
        // unreadable location is not a verified move: restore then too, and
        // report a failure like any other.
        let moved_elsewhere = own_cgroup_path().is_some_and(|now| now != self.inside);
        if !moved_elsewhere && !write_at(self.parent.as_raw_fd(), "cgroup.procs", b"0") {
            STRANDED.store(true, Ordering::Relaxed);
            crate::output::warn(&format!(
                "could not move brokkr back out of its accounting cgroup {}; later holds by \
                 this process skip the cgroup layer",
                self.inside
            ));
            return;
        }
        // SAFETY: valid dirfd and NUL-terminated name; EBUSY (processes the
        // hold leaked are still inside) leaves it for a later sweep.
        unsafe {
            libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), libc::AT_REMOVEDIR);
        }
    }
}

impl CgroupView {
    /// Cumulative CPU of everything charged to the group, microseconds.
    pub(crate) fn cpu_usec(&self) -> Option<u64> {
        let text = read_at(self.dir.as_raw_fd(), "cpu.stat")?;
        text.lines()
            .find_map(|line| line.strip_prefix("usage_usec "))
            .and_then(|v| v.trim().parse().ok())
    }

    /// `memory.current`, when the memory controller is enabled for the group.
    pub(crate) fn memory_bytes(&self) -> Option<u64> {
        read_at(self.dir.as_raw_fd(), "memory.current")?.trim().parse().ok()
    }

    /// Member PIDs of the group and its descendants, numbered in the holder's
    /// active PID namespace. Bounded.
    pub(crate) fn pids(&self) -> Vec<u32> {
        let mut out = Vec::new();
        collect_pids(self.dir.as_raw_fd(), 0, &mut out);
        out
    }
}

fn collect_pids(dir: RawFd, depth: usize, out: &mut Vec<u32>) {
    if depth > 8 || out.len() > 65_536 {
        return;
    }
    if let Some(text) = read_at(dir, "cgroup.procs") {
        out.extend(text.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    }
    for child in subdirs(dir) {
        if let Some(fd) = open_dir(Some(dir), &child) {
            collect_pids(fd.as_raw_fd(), depth + 1, out);
        }
    }
}

/// Remove leftover `brokkr-<hex>` groups under `dir`, bottom-up. Only groups
/// with brokkr's own name pattern are descended into or removed.
fn sweep_leftovers(dir: RawFd, depth: usize) {
    if depth > 4 {
        return;
    }
    for child in subdirs(dir) {
        let Some(id) = child.strip_prefix(PREFIX) else { continue };
        if AcqId::parse(id).is_none() {
            continue;
        }
        if let Some(fd) = open_dir(Some(dir), &child) {
            sweep_leftovers(fd.as_raw_fd(), depth + 1);
        }
        if let Ok(name) = CString::new(child) {
            // SAFETY: valid dirfd and NUL-terminated name. Failure (EBUSY,
            // a live process inside) leaves it.
            unsafe { libc::unlinkat(dir, name.as_ptr(), libc::AT_REMOVEDIR) };
        }
    }
}

/// Directory entries of `dir` that are directories.
fn subdirs(dir: RawFd) -> Vec<String> {
    let Ok(path) = std::fs::read_link(format!("/proc/self/fd/{dir}")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// The `0::` line of `/proc/self/cgroup`: this process's cgroup v2 path.
fn own_cgroup_path() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    parse_own_path(&text)
}

fn parse_own_path(text: &str) -> Option<String> {
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    if !path.starts_with('/') || path.split('/').any(|c| c == ".." || c == ".") {
        return None;
    }
    // A deleted cgroup reads with a suffix; never act on it.
    if path.ends_with("(deleted)") {
        return None;
    }
    Some(path.to_owned())
}

/// The mountpoint of a cgroup2 mount whose root is `/` - the only shape where
/// the path `/proc/self/cgroup` prints resolves directly beneath it.
fn cgroup2_mountpoint() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    parse_mountinfo(&text)
}

fn parse_mountinfo(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some((pre, post)) = line.split_once(" - ") else { continue };
        let Some(fstype) = post.split_whitespace().next() else { continue };
        if fstype != "cgroup2" {
            continue;
        }
        let fields: Vec<&str> = pre.split_whitespace().collect();
        let (Some(root), Some(mountpoint)) = (fields.get(3), fields.get(4)) else {
            continue;
        };
        if *root == "/" && !mountpoint.contains('\\') {
            return Some((*mountpoint).to_owned());
        }
    }
    None
}

fn open_dir(at: Option<RawFd>, path: &str) -> Option<OwnedFd> {
    let c = CString::new(path).ok()?;
    let flags = libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    // SAFETY: NUL-terminated path; dirfd valid when given.
    let fd = unsafe { libc::openat(at.unwrap_or(libc::AT_FDCWD), c.as_ptr(), flags) };
    if fd < 0 {
        return None;
    }
    // SAFETY: fresh descriptor we own.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn owned_by_me(dir: RawFd, file: Option<&str>) -> bool {
    let name = CString::new(file.unwrap_or("")).unwrap_or_default();
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let flags = if file.is_none() { libc::AT_EMPTY_PATH } else { 0 };
    // SAFETY: valid dirfd, NUL-terminated name, zeroed out-parameter.
    let rc = unsafe { libc::fstatat(dir, name.as_ptr(), &raw mut st, flags) };
    rc == 0 && st.st_uid == unsafe { libc::geteuid() }
}

fn read_at(dir: RawFd, file: &str) -> Option<String> {
    let c = CString::new(file).ok()?;
    // SAFETY: valid dirfd and NUL-terminated name.
    let fd = unsafe { libc::openat(dir, c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: fresh descriptor we own.
    let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    std::io::read_to_string(file).ok()
}

fn write_at(dir: RawFd, file: &str, bytes: &[u8]) -> bool {
    use std::io::Write;
    let Ok(c) = CString::new(file) else { return false };
    // SAFETY: valid dirfd and NUL-terminated name.
    let fd = unsafe { libc::openat(dir, c.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return false;
    }
    // SAFETY: fresh descriptor we own.
    let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    file.write_all(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_path_takes_the_v2_line_and_refuses_odd_paths() {
        assert_eq!(
            parse_own_path("12:cpu:/x\n0::/user.slice/a.scope\n").as_deref(),
            Some("/user.slice/a.scope")
        );
        assert_eq!(parse_own_path("0::/a/../b\n"), None);
        assert_eq!(parse_own_path("0::/a (deleted)\n"), None);
        assert_eq!(parse_own_path("1:name=systemd:/x\n"), None);
    }

    #[test]
    fn mountinfo_requires_a_cgroup2_mount_rooted_at_slash() {
        let host = "24 1 0:22 / /sys/fs/cgroup rw,nosuid - cgroup2 cgroup2 rw\n";
        assert_eq!(parse_mountinfo(host).as_deref(), Some("/sys/fs/cgroup"));
        let offset = "24 1 0:22 /user.slice /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n";
        assert_eq!(parse_mountinfo(offset), None);
        let none = "22 1 0:5 / /proc rw - proc proc rw\n";
        assert_eq!(parse_mountinfo(none), None);
    }
}
