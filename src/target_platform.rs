//! A `[[check]]` sweep's `target` triple, resolved against the toolchain cargo
//! will build it with.
//!
//! A target sweep passes `--target <triple>` to every cargo run, and brokkr
//! then executes the binaries itself. Doing that honestly needs facts about
//! the triple that only the toolchain has: which `cfg` it sets (for cargo's
//! `[target.'cfg(...)']` runner and rustflags tables), where its runtime
//! libraries live (for the loader path), and that cargo runs its doctests at
//! all. Every probe goes through the compiler cargo itself would pick, under
//! the sweep's environment, so preflight never inspects one toolchain while
//! cargo builds with another.
//!
//! The cfg facts are deliberately partial. Only names the triple alone fixes
//! are decided ([`DECIDED_KEYS`], `unix`, `windows`); anything flags can move
//! (`target_feature`, `panic`, `debug_assertions`, a custom `--cfg`) is left
//! undecided, which callers treat in the loud direction: an undecided runner
//! table refuses, an undecided rustflags table falls back with a notice.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::output;
use crate::rustflags::EffectiveEnv;

/// The first cargo that runs doctests for a `--target` other than the host
/// unconditionally (`doctest-xcompile`, stabilized in 1.89). Older ones skip
/// them silently, which a doctest obligation must not pass for.
const MIN_CARGO_MINOR: u64 = 89;

/// `cfg` keys whose values the target triple alone decides.
const DECIDED_KEYS: [&str; 8] = [
    "target_arch",
    "target_os",
    "target_family",
    "target_env",
    "target_vendor",
    "target_abi",
    "target_pointer_width",
    "target_endian",
];

/// Bare `cfg` names the target triple alone decides.
const DECIDED_FLAGS: [&str; 2] = ["unix", "windows"];

/// What `rustc --print cfg --target <triple>` reported, restricted to what
/// it decides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TargetCfg {
    values: BTreeMap<String, BTreeSet<String>>,
    flags: BTreeSet<String>,
}

impl TargetCfg {
    /// Parse `rustc --print cfg` output: `name` or `key="value"` per line.
    pub(crate) fn parse(text: &str) -> Self {
        let mut out = Self::default();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match line.split_once('=') {
                Some((key, value)) => {
                    let value = value.trim().trim_matches('"');
                    out.values
                        .entry(key.trim().to_owned())
                        .or_default()
                        .insert(value.to_owned());
                }
                None => {
                    out.flags.insert(line.to_owned());
                }
            }
        }
        out
    }

    /// Whether `key = "value"` holds, or `None` when the triple does not
    /// decide `key`.
    pub(crate) fn key_matches(&self, key: &str, value: &str) -> Option<bool> {
        DECIDED_KEYS
            .contains(&key)
            .then(|| self.values.get(key).is_some_and(|v| v.contains(value)))
    }

    /// Whether bare `name` holds, or `None` when the triple does not decide it.
    pub(crate) fn flag(&self, name: &str) -> Option<bool> {
        DECIDED_FLAGS
            .contains(&name)
            .then(|| self.flags.contains(name))
    }
}

/// A target sweep's triple, with the toolchain facts every lane needs.
#[derive(Debug, Clone)]
pub(crate) struct SweepTarget {
    pub(crate) triple: String,
    pub(crate) cfg: TargetCfg,
    /// `rustc --print target-libdir --target <triple>`: the runtime libraries
    /// a binary built for the triple loads from.
    pub(crate) libdir: String,
}

impl SweepTarget {
    /// Resolve `triple` against the toolchain cargo would use for a build in
    /// `root` under `env`, refusing a toolchain that cannot build or test it.
    pub(crate) fn resolve(
        root: &Path,
        triple: &str,
        env: EffectiveEnv<'_>,
        files: &[(PathBuf, toml::Table)],
    ) -> Result<Self, DevError> {
        require_cargo_version(root, env)?;
        let rustc = compiler(root, env, files);
        let sysroot = probe(root, env, &rustc, &["--print", "sysroot"])?;
        let installed = Path::new(sysroot.trim())
            .join("lib/rustlib")
            .join(triple)
            .join("lib");
        if !installed.is_dir() {
            return Err(DevError::Config(format!(
                "target `{triple}` is not installed for the toolchain at {} (`rustup target add \
                 {triple}`)",
                sysroot.trim()
            )));
        }
        let cfg = TargetCfg::parse(&probe(
            root,
            env,
            &rustc,
            &["--print", "cfg", "--target", triple],
        )?);
        let libdir = probe(
            root,
            env,
            &rustc,
            &["--print", "target-libdir", "--target", triple],
        )?
        .trim()
        .to_owned();
        Ok(Self {
            triple: triple.to_owned(),
            cfg,
            libdir,
        })
    }
}

/// The host libdir of the compiler cargo runs in `root` under `env`: where a
/// host unit (a proc-macro's test harness, which links `libstd` dynamically)
/// loads its runtime from.
pub(crate) fn host_libdir(root: &Path, env: EffectiveEnv<'_>) -> Result<String, DevError> {
    let files = crate::rustflags::read_config_files_under(root, env)?;
    let rustc = compiler(root, env, &files);
    Ok(probe(root, env, &rustc, &["--print", "target-libdir"])?
        .trim()
        .to_owned())
}

