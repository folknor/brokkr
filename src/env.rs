use std::path::{Path, PathBuf};

use crate::config::ResolvedPaths;
use crate::context::{acquire_cmd_lock, bootstrap, bootstrap_config};
use crate::datasets::status::DatasetStatus;
use crate::error::DevError;
use crate::host::storage::StorageInfo;
use crate::output;
use crate::project::Project;

/// Environment information for `brokkr env`.
pub struct EnvInfo {
    pub hostname: String,
    pub kernel: crate::host::kernel::KernelInfo,
    pub cpu: crate::host::cpu::CpuInfo,
    pub memory: crate::host::memory::MemoryInfo,
    pub drives: Vec<(String, String)>,
    pub storage: Vec<StorageInfo>,
    pub tools: Vec<(String, String)>,
    pub datasets: Vec<(String, DatasetStatus)>,
    /// The user-wide `brokkr.toml` layer: where it is looked for, and what it
    /// contributed. `None` when the layer is switched off (`BROKKR_USER_CONFIG=`)
    /// or no config directory can be determined. Reported because a rule that
    /// comes from outside the tree is otherwise invisible from inside it.
    pub user_config: Option<UserConfigStatus>,
}

/// Where the user-wide config lives and what it added to this project.
pub struct UserConfigStatus {
    pub path: PathBuf,
    pub present: bool,
    pub textlint: usize,
    pub script_checks: usize,
}

/// Collect all environment information.
pub fn collect(
    paths: &ResolvedPaths,
    project: &Project,
    project_root: &Path,
    sources: &crate::config::ConfigSources,
) -> EnvInfo {
    let storage_paths: [(&str, &Path); 3] = [
        ("data", &paths.data_dir),
        ("scratch", &paths.scratch_dir),
        ("target", &paths.target_dir),
    ];

    EnvInfo {
        hostname: paths.hostname.clone(),
        kernel: crate::host::kernel::collect(),
        cpu: crate::host::cpu::collect(),
        memory: crate::host::memory::collect(),
        drives: collect_drives(paths),
        storage: crate::host::storage::probe_paths(&storage_paths),
        tools: collect_tools(project, project_root),
        datasets: paths.dataset_status(project_root),
        user_config: collect_user_config(sources),
    }
}

/// Collect and print the environment report for `brokkr env`.
pub(crate) fn cmd_env(
    dev_config: &crate::config::DevConfig,
    project: &Project,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
    sources: &crate::config::ConfigSources,
) -> Result<(), DevError> {
    // `bootstrap` shells out to `cargo metadata`, which rustup mediates - so in
    // a tree pinning a toolchain we don't have installed, `env` fails outright
    // unless the pin is moved aside. That window is the *locked* window (see
    // `crate::toolchain`), so reporting the environment has to take the lock
    // for the same reason `fmt` does, despite touching nothing. Only when
    // `disable_toolchain` is set: with nothing to move, `env` stays lock-free
    // (its command spec takes no dispatcher hold) and can still be run
    // alongside a long build.
    let _lock = if dev_config.disable_toolchain {
        Some(acquire_cmd_lock(project, project_root, build_root)?)
    } else {
        None
    };
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let info = collect(&paths, project, project_root.as_path(), sources);
    print(&info);
    if let Some(note) = crate::config::unmatched_host_note(dev_config, &paths.hostname) {
        output::warn(&format!(
            "{note}. Host-specific settings are ignored and defaults apply."
        ));
    }
    Ok(())
}

/// Report the user layer recorded by startup detection. Reading its entries in
/// isolation would reject valid references to project presets and recomposing
/// here could report a different file state from the config brokkr loaded.
fn collect_user_config(sources: &crate::config::ConfigSources) -> Option<UserConfigStatus> {
    let configured_path = configured_user_config_path();
    match sources.user_layer() {
        Some(user) => Some(UserConfigStatus {
            path: user.path.clone(),
            present: true,
            textlint: user.textlint,
            script_checks: user.script_checks,
        }),
        None => Some(UserConfigStatus {
            path: configured_path?,
            present: false,
            textlint: 0,
            script_checks: 0,
        }),
    }
}

