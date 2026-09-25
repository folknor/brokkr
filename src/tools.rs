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

    // Download. Not checksum-verified: this is a fixed release URL with no
    // metadata request in front of it, so there is no published digest in hand
    // to compare against. `OSMOSIS_VERSION` pins the release; verifying would
    // mean adding a GitHub releases API call to read the asset's `digest`, as
    // `ensure_planetiler_jar` does.
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

    // Cache-first: if the java binary exists and the version marker records
    // the major we want, the JDK is already installed. Skip the network call
    // entirely. The major has to be checked, not just the marker's existence,
    // or bumping `JDK_MAJOR` would never replace the installed JDK.
    if java.exists()
        && let Ok(marker) = fs::read_to_string(&version_file)
        && jdk_marker_matches(&marker)
    {
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

    let package = first.get("binary").and_then(|b| b.get("package"));
    let download_url = package
        .and_then(|p| p.get("link"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DevError::Config("adoptium API missing binary.package.link".into()))?;
    // The same response carries the package's sha256. Required rather than
    // optional: it is part of the documented asset shape, so its absence is
    // an API change to stop on, not a reason to install unverified.
    let checksum = package
        .and_then(|p| p.get("checksum"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DevError::Config("adoptium API missing binary.package.checksum".into()))?;

    // Download.
    let tarball = data_dir.join("jdk-download.tar.gz");
    let tarball_str = tarball.display().to_string();
    output::bench_msg(&format!("downloading JDK {release_name}"));
    fetch_verified(download_url, &tarball, Some(checksum))?;

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

    // Write version file: the major the cache check keys on, then the exact
    // release for a human reading it.
    fs::write(&version_file, jdk_marker(release_name))?;

    // Clean up tarball.
    fs::remove_file(&tarball).ok();

    output::bench_msg(&format!("installed JDK {release_name}"));
    Ok(java)
}

/// `.jdk-version` content: `JDK_MAJOR` on the first line, the release name on
/// the second.
fn jdk_marker(release_name: &str) -> String {
    format!("{JDK_MAJOR}\n{release_name}\n")
}

/// Whether a `.jdk-version` marker records the current `JDK_MAJOR`. A marker
/// from before the major was recorded (the release name alone) does not
/// match, which costs one re-download and then settles.
fn jdk_marker_matches(marker: &str) -> bool {
    marker.lines().next().map(str::trim) == Some(JDK_MAJOR.to_string().as_str())
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

    let asset = assets
        .iter()
        .find(|a| a.get("name").and_then(serde_json::Value::as_str) == Some("planetiler.jar"))
        .ok_or_else(|| {
            DevError::Config("github API: no planetiler.jar asset found in release".into())
        })?;
    let download_url = asset
        .get("browser_download_url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DevError::Config("github API: planetiler.jar asset has no download url".into())
        })?;
    // GitHub reports a `digest` of `sha256:<hex>` per release asset, but only
    // for assets uploaded since it started computing them - older releases
    // carry `null`. Verify when it is there; without it there is nothing
    // upstream publishes to check against, so the jar installs unverified.
    let digest = asset
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .and_then(|d| d.strip_prefix("sha256:"));
    if digest.is_none() {
        output::bench_msg(&format!(
            "Planetiler {tag_name}: release publishes no sha256 digest; installing unverified"
        ));
    }

    // Download. Staged, so a partial jar never sits at `jar_path`.
    output::bench_msg(&format!("downloading Planetiler {tag_name}"));
    fetch_verified(download_url, &jar_path, digest)?;

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

/// curl flags that bound a transfer without capping its length.
///
/// Every curl brokkr runs holds the global lock, so a transfer that stalls
/// would hold it indefinitely. A hard `--max-time` is wrong for the large
/// downloads (a planet PBF legitimately takes hours), so the bound is a stall
/// detector instead: give up on a connection that has not opened in 30s, or on
/// a transfer that has averaged under 1 KiB/s for a full two minutes. A slow
/// but moving download survives; a dead one fails.
const CURL_STALL_ARGS: [&str; 6] = [
    "--connect-timeout",
    "30",
    "--speed-limit",
    "1024",
    "--speed-time",
    "120",
];

/// Run curl with the given arguments, returning stdout bytes on success.
/// Bounded by [`CURL_STALL_ARGS`].
pub(crate) fn run_curl(args: &[&str], cwd: &Path) -> Result<Vec<u8>, DevError> {
    let mut all: Vec<&str> = CURL_STALL_ARGS.to_vec();
    all.extend_from_slice(args);
    let captured = output::run_captured("curl", &all, cwd)?;

    captured.check_success("curl")?;

    Ok(captured.stdout)
}

/// Download a URL to a file with a visible progress bar.
///
/// Uses curl with `--progress-bar` and inherited stderr so the user can see
/// download progress for large files.
///
/// No hash check: the callers fetch map data brokkr pins no digest for, and
/// much of it (a region's latest extract, osmdata's daily-rebuilt ocean
/// shapefiles) is republished under a fixed URL, so no stable digest could be
/// pinned. What is guaranteed is that `dest` is absent or a complete transfer.
pub(crate) fn download_file(url: &str, dest: &Path) -> Result<(), DevError> {
    // Download to a staged sibling and rename on success, so a partial file
    // never sits at `dest` to block future retries. The stable `.partial`
    // name (not a per-process one) means a killed multi-gigabyte transfer is
    // overwritten by the retry instead of leaked; downloads run under the
    // global lock, so no second writer shares it.
    let staged = crate::atomic_write::Staged::stable(dest)?;
    let tmp_str = staged.tmp_path().display().to_string();

    // No `--max-time`: see `CURL_STALL_ARGS` for why a stall detector bounds
    // this rather than a total-duration cap.
    let status = std::process::Command::new("curl")
        .args(CURL_STALL_ARGS)
        .args(["-fL", "--progress-bar", "-o", &tmp_str, url])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|error| DevError::Spawn {
            program: "curl".into(),
            error,
        })?;

    if !status.success() {
        // Dropping `staged` removes the partial download.
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: status.code(),
            stderr: format!("download failed: {url}"),
        });
    }

    staged.commit()?;
    Ok(())
}

