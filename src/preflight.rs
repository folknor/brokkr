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
/// Results are cached in `{project_root}/.brokkr/hash_cache` keyed on the
/// canonical path and a nanosecond mtime/ctime + inode + size stamp (see the
/// hash cache section below). Re-hashing only happens when the file changes.
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
    let mut cache = HashCache::load(project_root);
    let hex = hash_file_cached(path, &meta, &mut cache)?;
    cache.save();
    Ok(hex)
}

/// One file's digest through `cache`: a hit when the file's stamp matches the
/// recorded one, otherwise a full read, recorded for the next caller.
fn hash_file_cached(
    path: &Path,
    meta: &std::fs::Metadata,
    cache: &mut HashCache,
) -> Result<String, DevError> {
    let key = cache_key(path);
    let stamp = FileStamp::of(meta);
    if let Some(key) = &key
        && let Some(hit) = cache.lookup(key, stamp)
    {
        return Ok(hit);
    }
    let hex = compute_xxh128(path)?;
    if let Some(key) = key {
        cache.record(key, stamp, &hex);
    }
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
/// Per-file digests go through the hash cache, so re-running over an
/// unchanged multi-gigabyte delivery is a stat per file rather than a re-read.
/// The cache is loaded once and saved once for the whole walk, not once per
/// file.
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
    let mut cache = HashCache::load(project_root);
    let walked = collect_tree_entries(root, root, &mut cache, &mut entries);
    // Saved even when the walk failed part-way: the digests it did compute
    // are correct and cost a full read each.
    cache.save();
    walked?;

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
    cache: &mut HashCache,
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
            collect_tree_entries(root, &path, cache, out)?;
        } else if meta.is_symlink() {
            let target = std::fs::read_link(&path)?;
            let mut hasher = Xxh3::new();
            hasher.update(b"symlink\0");
            hasher.update(target.as_os_str().as_encoded_bytes());
            out.push((rel, format!("{:032x}", hasher.digest128())));
        } else {
            // A regular file (or other non-directory, non-link): its
            // `symlink_metadata` is its metadata.
            out.push((rel, hash_file_cached(&path, &meta, cache)?));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The hash cache
// ---------------------------------------------------------------------------
//
// `{project_root}/.brokkr/hash_cache`, one line per file:
// `<canonical path>\t<mtime ns>\t<ctime ns>\t<inode>\t<size>\t<xxh128>`.
//
// The cache serves pinning - a hit is a claim that the bytes are the ones the
// digest describes - so every rule below errs toward a re-read:
//
// - The key is the canonical absolute path, so a relative and an absolute
//   spelling of one file share an entry. A path with a tab or line break in it
//   (or one that is not UTF-8) is simply not cached; it could not be written
//   back unambiguously.
// - The stamp is nanosecond mtime AND ctime, inode and size. ctime cannot be
//   set from userspace, so a rewrite that restores the old mtime (`touch -d`,
//   `rsync -t`, `cp -p`) still misses.
// - An entry is recorded only once the file has settled: if its mtime or ctime
//   is within `RACY_WINDOW_NS` of the moment it was hashed, a further write in
//   the same timestamp tick could leave the stamp unchanged with different
//   bytes (git's "racy index" problem, and the reason whole-second mtimes
//   served stale digests). A fresh file is hashed every time until it settles.
// - Writers serialise on `hash_cache.lock` (flock) and re-read the file under
//   it, merging their updates into what is on disk - so concurrent brokkr
//   processes no longer overwrite each other's entries.
// - Entries whose file no longer exists are pruned on every save, so the file
//   does not grow without bound.
// - A save happens once per top-level call, not once per miss, so digesting a
//   tree of N new files is one write rather than N rewrites of a growing file.
//
// Lines in any other shape (including the old four-field format) are dropped
// on read, which costs one re-hash per file after an upgrade.

/// How long after a file's last change its stamp is trusted to identify its
/// contents. Generous against coarse filesystem timestamp granularity.
const RACY_WINDOW_NS: i128 = 2_000_000_000;

/// A file's identity as the cache sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    mtime_ns: i128,
    ctime_ns: i128,
    ino: u64,
    size: u64,
}