/// Fixture tests must not consult or race the developer's process environment.
/// An explicitly injected test layer is reported through `ConfigSources`.
fn configured_user_config_path() -> Option<PathBuf> {
    #[cfg(not(test))]
    {
        crate::config::user_config_path()
    }
    #[cfg(test)]
    {
        None
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
    println!(
        "{:<12} {}",
        "kernel:",
        info.kernel.version.as_deref().unwrap_or("unknown")
    );
    println!("{:<12} {}", "cpu:", info.cpu.summary());
    // The label ("CCX/CCD" on AMD, "die" on Intel) is cosmetic - it names the
    // number, never derives it - so a vendor miss costs a word and nothing else.
    // The `cpu:` line above already carries total cores/threads, so this one
    // adds only what it cannot: the cache-sharing boundary, and the fact that
    // it is what an unset `parallel` budget resolves to.
    match info.cpu.cache_domain_cores {
        Some(cores) => println!(
            "{:<12} {} cores per {} (parallel budget)",
            "l3 domain:",
            cores,
            crate::cpu_topology::domain_label(info.cpu.model.as_deref().unwrap_or("unknown")),
        ),
        None => println!("{:<12} not detected", "l3 domain:"),
    }
    println!(
        "{:<12} {}",
        "governor:",
        info.cpu.governor.as_deref().unwrap_or("unknown")
    );
    println!(
        "{:<12} {} ({} available)",
        "memory:",
        format_gib(info.memory.total_mb),
        format_gib(info.memory.available_mb),
    );
    println!(
        "{:<12} {} ({} free)",
        "swap:",
        format_gib(info.memory.swap_total_mb),
        format_gib(info.memory.swap_free_mb),
    );
    println!("{:<12} {}", "io_uring:", info.kernel.io_uring.status());
    if let Some(uc) = &info.user_config {
        let summary = if uc.present {
            format!(
                "{} ({} textlint, {} script_check)",
                uc.path.display(),
                uc.textlint,
                uc.script_checks
            )
        } else {
            format!("{} (absent)", uc.path.display())
        };
        println!("{:<12} {}", "user cfg:", summary);
    }
}

/// Format a MiB count as GiB with one decimal, e.g. 30968 -> "30.2 GiB".
fn format_gib(mib: u64) -> String {
    format!("{:.1} GiB", mib as f64 / 1024.0)
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
        "{}  {}  {} {}  {}  {} free / {}{}",
        s.mount_point,
        s.device,
        s.fstype,
        s.options,
        model,
        output::format_bytes(s.free_bytes),
        output::format_bytes(s.total_bytes),
        free_share(s.free_bytes, s.total_bytes),
    )
}

