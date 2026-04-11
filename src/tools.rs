//! External tool download and cache management.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct PlanetilerTools {
    pub java: PathBuf,
    pub planetiler_jar: PathBuf,
    pub bench_class_dir: PathBuf,
}

pub struct OsmosisTools {
    pub osmosis: PathBuf,
    pub java_home: PathBuf,
}

pub struct TilemakerTools {
    pub tilemaker: PathBuf,
    pub config: PathBuf,
    pub process: PathBuf,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const JDK_MAJOR: u32 = 25;
const OSMOSIS_VERSION: &str = "0.49.2";

// ---------------------------------------------------------------------------
// Top-level entry point
// ---------------------------------------------------------------------------

/// Ensure JDK + Planetiler JAR + compiled benchmark class are ready.
pub fn ensure_planetiler(
    data_dir: &Path,
    workspace_root: &Path,
) -> Result<PlanetilerTools, DevError> {
    check_curl()?;

    let java = ensure_jdk(data_dir)?;
    let javac = data_dir.join("jdk/bin/javac");
    let planetiler_jar = ensure_planetiler_jar(data_dir)?;
    let bench_class_dir = compile_bench(data_dir, workspace_root, &javac, &planetiler_jar)?;

    Ok(PlanetilerTools {
        java,
        planetiler_jar,
        bench_class_dir,
    })
}

/// Ensure JDK + Osmosis are ready for merge verification.
pub fn ensure_osmosis(
    data_dir: &Path,
    #[allow(unused_variables)] workspace_root: &Path,
) -> Result<OsmosisTools, DevError> {
    check_curl()?;

    let java_home = data_dir.join("jdk");
    ensure_jdk(data_dir)?;

    let osmosis = ensure_osmosis_binary(data_dir)?;

    Ok(OsmosisTools { osmosis, java_home })
}

// ---------------------------------------------------------------------------
// Osmosis
// ---------------------------------------------------------------------------

fn ensure_osmosis_binary(data_dir: &Path) -> Result<PathBuf, DevError> {
    let osmosis_dir = data_dir.join("osmosis");
    let version_file = data_dir.join(".osmosis-version");
    let osmosis_bin = osmosis_dir.join("bin/osmosis");

    // Check cached version.
    if osmosis_bin.exists()
        && let Ok(cached) = fs::read_to_string(&version_file)
        && cached.trim() == OSMOSIS_VERSION
    {
        return Ok(osmosis_bin);
    }

    let download_url = format!(
        "https://github.com/openstreetmap/osmosis/releases/download/{OSMOSIS_VERSION}/osmosis-{OSMOSIS_VERSION}.tgz"
    );

    // Download.
    let tarball = data_dir.join("osmosis-download.tgz");
    let tarball_str = tarball.display().to_string();
    output::verify_msg(&format!("downloading Osmosis {OSMOSIS_VERSION}"));
    run_curl(
        &["-fsSL", "-o", &tarball_str, &download_url],
        Path::new("."),
    )?;

    // Remove old dir and recreate.
    if osmosis_dir.exists() {
        fs::remove_dir_all(&osmosis_dir)?;
    }
    fs::create_dir_all(&osmosis_dir)?;

    // Extract.
    let osmosis_dir_str = osmosis_dir.display().to_string();
    let captured = output::run_captured(
        "tar",
        &["xzf", &tarball_str, "-C", &osmosis_dir_str],
        Path::new("."),
    )?;
    captured.check_success("tar")?;

    // Write version file.
    fs::write(&version_file, OSMOSIS_VERSION)?;

    // Clean up tarball.
    fs::remove_file(&tarball).ok();

    output::verify_msg(&format!("installed Osmosis {OSMOSIS_VERSION}"));
    Ok(osmosis_bin)
}

// ---------------------------------------------------------------------------
// curl preflight
// ---------------------------------------------------------------------------

pub(crate) fn check_curl() -> Result<(), DevError> {
    let result = std::process::Command::new("which")
        .arg("curl")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => Ok(()),
        _ => Err(DevError::Preflight(vec![
            "'curl' not found in PATH (required for tool downloads)".into(),
        ])),
    }
}