/// Download `url` to `dest` through a staged sibling, checking the transfer
/// against `sha256` (hex) before it is renamed into place when one is given.
/// A partial or mismatched download never sits at `dest`. Quiet (no progress
/// bar): for the tool downloads, where [`download_file`]'s bar is noise.
fn fetch_verified(url: &str, dest: &Path, sha256: Option<&str>) -> Result<(), DevError> {
    // Stable temp name for the reason `download_file` gives.
    let staged = crate::atomic_write::Staged::stable(dest)?;
    let tmp_str = staged.tmp_path().display().to_string();
    run_curl(&["-fsSL", "-o", &tmp_str, url], Path::new("."))?;
    if let Some(expected) = sha256 {
        let actual = sha256_file(staged.tmp_path())?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(DevError::Preflight(vec![format!(
                "sha256 mismatch downloading {url}\n  expected: {expected}\n  actual:   {actual}"
            )]));
        }
    }
    staged.commit()?;
    Ok(())
}

/// SHA-256 of a file as lowercase hex, via `sha256sum` (coreutils) or, where
/// that is absent (macOS), `shasum -a 256`. Shelled out like the rest of this
/// module's tooling rather than linking a hash crate for one caller.
fn sha256_file(path: &Path) -> Result<String, DevError> {
    let path_str = path.display().to_string();
    let coreutils = output::run_captured("sha256sum", &[&path_str], Path::new("."));
    let (program, captured) = match coreutils {
        Err(DevError::Spawn { .. }) => (
            "shasum",
            output::run_captured("shasum", &["-a", "256", &path_str], Path::new("."))?,
        ),
        other => ("sha256sum", other?),
    };
    captured.check_success(program)?;
    String::from_utf8_lossy(&captured.stdout)
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| DevError::Config(format!("{program} printed no digest for {path_str}")))
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
    // A HEAD carries no body, so unlike the downloads it can take a hard cap:
    // a server that accepts the connection and never answers fails in 60s.
    let output = std::process::Command::new("curl")
        .args(["--connect-timeout", "30", "--max-time", "60", "-fsIL", url])
        .output()
        .map_err(|error| DevError::Spawn {
            program: "curl".into(),
            error,
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
    // header blocks separated by blank lines - we want the LAST one (the final
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
        .and_then(Result::ok);

    Ok(HeadResponse {
        last_modified_unix: last_modified,
    })
}

/// Parse an HTTP-date string (RFC 7231 IMF-fixdate format) into Unix epoch
/// seconds. Returns `Err` if the format isn't recognized.
///
/// Only supports the IMF-fixdate format (`"Sun, 06 Nov 1994 08:49:37 GMT"`)
/// which is the canonical form per RFC 7231 §7.1.1.1 - what every modern
/// origin server emits. Doesn't support the obsolete RFC 850 or asctime forms.
fn parse_http_date(s: &str) -> Result<i64, String> {
    // Format: "Day, DD Mon YYYY HH:MM:SS GMT"
    // Example: "Tue, 11 Apr 2026 12:34:56 GMT"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 6 {
        return Err(format!("not enough parts: '{s}'"));
    }
    let day: i64 = parts[1].parse().map_err(|e| format!("day: {e}"))?;
    let month: i32 = match parts[2] {
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
    // algorithm - same one used by `pbfhogg::download::days_to_civil` in
    // reverse.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    #[allow(clippy::cast_sign_loss)] // yoe, month, day are all non-negative here
    let yoe = (y - era * 400) as u64;
    #[allow(clippy::cast_sign_loss)]
    let m = month as u64;
    #[allow(clippy::cast_sign_loss)]
    let d = day as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    #[allow(clippy::cast_possible_wrap)]
    let days = era * 146097 + doe as i64 - 719468;
    Ok(days * 86400 + hour * 3600 + minute * 60 + second)
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
        // Tolerate failure - just use what's there.
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
        // Version matches and binary exists - skip build.
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
        // Tolerate failure - just use what's there.
        drop(output::run_captured(
            "git",
            &["-C", &shortbread_dir_str, "pull", "--ff-only"],
            data_dir,
        ));
    }

    Ok(shortbread_dir)
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

    #[test]
    fn parse_http_date_imf_fixdate() {
        // Sun, 06 Nov 1994 08:49:37 GMT - the canonical example from RFC 7231.
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
    fn jdk_marker_keys_on_the_major() {
        assert!(jdk_marker_matches(&jdk_marker("jdk-25.0.1+8")));
        // A marker naming another major - what a `JDK_MAJOR` bump leaves
        // behind - must not satisfy the cache.
        let other = format!("{}\njdk-x\n", JDK_MAJOR + 1);
        assert!(!jdk_marker_matches(&other));
        // The pre-major marker format (the release name alone).
        assert!(!jdk_marker_matches("jdk-21.0.5+11"));
        assert!(!jdk_marker_matches(""));
    }

    #[test]
    fn sha256_file_matches_the_fips_vector() {
        let dir = crate::test_scratch::scratch("tools", "sha256");
        let path = dir.join("abc");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn parse_http_date_rejects_garbage() {
        assert!(parse_http_date("not a date").is_err());
        assert!(parse_http_date("Sun, 06 XYZ 1994 08:49:37 GMT").is_err());
    }
}
