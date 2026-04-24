use std::path::{Path, PathBuf};
use std::process::Command;

use std::collections::HashMap;

use crate::config::{Dataset, ResolvedPaths};
use crate::project::Project;

/// Environment information for the `dev env` subcommand.
pub struct EnvInfo {
    pub hostname: String,
    pub kernel: String,
    pub governor: String,
    pub memory_total_mb: u64,
    pub memory_available_mb: u64,
    pub io_uring_status: String,
    pub drives: Vec<(String, String)>,
    pub storage: Vec<StorageInfo>,
    pub tools: Vec<(String, String)>,
    pub datasets: Vec<(String, DatasetStatus)>,
}

/// Probed details for a configured storage path.
pub struct StorageInfo {
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

/// Whether a dataset PBF file exists on disk and passes hash verification.
pub enum DatasetStatus {
    /// File exists and hash matches.
    Verified,
    /// File exists but no hash is configured. Contains computed hash.
    Present(String),
    /// File exists but hash does not match. Contains actual hash.
    HashMismatch(String),
    /// File does not exist.
    Missing,
    /// Dataset has no files configured.
    NoFiles,
}

/// Collect all environment information.
pub fn collect(paths: &ResolvedPaths, project: Project, project_root: &Path) -> EnvInfo {
    let (mem_total, mem_avail) = read_memory();

    EnvInfo {
        hostname: paths.hostname.clone(),
        kernel: read_kernel(),
        governor: read_governor(),
        memory_total_mb: mem_total,
        memory_available_mb: mem_avail,
        io_uring_status: read_io_uring_status(),
        drives: collect_drives(paths),
        storage: collect_storage(paths),
        tools: collect_tools(project),
        datasets: check_datasets(&paths.datasets, &paths.data_dir, project_root),
    }
}

/// Print environment info in formatted output.
pub fn print(info: &EnvInfo) {
    print_header(info);
    print_drives(info);
    print_storage(info);
    print_tools(info);
    print_datasets(info);
}

fn print_header(info: &EnvInfo) {
    println!("{:<12} {}", "hostname:", info.hostname);
    println!("{:<12} {}", "kernel:", info.kernel);
    println!("{:<12} {}", "governor:", info.governor);
    println!(
        "{:<12} {} GB ({} GB available)",
        "memory:",
        info.memory_total_mb / 1024,
        info.memory_available_mb / 1024,
    );
    println!("{:<12} {}", "io_uring:", info.io_uring_status);
}

fn print_drives(info: &EnvInfo) {
    let parts: Vec<String> = info
        .drives
        .iter()
        .map(|(label, dtype)| format!("{label}={dtype}"))
        .collect();
    println!("{:<12} {}", "drives:", parts.join("  "));
}

fn print_storage(info: &EnvInfo) {
    if info.storage.is_empty() {
        return;
    }
    let label_width = info
        .storage
        .iter()
        .map(|s| s.label.len())
        .max()
        .unwrap_or(0);
    let mut prev_key: Option<(String, String)> = None;
    for (i, s) in info.storage.iter().enumerate() {
        let header = if i == 0 { "storage:" } else { "" };
        let key = (s.mount_point.clone(), s.device.clone());
        let body = if prev_key.as_ref() == Some(&key) {
            "(same mount)".to_owned()
        } else {
            format_storage_row(s)
        };
        println!(
            "{:<12} {:<width$}  {}",
            header,
            format!("{}:", s.label),
            body,
            width = label_width + 1,
        );
        prev_key = Some(key);
    }
}

fn format_storage_row(s: &StorageInfo) -> String {
    let model = match (s.model.as_deref(), s.rotational) {
        (Some(m), Some(false)) => format!("{m} (SSD)"),
        (Some(m), Some(true)) => format!("{m} (HDD)"),
        (Some(m), None) => m.to_owned(),
        (None, Some(false)) => "SSD".to_owned(),
        (None, Some(true)) => "HDD".to_owned(),
        (None, None) => "unknown device".to_owned(),
    };
    format!(
        "{}  {}  {} {}  {}  {} free / {}",
        s.mount_point,
        s.device,
        s.fstype,
        s.options,
        model,
        format_bytes(s.free_bytes),
        format_bytes(s.total_bytes),
    )
}

fn print_tools(info: &EnvInfo) {
    let parts: Vec<String> = info
        .tools
        .iter()
        .map(|(name, ver)| format!("{name} {ver}"))
        .collect();
    println!("{:<12} {}", "tools:", parts.join("  "));
}

fn print_datasets(info: &EnvInfo) {
    for (i, (name, status)) in info.datasets.iter().enumerate() {
        let label = if i == 0 { "datasets:" } else { "" };
        println!("{:<12} {}", label, format_dataset(name, status));
    }
}

fn format_dataset(name: &str, status: &DatasetStatus) -> String {
    match status {
        DatasetStatus::Verified => format!("{name} \u{2713}"),
        DatasetStatus::Present(hash) => {
            format!("{name} \u{2713} (no hash configured, actual: {hash})")
        }
        DatasetStatus::HashMismatch(hash) => {
            format!("{name} \u{2717} (hash mismatch, actual: {hash})")
        }
        DatasetStatus::Missing => format!("{name} \u{2717} (missing)"),
        DatasetStatus::NoFiles => format!("{name} (no files configured)"),
    }
}

// ---------------------------------------------------------------------------
// System readers
// ---------------------------------------------------------------------------

/// Read the kernel version from `/proc/version`.
fn read_kernel() -> String {
    let content = match std::fs::read_to_string("/proc/version") {
        Ok(s) => s,
        Err(_) => return "unknown".to_owned(),
    };

    // Format: "Linux version 6.18.0-9-generic ..."
    // We want the third whitespace-delimited word (the version number).
    extract_kernel_version(&content)
}

fn extract_kernel_version(content: &str) -> String {
    content
        .split_whitespace()
        .nth(2)
        .unwrap_or("unknown")
        .to_owned()
}

/// Read the CPU frequency governor.
fn read_governor() -> String {
    read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
}

/// Read total and available memory from `/proc/meminfo`, returning MB values.
pub(crate) fn read_memory() -> (u64, u64) {
    let content = match std::fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_) => return (0, 0),
    };

    let total = parse_meminfo_field(&content, "MemTotal:");
    let avail = parse_meminfo_field(&content, "MemAvailable:");
    (total, avail)
}

