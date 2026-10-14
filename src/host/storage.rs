//! Filesystem and backing-device facts for paths brokkr may use.

use std::path::{Path, PathBuf};

/// Probed details for the filesystem holding a configured path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StorageInfo {
    pub label: String,
    pub mount_point: String,
    pub device: String,
    pub fstype: String,
    pub options: String,
    pub model: Option<String>,
    pub rotational: Option<bool>,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountEntry {
    device: String,
    mount_point: String,
    fstype: String,
    options: String,
}

/// Probe several labeled paths, returning only paths whose mount could be
/// identified.
pub(crate) fn probe_paths(paths: &[(&str, &Path)]) -> Vec<StorageInfo> {
    let mounts = read_mounts();
    paths
        .iter()
        .filter_map(|(label, path)| probe_storage(label, path, &mounts))
        .collect()
}

/// Probe the filesystem holding `path`, following symlinks and checking the
/// nearest existing ancestor. `None` means no containing mount was found; a
/// zero `total_bytes` means `statvfs` failed.
pub(crate) fn probe_path(label: &str, path: &Path) -> Option<StorageInfo> {
    probe_storage(label, path, &read_mounts())
}

fn probe_storage(label: &str, path: &Path, mounts: &[MountEntry]) -> Option<StorageInfo> {
    let existing = existing_ancestor(path)?;
    let canonical = std::fs::canonicalize(&existing).unwrap_or(existing);
    let mount = find_mount_for(&canonical, mounts)?;
    let (model, rotational) = block_device_info(&mount.device);
    let (free, total) = statvfs_bytes(&canonical).unwrap_or((0, 0));
    Some(StorageInfo {
        label: label.to_owned(),
        mount_point: mount.mount_point.clone(),
        device: mount.device.clone(),
        fstype: mount.fstype.clone(),
        options: mount.options.clone(),
        model,
        rotational,
        free_bytes: free,
        total_bytes: total,
    })
}

fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur: &Path = path;
    loop {
        if cur.exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

fn read_mounts() -> Vec<MountEntry> {
    let content = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    content.lines().filter_map(parse_mount_line).collect()
}

fn parse_mount_line(line: &str) -> Option<MountEntry> {
    let mut parts = line.split_whitespace();
    let device = parts.next()?.to_owned();
    let mount_point = unescape_mount_field(parts.next()?);
    let fstype = parts.next()?.to_owned();
    let options = parts.next()?.to_owned();
    Some(MountEntry {
        device,
        mount_point,
        fstype,
        options,
    })
}

/// `/proc/mounts` encodes spaces, tabs, newlines and backslashes as octal
/// escapes (for example `\040` for space).
fn unescape_mount_field(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let a = bytes[i + 1];
            let b = bytes[i + 2];
            let c = bytes[i + 3];
            if a.is_ascii_digit() && b.is_ascii_digit() && c.is_ascii_digit() {
                let val = (a - b'0') * 64 + (b - b'0') * 8 + (c - b'0');
                out.push(val as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn find_mount_for<'a>(path: &Path, mounts: &'a [MountEntry]) -> Option<&'a MountEntry> {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point))
        .max_by_key(|m| m.mount_point.len())
}

fn statvfs_bytes(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    let frsize = buf.f_frsize as u64;
    let total = (buf.f_blocks as u64).saturating_mul(frsize);
    let free = (buf.f_bavail as u64).saturating_mul(frsize);
    Some((free, total))
}

fn block_device_info(device: &str) -> (Option<String>, Option<bool>) {
    let Some(name) = device.strip_prefix("/dev/") else {
        return (None, None);
    };
    let parent = parent_block_device(name);
    let model = std::fs::read_to_string(format!("/sys/block/{parent}/device/model"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let rotational = std::fs::read_to_string(format!("/sys/block/{parent}/queue/rotational"))
        .ok()
        .and_then(|s| match s.trim() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        });
    (model, rotational)
}

/// Strip a partition suffix to get the underlying block device name.
fn parent_block_device(name: &str) -> String {
    if let Some(idx) = name.rfind('p')
        && idx > 0
        && !name[idx + 1..].is_empty()
        && name[idx + 1..].chars().all(|c| c.is_ascii_digit())
    {
        return name[..idx].to_owned();
    }
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.is_empty() {
        name.to_owned()
    } else {
        trimmed.to_owned()
    }
}
