use std::io::Read;
use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::Xxh3;

use crate::error::DevError;

/// A single requirement that must be satisfied before a subcommand runs.
///
/// Some variants (File, DiskSpace, KernelParam) are dispatched in `run_check`
/// but not yet constructed by any caller - they exist for future preflight checks.
#[allow(dead_code)]
pub enum Check {
    /// Binary must exist in PATH.
    Binary { name: String, help: String },
    /// File must exist at path.
    File { path: PathBuf, description: String },
    /// Minimum free disk space in bytes.
    DiskSpace { path: PathBuf, min_bytes: u64 },
    /// Read a /proc or /sys file and check it contains expected value.
    KernelParam {
        path: &'static str,
        expected: &'static str,
        description: &'static str,
    },
    /// Read an integer from /proc or /sys and check it is at most `max_value`.
    KernelParamAtMost {
        path: &'static str,
        max_value: i32,
        description: &'static str,
    },
    /// Resource limit (rlimit) must be at least `min_bytes`.
    Rlimit {
        resource: libc::__rlimit_resource_t,
        min_bytes: u64,
        description: &'static str,
    },
}

/// Run all checks, collecting failures. If any fail, return `DevError::Preflight`
/// with all failure messages (not just the first).
pub fn run_preflight(checks: &[Check]) -> Result<(), DevError> {
    let mut failures = Vec::new();

    for check in checks {
        if let Some(msg) = run_single(check) {
            failures.push(msg);
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(DevError::Preflight(failures))
    }
}

/// Run a single check. Returns `Some(message)` on failure, `None` on success.
fn run_single(check: &Check) -> Option<String> {
    match check {
        Check::Binary { name, help } => check_binary(name, help),
        Check::File { path, description } => check_file(path, description),
        Check::DiskSpace { path, min_bytes } => check_disk_space(path, *min_bytes),
        Check::KernelParam {
            path,
            expected,
            description,
        } => check_kernel_param(path, expected, description),
        Check::KernelParamAtMost {
            path,
            max_value,
            description,
        } => check_kernel_param_at_most(path, *max_value, description),
        Check::Rlimit {
            resource,
            min_bytes,
            description,
        } => check_rlimit(*resource, *min_bytes, description),
    }
}

fn check_binary(name: &str, help: &str) -> Option<String> {
    let result = std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => None,
        _ => Some(format!("'{name}' not found in PATH ({help})")),
    }
}

fn check_file(path: &Path, description: &str) -> Option<String> {
    if path.exists() {
        None
    } else {
        Some(format!("{description}: {}", path.display()))
    }
}

fn check_disk_space(path: &Path, min_bytes: u64) -> Option<String> {
    match available_bytes(path) {
        Some(avail) if avail >= min_bytes => None,
        Some(avail) => Some(format!(
            "insufficient disk space at {}: {} MB available, {} MB required",
            path.display(),
            avail / (1024 * 1024),
            min_bytes / (1024 * 1024),
        )),
        None => Some(format!("could not check disk space at {}", path.display())),
    }
}

///// Query available disk space via `libc::statvfs`.
fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };

    if ret != 0 {
        return None;
    }

    // f_bavail and f_frsize are both c_ulong (u64 on 64-bit Linux).
    Some(stat.f_bavail * stat.f_frsize)
}

fn check_kernel_param(path: &str, expected: &str, description: &str) -> Option<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        // Not on Linux, or procfs not mounted - skip the check.
        Err(_) => return None,
    };

    let trimmed = content.trim();
    if trimmed == expected {
        None
    } else {
        Some(format!(
            "{description}: expected '{expected}', got '{trimmed}' (in {path})"
        ))
    }
}

fn check_kernel_param_at_most(path: &str, max_value: i32, description: &str) -> Option<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        // Not on Linux, or procfs not mounted - skip the check.
        Err(_) => return None,
    };

    let value: i32 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return Some(format!("{description}: could not parse {path}")),
    };

    if value <= max_value {
        None
    } else {
        Some(format!(
            "{description}: {path} is {value}, need <= {max_value}"
        ))
    }
}

