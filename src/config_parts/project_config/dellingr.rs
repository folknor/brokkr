use crate::error::DevError;
use crate::preflight::FileDigest;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Dellingr-specific configuration from `[dellingr]` in brokkr.toml.
///
/// Drives `brokkr dellingr`, the Lua-workload bench harness. Two parts:
///
/// - `example` names the cargo `--example` target brokkr builds. The features
///   are *not* configured: `--bench` builds it bare, `--hotpath` adds
///   `hotpath`, `--alloc` adds `hotpath-alloc`. Those names are brokkr's
///   standing convention across every project ([`crate::harness::hotpath_feature`]),
///   so restating them per project would only create a way for them to disagree.
/// - `[dellingr.workloads.<name>]` is the registry: `--lua <name>` resolves to
///   `file`, and `xxh128` pins its content.
///
/// The registry is deliberately *not* hostname-keyed, unlike `[<host>.datasets]`.
/// Workloads are in-repo files identical on every host, so a per-host section
/// would imply a variability that does not exist.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DellingrConfig {
    /// The cargo `--example` target implementing the bench harness.
    pub example: String,
    /// Registered workloads, keyed by the name `--lua` takes.
    #[serde(default)]
    pub workloads: BTreeMap<String, DellingrWorkload>,
}

/// One registered workload: a repo-relative path plus its pinned digest.
///
/// The hash is what makes a stored row mean something. These files are
/// editable source, not immutable datasets - silently retuning one would
/// leave every historical row under the same name describing a workload that
/// no longer exists. Verification turns that into a deliberate
/// re-registration.
///
/// `hotpath_file` / `hotpath_xxh128` register a second, instrumentation-scale
/// variant of the same kernel, resolved *instead of* `file` by the
/// instrumented modes (`--hotpath` / `--alloc`). The split exists because the
/// two mode families need opposite workload scales: `--bench` wants
/// seconds-scale files so wall deltas resolve above launch noise, while the
/// hotpath crate queues one event per instrumented call in an unbounded
/// per-thread queue that its 50ms-tick consumer drains far slower than a VM
/// dispatch loop fills it - a seconds-scale file backlogs tens of GB of RAM
/// before the run ends. Instrumented modes therefore *require* the pair: a
/// workload without it refuses `--hotpath`/`--alloc` rather than risk the
/// memory cliff.
///
/// Both pins are parsed as [`FileDigest`] values during config deserialization;
/// `parse` checks that their paths are repo-relative, and the hotpath
/// pair must be whole - `hotpath_file` and `hotpath_xxh128` are rejected apart.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DellingrWorkload {
    /// Path to the `.lua` file, relative to the directory holding `brokkr.toml`.
    pub file: PathBuf,
    /// Expected 32-hex XXH128 file digest (brokkr's standard content digest).
    pub xxh128: FileDigest,
    /// Instrumentation-scale variant resolved by `--hotpath` / `--alloc`
    /// (tens-of-ms per `_bench` call, not seconds). Must be registered
    /// together with `hotpath_xxh128`.
    #[serde(default)]
    pub hotpath_file: Option<PathBuf>,
    /// Pinned digest of `hotpath_file`. Must be registered together with it.
    #[serde(default)]
    pub hotpath_xxh128: Option<FileDigest>,
}

impl DellingrConfig {
    /// Look up a workload by the name `--lua` takes, with an error listing
    /// the registered names when it is absent.
    pub fn workload(&self, name: &str) -> Result<&DellingrWorkload, DevError> {
        self.workloads.get(name).ok_or_else(|| {
            let known: Vec<&str> = self.workloads.keys().map(String::as_str).collect();
            let known = if known.is_empty() {
                "none registered".to_owned()
            } else {
                known.join(", ")
            };
            DevError::Config(format!(
                "unknown workload {name:?}\n  registered: {known}\n  \
                 add it as [dellingr.workloads.{name}] in brokkr.toml"
            ))
        })
    }
}

/// Parse the required `[dellingr]` section.
///
/// Each workload's file/digest pairs are validated here rather than at
/// resolution time: an absolute path would escape the project root and read a
/// workload nobody registered, and it is a config mistake worth naming at
/// parse time rather than as a confusing hash mismatch later. The optional
/// `hotpath_file` / `hotpath_xxh128` pair gets the identical treatment - it is
/// resolved by `--hotpath`/`--alloc` exactly the way `file` is by `--bench`,
/// so a guard that covered only one of them would be half a guard.
///
/// The pair must also be whole. Validating that here and not in
/// [`crate::dellingr::workload::resolve`] means a half-registered workload is
/// named even when nobody runs *that* workload today.
pub(crate) fn parse(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<DellingrConfig, DevError> {
    let value = table.get("dellingr").ok_or_else(|| {
        DevError::Config(
            "missing required [dellingr] section in brokkr.toml - a dellingr project \
             names the harness target and at least one workload:\n\n  [dellingr]\n  \
             example = \"hotpath\"\n\n  [dellingr.workloads.same_obj_read]\n  \
             file = \"examples/fields/same_obj_read.lua\"\n  xxh128 = \"...\""
                .into(),
        )
    })?;
    let config: DellingrConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("dellingr: {e}")))?;

    if config.example.trim().is_empty() {
        return Err(DevError::Config(
            "[dellingr].example is empty - name the cargo --example target \
             implementing the bench harness."
                .into(),
        ));
    }
    for (name, workload) in &config.workloads {
        check_workload_pin(name, "file", &workload.file)?;

        match (&workload.hotpath_file, &workload.hotpath_xxh128) {
            (Some(file), Some(_)) => {
                check_workload_pin(name, "hotpath_file", file)?;
            }
            (None, None) => {}
            (present, _) => {
                let (have, missing) = if present.is_some() {
                    ("hotpath_file", "hotpath_xxh128")
                } else {
                    ("hotpath_xxh128", "hotpath_file")
                };
                return Err(DevError::Config(format!(
                    "[dellingr.workloads.{name}] sets {have} without {missing} - \
                     the instrumentation-scale variant needs both a path and a \
                     pinned digest."
                )));
            }
        }
    }
    Ok(config)
}

/// Validate one workload pin's path. The digest half needs no check here: it
/// deserialized as a `FileDigest`, so an empty or malformed value was already
/// refused, naming the key.
///
/// `path_key` is the key being checked, so the error points at the offending
/// line: a workload carries two independent pins (`file` + `xxh128` and
/// `hotpath_file` + `hotpath_xxh128`) and the fix differs between them.
fn check_workload_pin(name: &str, path_key: &str, file: &Path) -> Result<(), DevError> {
    if file.is_absolute() {
        return Err(DevError::Config(format!(
            "[dellingr.workloads.{name}].{path_key} {file:?} is absolute; \
             workload paths are relative to the directory holding brokkr.toml."
        )));
    }
    Ok(())
}