// ---------------------------------------------------------------------------
// JDK
// ---------------------------------------------------------------------------

fn ensure_jdk(data_dir: &Path) -> Result<PathBuf, DevError> {
    let jdk_dir = data_dir.join("jdk");
    let version_file = data_dir.join(".jdk-version");
    let java = jdk_dir.join("bin/java");

    // Cache-first: if both the java binary and version marker exist, the JDK
    // is already installed. Skip the network call entirely.
    if java.exists() && version_file.exists() {
        return Ok(java);
    }

    let arch = detect_arch()?;
    let os = detect_os()?;
    let api_url = format!(
        "https://api.adoptium.net/v3/assets/latest/{JDK_MAJOR}/hotspot\
         ?architecture={arch}&image_type=jdk&os={os}&vendor=eclipse"
    );

    let api_body = run_curl(&["-sfL", &api_url], Path::new("."))?;
    let api_json: serde_json::Value = serde_json::from_slice(&api_body)?;

    let first = api_json
        .as_array()
        .and_then(|arr| arr.first())
        .ok_or_else(|| DevError::Config("adoptium API returned empty response".into()))?;

    let release_name = first
        .get("release_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DevError::Config("adoptium API missing release_name".into()))?;

    let download_url = first
        .get("binary")
        .and_then(|b| b.get("package"))
        .and_then(|p| p.get("link"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DevError::Config("adoptium API missing binary.package.link".into()))?;

    // Download.
    let tarball = data_dir.join("jdk-download.tar.gz");
    let tarball_str = tarball.display().to_string();
    output::bench_msg(&format!("downloading JDK {release_name}"));
    run_curl(&["-fsSL", "-o", &tarball_str, download_url], Path::new("."))?;

    // Remove old JDK dir and recreate.
    if jdk_dir.exists() {
        fs::remove_dir_all(&jdk_dir)?;
    }
    fs::create_dir_all(&jdk_dir)?;

    // Extract.
    let jdk_dir_str = jdk_dir.display().to_string();
    let captured = output::run_captured(
        "tar",
        &[
            "xzf",
            &tarball_str,
            "-C",
            &jdk_dir_str,
            "--strip-components=1",
        ],
        Path::new("."),
    )?;
    captured.check_success("tar")?;

    // Write version file.
    fs::write(&version_file, release_name)?;

    // Clean up tarball.
    fs::remove_file(&tarball).ok();

    output::bench_msg(&format!("installed JDK {release_name}"));
    Ok(java)
}

// ---------------------------------------------------------------------------
// Planetiler JAR
// ---------------------------------------------------------------------------

fn ensure_planetiler_jar(data_dir: &Path) -> Result<PathBuf, DevError> {
    let jar_path = data_dir.join("planetiler.jar");
    let version_file = data_dir.join(".planetiler-version");

    // Cache-first: if both the jar and version marker exist, the jar is
    // already installed. Skip the network call entirely.
    if jar_path.exists() && version_file.exists() {
        return Ok(jar_path);
    }

    let api_url = "https://api.github.com/repos/onthegomap/planetiler/releases/latest";

    let api_body = run_curl(&["-sfL", api_url], Path::new("."))?;
    let api_json: serde_json::Value = serde_json::from_slice(&api_body)?;

    let tag_name = api_json
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DevError::Config("github API missing tag_name".into()))?;

    let assets = api_json
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| DevError::Config("github API missing assets array".into()))?;

    let download_url = assets
        .iter()
        .find(|a| a.get("name").and_then(serde_json::Value::as_str) == Some("planetiler.jar"))
        .and_then(|a| a.get("browser_download_url"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DevError::Config("github API: no planetiler.jar asset found in release".into())
        })?;

    // Download.
    let jar_str = jar_path.display().to_string();
    output::bench_msg(&format!("downloading Planetiler {tag_name}"));
    run_curl(&["-fsSL", "-o", &jar_str, download_url], Path::new("."))?;

    // Write version file.
    fs::write(&version_file, tag_name)?;

    output::bench_msg(&format!("installed Planetiler {tag_name}"));
    Ok(jar_path)
}

// ---------------------------------------------------------------------------
// Compile benchmark class
// ---------------------------------------------------------------------------

