//! External tool download and cache management.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::DevError;
use crate::output;
use crate::preflight::Sha256;

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

/// Resolve an executable using the process PATH, or validate a supplied path.
/// All brokkr preflight checks use this lookup instead of spawning `which` or
/// maintaining a second PATH walker.
pub(crate) fn find_executable(name: &str) -> Option<PathBuf> {
    let requested = Path::new(name);
    if requested.is_absolute() || name.contains(std::path::MAIN_SEPARATOR) {
        return is_executable(requested).then(|| requested.to_path_buf());
    }

    let path = std::env::var_os("PATH")?;
    find_executable_in(name, &path)
}

fn find_executable_in(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// Update pins together with their digests after reviewing an upstream release.
const JDK_RELEASE: &str = "jdk-25.0.4.1+1";
const JDK_ARCHIVE_VERSION: &str = "25.0.4.1_1";
const PLANETILER_RELEASE: &str = "v0.10.2";
const PLANETILER_SHA256: &str = "f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d";
const OSMOSIS_VERSION: &str = "0.49.2";
const OSMOSIS_SHA256: &str = "2a23a7bf7499a0727ae6987cf66fcafc03b37379dc02177c730f352bd67a0b97";
const TILEMAKER_COMMIT: &str = "facdf933f4f5ac2944a9d65c27970270583d68fa";
const SHORTBREAD_COMMIT: &str = "9ec009826b49b71bb1481a1c28c3681766c600e3";

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
pub fn ensure_osmosis(data_dir: &Path) -> Result<OsmosisTools, DevError> {
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
    let marker = format!("{OSMOSIS_VERSION}\n{OSMOSIS_SHA256}\n");

    // A changed checksum pin invalidates the installed cache as well as a
    // version bump.
    if osmosis_bin.exists()
        && let Ok(cached) = fs::read_to_string(&version_file)
        && cached == marker
    {
        return Ok(osmosis_bin);
    }

    let download_url = format!(
        "https://github.com/openstreetmap/osmosis/releases/download/{OSMOSIS_VERSION}/osmosis-{OSMOSIS_VERSION}.tar"
    );

    // The release asset has no published digest, so this pin carries the
    // reviewed SHA-256 beside its version.
    let tarball = data_dir.join("osmosis-download.tar");
    output::verify_msg(&format!("downloading Osmosis {OSMOSIS_VERSION}"));
    fetch_verified(&download_url, &tarball, Some(OSMOSIS_SHA256))?;

    // Extract into a sibling staging directory. The release archive has a
    // versioned top-level directory and is an uncompressed tar file.
    let extract_dir = data_dir.join("osmosis-extract");
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)?;
    }
    fs::create_dir_all(&extract_dir)?;
    let tarball_str = tarball.display().to_string();
    let extract_dir_str = extract_dir.display().to_string();
    let captured = output::run_captured(
        "tar",
        &["xf", &tarball_str, "-C", &extract_dir_str],
        data_dir,
    )?;
    captured.check_success("tar")?;

    let unpacked = extract_dir.join(format!("osmosis-{OSMOSIS_VERSION}"));
    if !unpacked.join("bin/osmosis").is_file() {
        return Err(DevError::Config(format!(
            "Osmosis {OSMOSIS_VERSION} archive has no bin/osmosis"
        )));
    }

    // Replace the old version only after the pinned archive was verified and
    // extracted successfully.
    replace_directory(&unpacked, &osmosis_dir, &data_dir.join("osmosis-old"))?;
    fs::remove_dir_all(&extract_dir)?;

    // Write version file.
    fs::write(&version_file, marker)?;

    // Clean up tarball.
    fs::remove_file(&tarball).ok();

    output::verify_msg(&format!("installed Osmosis {OSMOSIS_VERSION}"));
    Ok(osmosis_bin)
}

// ---------------------------------------------------------------------------
// curl preflight
// ---------------------------------------------------------------------------

pub(crate) fn check_curl() -> Result<(), DevError> {
    if find_executable("curl").is_some() {
        Ok(())
    } else {
        Err(DevError::Preflight(vec![
            "'curl' not found in PATH (required for tool downloads)".into(),
        ]))
    }
}

// ---------------------------------------------------------------------------
// JDK
// ---------------------------------------------------------------------------