fn check_rlimit(
    resource: libc::__rlimit_resource_t,
    min_bytes: u64,
    description: &str,
) -> Option<String> {
    let mut rlim: libc::rlimit = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::getrlimit(resource, &mut rlim) };
    if ret != 0 {
        return Some(format!("{description}: could not read resource limit"));
    }
    if rlim.rlim_cur >= min_bytes {
        None
    } else {
        let cur_mb = rlim.rlim_cur / (1024 * 1024);
        let min_mb = min_bytes / (1024 * 1024);
        Some(format!(
            "{description}: current {cur_mb} MB, need >= {min_mb} MB"
        ))
    }
}

// ---------------------------------------------------------------------------
// Convenience check sets
// ---------------------------------------------------------------------------

/// Preflight checks for io_uring.
///
/// Four tunables can block io_uring:
/// 1. `/proc/sys/kernel/io_uring_disabled` must be 0 (upstream kill switch, kernel ≥6.6)
/// 2. `/proc/sys/kernel/apparmor_restrict_unprivileged_io_uring` must be 0 (Ubuntu/Debian)
/// 3. `/proc/sys/kernel/apparmor_restrict_unprivileged_userns` must be 0 (Ubuntu/Debian)
/// 4. `RLIMIT_MEMLOCK` >= 16 MB (for pinned ring buffers)
///
/// The kernel param checks pass when the file is absent (older kernels, non-Ubuntu).
pub fn uring_checks() -> Vec<Check> {
    vec![
        Check::KernelParamAtMost {
            path: "/proc/sys/kernel/io_uring_disabled",
            max_value: 0,
            description: "io_uring is disabled by kernel\n\
                          Fix: sudo sysctl -w kernel.io_uring_disabled=0",
        },
        Check::KernelParamAtMost {
            path: "/proc/sys/kernel/apparmor_restrict_unprivileged_io_uring",
            max_value: 0,
            description: "AppArmor restricts unprivileged io_uring\n\
                          Fix: sudo sysctl -w kernel.apparmor_restrict_unprivileged_io_uring=0",
        },
        Check::KernelParamAtMost {
            path: "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
            max_value: 0,
            description: "AppArmor restricts unprivileged user namespaces\n\
                          Fix: sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0",
        },
        Check::Rlimit {
            resource: libc::RLIMIT_MEMLOCK,
            min_bytes: 16 * 1024 * 1024,
            description: "RLIMIT_MEMLOCK too low for io_uring\n\
                          Fix: sudo prlimit --pid=$$ --memlock=unlimited:unlimited",
        },
    ]
}

// ---------------------------------------------------------------------------
// XXH128 file verification with mtime cache
// ---------------------------------------------------------------------------

/// Verify that a file matches the expected XXH128 hash.
///
/// Results are cached in `{project_root}/.brokkr/hash_cache` keyed on path,
/// mtime, and size. Re-hashing only happens when the file changes.
pub fn verify_file_hash(
    path: &Path,
    expected_hex: &str,
    project_root: &Path,
    origin: Option<&str>,
) -> Result<(), DevError> {
    let actual = cached_xxh128(path, project_root)?;

    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        let mut msg = format!(
            "hash mismatch for {}\n  expected: {expected_hex}\n  actual:   {actual}",
            path.display(),
        );
        if let Some(o) = origin {
            msg.push_str(&format!("\n  origin: {o}"));
        }
        Err(DevError::Preflight(vec![msg]))
    }
}

/// Return the XXH128 hex digest of a file or directory, using the mtime cache
/// when possible.
///
/// A DIRECTORY digests as the fold of its contents - see
/// [`compute_xxh128_tree`]. Some inputs are delivered as a directory rather
/// than a file (a Databento delivery is two `.csv.zst` archives beside three
/// JSON descriptors, and the consuming CLI takes the directory), and a pin that
/// could only name one file inside it would either leave the real input
/// unpinned or read as verified while covering a 4 KB descriptor.
pub fn cached_xxh128(path: &Path, project_root: &Path) -> Result<String, DevError> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        // Deliberately not cached against the directory's own mtime: a
        // directory's mtime tracks its entry list, not its contents, so a file
        // rewritten in place leaves it untouched and the cache would serve a
        // digest for data that no longer exists. The per-file caches inside the
        // fold are what keep a multi-gigabyte delivery from being re-read.
        return compute_xxh128_tree(path, project_root);
    }
    let mtime = file_mtime(&meta);
    let size = meta.len();

    let cache_dir = project_root.join(".brokkr");
    let cache_path = cache_dir.join("hash_cache");

    // Check cache.
    if let Some(hit) = read_cache_entry(&cache_path, path, mtime, size) {
        return Ok(hit);
    }

    // Compute hash.
    let hex = compute_xxh128(path)?;

    // Write to cache.
    std::fs::create_dir_all(&cache_dir)?;
    append_cache_entry(&cache_path, path, mtime, size, &hex);

    Ok(hex)
}

