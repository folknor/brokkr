//! What a measured binary was built with that its commit does not record.
//!
//! A results row names the commit, the cargo profile and the features. What
//! else decides the code a build emits lives outside the tree: the compiler
//! the toolchain resolves to, rustflags from a user-wide `~/.cargo/config.toml`
//! (`-Ctarget-cpu=native`, a linker), profile overrides in a config file or
//! `CARGO_PROFILE_*`, a rustc wrapper. Two rows of one commit can differ in all
//! of them, and a wall delta between them is then not the code's.
//!
//! Two halves, captured where each is known:
//!
//! - **Host side** ([`host_pairs`]): the compiler's `-V` line, the rustflags
//!   cargo applies and the source that won, and the raw inputs they came from -
//!   the codegen-relevant keys of every config file in the chain and the
//!   environment variables cargo reads for them. A function of the build root
//!   and brokkr's environment alone: a measured cargo build gets that
//!   environment plus nothing but `CARGO_TARGET_DIR` (`[cargo_env]` refuses
//!   every cargo and rustc name), so every build of one root in one brokkr
//!   process shares the answer, and it is computed once per root.
//! - **Unit side** ([`unit_pairs`]): cargo's own resolved profile for the
//!   measured executable, from its compiler-artifact record - `opt-level`,
//!   `debuginfo`, `debug-assertions`, `overflow-checks` as cargo applied them,
//!   whatever mix of manifest and config decided them.
//!
//! Stored as `provenance.*` pairs, a namespace only the harness writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::db::{KvKey, KvPair};
use crate::rustflags::EffectiveEnv;

/// One measured build's provenance: name -> value, in a stable order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildProvenance {
    pairs: Vec<(String, String)>,
}

impl BuildProvenance {
    /// The host side for `build_root` joined with a unit's own pairs.
    pub(crate) fn of(build_root: &Path, unit: &[(String, String)]) -> Self {
        let mut pairs = host_pairs(build_root);
        pairs.extend(unit.iter().cloned());
        Self { pairs }
    }

    /// The row pairs, each under the `provenance.` namespace.
    pub(crate) fn kv(&self) -> Vec<KvPair> {
        self.pairs
            .iter()
            .map(|(name, value)| KvPair::text(KvKey::provenance(name.as_str()), value.as_str()))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self {
            pairs: pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
        }
    }
}

/// Host-side pairs per build root, computed once per brokkr process.
static HOST_CACHE: Mutex<BTreeMap<PathBuf, Vec<(String, String)>>> = Mutex::new(BTreeMap::new());

/// The host-side provenance of builds rooted at `build_root` (see the module
/// docs), cached per root.
pub(crate) fn host_pairs(build_root: &Path) -> Vec<(String, String)> {
    let mut cache = HOST_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .entry(build_root.to_path_buf())
        .or_insert_with(|| compute_host_pairs(build_root, EffectiveEnv::new(&[]), std::env::vars_os()))
        .clone()
}

fn compute_host_pairs(
    build_root: &Path,
    env: EffectiveEnv<'_>,
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    // A compiler that cannot be asked is recorded as such: absence would read
    // as "not recorded", which a comparison treats as unknown, not as this.
    let rustc = crate::target_platform::compiler_version(build_root, env)
        .unwrap_or_else(|e| format!("unavailable ({e})"));
    pairs.push(("rustc".to_owned(), rustc));
    let flags = crate::rustflags::effective_rustflags(build_root, env);
    pairs.push(("rustflags_source".to_owned(), flags.source.to_owned()));
    if let Some(flags) = flags.flags {
        pairs.push(("rustflags".to_owned(), flags.join(" ")));
    }
    let config = config_inputs(build_root, env);
    if !config.is_empty() {
        pairs.push(("config".to_owned(), config));
    }
    let env_inputs = env_inputs(vars);
    if !env_inputs.is_empty() {
        pairs.push(("env".to_owned(), env_inputs));
    }
    pairs
}