fn ensure_jdk(data_dir: &Path) -> Result<PathBuf, DevError> {
    let jdk_dir = data_dir.join("jdk");
    let version_file = data_dir.join(".jdk-version");
    let java = jdk_dir.join("bin/java");

    let arch = detect_arch()?;
    let os = detect_os()?;
    let checksum = jdk_checksum(arch, os)?;

    // Cache only the exact release and archive digest this build records.
    if java.exists()
        && let Ok(marker) = fs::read_to_string(&version_file)
        && jdk_marker_matches(&marker, checksum)
    {
        return Ok(java);
    }

    let release = JDK_RELEASE.replace('+', "%2B");
    let archive = format!("OpenJDK25U-jdk_{arch}_{os}_hotspot_{JDK_ARCHIVE_VERSION}.tar.gz");
    let download_url = format!(
        "https://github.com/adoptium/temurin25-binaries/releases/download/{release}/{archive}"
    );

    // Download.
    let tarball = data_dir.join("jdk-download.tar.gz");
    let tarball_str = tarball.display().to_string();
    output::bench_msg(&format!("downloading JDK {JDK_RELEASE}"));
    fetch_verified(&download_url, &tarball, Some(checksum))?;

    // Extract to staging and keep the existing JDK until the new archive has
    // unpacked and passed its expected-file check.
    let staged_jdk = data_dir.join("jdk-extract");
    if staged_jdk.exists() {
        fs::remove_dir_all(&staged_jdk)?;
    }
    fs::create_dir_all(&staged_jdk)?;

    // Extract.
    let jdk_dir_str = staged_jdk.display().to_string();
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
    if !staged_jdk.join("bin/java").is_file() {
        return Err(DevError::Config(format!(
            "Temurin {JDK_RELEASE} archive has no bin/java"
        )));
    }
    replace_directory(&staged_jdk, &jdk_dir, &data_dir.join("jdk-old"))?;

    // Record the exact release so a version change invalidates this cache.
    fs::write(&version_file, jdk_marker(checksum))?;

    // Clean up tarball.
    fs::remove_file(&tarball).ok();

    output::bench_msg(&format!("installed JDK {JDK_RELEASE}"));
    Ok(java)
}

fn jdk_checksum(arch: &str, os: &str) -> Result<&'static str, DevError> {
    match (arch, os) {
        ("x64", "linux") => Ok("dbb698396d478e7fa2b1e50f4103324b2a99b90569ee27c33f2261f9215cf41e"),
        ("aarch64", "linux") => {
            Ok("69df11a02cfa3ef7d7ca645e03edce6778ec090e100f6ae2b42097865730ac52")
        }
        ("x64", "mac") => Ok("e6229d9504f7922053ab31821b9e6bee8761daf7b026a3476d1a027563009880"),
        ("aarch64", "mac") => {
            Ok("61979887f7506a24a57439ff99adb8b3a7fc89977d9cfe3b8984f58a981b7b9d")
        }
        _ => Err(DevError::Config(format!(
            "no JDK {JDK_RELEASE} checksum for {arch}/{os}"
        ))),
    }
}

/// `.jdk-version` pins the exact Temurin release, not only its feature line.
fn jdk_marker(checksum: &str) -> String {
    format!("{JDK_RELEASE}\n{checksum}\n")
}

fn jdk_marker_matches(marker: &str, checksum: &str) -> bool {
    let mut lines = marker.lines().map(str::trim);
    lines.next() == Some(JDK_RELEASE) && lines.next() == Some(checksum)
}