/// Find a line starting with `prefix` in meminfo content and parse the kB
/// value, returning megabytes.
fn parse_meminfo_field(content: &str, prefix: &str) -> u64 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return parse_kb_to_mb(rest);
        }
    }
    0
}

/// Parse a meminfo value like "  32637372 kB" into megabytes.
fn parse_kb_to_mb(rest: &str) -> u64 {
    let kb: u64 = rest
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    kb / 1024
}

/// Check io_uring support and memlock limit.
///
/// Checks the same kernel parameters as `preflight::uring_checks()`:
/// kill switch, AppArmor io_uring restriction, and AppArmor userns restriction.
fn read_io_uring_status() -> String {
    let memlock = read_memlock_limit();

    if let Some(reason) = check_uring_blocked() {
        return format!("disabled: {reason} ({memlock})");
    }

    format!("supported ({memlock})")
}

/// Check if io_uring is blocked by any kernel parameter.
/// Returns `Some(reason)` if blocked, `None` if all checks pass.
fn check_uring_blocked() -> Option<&'static str> {
    if kernel_param_nonzero("/proc/sys/kernel/io_uring_disabled") {
        return Some("kernel kill switch");
    }
    if kernel_param_nonzero("/proc/sys/kernel/apparmor_restrict_unprivileged_io_uring") {
        return Some("AppArmor io_uring restriction");
    }
    if kernel_param_nonzero("/proc/sys/kernel/apparmor_restrict_unprivileged_userns") {
        return Some("AppArmor userns restriction");
    }
    None
}

/// Check if a kernel parameter file exists and has a non-zero value.
fn kernel_param_nonzero(path: &str) -> bool {
    match std::fs::read_to_string(path) {
        Ok(content) => content.trim() != "0",
        Err(_) => false,
    }
}

/// Read RLIMIT_MEMLOCK and format it.
fn read_memlock_limit() -> String {
    let mut rlim: libc::rlimit = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut rlim) };

    if ret != 0 {
        return "memlock=unknown".to_owned();
    }

    format_memlock(rlim.rlim_cur)
}

fn format_memlock(cur: u64) -> String {
    if cur == libc::RLIM_INFINITY {
        "memlock=unlimited".to_owned()
    } else {
        format!("memlock={} KB", cur / 1024)
    }
}

// ---------------------------------------------------------------------------
// Drives
// ---------------------------------------------------------------------------