fn compile_bench(
    data_dir: &Path,
    workspace_root: &Path,
    javac: &Path,
    planetiler_jar: &Path,
) -> Result<PathBuf, DevError> {
    let bench_src = workspace_root.join("bench/planetiler-baseline/BenchPbfRead.java");
    let class_dir = data_dir.join("planetiler-bench-classes");
    let class_file = class_dir.join("BenchPbfRead.class");

    // Check if recompilation is needed.
    if class_file.exists()
        && let Some(false) = needs_recompile(&class_file, &bench_src, planetiler_jar)
    {
        return Ok(class_dir);
    }

    fs::create_dir_all(&class_dir)?;

    let javac_str = javac.display().to_string();
    let jar_str = planetiler_jar.display().to_string();
    let class_dir_str = class_dir.display().to_string();
    let bench_src_str = bench_src.display().to_string();

    let captured = output::run_captured(
        &javac_str,
        &[
            "-proc:none",
            "-cp",
            &jar_str,
            "-d",
            &class_dir_str,
            &bench_src_str,
        ],
        workspace_root,
    )?;

    captured.check_success("javac")?;

    output::bench_msg("compiled planetiler benchmark");
    Ok(class_dir)
}

/// Returns `Some(true)` if the class file is older than any source, `Some(false)`
/// if it is up to date, or `None` if timestamps could not be compared.
fn needs_recompile(class_file: &Path, bench_src: &Path, planetiler_jar: &Path) -> Option<bool> {
    let class_mtime = file_mtime(class_file)?;
    let src_mtime = file_mtime(bench_src)?;
    let jar_mtime = file_mtime(planetiler_jar)?;

    Some(src_mtime > class_mtime || jar_mtime > class_mtime)
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

// ---------------------------------------------------------------------------
// Helpers: architecture / OS detection
// ---------------------------------------------------------------------------

fn detect_arch() -> Result<&'static str, DevError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("aarch64"),
        other => Err(DevError::Config(format!(
            "unsupported architecture: {other}"
        ))),
    }
}

fn detect_os() -> Result<&'static str, DevError> {
    match std::env::consts::OS {
        "linux" => Ok("linux"),
        "macos" => Ok("mac"),
        other => Err(DevError::Config(format!("unsupported OS: {other}"))),
    }
}

// ---------------------------------------------------------------------------
// Helpers: curl wrapper
// ---------------------------------------------------------------------------

/// Run curl with the given arguments, returning stdout bytes on success.
pub(crate) fn run_curl(args: &[&str], cwd: &Path) -> Result<Vec<u8>, DevError> {
    let captured = output::run_captured("curl", args, cwd)?;

    captured.check_success("curl")?;

    Ok(captured.stdout)
}

/// Download a URL to a file with a visible progress bar.
///
/// Uses curl with `--progress-bar` and inherited stderr so the user can see
/// download progress for large files.
pub(crate) fn download_file(url: &str, dest: &Path) -> Result<(), DevError> {
    // Download to a temp file and rename on success to avoid leaving partial
    // files that block future retries.
    let tmp = dest.with_extension("tmp");
    let tmp_str = tmp.display().to_string();

    let status = std::process::Command::new("curl")
        .args(["-fL", "--progress-bar", "-o", &tmp_str, url])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| DevError::Subprocess {
            program: "curl".into(),
            code: None,
            stderr: e.to_string(),
        })?;

    if !status.success() {
        // Clean up partial download.
        drop(std::fs::remove_file(&tmp));
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: status.code(),
            stderr: format!("download failed: {url}"),
        });
    }

    std::fs::rename(&tmp, dest)?;

    Ok(())
}

/// Result of an HTTP HEAD request to a URL. Used by `download::run_refresh`
/// to detect upstream newness without fetching the full PBF.
pub(crate) struct HeadResponse {
    /// `Last-Modified` header parsed as Unix epoch seconds, if present and parseable.
    /// Refresh compares this against the on-disk mtime of the existing PBF to
    /// decide whether to rotate.
    pub last_modified_unix: Option<i64>,
}