impl FileStamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        let ns = |secs: i64, nsec: i64| i128::from(secs) * 1_000_000_000 + i128::from(nsec);
        Self {
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
            ino: meta.ino(),
            size: meta.len(),
        }
    }

    /// True when the last change is far enough in the past that another write
    /// would necessarily move the stamp. An unreadable clock is "not settled".
    fn is_settled(&self) -> bool {
        let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
            return false;
        };
        let Ok(now_ns) = i128::try_from(now.as_nanos()) else {
            return false;
        };
        now_ns - self.mtime_ns.max(self.ctime_ns) > RACY_WINDOW_NS
    }

    fn render(&self) -> String {
        format!("{}\t{}\t{}\t{}", self.mtime_ns, self.ctime_ns, self.ino, self.size)
    }
}

/// The cache key for `path`: its canonical absolute path, or `None` when it
/// cannot be stored unambiguously in the line format.
fn cache_key(path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let key = canonical.to_str()?.to_owned();
    if key.contains(['\t', '\n', '\r']) {
        return None;
    }
    Some(key)
}

/// Parse one cache line; `None` for anything not in the current format.
fn parse_cache_line(line: &str) -> Option<(String, FileStamp, String)> {
    let mut parts = line.split('\t');
    let key = parts.next()?.to_owned();
    let stamp = FileStamp {
        mtime_ns: parts.next()?.parse().ok()?,
        ctime_ns: parts.next()?.parse().ok()?,
        ino: parts.next()?.parse().ok()?,
        size: parts.next()?.parse().ok()?,
    };
    let hex = parts.next()?.to_owned();
    if parts.next().is_some() || key.is_empty() || hex.is_empty() {
        return None;
    }
    Some((key, stamp, hex))
}

/// Read every well-formed entry from the cache file. Missing or unreadable is
/// an empty cache: the cache is an optimisation, never a source of truth.
fn read_cache_file(path: &Path) -> std::collections::BTreeMap<String, (FileStamp, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(parse_cache_line)
        .map(|(key, stamp, hex)| (key, (stamp, hex)))
        .collect()
}

/// The hash cache for one top-level digest request: loaded once, consulted
/// and added to in memory, merged back to disk once by [`HashCache::save`].
struct HashCache {
    dir: PathBuf,
    entries: std::collections::BTreeMap<String, (FileStamp, String)>,
    updates: Vec<(String, FileStamp, String)>,
}

impl HashCache {
    fn load(project_root: &Path) -> Self {
        let dir = project_root.join(".brokkr");
        let entries = read_cache_file(&dir.join("hash_cache"));
        Self { dir, entries, updates: Vec::new() }
    }

    fn lookup(&self, key: &str, stamp: FileStamp) -> Option<String> {
        match self.entries.get(key) {
            Some((recorded, hex)) if *recorded == stamp => Some(hex.clone()),
            _ => None,
        }
    }

    /// Record a freshly computed digest, if the file has settled.
    fn record(&mut self, key: String, stamp: FileStamp, hex: &str) {
        if !stamp.is_settled() {
            return;
        }
        self.entries.insert(key.clone(), (stamp, hex.to_owned()));
        self.updates.push((key, stamp, hex.to_owned()));
    }

    /// Merge this request's new entries into the on-disk cache. Best-effort:
    /// a cache that cannot be written costs a re-hash next time, never a
    /// failed command.
    fn save(self) {
        if self.updates.is_empty() {
            return;
        }
        drop(self.try_save());
    }

    fn try_save(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        std::fs::create_dir_all(&self.dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("hash_cache.lock"))?;
        // SAFETY: `lock` is an open file descriptor owned for the duration of
        // the call; the lock is released when `lock` is dropped (closed).
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Re-read under the lock: another process may have saved since `load`.
        let cache_path = self.dir.join("hash_cache");
        let mut merged = read_cache_file(&cache_path);
        for (key, stamp, hex) in &self.updates {
            merged.insert(key.clone(), (*stamp, hex.clone()));
        }
        merged.retain(|key, _| Path::new(key).exists());

        let mut text = String::new();
        for (key, (stamp, hex)) in &merged {
            text.push_str(&format!("{key}\t{}\t{hex}\n", stamp.render()));
        }
        // Atomic replace so a reader outside the lock never sees a partial file.
        let written = crate::atomic_write::replace(&cache_path, text.as_bytes());
        drop(lock);
        written
    }
}

