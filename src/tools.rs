//! External tool download and cache management.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::output;
use crate::preflight::{Sha256, Xxh128};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The pinned JDK and Planetiler jar used by elivagar.
pub struct PlanetilerRuntime {
    pub java: PathBuf,
    pub planetiler_jar: PathBuf,
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
const TILEMAKER_COMMIT: &str = "facdf933f4f5ac2944a9d65c27970270583d68fa";
const SHORTBREAD_COMMIT: &str = "9ec009826b49b71bb1481a1c28c3681766c600e3";

// ---------------------------------------------------------------------------
// Top-level entry point
// ---------------------------------------------------------------------------

/// Ensure the pinned JDK and Planetiler jar are ready.
pub fn ensure_planetiler_runtime(data_dir: &Path) -> Result<PlanetilerRuntime, DevError> {
    check_curl()?;

    let java = ensure_jdk(data_dir)?;
    let planetiler_jar = ensure_planetiler_jar(data_dir)?;

    Ok(PlanetilerRuntime {
        java,
        planetiler_jar,
    })
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
    let marker = planetiler_marker();

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

/// `.planetiler-version`: the exact release and its asset digest.
fn planetiler_marker() -> String {
    format!("{PLANETILER_RELEASE}\n{PLANETILER_SHA256}\n")
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
/// No hash check. Use this for mutable upstream files without a pin, such as
/// the daily-regenerated ocean archives. What is guaranteed is that `dest` is
/// absent or a complete transfer.
pub(crate) fn download_file(url: &str, dest: &Path) -> Result<(), DevError> {
    download_file_inner(url, dest, None)
}

/// Download a versioned archive and verify its pinned XXH128 before it is
/// committed to `dest`.
pub(crate) fn download_file_verified(
    url: &str,
    dest: &Path,
    expected_xxh128: &str,
) -> Result<(), DevError> {
    let expected: Xxh128 = expected_xxh128
        .parse()
        .map_err(|error| DevError::Config(format!("invalid XXH128 pin: {error}")))?;
    download_file_inner(url, dest, Some(expected))
}

fn download_file_inner(
    url: &str,
    dest: &Path,
    expected_xxh128: Option<Xxh128>,
) -> Result<(), DevError> {
    // Download to a staged sibling and rename on success, so a partial file
    // never sits at `dest` to block future retries. The stable `.partial`
    // name (not a per-process one) means a killed multi-gigabyte transfer is
    // overwritten by the retry instead of leaked; downloads run under the
    // global lock, so no second writer shares it.
    let staged = crate::atomic_write::Staged::stable(dest)?;
    let tmp_str = staged.tmp_path().display().to_string();
    let _activity = crate::lockfile::activity::Activity::begin(format!("download {url}"));

    // No `--max-time`: see `CURL_STALL_ARGS` for why a stall detector bounds
    // this rather than a total-duration cap.
    let status = crate::output::child::command_status(
        crate::process::spawn::ChildPolicy::plain_command("curl")
            .args(CURL_STALL_ARGS)
            .args(CURL_RETRY_ARGS)
            .args(["-fL", "--progress-bar", "-o", &tmp_str, url])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit()),
    )
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

    if let Some(expected) = expected_xxh128 {
        let actual = crate::preflight::compute_file_digest(staged.tmp_path())?;
        if actual.value() != expected {
            return Err(DevError::Preflight(vec![format!(
                "XXH128 mismatch downloading {url}\n  expected: {expected}\n  actual:   {actual}"
            )]));
        }
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
}
