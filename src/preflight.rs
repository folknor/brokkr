use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::DevError;

/// A single requirement that must be satisfied before a subcommand runs.
pub enum Check {
    /// Binary must exist in PATH.
    Binary {
        name: &'static str,
        help: &'static str,
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

/// Query available disk space via `libc::statvfs`.
fn available_bytes(path: &Path) -> Option<u64> {
    let c_path = path_to_cstring(path)?;

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
        Err(_) => {
            return Some(format!(
                "{description}: could not read {path}"
            ));
        }
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

/// Convert a `PathBuf` to a `CString`, returning `None` if the path contains
/// interior nul bytes.
fn path_to_cstring(path: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).ok()
}

// ---------------------------------------------------------------------------
// SHA256 file verification with mtime cache
// ---------------------------------------------------------------------------

/// Verify that a file matches the expected SHA256 hash.
///
/// Results are cached in `{project_root}/.brokkr/sha256_cache` keyed on path,
/// mtime, and size. Re-hashing only happens when the file changes.
pub fn verify_file_hash(
    path: &Path,
    expected_hex: &str,
    project_root: &Path,
    origin: Option<&str>,
) -> Result<(), DevError> {
    let actual = cached_sha256(path, project_root)?;

    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        let mut msg = format!(
            "SHA256 mismatch for {}\n  expected: {expected_hex}\n  actual:   {actual}",
            path.display(),
        );
        if let Some(o) = origin {
            msg.push_str(&format!("\n  origin: {o}"));
        }
        Err(DevError::Preflight(vec![msg]))
    }
}

/// Return the SHA256 hex digest of a file, using the mtime cache when possible.
pub fn cached_sha256(path: &Path, project_root: &Path) -> Result<String, DevError> {
    let meta = std::fs::metadata(path)?;
    let mtime = file_mtime(&meta);
    let size = meta.len();

    let cache_dir = project_root.join(".brokkr");
    let cache_path = cache_dir.join("sha256_cache");

    // Check cache.
    if let Some(hit) = read_cache_entry(&cache_path, path, mtime, size) {
        return Ok(hit);
    }

    // Compute hash.
    let hex = compute_sha256(path)?;

    // Write to cache.
    std::fs::create_dir_all(&cache_dir)?;
    append_cache_entry(&cache_path, path, mtime, size, &hex);

    Ok(hex)
}

/// Compute SHA256 of a file, reading in 64 KB chunks.
fn compute_sha256(path: &Path) -> Result<String, DevError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let digest = hasher.finalize();
    Ok(hex_encode(&digest))
}

/// Encode bytes as lowercase hex.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
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

    // Best-effort write; don't fail the whole command if cache write fails.
    std::fs::write(cache_path, lines.join("\n") + "\n").ok();
}