/// The compiler cargo runs, by cargo's own precedence: `RUSTC`, then
/// `CARGO_BUILD_RUSTC`, then the highest-precedence `build.rustc` in the
/// config chain, then `rustc` from `PATH`. A config path holding a `/` is
/// taken relative to the directory containing that config's `.cargo`, as
/// cargo does; a bare name is looked up on `PATH`.
fn compiler(root: &Path, env: EffectiveEnv<'_>, files: &[(PathBuf, toml::Table)]) -> String {
    for key in ["RUSTC", "CARGO_BUILD_RUSTC"] {
        if let Some(value) = env
            .get_os(key)
            .and_then(|v| v.into_string().ok())
            .filter(|v| !v.is_empty())
        {
            return absolute_if_path(root, &value);
        }
    }
    let configured = files.iter().find_map(|(path, doc)| {
        let value = doc.get("build")?.as_table()?.get("rustc")?.as_str()?;
        Some((path, value.to_owned()))
    });
    match configured {
        Some((path, value)) if value.contains('/') => {
            let anchor = path.parent().and_then(Path::parent).unwrap_or(root);
            anchor.join(value).display().to_string()
        }
        Some((_, value)) => value,
        None => "rustc".to_owned(),
    }
}

fn absolute_if_path(root: &Path, value: &str) -> String {
    if value.contains('/') {
        root.join(value).display().to_string()
    } else {
        value.to_owned()
    }
}

fn probe(
    root: &Path,
    env: EffectiveEnv<'_>,
    program: &str,
    args: &[&str],
) -> Result<String, DevError> {
    let captured = output::run_captured_with_env(program, args, root, env.pairs())?;
    if !captured.success() {
        return Err(DevError::Build(format!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&captured.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&captured.stdout).into_owned())
}

/// Refuse a cargo older than [`MIN_CARGO_MINOR`]: it would build a target
/// sweep's doctests and then not run them.
fn require_cargo_version(root: &Path, env: EffectiveEnv<'_>) -> Result<(), DevError> {
    let text = probe(root, env, "cargo", &["--version"])?;
    match cargo_minor(&text) {
        Some(minor) if minor >= MIN_CARGO_MINOR => Ok(()),
        Some(_) => Err(DevError::Config(format!(
            "{} does not run doctests for a `--target` other than the host; a `[[check]] target` \
             sweep needs cargo 1.{MIN_CARGO_MINOR} or newer",
            text.trim()
        ))),
        None => Err(DevError::Config(format!(
            "cannot read the cargo version from `{}`",
            text.trim()
        ))),
    }
}

/// The minor version of `cargo 1.<minor>.<patch> ...`.
fn cargo_minor(text: &str) -> Option<u64> {
    let version = text.split_whitespace().nth(1)?;
    let mut parts = version.split('.');
    (parts.next()? == "1").then_some(())?;
    parts.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn cfg_decides_only_what_the_triple_fixes() {
        let cfg = TargetCfg::parse(
            "debug_assertions\npanic=\"unwind\"\ntarget_arch=\"x86_64\"\ntarget_env=\"musl\"\ntarget_family=\"unix\"\ntarget_feature=\"sse2\"\ntarget_os=\"linux\"\nunix\n",
        );
        assert_eq!(cfg.key_matches("target_env", "musl"), Some(true));
        assert_eq!(cfg.key_matches("target_env", "gnu"), Some(false));
        assert_eq!(cfg.key_matches("target_family", "unix"), Some(true));
        // A decided key rustc did not print matches nothing.
        assert_eq!(cfg.key_matches("target_abi", "eabi"), Some(false));
        // Flags can move these, so they stay undecided.
        assert_eq!(cfg.key_matches("target_feature", "sse2"), None);
        assert_eq!(cfg.key_matches("panic", "unwind"), None);
        assert_eq!(cfg.flag("unix"), Some(true));
        assert_eq!(cfg.flag("windows"), Some(false));
        assert_eq!(cfg.flag("debug_assertions"), None);
        assert_eq!(cfg.flag("madsim"), None);
    }

    #[test]
    fn cargo_minor_reads_release_and_nightly_spellings() {
        assert_eq!(cargo_minor("cargo 1.89.0 (abc 2025-06-01)\n"), Some(89));
        assert_eq!(cargo_minor("cargo 1.101.0-nightly (e9bbadf68 2026-10-12)"), Some(101));
        assert_eq!(cargo_minor("cargo 2.0.0"), None);
        assert_eq!(cargo_minor("garbage"), None);
    }

    #[test]
    fn compiler_follows_cargo_precedence() {
        let root = Path::new("/w");
        let config: toml::Table = "[build]\nrustc = \"tools/rustc\"\n".parse().unwrap();
        let files = vec![(PathBuf::from("/w/.cargo/config.toml"), config)];
        assert_eq!(
            compiler(root, EffectiveEnv::new(&[("RUSTC", "my-rustc")]), &files),
            "my-rustc"
        );
        assert_eq!(
            compiler(
                root,
                EffectiveEnv::new(&[("RUSTC", ""), ("CARGO_BUILD_RUSTC", "bin/rc")]),
                &files
            ),
            "/w/bin/rc"
        );
        // Empty values are unset; the config path is anchored at the
        // directory holding `.cargo`.
        assert_eq!(
            compiler(
                root,
                EffectiveEnv::new(&[("RUSTC", ""), ("CARGO_BUILD_RUSTC", "")]),
                &files
            ),
            "/w/tools/rustc"
        );
    }
}