fn collect_drives(paths: &ResolvedPaths) -> Vec<(String, String)> {
    match &paths.drives {
        Some(d) => {
            let mut out = Vec::with_capacity(4);
            push_drive(&mut out, "source", d.source.as_deref());
            push_drive(&mut out, "data", d.data.as_deref());
            push_drive(&mut out, "scratch", d.scratch.as_deref());
            push_drive(&mut out, "target", d.target.as_deref());
            out
        }
        None => vec![("all".to_owned(), "unknown".to_owned())],
    }
}

fn push_drive(out: &mut Vec<(String, String)>, label: &str, value: Option<&str>) {
    let dtype = value.unwrap_or("unknown");
    out.push((label.to_owned(), dtype.to_owned()));
}

// ---------------------------------------------------------------------------
// Storage probing
// ---------------------------------------------------------------------------

struct MountEntry {
    device: String,
    mount_point: String,
    fstype: String,
    options: String,
}

fn collect_storage(paths: &ResolvedPaths) -> Vec<StorageInfo> {
    let mounts = read_mounts();
    let candidates: [(&str, &Path); 3] = [
        ("data", &paths.data_dir),
        ("scratch", &paths.scratch_dir),
        ("target", &paths.target_dir),
    ];
    candidates
        .into_iter()
        .filter_map(|(label, path)| probe_storage(label, path, &mounts))
        .collect()
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
/// escapes (e.g. `\040` for space). Decode them so path matching works.
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
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if ret != 0 {
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
/// `nvme0n1p1` -> `nvme0n1`, `mmcblk0p1` -> `mmcblk0`, `sda1` -> `sda`.
fn parent_block_device(name: &str) -> String {
    if let Some(idx) = name.rfind('p')
        && idx > 0
        && name[idx + 1..].chars().all(|c| c.is_ascii_digit())
        && !name[idx + 1..].is_empty()
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

fn format_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if b >= TB {
        format!("{:.1}T", b as f64 / TB as f64)
    } else if b >= GB {
        format!("{}G", b / GB)
    } else if b >= MB {
        format!("{}M", b / MB)
    } else if b >= KB {
        format!("{}K", b / KB)
    } else {
        format!("{b}B")
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn collect_tools(project: Project) -> Vec<(String, String)> {
    let mut tools = vec![(
        "cargo".to_owned(),
        read_tool_version("cargo", &["--version"]),
    )];

    match project {
        Project::Pbfhogg => {
            tools.push((
                "osmium".to_owned(),
                read_tool_version("osmium", &["--version"]),
            ));
        }
        Project::Elivagar => {
            tools.push((
                "samply".to_owned(),
                read_tool_version("samply", &["--version"]),
            ));
        }
        Project::Nidhogg => {
            tools.push(("curl".to_owned(), read_tool_version("curl", &["--version"])));
        }
        Project::Litehtml => {
            tools.push(("node".to_owned(), read_tool_version("node", &["--version"])));
        }
        Project::Sluggrs | Project::Brokkr | Project::Other(_) => {}
    }

    tools.push((project.name().to_owned(), read_git_rev()));

    tools
}

/// Run a command and extract the version from its first line of stdout.
fn read_tool_version(name: &str, args: &[&str]) -> String {
    let output = match Command::new(name).args(args).output() {
        Ok(o) => o,
        Err(_) => return "not found".to_owned(),
    };

    if !output.status.success() {
        return "not found".to_owned();
    }

    extract_version_from_stdout(&output.stdout)
}

fn extract_version_from_stdout(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let first_line = text.lines().next().unwrap_or("unknown");
    // Find the first word that starts with a digit (the version number).
    // Handles "cargo 1.95.0-nightly (...)" and "osmium version 1.19.0".
    first_line
        .split_whitespace()
        .find(|w| w.as_bytes().first().is_some_and(u8::is_ascii_digit))
        .unwrap_or("unknown")
        .to_owned()
}

/// Get the current git short rev for the project.
fn read_git_rev() -> String {
    let output = match Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return "unknown".to_owned(),
    };

    if !output.status.success() {
        return "unknown".to_owned();
    }

    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

// ---------------------------------------------------------------------------
// Datasets
// ---------------------------------------------------------------------------

fn check_datasets(
    datasets: &HashMap<String, Dataset>,
    data_dir: &Path,
    project_root: &Path,
) -> Vec<(String, DatasetStatus)> {
    let mut out: Vec<(String, DatasetStatus)> = Vec::new();

    for (name, ds) in datasets {
        check_file_entries(&mut out, name, &ds.pbf, "", data_dir, project_root);
        check_file_entries(&mut out, name, &ds.osc, "osc.", data_dir, project_root);
        check_file_entries(
            &mut out,
            name,
            &ds.pmtiles,
            "pmtiles.",
            data_dir,
            project_root,
        );

        if ds.pbf.is_empty() && ds.osc.is_empty() && ds.pmtiles.is_empty() {
            out.push((name.clone(), DatasetStatus::NoFiles));
        }
    }

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Check all entries in a file map and push their status to `out`.
fn check_file_entries<E: crate::resolve::FileEntry>(
    out: &mut Vec<(String, DatasetStatus)>,
    dataset_name: &str,
    entries: &HashMap<String, E>,
    prefix: &str,
    data_dir: &Path,
    project_root: &Path,
) {
    for (key, entry) in entries {
        let label = format!("{dataset_name}/{prefix}{key}");
        let path = data_dir.join(entry.file());
        let status = if !path.exists() {
            DatasetStatus::Missing
        } else {
            check_hash_status(&path, entry.xxhash(), project_root)
        };
        out.push((label, status));
    }
}

fn check_hash_status(path: &Path, expected: Option<&str>, project_root: &Path) -> DatasetStatus {
    let hash = crate::preflight::cached_xxh128(path, project_root)
        .unwrap_or_else(|_| String::from("error"));

    match expected {
        None => DatasetStatus::Present(hash),
        Some(hex) if hash.eq_ignore_ascii_case(hex) => DatasetStatus::Verified,
        Some(_) => DatasetStatus::HashMismatch(hash),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a file and return its trimmed contents, or "unknown" on error.
fn read_trimmed(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().to_owned(),
        Err(_) => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;

    // -----------------------------------------------------------------------
    // extract_version_from_stdout
    // -----------------------------------------------------------------------

    #[test]
    fn version_from_cargo_output() {
        let stdout = b"cargo 1.95.0-nightly (abc123 2025-01-01)";
        assert_eq!(extract_version_from_stdout(stdout), "1.95.0-nightly");
    }

    #[test]
    fn version_from_osmium_output() {
        let stdout = b"osmium version 1.19.0\n";
        assert_eq!(extract_version_from_stdout(stdout), "1.19.0");
    }

    #[test]
    fn version_from_samply_output() {
        let stdout = b"samply 0.12.0";
        assert_eq!(extract_version_from_stdout(stdout), "0.12.0");
    }

    #[test]
    fn version_from_empty_output() {
        assert_eq!(extract_version_from_stdout(b""), "unknown");
    }

    #[test]
    fn version_from_no_digit_word() {
        // No word starts with a digit - should fall back to "unknown".
        let stdout = b"some tool running fine";
        assert_eq!(extract_version_from_stdout(stdout), "unknown");
    }

    #[test]
    fn version_picks_first_digit_word_not_later() {
        // Two digit-words: should return the first one.
        let stdout = b"tool 2.0.0 built 3.1.4";
        assert_eq!(extract_version_from_stdout(stdout), "2.0.0");
    }

    #[test]
    fn version_only_uses_first_line() {
        // Second line has a version, first line does not.
        let stdout = b"Welcome to tool\n1.2.3 is the version";
        assert_eq!(extract_version_from_stdout(stdout), "unknown");
    }

    #[test]
    fn version_from_single_version_word() {
        let stdout = b"3.14.159";
        assert_eq!(extract_version_from_stdout(stdout), "3.14.159");
    }

    // -----------------------------------------------------------------------
    // extract_kernel_version
    // -----------------------------------------------------------------------

    #[test]
    fn kernel_version_typical() {
        let content = "Linux version 6.18.0-9-generic (builder@host) (gcc 14.2.0)";
        assert_eq!(extract_kernel_version(content), "6.18.0-9-generic");
    }

    #[test]
    fn kernel_version_empty() {
        assert_eq!(extract_kernel_version(""), "unknown");
    }

    #[test]
    fn kernel_version_only_two_words() {
        assert_eq!(extract_kernel_version("Linux version"), "unknown");
    }

    #[test]
    fn kernel_version_exactly_three_words() {
        assert_eq!(extract_kernel_version("Linux version 5.4.0"), "5.4.0");
    }

    #[test]
    fn kernel_version_extra_whitespace() {
        // split_whitespace handles multiple spaces/tabs.
        let content = "Linux   version\t6.1.0-rc1  rest";
        assert_eq!(extract_kernel_version(content), "6.1.0-rc1");
    }

    #[test]
    fn kernel_version_single_word() {
        assert_eq!(extract_kernel_version("Linux"), "unknown");
    }
}