#[cfg(test)]
mod hash_cache_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Backdate a file's mtime so it counts as settled. Its ctime stays "now",
    /// which is what makes the settle check interesting: see below.
    fn backdate(path: &Path) {
        let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(an_hour_ago)
            .unwrap();
    }

    fn cache_text(root: &Path) -> String {
        std::fs::read_to_string(root.join(".brokkr/hash_cache")).unwrap_or_default()
    }

    #[test]
    fn a_just_written_file_is_not_cached() {
        let root = crate::test_scratch::scratch("preflight", "fresh_not_cached");
        let file = root.join("f.bin");
        std::fs::write(&file, b"fresh").unwrap();
        backdate(&file);
        // mtime is an hour old but ctime is now: a same-tick rewrite could
        // still leave the stamp unchanged, so nothing may be recorded yet.
        let hex = cached_xxh128(&file, &root).unwrap();
        assert_eq!(hex, compute_xxh128(&file).unwrap());
        assert!(!cache_text(&root).contains("f.bin"), "{}", cache_text(&root));
    }

    #[test]
    fn a_settled_entry_is_keyed_canonically_and_served() {
        let root = crate::test_scratch::scratch("preflight", "settled_served");
        let file = root.join("g.bin");
        std::fs::write(&file, b"settled").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        let mut stamp = FileStamp::of(&meta);
        // Pretend the change happened long ago so the entry is recordable.
        stamp.mtime_ns -= 10 * RACY_WINDOW_NS;
        stamp.ctime_ns -= 10 * RACY_WINDOW_NS;
        let mut cache = HashCache::load(&root);
        cache.record(cache_key(&file).unwrap(), stamp, "feedface");
        cache.save();

        let reloaded = HashCache::load(&root);
        // A relative-vs-absolute spelling resolves to one key.
        let key = cache_key(&root.join(".").join("g.bin")).unwrap();
        assert_eq!(reloaded.lookup(&key, stamp).as_deref(), Some("feedface"));
        // Any change to the stamp - here the real, current one - misses.
        assert_eq!(reloaded.lookup(&key, FileStamp::of(&meta)), None);
    }

    #[test]
    fn entries_for_deleted_files_are_pruned_on_save() {
        let root = crate::test_scratch::scratch("preflight", "pruned");
        let gone = root.join("gone.bin");
        let kept = root.join("kept.bin");
        std::fs::write(&gone, b"a").unwrap();
        std::fs::write(&kept, b"b").unwrap();
        let old = |p: &Path| {
            let mut s = FileStamp::of(&std::fs::metadata(p).unwrap());
            s.mtime_ns -= 10 * RACY_WINDOW_NS;
            s.ctime_ns -= 10 * RACY_WINDOW_NS;
            s
        };
        let mut cache = HashCache::load(&root);
        cache.record(cache_key(&gone).unwrap(), old(&gone), "aa");
        cache.save();
        std::fs::remove_file(&gone).unwrap();

        let mut cache = HashCache::load(&root);
        cache.record(cache_key(&kept).unwrap(), old(&kept), "bb");
        cache.save();
        let text = cache_text(&root);
        assert!(text.contains("kept.bin") && !text.contains("gone.bin"), "{text}");
    }

    #[test]
    fn malformed_and_old_format_lines_are_ignored() {
        assert!(parse_cache_line("/a\t1\t2\tdeadbeef").is_none());
        assert!(parse_cache_line("/a\tx\t1\t2\t3\tdeadbeef").is_none());
        assert!(parse_cache_line("/a\t1\t2\t3\t4\tdeadbeef\textra").is_none());
        let (key, stamp, hex) = parse_cache_line("/a\t1\t2\t3\t4\tdeadbeef").unwrap();
        assert_eq!((key.as_str(), hex.as_str()), ("/a", "deadbeef"));
        assert_eq!(stamp, FileStamp { mtime_ns: 1, ctime_ns: 2, ino: 3, size: 4 });
    }
}

#[cfg(test)]
mod tree_hash_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;

    /// Through the shared allocator, whose fixed per-test path is cleared on
    /// reuse - a pid-and-time-stamped name never repeats, so it is never
    /// cleared, and every run used to leave six more dirs in `target/`.
    fn tmpdir(name: &str) -> PathBuf {
        crate::test_scratch::scratch("preflight-tree", name)
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