/// The free share as ` (N%)`, flagged when it is under the free-space gate's
/// floor, so `env` shows why a locked command would refuse. Empty when the
/// size could not be read.
fn free_share(free: u64, total: u64) -> String {
    if total == 0 {
        return String::new();
    }
    let pct = crate::host::storage::percent(free, total);
    if crate::host::storage::is_low(free, total) {
        format!(
            " ({pct:.1}%, under the {}% floor - locked commands refuse)",
            crate::host::storage::MIN_FREE_PERCENT
        )
    } else {
        format!(" ({pct:.0}%)")
    }
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
        DatasetStatus::HashError(why) => format!("{name} \u{2717} (cannot hash: {why})"),
        DatasetStatus::NoFiles => format!("{name} (no files configured)"),
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
// Tools
// ---------------------------------------------------------------------------

/// One version row per tool the project's applicable commands use (their
/// command specs name them, `crate::cli::tools_in`), then the project's own
/// git revision.
fn collect_tools(project: &Project, project_root: &Path) -> Vec<(String, String)> {
    let mut tools: Vec<(String, String)> = crate::cli::tools_in(project)
        .into_iter()
        .map(|name| (name.to_owned(), read_tool_version(name, &["--version"])))
        .collect();
    tools.push((project.name().to_owned(), read_git_rev(project_root)));
    tools
}

/// Run a command and extract the version from its first line of stdout.
fn read_tool_version(name: &str, args: &[&str]) -> String {
    let output = match crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command(name).args(args),
    ) {
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

/// Get the current fixed-width git short rev for the project.
fn read_git_rev(project_root: &Path) -> String {
    let output = match crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(project_root),
    ) {
        Ok(o) => o,
        Err(_) => return "unknown".to_owned(),
    };

    if !output.status.success() {
        return "unknown".to_owned();
    }

    let full = String::from_utf8_lossy(&output.stdout);
    full.trim()
        .parse::<crate::git::Commit>()
        .map(|commit| commit.short().to_string())
        .unwrap_or_else(|_| "unknown".to_owned())
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
    use crate::host::cpu::{format_ghz, parse_cpuinfo};
    use crate::host::kernel::extract_kernel_version;

    #[test]
    fn free_share_shows_percent_and_flags_the_gate_floor() {
        assert_eq!(free_share(43, 100), " (43%)");
        assert_eq!(
            free_share(3, 100),
            " (3.0%, under the 5% floor - locked commands refuse)"
        );
        assert_eq!(free_share(0, 0), ""); // unreadable size
    }

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
    // parse_cpuinfo / format_ghz
    // -----------------------------------------------------------------------

    #[test]
    fn cpuinfo_typical() {
        let content = "processor\t: 0\n\
                       model name\t: AMD Ryzen 9 5900X 12-Core Processor\n\
                       cpu cores\t: 12\n\
                       processor\t: 1\n\
                       model name\t: AMD Ryzen 9 5900X 12-Core Processor\n\
                       cpu cores\t: 12\n";
        let (model, cores, threads) = parse_cpuinfo(content);
        assert_eq!(
            model.as_deref(),
            Some("AMD Ryzen 9 5900X 12-Core Processor")
        );
        assert_eq!(cores, Some(12));
        assert_eq!(threads, Some(2));
    }

    #[test]
    fn cpuinfo_empty() {
        let (model, cores, threads) = parse_cpuinfo("");
        assert_eq!(model, None);
        assert_eq!(cores, None);
        assert_eq!(threads, None);
    }

    #[test]
    fn cpuinfo_missing_cores_field() {
        // ARM cpuinfo often lacks "cpu cores" and "model name".
        let content = "processor\t: 0\nBogoMIPS\t: 48.00\nprocessor\t: 1\n";
        let (model, cores, threads) = parse_cpuinfo(content);
        assert_eq!(model, None);
        assert_eq!(cores, None);
        assert_eq!(threads, Some(2));
    }

    #[test]
    fn ghz_from_khz() {
        assert_eq!(format_ghz(4_954_565), "5.0");
        assert_eq!(format_ghz(567_089), "0.6");
        assert_eq!(format_ghz(3_600_000), "3.6");
    }

    // -----------------------------------------------------------------------
    // format_gib
    // -----------------------------------------------------------------------

    #[test]
    fn gib_from_typical_meminfo_total() {
        // MemTotal: 31711476 kB -> 30968 MiB -> 30.2 GiB
        assert_eq!(format_gib(30968), "30.2 GiB");
    }

    #[test]
    fn gib_exact() {
        assert_eq!(format_gib(16384), "16.0 GiB");
    }

    #[test]
    fn gib_zero() {
        assert_eq!(format_gib(0), "0.0 GiB");
    }

    // -----------------------------------------------------------------------
    // extract_kernel_version
    // -----------------------------------------------------------------------

    #[test]
    fn kernel_version_typical() {
        let content = "Linux version 6.18.0-9-generic (builder@host) (gcc 14.2.0)";
        assert_eq!(
            extract_kernel_version(content).as_deref(),
            Some("6.18.0-9-generic")
        );
    }

    #[test]
    fn kernel_version_empty() {
        assert_eq!(extract_kernel_version(""), None);
    }

    #[test]
    fn kernel_version_only_two_words() {
        assert_eq!(extract_kernel_version("Linux version"), None);
    }

    #[test]
    fn kernel_version_exactly_three_words() {
        assert_eq!(
            extract_kernel_version("Linux version 5.4.0").as_deref(),
            Some("5.4.0")
        );
    }

    #[test]
    fn kernel_version_extra_whitespace() {
        // split_whitespace handles multiple spaces/tabs.
        let content = "Linux   version\t6.1.0-rc1  rest";
        assert_eq!(
            extract_kernel_version(content).as_deref(),
            Some("6.1.0-rc1")
        );
    }

    #[test]
    fn kernel_version_single_word() {
        assert_eq!(extract_kernel_version("Linux"), None);
    }
}