/// HEAD a URL via `curl -fIL` and parse the `Last-Modified` header.
///
/// Returns `Ok(HeadResponse { last_modified_unix: None })` when the request
/// succeeds but no `Last-Modified` header is present (or it can't be parsed).
/// Errors only when the HEAD request itself fails (e.g. 404, network error).
pub(crate) fn head_url(url: &str) -> Result<HeadResponse, DevError> {
    let output = std::process::Command::new("curl")
        .args(["-fsIL", url])
        .output()
        .map_err(|e| DevError::Subprocess {
            program: "curl".into(),
            code: None,
            stderr: e.to_string(),
        })?;

    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: output.status.code(),
            stderr: format!(
                "HEAD request failed for {url}: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }

    // Parse the headers. With `-L`, redirects are followed and we get multiple
    // header blocks separated by blank lines — we want the LAST one (the final
    // resource). Look for `Last-Modified:` case-insensitively.
    let body = String::from_utf8_lossy(&output.stdout);
    let last_modified = body
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(2, ':');
            let name = parts.next()?.trim();
            let value = parts.next()?.trim();
            name.eq_ignore_ascii_case("last-modified").then_some(value)
        })
        .next_back()
        .map(parse_http_date)
        .and_then(|r| r.ok());

    Ok(HeadResponse {
        last_modified_unix: last_modified,
    })
}

/// Parse an HTTP-date string (RFC 7231 IMF-fixdate format) into Unix epoch
/// seconds. Returns `Err` if the format isn't recognized.
///
/// Only supports the IMF-fixdate format (`"Sun, 06 Nov 1994 08:49:37 GMT"`)
/// which is the canonical form per RFC 7231 §7.1.1.1 — what every modern
/// origin server emits. Doesn't support the obsolete RFC 850 or asctime forms.
fn parse_http_date(s: &str) -> Result<i64, String> {
    // Format: "Day, DD Mon YYYY HH:MM:SS GMT"
    // Example: "Tue, 11 Apr 2026 12:34:56 GMT"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 6 {
        return Err(format!("not enough parts: '{s}'"));
    }
    let day: i64 = parts[1].parse().map_err(|e| format!("day: {e}"))?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        other => return Err(format!("unknown month: {other}")),
    };
    let year: i64 = parts[3].parse().map_err(|e| format!("year: {e}"))?;
    let time_parts: Vec<&str> = parts[4].split(':').collect();
    if time_parts.len() != 3 {
        return Err(format!("bad time: '{}'", parts[4]));
    }
    let hour: i64 = time_parts[0].parse().map_err(|e| format!("hour: {e}"))?;
    let minute: i64 = time_parts[1]
        .parse()
        .map_err(|e| format!("minute: {e}"))?;
    let second: i64 = time_parts[2]
        .parse()
        .map_err(|e| format!("second: {e}"))?;

    // Convert (Y, M, D) to days since 1970-01-01 using Howard Hinnant's
    // algorithm — same one used by `pbfhogg::download::days_to_civil` in
    // reverse.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = month as u64;
    let d = day as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;
    Ok(days * 86400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_date_imf_fixdate() {
        // Sun, 06 Nov 1994 08:49:37 GMT — the canonical example from RFC 7231.
        let unix = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(unix, 784111777);
    }

    #[test]
    fn parse_http_date_modern() {
        // Sat, 11 Apr 2026 12:34:56 GMT
        let unix = parse_http_date("Sat, 11 Apr 2026 12:34:56 GMT").unwrap();
        // Cross-check: 2026-04-11T12:34:56Z = 1775910896
        assert_eq!(unix, 1775910896);
    }

    #[test]
    fn parse_http_date_rejects_garbage() {
        assert!(parse_http_date("not a date").is_err());
        assert!(parse_http_date("Sun, 06 XYZ 1994 08:49:37 GMT").is_err());
    }
}

// ---------------------------------------------------------------------------
// Tilemaker
// ---------------------------------------------------------------------------

fn check_build_tool(name: &str) -> Result<(), DevError> {
    let result = std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => Ok(()),
        _ => Err(DevError::Preflight(vec![format!(
            "'{name}' not found in PATH (required to build tilemaker)"
        )])),
    }
}

