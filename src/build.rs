use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Build configuration
// ---------------------------------------------------------------------------

pub struct BuildConfig {
    pub package: Option<String>,
    pub bin: Option<String>,
    pub example: Option<String>,
    pub features: Vec<String>,
    pub default_features: bool,
    pub profile: &'static str,
}

impl BuildConfig {
    pub fn release(package: Option<&str>) -> Self {
        Self {
            package: package.map(std::borrow::ToOwned::to_owned),
            bin: None,
            example: None,
            features: Vec::new(),
            default_features: true,
            profile: "release",
        }
    }

    pub fn release_with_features(package: Option<&str>, features: &[&str]) -> Self {
        Self {
            package: package.map(std::borrow::ToOwned::to_owned),
            bin: None,
            example: None,
            features: features.iter().map(|s| (*s).to_owned()).collect(),
            default_features: true,
            profile: "release",
        }
    }

    /// Release build with features from owned strings (e.g. host config features).
    pub fn release_with_owned_features(package: Option<&str>, features: &[String]) -> Self {
        Self {
            package: package.map(std::borrow::ToOwned::to_owned),
            bin: None,
            example: None,
            features: features.to_vec(),
            default_features: true,
            profile: "release",
        }
    }

    pub fn release_no_defaults(package: Option<&str>, features: &[&str]) -> Self {
        Self {
            package: package.map(std::borrow::ToOwned::to_owned),
            bin: None,
            example: None,
            features: features.iter().map(|s| (*s).to_owned()).collect(),
            default_features: false,
            profile: "release",
        }
    }
}

// ---------------------------------------------------------------------------
// Project info
// ---------------------------------------------------------------------------

pub struct ProjectInfo {
    pub target_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// project_info
// ---------------------------------------------------------------------------

/// Resolve target directory via `cargo metadata`.
///
/// Runs `cargo metadata` in the given directory (or cwd if `None`).
pub fn project_info(cwd: Option<&Path>) -> Result<ProjectInfo, DevError> {
    let project_root = match cwd {
        Some(p) => p.to_owned(),
        None => std::env::current_dir()
            .map_err(|e| DevError::Build(format!("cannot determine current directory: {e}")))?,
    };

    let captured = output::run_captured(
        "cargo",
        &["metadata", "--format-version", "1", "--no-deps"],
        &project_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("cargo metadata failed: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&captured.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout)?;

    let target_dir = extract_string(&val, "target_directory")?;

    Ok(ProjectInfo {
        target_dir: PathBuf::from(target_dir),
    })
}

/// Extract a string field from a JSON value, returning a `DevError::Build` on
/// missing or wrong type.
fn extract_string<'a>(val: &'a serde_json::Value, key: &str) -> Result<&'a str, DevError> {
    val.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DevError::Build(format!("cargo metadata missing \"{key}\" field")))
}

// ---------------------------------------------------------------------------
// cargo build
// ---------------------------------------------------------------------------

/// Build a crate and return the path to the compiled binary.
///
/// Parses `--message-format=json` output to find the `"executable"` field.
pub fn cargo_build(config: &BuildConfig, project_root: &Path) -> Result<PathBuf, DevError> {
    let args = build_args(config);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::build_msg(&format!("cargo {}", arg_refs.join(" ")));

    let start = Instant::now();
    let captured = output::run_captured("cargo", &arg_refs, project_root)?;
    let elapsed = start.elapsed();

    if !captured.status.success() {
        dump_compiler_messages(&captured.stdout);
        dump_build_stderr(&captured.stderr);
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("cargo build failed: {stderr}")));
    }

    let expected = config.bin.as_deref().or(config.package.as_deref());
    let executable = find_executable(&captured.stdout, expected)?;

    output::build_msg(&format!(
        "done in {:.1}s -> {}",
        elapsed.as_secs_f64(),
        executable.display(),
    ));

    Ok(executable)
}