/// Compute XXH128 of a file, reading in 64 KB chunks.
pub(crate) fn compute_xxh128(path: &Path) -> Result<String, DevError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Xxh3::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let digest = hasher.digest128();
    Ok(format!("{digest:032x}"))
}

/// Compute the XXH128 digest of a directory tree.
///
/// The digest is a fold over every file beneath `root`, sorted by path relative
/// to `root`, of `<relative path>\0<file digest>\n`. Sorting is what makes it
/// reproducible - readdir order is a filesystem detail and varies between two
/// copies of identical data. The relative path is inside the fold so that
/// renaming a file, or two files swapping contents, changes the digest: a
/// delivery is its layout as well as its bytes.
///
/// Per-file digests go through [`cached_xxh128`], so re-running over an
/// unchanged multi-gigabyte delivery is a stat per file rather than a re-read.
///
/// Symlinks are recorded by their TARGET TEXT and never followed. Following
/// them would admit cycles and would silently pull in data from outside the
/// tree being pinned; the target string is what the delivery actually contains.
///
/// An empty tree is refused. A directory with no files in it is a wrong path
/// far more often than it is a real input, and a digest over nothing would
/// verify happily forever.
pub fn compute_xxh128_tree(root: &Path, project_root: &Path) -> Result<String, DevError> {
    let mut entries: Vec<(String, String)> = Vec::new();
    collect_tree_entries(root, root, project_root, &mut entries)?;

    if entries.is_empty() {
        return Err(DevError::Preflight(vec![format!(
            "{} is an empty directory - nothing to digest.\n  \
             A directory with no files is a wrong path more often than it is \
             an input, and a digest over nothing would verify forever.",
            root.display()
        )]));
    }

    entries.sort();

    let mut hasher = Xxh3::new();
    for (rel, digest) in &entries {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(digest.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.digest128();
    Ok(format!("{digest:032x}"))
}

/// Walk `dir`, pushing `(path relative to root, digest)` for every entry.
fn collect_tree_entries(
    root: &Path,
    dir: &Path,
    project_root: &Path,
    out: &mut Vec<(String, String)>,
) -> Result<(), DevError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // `symlink_metadata`, not `metadata`: the distinction between a symlink
        // and what it points at is the whole reason links are not followed.
        let meta = std::fs::symlink_metadata(&path)?;

        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();

        if meta.is_dir() {
            collect_tree_entries(root, &path, project_root, out)?;
        } else if meta.is_symlink() {
            let target = std::fs::read_link(&path)?;
            let mut hasher = Xxh3::new();
            hasher.update(b"symlink\0");
            hasher.update(target.as_os_str().as_encoded_bytes());
            out.push((rel, format!("{:032x}", hasher.digest128())));
        } else {
            out.push((rel, cached_xxh128(&path, project_root)?));
        }
    }
    Ok(())
}

/// Extract mtime as seconds since epoch from metadata.
fn file_mtime(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // mtime() returns i64; files with valid timestamps are non-negative.
    #[allow(clippy::cast_sign_loss)]
    let t = meta.mtime().max(0) as u64;
    t
}

/// Look up a cache entry matching path, mtime, and size.
fn read_cache_entry(cache_path: &Path, path: &Path, mtime: u64, size: u64) -> Option<String> {
    let contents = std::fs::read_to_string(cache_path).ok()?;
    let path_str = path.display().to_string();

    for line in contents.lines() {
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() == 4
            && parts[0] == path_str
            && parts[1] == mtime.to_string()
            && parts[2] == size.to_string()
        {
            return Some(parts[3].to_owned());
        }
    }
    None
}