/// Ensure tilemaker binary and shortbread config are ready.
pub fn ensure_tilemaker(data_dir: &Path) -> Result<TilemakerTools, DevError> {
    // Preflight: check build tools.
    check_build_tool("cmake")?;
    check_build_tool("g++")?;
    check_build_tool("make")?;

    let tilemaker_dir = data_dir.join("tilemaker");
    let version_file = data_dir.join(".tilemaker-version");
    let build_dir = tilemaker_dir.join("build");
    let tilemaker_bin = build_dir.join("tilemaker");

    // Clone or update tilemaker source.
    if !tilemaker_dir.exists() {
        let tilemaker_dir_str = tilemaker_dir.display().to_string();
        output::bench_msg("cloning tilemaker");
        let captured = output::run_captured(
            "git",
            &[
                "clone",
                "--depth",
                "1",
                "https://github.com/systemed/tilemaker.git",
                &tilemaker_dir_str,
            ],
            data_dir,
        )?;
        captured.check_success("git")?;
    } else {
        let tilemaker_dir_str = tilemaker_dir.display().to_string();
        // Tolerate failure — just use what's there.
        drop(output::run_captured(
            "git",
            &["-C", &tilemaker_dir_str, "pull", "--ff-only"],
            data_dir,
        ));
    }

    // Get current commit hash.
    let tilemaker_dir_str = tilemaker_dir.display().to_string();
    let captured = output::run_captured(
        "git",
        &["-C", &tilemaker_dir_str, "rev-parse", "HEAD"],
        data_dir,
    )?;
    captured.check_success("git")?;
    let commit = String::from_utf8_lossy(&captured.stdout).trim().to_string();

    // Check if build can be skipped.
    if tilemaker_bin.exists()
        && let Ok(cached) = fs::read_to_string(&version_file)
        && cached.trim() == commit
    {
        // Version matches and binary exists — skip build.
        let shortbread_dir = ensure_shortbread_config(data_dir)?;
        return Ok(TilemakerTools {
            tilemaker: tilemaker_bin,
            config: shortbread_dir.join("config.json"),
            process: shortbread_dir.join("process.lua"),
        });
    }

    // CMake build.
    fs::create_dir_all(&build_dir)?;

    let build_dir_str = build_dir.display().to_string();
    output::bench_msg("configuring tilemaker (cmake)");
    let captured = output::run_captured(
        "cmake",
        &[
            "-S",
            &tilemaker_dir_str,
            "-B",
            &build_dir_str,
            "-DCMAKE_BUILD_TYPE=Release",
        ],
        data_dir,
    )?;
    captured.check_success("cmake")?;

    let nproc = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let jobs = format!("-j{nproc}");
    output::bench_msg("building tilemaker");
    let captured =
        output::run_captured("cmake", &["--build", &build_dir_str, "--", &jobs], data_dir)?;
    captured.check_success("cmake")?;

    // Write version file.
    fs::write(&version_file, &commit)?;

    let commit_short = &commit[..commit.len().min(8)];
    output::bench_msg(&format!("built tilemaker ({commit_short})"));

    // Ensure shortbread config.
    let shortbread_dir = ensure_shortbread_config(data_dir)?;

    Ok(TilemakerTools {
        tilemaker: tilemaker_bin,
        config: shortbread_dir.join("config.json"),
        process: shortbread_dir.join("process.lua"),
    })
}

fn ensure_shortbread_config(data_dir: &Path) -> Result<PathBuf, DevError> {
    let shortbread_dir = data_dir.join("shortbread-tilemaker");

    if !shortbread_dir.exists() {
        let shortbread_dir_str = shortbread_dir.display().to_string();
        output::bench_msg("cloning shortbread-tilemaker config");
        let captured = output::run_captured(
            "git",
            &[
                "clone",
                "--depth",
                "1",
                "https://github.com/shortbread-tiles/shortbread-tilemaker.git",
                &shortbread_dir_str,
            ],
            data_dir,
        )?;
        captured.check_success("git")?;
    } else {
        let shortbread_dir_str = shortbread_dir.display().to_string();
        // Tolerate failure — just use what's there.
        drop(output::run_captured(
            "git",
            &["-C", &shortbread_dir_str, "pull", "--ff-only"],
            data_dir,
        ));
    }

    Ok(shortbread_dir)
}