/// Resolve the expected release binary path from cargo metadata without
/// building.
pub fn resolve_existing_binary(
    config: &BuildConfig,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let captured = output::run_captured(
        "cargo",
        &["metadata", "--format-version", "1", "--no-deps"],
        project_root,
    )?;
    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("cargo metadata failed: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&captured.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout)?;
    let target_dir = extract_string(&val, "target_directory")?;
    let bin_name = resolve_bin_name(&val, config)?;

    let mut path = PathBuf::from(target_dir);
    path.push(config.profile);
    path.push(bin_name);

    if cfg!(windows) {
        path.set_extension("exe");
    }

    if !path.exists() {
        return Err(DevError::Build(format!(
            "binary not found at {} (build first or omit --no-build)",
            path.display()
        )));
    }

    Ok(path)
}

/// Assemble the argument list for `cargo build`.
fn build_args(config: &BuildConfig) -> Vec<String> {
    let mut args = Vec::with_capacity(10);
    args.push("build".into());

    if let Some(ref pkg) = config.package {
        args.push("-p".into());
        args.push(pkg.clone());
    }

    if let Some(ref bin) = config.bin {
        args.push("--bin".into());
        args.push(bin.clone());
    }

    if let Some(ref example) = config.example {
        args.push("--example".into());
        args.push(example.clone());
    }

    if config.profile == "release" {
        args.push("--release".into());
    } else {
        args.push("--profile".into());
        args.push(config.profile.into());
    }

    args.push("--message-format=json".into());

    if !config.default_features {
        args.push("--no-default-features".into());
    }

    if !config.features.is_empty() {
        args.push("--features".into());
        args.push(config.features.join(","));
    }

    args
}

fn resolve_bin_name(
    metadata: &serde_json::Value,
    config: &BuildConfig,
) -> Result<String, DevError> {
    if let Some(ref bin) = config.bin {
        return Ok(bin.clone());
    }

    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| DevError::Build("cargo metadata missing packages".into()))?;

    let package = if let Some(ref name) = config.package {
        packages
            .iter()
            .find(|pkg| pkg.get("name").and_then(serde_json::Value::as_str) == Some(name.as_str()))
            .ok_or_else(|| DevError::Build(format!("package '{name}' not found in metadata")))?
    } else {
        let root_id = metadata
            .get("resolve")
            .and_then(|r| r.get("root"))
            .and_then(serde_json::Value::as_str);
        if let Some(id) = root_id {
            packages
                .iter()
                .find(|pkg| pkg.get("id").and_then(serde_json::Value::as_str) == Some(id))
                .ok_or_else(|| {
                    DevError::Build(format!("root package '{id}' not found in metadata"))
                })?
        } else {
            packages
                .first()
                .ok_or_else(|| DevError::Build("cargo metadata has no packages".into()))?
        }
    };

    let targets = package
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| DevError::Build("package metadata missing targets".into()))?;

    let mut bins: Vec<&serde_json::Value> = Vec::new();
    for target in targets {
        let kinds = target
            .get("kind")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| DevError::Build("target metadata missing kind".into()))?;
        if kinds.iter().any(|k| k.as_str() == Some("bin")) {
            bins.push(target);
        }
    }

    if bins.is_empty() {
        return Err(DevError::Build(
            "no binary targets found in package metadata".into(),
        ));
    }

    if bins.len() == 1 {
        return bins[0]
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(std::borrow::ToOwned::to_owned)
            .ok_or_else(|| DevError::Build("binary target missing name".into()));
    }

    if let Some(ref pkg) = config.package
        && let Some(named) = bins.iter().find(|target| {
            target.get("name").and_then(serde_json::Value::as_str) == Some(pkg.as_str())
        })
    {
        return named
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(std::borrow::ToOwned::to_owned)
            .ok_or_else(|| DevError::Build("binary target missing name".into()));
    }

    bins[0]
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(std::borrow::ToOwned::to_owned)
        .ok_or_else(|| DevError::Build("binary target missing name".into()))
}

/// Scan JSON lines from cargo output to find the compiled executable.
///
/// When `expected_name` is `Some`, prefer the executable whose file stem
/// matches the name exactly (e.g. `"nidhogg"` matches `target/release/nidhogg`
/// but not `target/release/nidhogg-update`).  Falls back to the last
/// executable if no exact match is found.
pub fn find_executable(stdout: &[u8], expected_name: Option<&str>) -> Result<PathBuf, DevError> {
    let text = String::from_utf8_lossy(stdout);
    let mut all_exes: Vec<PathBuf> = Vec::new();
    let mut last_exe: Option<PathBuf> = None;
    let mut matched_exe: Option<PathBuf> = None;

    for line in text.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(exe) = val.get("executable").and_then(serde_json::Value::as_str) {
            let path = PathBuf::from(exe);
            if expected_name.is_some() && path.file_stem().and_then(|s| s.to_str()) == expected_name
            {
                matched_exe = Some(path.clone());
            }
            all_exes.push(path.clone());
            last_exe = Some(path);
        }
    }

    if let Some(exe) = matched_exe {
        return Ok(exe);
    }

    // No name to match — require exactly one executable to avoid
    // order-dependent behaviour (cargo doesn't guarantee JSON ordering).
    match last_exe {
        Some(exe) if all_exes.len() == 1 => Ok(exe),
        Some(_) => Err(DevError::Build(format!(
            "found {} executables in cargo output but no expected name to disambiguate; \
             specify --bin or -p",
            all_exes.len()
        ))),
        None => Err(DevError::Build("no executable in cargo output".into())),
    }
}

/// Extract compiler diagnostics from `--message-format=json` stdout and print
/// the rendered messages.  With JSON message format, cargo sends the actual
/// error details (file, line, message) as JSON to stdout — stderr only contains
/// the "Compiling…" progress lines and the final summary.
fn dump_compiler_messages(stdout: &[u8]) {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-message") {
            continue;
        }
        if let Some(rendered) = val
            .get("message")
            .and_then(|m| m.get("rendered"))
            .and_then(serde_json::Value::as_str)
        {
            // rendered already ends with newline; trim to avoid double-spacing
            output::error(rendered.trim_end());
        }
    }
}

/// Print captured stderr through the error output channel.
fn dump_build_stderr(stderr: &[u8]) {
    let text = String::from_utf8_lossy(stderr);
    if !text.is_empty() {
        output::error(&text);
    }
}