/// The codegen-relevant keys of every config file in the chain, highest
/// precedence first, as `key=value` joined by `; `. File paths are left out:
/// the same settings at another home directory are the same build.
fn config_inputs(build_root: &Path, env: EffectiveEnv<'_>) -> String {
    const BUILD_KEYS: [&str; 7] = [
        "rustflags",
        "rustc",
        "rustc-wrapper",
        "rustc-workspace-wrapper",
        "target",
        "rustdocflags",
        "incremental",
    ];
    let Ok(files) = crate::rustflags::read_config_files_under(build_root, env) else {
        return "unreadable".to_owned();
    };
    let mut lines: Vec<String> = Vec::new();
    for (_, doc) in &files {
        if let Some(build) = doc.get("build").and_then(toml::Value::as_table) {
            for key in BUILD_KEYS {
                if let Some(value) = build.get(key) {
                    lines.push(format!("build.{key}={value}"));
                }
            }
        }
        if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
            for (selector, table) in targets {
                let Some(table) = table.as_table() else {
                    continue;
                };
                for key in ["rustflags", "linker"] {
                    if let Some(value) = table.get(key) {
                        lines.push(format!("target.{selector}.{key}={value}"));
                    }
                }
            }
        }
        if let Some(profiles) = doc.get("profile").and_then(toml::Value::as_table) {
            for (name, value) in profiles {
                lines.push(format!("profile.{name}={value}"));
            }
        }
    }
    lines.join("; ")
}

/// Whether cargo reads `name` for how it compiles: the rustflags and compiler
/// selectors, the build target, incremental compilation, and every profile and
/// target-table override.
fn is_build_env(name: &str) -> bool {
    const EXACT: [&str; 12] = [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_RUSTC",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_TARGET",
        "CARGO_INCREMENTAL",
        "CARGO_BUILD_INCREMENTAL",
    ];
    EXACT.contains(&name)
        || name.starts_with("CARGO_PROFILE_")
        || (name.starts_with("CARGO_TARGET_")
            && (name.ends_with("_RUSTFLAGS") || name.ends_with("_LINKER")))
}

/// Every build-relevant variable in `vars`, sorted, as `NAME=value; ...`.
fn env_inputs(vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>) -> String {
    let mut found: Vec<String> = vars
        .into_iter()
        .filter_map(|(name, value)| {
            let name = name.into_string().ok()?;
            is_build_env(&name).then(|| format!("{name}={}", value.to_string_lossy()))
        })
        .collect();
    found.sort();
    found.join("; ")
}

/// Cargo's resolved profile for one compiled unit, from its
/// compiler-artifact record's `profile` object.
pub(crate) fn unit_pairs(profile: &serde_json::Value) -> Vec<(String, String)> {
    ["opt_level", "debuginfo", "debug_assertions", "overflow_checks"]
        .into_iter()
        .filter_map(|key| {
            let value = profile.get(key)?;
            let text = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Some((key.to_owned(), text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn only_build_relevant_variables_are_recorded() {
        let vars = [
            ("RUSTFLAGS", "-Ctarget-cpu=native"),
            ("CARGO_INCREMENTAL", "0"),
            ("CARGO_PROFILE_RELEASE_LTO", "fat"),
            ("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER", "clang"),
            ("CARGO_TARGET_DIR", "/elsewhere"),
            ("HOME", "/home/x"),
        ]
        .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)));
        assert_eq!(
            env_inputs(vars),
            "CARGO_INCREMENTAL=0; \
             CARGO_PROFILE_RELEASE_LTO=fat; \
             CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang; \
             RUSTFLAGS=-Ctarget-cpu=native"
        );
    }

    #[test]
    fn config_inputs_name_the_codegen_keys_of_the_chain() {
        let root = crate::test_scratch::scratch("build_provenance", "config");
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"-Ctarget-cpu=native\"]\njobs = 4\nincremental = false\n\
             [target.x86_64-unknown-linux-gnu]\nlinker = \"clang\"\n\
             [profile.dev]\ndebug = \"line-tables-only\"\n",
        )
        .unwrap();
        let home = [("CARGO_HOME", "no-cargo-home")];
        let text = config_inputs(&root, EffectiveEnv::new(&home));
        assert!(text.contains("build.rustflags=[\"-Ctarget-cpu=native\"]"), "{text}");
        assert!(text.contains("target.x86_64-unknown-linux-gnu.linker=\"clang\""), "{text}");
        assert!(text.contains("profile.dev="), "{text}");
        assert!(text.contains("build.incremental=false"), "{text}");
        assert!(!text.contains("jobs"), "not codegen: {text}");
    }

    #[test]
    fn the_unit_profile_reads_cargos_record() {
        let profile = serde_json::json!({
            "opt_level": "3", "debuginfo": 0, "debug_assertions": false,
            "overflow_checks": false, "test": false
        });
        assert_eq!(
            unit_pairs(&profile),
            [
                ("opt_level".to_owned(), "3".to_owned()),
                ("debuginfo".to_owned(), "0".to_owned()),
                ("debug_assertions".to_owned(), "false".to_owned()),
                ("overflow_checks".to_owned(), "false".to_owned()),
            ]
        );
    }
}