/// Append a cache entry. Overwrites stale entries for the same path.
///
/// Uses atomic write (write to `.tmp`, then rename) to avoid races between
/// concurrent `brokkr env` processes.
fn append_cache_entry(cache_path: &Path, path: &Path, mtime: u64, size: u64, hex: &str) {
    let path_str = path.display().to_string();

    // Read existing entries, drop any for the same path (stale).
    let mut lines: Vec<String> = std::fs::read_to_string(cache_path)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.split('\t').next().is_none_or(|p| p != path_str))
        .map(String::from)
        .collect();

    lines.push(format!("{path_str}\t{mtime}\t{size}\t{hex}"));

    // Atomic write: write to a temp file in the same directory, then rename.
    // Rename is atomic on the same filesystem, preventing partial reads by
    // concurrent processes.
    let tmp_path = cache_path.with_extension("tmp");
    if std::fs::write(&tmp_path, lines.join("\n") + "\n").is_ok() {
        // Best-effort rename; don't fail the whole command if cache write fails.
        std::fs::rename(&tmp_path, cache_path).ok();
    }
}

#[cfg(test)]
mod tree_hash_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;

    /// Scratch dir under the crate's gitignored `target/` (project rules
    /// forbid `/tmp`).
    fn tmpdir(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("tree-{name}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a delivery-shaped tree: two archives beside their descriptors.
    fn delivery(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("mnq-0.csv.zst"), b"first archive").unwrap();
        std::fs::write(root.join("mnq-1.csv.zst"), b"second archive").unwrap();
        std::fs::write(root.join("manifest.json"), b"{\"files\":[]}").unwrap();
        std::fs::write(root.join("metadata.json"), b"{}").unwrap();
    }

    #[test]
    fn identical_trees_digest_identically() {
        let base = tmpdir("identical");
        let a = base.join("a");
        let b = base.join("b");
        delivery(&a);
        delivery(&b);
        // Two copies of the same delivery must agree despite living at
        // different paths and being read in whatever order readdir gives -
        // which is a filesystem detail, and is why the fold sorts.
        assert_eq!(
            compute_xxh128_tree(&a, &base).unwrap(),
            compute_xxh128_tree(&b, &base).unwrap()
        );
    }

    #[test]
    fn a_changed_file_changes_the_tree_digest() {
        let base = tmpdir("changed");
        let root = base.join("d");
        delivery(&root);
        let before = compute_xxh128_tree(&root, &base).unwrap();
        std::fs::write(root.join("mnq-0.csv.zst"), b"different archive").unwrap();
        assert_ne!(before, compute_xxh128_tree(&root, &base).unwrap());
    }

    #[test]
    fn a_renamed_file_changes_the_tree_digest() {
        let base = tmpdir("renamed");
        let root = base.join("d");
        delivery(&root);
        let before = compute_xxh128_tree(&root, &base).unwrap();
        std::fs::rename(root.join("mnq-0.csv.zst"), root.join("mnq-2.csv.zst")).unwrap();
        // Same bytes, different layout. A delivery is its layout too - the
        // consuming CLI resolves files inside it by name.
        assert_ne!(before, compute_xxh128_tree(&root, &base).unwrap());
    }

    #[test]
    fn nested_directories_are_included() {
        let base = tmpdir("nested");
        let root = base.join("d");
        delivery(&root);
        let before = compute_xxh128_tree(&root, &base).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/extra.json"), b"{}").unwrap();
        assert_ne!(before, compute_xxh128_tree(&root, &base).unwrap());
    }

    #[test]
    fn an_empty_tree_is_refused() {
        let base = tmpdir("empty");
        let root = base.join("d");
        std::fs::create_dir_all(&root).unwrap();
        let err = compute_xxh128_tree(&root, &base).unwrap_err();
        let DevError::Preflight(msgs) = err else {
            panic!("expected a preflight refusal, got {err:?}");
        };
        assert!(msgs.join(" ").contains("empty directory"), "{msgs:?}");
    }

    #[test]
    fn cached_xxh128_dispatches_on_file_versus_directory() {
        let base = tmpdir("dispatch");
        let root = base.join("d");
        delivery(&root);
        // The entry point the corpus registry and `brokkr env` both go through.
        let tree = cached_xxh128(&root, &base).unwrap();
        assert_eq!(tree, compute_xxh128_tree(&root, &base).unwrap());
        // A directory's digest is not any one file's digest - which is exactly
        // the failure mode of pinning `manifest.json` and calling the delivery
        // pinned.
        let manifest = cached_xxh128(&root.join("manifest.json"), &base).unwrap();
        assert_ne!(tree, manifest);
    }
}
