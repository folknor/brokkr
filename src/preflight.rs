use std::io::Read;
use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::Xxh3;

use crate::error::DevError;

/// A single requirement that must be satisfied before a subcommand runs.
///
/// Some variants (File, DiskSpace, KernelParam) are dispatched in `run_check`
/// but not yet constructed by any caller — they exist for future preflight checks.
#[allow(dead_code)]
pub enum Check {
    /// Binary must exist in PATH.
    Binary {
        name: String,
        help: String,
    },
    /// File must exist at path.
    File {
        path: PathBuf,
        description: String,
    },
    /// Minimum free disk space in bytes.
    DiskSpace {
        path: PathBuf,
        min_bytes: u64,
    },
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
        None => Some(format!(
            "could not check disk space at {}",
            path.display()
        )),
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
        // Not on Linux, or procfs not mounted — skip the check.
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
        // Not on Linux, or procfs not mounted — skip the check.
        Err(_) => return None,
    };

    let value: i32 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return Some(format!("{description}: could not parse {path}")),
    };

    if value <= max_value {
        None
    } else {
        Some(format!("{description}: {path} is {value}, need <= {max_value}"))
    }
}

fn check_rlimit(resource: libc::__rlimit_resource_t, min_bytes: u64, description: &str) -> Option<String> {
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
        Some(format!("{description}: current {cur_mb} MB, need >= {min_mb} MB"))
    }
}


// ---------------------------------------------------------------------------
// Convenience check sets
// ---------------------------------------------------------------------------

/// Preflight checks for sampling profilers (perf, samply).
///
/// Checks that `perf_event_paranoid` is permissive enough and that the tool
/// binary is installed.
pub fn profile_checks(tool: &str) -> Vec<Check> {
    let help = match tool {
        "perf" => "sudo apt install linux-tools-common linux-tools-$(uname -r)",
        "samply" => "cargo install samply",
        _ => "",
    };
    vec![
        Check::KernelParamAtMost {
            path: "/proc/sys/kernel/perf_event_paranoid",
            max_value: 1,
            description: "perf_event_paranoid must be <= 1 for profiling\n\
                          Fix: sudo sysctl -w kernel.perf_event_paranoid=1",
        },
        Check::Binary {
            name: tool.into(),
            help: help.into(),
        },
    ]
}

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

/// Return the XXH128 hex digest of a file, using the mtime cache when possible.
pub fn cached_xxh128(path: &Path, project_root: &Path) -> Result<String, DevError> {
    let meta = std::fs::metadata(path)?;
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
fn compute_xxh128(path: &Path) -> Result<String, DevError> {
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
        .filter(|line| {
            line.split('\t')
                .next()
                .is_none_or(|p| p != path_str)
        })
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