/// Swap an extracted directory into place without discarding the prior cache
/// if the final rename fails. A leftover backup is recovered on the next run.
fn replace_directory(staged: &Path, destination: &Path, backup: &Path) -> Result<(), DevError> {
    if backup.exists() {
        if destination.exists() {
            fs::remove_dir_all(backup)?;
        } else {
            fs::rename(backup, destination)?;
        }
    }

    let had_destination = destination.exists();
    if had_destination {
        fs::rename(destination, backup)?;
    }
    if let Err(error) = fs::rename(staged, destination) {
        if had_destination {
            drop(fs::rename(backup, destination));
        }
        return Err(error.into());
    }
    if had_destination {
        fs::remove_dir_all(backup)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Planetiler JAR
// ---------------------------------------------------------------------------

fn ensure_planetiler_jar(data_dir: &Path) -> Result<PathBuf, DevError> {
    let jar_path = data_dir.join("planetiler.jar");
    let version_file = data_dir.join(".planetiler-version");
    let marker = format!("{PLANETILER_RELEASE}\n{PLANETILER_SHA256}\n");

    // Cache only the release this build records. A different marker gets
    // replaced by the pinned asset below.
    if jar_path.exists()
        && let Ok(cached_marker) = fs::read_to_string(&version_file)
        && cached_marker == marker
    {
        return Ok(jar_path);
    }

    let download_url = format!(
        "https://github.com/onthegomap/planetiler/releases/download/{PLANETILER_RELEASE}/planetiler.jar"
    );

    // Download. Staged and digest-checked, so a partial or replaced asset
    // never sits at `jar_path`.
    output::bench_msg(&format!("downloading Planetiler {PLANETILER_RELEASE}"));
    fetch_verified(&download_url, &jar_path, Some(PLANETILER_SHA256))?;

    // Write version file.
    fs::write(&version_file, marker)?;

    output::bench_msg(&format!("installed Planetiler {PLANETILER_RELEASE}"));
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

/// Retry transient connection and server errors without imposing a total cap
/// on long-running downloads. `--retry-max-time` bounds the retry window, not
/// the duration of a transfer that is making progress.
const CURL_RETRY_ARGS: [&str; 6] = [
    "--retry",
    "3",
    "--retry-delay",
    "1",
    "--retry-max-time",
    "30",
];

/// Run curl with the given arguments, returning stdout bytes on success.
/// Bounded by [`CURL_STALL_ARGS`].
pub(crate) fn run_curl(args: &[&str], cwd: &Path) -> Result<Vec<u8>, DevError> {
    let mut all: Vec<&str> = CURL_STALL_ARGS.into_iter().chain(CURL_RETRY_ARGS).collect();
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
        .args(CURL_RETRY_ARGS)
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
            ending: crate::error::ProcessEnding::from_status(&status),
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
        let expected: Sha256 = expected.parse().map_err(|error| {
            DevError::Config(format!(
                "invalid SHA-256 digest from release metadata: {error}"
            ))
        })?;
        let actual = sha256_file(staged.tmp_path())?;
        if actual != expected {
            return Err(DevError::Preflight(vec![format!(
                "sha256 mismatch downloading {url}\n  expected: {expected}\n  actual:   {actual}"
            )]));
        }
    }
    staged.commit()?;
    Ok(())
}

/// SHA-256 of a file, computed in-process so verification does not depend on a
/// platform-specific hashing command.
fn sha256_file(path: &Path) -> Result<Sha256, DevError> {
    let file = fs::File::open(path)?;
    Sha256::from_reader(file).map_err(DevError::Io)
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
        .args(["--connect-timeout", "30", "--max-time", "60"])
        .args(CURL_RETRY_ARGS)
        .args(["-fsIL", url])
        .output()
        .map_err(|error| DevError::Spawn {
            program: "curl".into(),
            error,
        })?;

    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "curl".into(),
            ending: crate::error::ProcessEnding::from_status(&output.status),
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
    let minute: i64 = time_parts[1].parse().map_err(|e| format!("minute: {e}"))?;
    let second: i64 = time_parts[2].parse().map_err(|e| format!("second: {e}"))?;

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
    if find_executable(name).is_some() {
        Ok(())
    } else {
        Err(DevError::Preflight(vec![format!(
            "'{name}' not found in PATH (required to build tilemaker)"
        )]))
    }
}

/// Ensure tilemaker binary and shortbread config are ready.
pub fn ensure_tilemaker(data_dir: &Path) -> Result<TilemakerTools, DevError> {
    // Preflight: check build tools.
    check_build_tool("cmake")?;
    check_build_tool("g++")?;
    check_build_tool("make")?;

    let tilemaker_dir = ensure_pinned_git_source(
        data_dir,
        "tilemaker",
        "https://github.com/systemed/tilemaker.git",
        TILEMAKER_COMMIT,
        "tilemaker",
        &[":(exclude,top)build/"],
    )?;
    let version_file = data_dir.join(".tilemaker-version");
    // Keep generated CMake files outside the pinned source checkout so the
    // next source verification sees only upstream-controlled files.
    let build_dir = data_dir.join("tilemaker-build");
    let tilemaker_bin = build_dir.join("tilemaker");
    let tilemaker_dir_str = tilemaker_dir.display().to_string();

    // Check if build can be skipped.
    if tilemaker_bin.exists()
        && let Ok(cached) = fs::read_to_string(&version_file)
        && cached.trim() == TILEMAKER_COMMIT
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

    let nproc = crate::cpu_topology::available_parallelism_or_default();
    let jobs = format!("-j{nproc}");
    output::bench_msg("building tilemaker");
    let captured =
        output::run_captured("cmake", &["--build", &build_dir_str, "--", &jobs], data_dir)?;
    captured.check_success("cmake")?;

    // Write version file.
    fs::write(&version_file, TILEMAKER_COMMIT)?;

    let commit_short = &TILEMAKER_COMMIT[..8];
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
    ensure_pinned_git_source(
        data_dir,
        "shortbread-tilemaker",
        "https://github.com/shortbread-tiles/shortbread-tilemaker.git",
        SHORTBREAD_COMMIT,
        "shortbread-tilemaker config",
        &[],
    )
}

fn ensure_pinned_git_source(
    data_dir: &Path,
    local_name: &str,
    repository: &str,
    commit: &str,
    label: &str,
    generated_paths: &[&str],
) -> Result<PathBuf, DevError> {
    let path = data_dir.join(local_name);
    let path_text = path.display().to_string();
    let created = !path.exists();
    if created {
        output::bench_msg(&format!("cloning pinned {label}"));
        let captured = output::run_captured(
            "git",
            &[
                "clone",
                "--depth",
                "1",
                "--no-checkout",
                "--filter=blob:none",
                repository,
                &path_text,
            ],
            data_dir,
        )?;
        if let Err(error) = captured.check_success("git") {
            drop(fs::remove_dir_all(&path));
            return Err(error);
        }
    }

    let result = (|| {
        let status = git_source_status(data_dir, &path_text, generated_paths)?;
        if !created && !status.is_empty() {
            return Err(DevError::Refused(format!(
                "pinned {label} source at {} has local changes; move or remove it before retrying",
                path.display()
            )));
        }

        let current = git_captured(data_dir, &["-C", &path_text, "rev-parse", "HEAD"])?;
        let current = String::from_utf8_lossy(&current).trim().to_owned();
        if current != commit {
            git_captured(
                data_dir,
                &["-C", &path_text, "fetch", "--depth=1", "origin", commit],
            )?;
        }
        if created || current != commit {
            git_captured(
                data_dir,
                &["-C", &path_text, "checkout", "--detach", commit],
            )?;
        }

        let checked_out = git_captured(data_dir, &["-C", &path_text, "rev-parse", "HEAD"])?;
        let checked_out = String::from_utf8_lossy(&checked_out).trim().to_owned();
        if checked_out != commit {
            return Err(DevError::Build(format!(
                "git checked out {checked_out} for pinned {label}, expected {commit}"
            )));
        }

        let status = git_source_status(data_dir, &path_text, generated_paths)?;
        if !status.is_empty() {
            return Err(DevError::Refused(format!(
                "pinned {label} source at {} is not clean after checkout",
                path.display()
            )));
        }
        Ok(())
    })();

    if created && result.is_err() {
        drop(fs::remove_dir_all(&path));
    }
    result.map(|()| path)
}

fn git_captured(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, DevError> {
    let captured = output::run_captured("git", args, cwd)?;
    captured.check_success("git")?;
    Ok(captured.stdout)
}

fn git_source_status(
    cwd: &Path,
    source_path: &str,
    generated_paths: &[&str],
) -> Result<Vec<u8>, DevError> {
    let mut args = vec![
        "-C",
        source_path,
        "status",
        "--porcelain=v1",
        "--no-renames",
        "--untracked-files=all",
    ];
    if !generated_paths.is_empty() {
        args.extend(["--", "."]);
        args.extend_from_slice(generated_paths);
    }
    git_captured(cwd, &args)
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
    fn jdk_marker_pins_the_exact_release() {
        assert!(jdk_marker_matches(&jdk_marker("digest-a"), "digest-a"));
        assert!(!jdk_marker_matches(&jdk_marker("digest-a"), "digest-b"));
        assert!(!jdk_marker_matches("jdk-25.0.1+8\ndigest-a\n", "digest-a"));
        assert!(!jdk_marker_matches("jdk-25.0.4+7\ndigest-a\n", "digest-a"));
        assert!(!jdk_marker_matches("", "digest-a"));
    }

    #[test]
    fn sha256_file_matches_the_fips_vector() {
        let dir = crate::test_scratch::scratch("tools", "sha256");
        let path = dir.join("abc");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap().to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn parse_http_date_rejects_garbage() {
        assert!(parse_http_date("not a date").is_err());
        assert!(parse_http_date("Sun, 06 XYZ 1994 08:49:37 GMT").is_err());
    }
}
