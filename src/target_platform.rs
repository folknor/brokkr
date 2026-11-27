//! A `[[check]]` sweep's `target` triple, resolved against the toolchain cargo
//! will build it with.
//!
//! A target sweep passes `--target <triple>` to every cargo run, and brokkr
//! then executes the binaries itself. Doing that honestly needs facts about
//! the triple that only the toolchain has: which `cfg` it sets (for cargo's
//! `[target.'cfg(...)']` runner and rustflags tables), where its runtime
//! libraries live (for the loader path), and that cargo runs its doctests at
//! all. The cfg facts alone are [`TargetFacts`], read without refusing
//! anything; a tested lane's [`SweepTarget`] adds the refusals and the libdir.
//! Every probe goes through the compiler cargo itself would pick, under
//! the sweep's environment, so preflight never inspects one toolchain while
//! cargo builds with another. The same compiler's host triple is what a
//! `{host_arch}` [`TargetSpec`] resolves against.
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
    /// The report came with the build's own rustflags, so it decides every
    /// key and flag, `target_feature` included, not only what the triple
    /// alone decides ([`TargetFacts::host`]).
    complete: bool,
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
        (self.complete || DECIDED_KEYS.contains(&key))
            .then(|| self.values.get(key).is_some_and(|v| v.contains(value)))
    }

    /// Whether bare `name` holds, or `None` when the triple does not decide it.
    pub(crate) fn flag(&self, name: &str) -> Option<bool> {
        (self.complete || DECIDED_FLAGS.contains(&name)).then(|| self.flags.contains(name))
    }
}

/// The placeholder a configured `target` may open with: the architecture
/// component of the compiler host triple (`rustc -vV`'s `host:` line), so one
/// entry names the host's own member of a target family
/// (`"{host_arch}-unknown-linux-musl"`).
pub(crate) const HOST_ARCH: &str = "{host_arch}";

/// A sweep's `target` as configured: a literal triple, or one opening with
/// [`HOST_ARCH`]. Config load validates the spelling only, since what a
/// template names depends on the host; planning turns it into the concrete
/// triple ([`TargetSpec::resolve`]), and nothing past planning sees a
/// template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSpec(String);

impl TargetSpec {
    /// Parse a configured `target`: a bare triple (ASCII alphanumerics, `-`,
    /// `_`, `.`; not a `.json` spec), optionally with its architecture
    /// component replaced by [`HOST_ARCH`].
    pub(crate) fn parse(spelling: &str) -> Option<Self> {
        let bare = |s: &str| {
            !s.is_empty()
                && !s.ends_with(".json")
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        let valid = match spelling.strip_prefix(HOST_ARCH) {
            Some(rest) => rest.strip_prefix('-').is_some_and(bare),
            None => bare(spelling),
        };
        valid.then(|| Self(spelling.to_owned()))
    }

    /// A spelling config load already validated with [`TargetSpec::parse`],
    /// kept verbatim: re-judging it here could only turn a load-time refusal
    /// into a silent host build.
    pub(crate) fn validated(spelling: &str) -> Self {
        Self(spelling.to_owned())
    }

    /// The spelling as configured.
    pub(crate) fn configured(&self) -> &str {
        &self.0
    }

    /// The concrete triple: a literal is itself; a template takes the
    /// architecture of the host triple of the compiler cargo would run in
    /// `root` under `env` (so a sweep's `RUSTC` decides it, as it decides the
    /// build).
    pub(crate) fn resolve(
        &self,
        root: &Path,
        env: EffectiveEnv<'_>,
        files: &[(PathBuf, toml::Table)],
    ) -> Result<String, DevError> {
        let Some(rest) = self.0.strip_prefix(HOST_ARCH) else {
            return Ok(self.0.clone());
        };
        let host = host_triple(root, env, files)?;
        let arch = host.split('-').next().unwrap_or_default();
        Ok(format!("{arch}{rest}"))
    }

    /// Whether resolving this spelling asks the host.
    pub(crate) fn is_template(&self) -> bool {
        self.0.starts_with(HOST_ARCH)
    }
}

/// How cargo itself queries its compiler: the selected rustc behind the
/// configured rustc wrapper (outer) and workspace wrapper (inner), with cargo
/// config `[env]` applied (a forced entry overrides, a plain one fills a
/// hole), in the build root. A wrapper may supply the compiler or add flags,
/// so a direct `rustc` can answer differently from the one cargo runs.
pub(crate) struct CargoProbe {
    root: PathBuf,
    chain: Vec<String>,
    env: Vec<(String, String)>,
}

impl CargoProbe {
    pub(crate) fn new(
        root: &Path,
        env: EffectiveEnv<'_>,
        files: &[(PathBuf, toml::Table)],
        config_env: &[crate::rustflags::ConfigEnvEntry],
    ) -> Self {
        let mut chain: Vec<String> = [
            wrapper(root, env, files, &["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"], "rustc-wrapper"),
            wrapper(
                root,
                env,
                files,
                &["RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"],
                "rustc-workspace-wrapper",
            ),
        ]
        .into_iter()
        .flatten()
        .collect();
        chain.push(compiler(root, env, files));
        let mut pairs: Vec<(String, String)> =
            env.pairs().iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        for entry in config_env {
            if entry.force || !env.contains(&entry.key) {
                pairs.retain(|(k, _)| k != &entry.key);
                pairs.push((entry.key.clone(), entry.value.clone()));
            }
        }
        Self {
            root: root.to_path_buf(),
            chain,
            env: pairs,
        }
    }

    /// The chain's stdout for `args`, or an error naming the command.
    fn run(&self, args: &[&str]) -> Result<String, DevError> {
        let Some((program, prefix)) = self.chain.split_first() else {
            return Err(DevError::Internal("empty compiler chain".into()));
        };
        let mut all: Vec<&str> = prefix.iter().map(String::as_str).collect();
        all.extend_from_slice(args);
        let refs: Vec<(&str, &str)> = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        probe(&self.root, EffectiveEnv::new(&refs), program, &all)
    }

    /// The host triple the chain reports.
    pub(crate) fn host_triple(&self) -> Result<String, DevError> {
        let refs: Vec<(&str, &str)> = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        Ok(crate::cargo::rustc::load_chain(&self.root, &self.chain, &refs)?
            .host
            .clone())
    }
}

/// The host triple of the compiler cargo would run in `root` under `env`.
fn host_triple(
    root: &Path,
    env: EffectiveEnv<'_>,
    files: &[(PathBuf, toml::Table)],
) -> Result<String, DevError> {
    let rustc = compiler(root, env, files);
    Ok(
        crate::cargo::rustc::load_selected(root, &rustc, env.pairs())?
            .host
            .clone(),
    )
}

/// The cfg keys on which a target's binaries and the compiler host's are
/// compared for [`differing_host`].
const EXECUTION_KEYS: [&str; 4] = [
    "target_os",
    "target_arch",
    "target_pointer_width",
    "target_endian",
];

/// The compiler host triple, when `triple`'s cfg facts differ from the
/// compiler host's on any of [`EXECUTION_KEYS`]; `None` when they agree or a
/// query failed.
///
/// A statement of difference, never a verdict on executability. Agreement
/// does not prove a binary of the triple runs here, and difference does not
/// prove it cannot: the compiler may itself run under emulation, and the
/// kernel's binfmt_misc handlers can accept or reject individual files of one
/// triple. Only executing the lane's own binaries decides, so brokkr refuses
/// nothing on this answer and does not probe before building.
pub(crate) fn differing_host(
    root: &Path,
    triple: &str,
    env: EffectiveEnv<'_>,
    files: &[(PathBuf, toml::Table)],
) -> Option<String> {
    let rustc = compiler(root, env, files);
    let host = TargetCfg::parse(&probe(root, env, &rustc, &["--print", "cfg"]).ok()?);
    let target =
        TargetCfg::parse(&probe(root, env, &rustc, &["--print", "cfg", "--target", triple]).ok()?);
    let differs = EXECUTION_KEYS
        .iter()
        .any(|key| host.values.get(*key) != target.values.get(*key));
    if !differs {
        return None;
    }
    host_triple(root, env, files).ok()
}

/// A triple with the cfg facts the compiler cargo would use reports for it:
/// what matching cargo's `[target.*]` tables needs, and nothing more.
///
/// Reading them refuses nothing. `rustc --print cfg --target` answers for a
/// built-in triple whether or not its standard library is installed, so a
/// missing library is left to rustc, which reports it where a crate needs one.
/// The rustdoc phase holds these alone: it runs no doctest and links no
/// binary, so the test lanes' preflight ([`SweepTarget::resolve`]) has nothing
/// to protect there, which is how a `[[lint_sweep]]` keeps having none.
#[derive(Debug, Clone)]
pub(crate) struct TargetFacts {
    triple: String,
    cfg: TargetCfg,
}

impl TargetFacts {
    /// `rustc --print cfg --target <triple>` from the compiler cargo would use
    /// in `root` under `env`.
    pub(crate) fn query(
        root: &Path,
        triple: &str,
        env: EffectiveEnv<'_>,
        files: &[(PathBuf, toml::Table)],
    ) -> Result<Self, DevError> {
        let rustc = compiler(root, env, files);
        Self::query_with(root, triple, env, &rustc)
    }

    fn query_with(
        root: &Path,
        triple: &str,
        env: EffectiveEnv<'_>,
        rustc: &str,
    ) -> Result<Self, DevError> {
        let cfg = TargetCfg::parse(&probe(
            root,
            env,
            rustc,
            &["--print", "cfg", "--target", triple],
        )?);
        Ok(Self {
            triple: triple.to_owned(),
            cfg,
        })
    }

    pub(crate) fn triple(&self) -> &str {
        &self.triple
    }

    pub(crate) fn cfg(&self) -> &TargetCfg {
        &self.cfg
    }

    /// The host `triple` with the cfg set cargo matches a host build's
    /// `[target.'cfg()']` tables against, probed as cargo probes it (see
    /// [`CargoProbe`]): `- --crate-name ___ --print=cfg` with the build's
    /// effective `rustflags`, since flags such as `-C target-cpu` change
    /// `target_feature`. Cargo then drops `proc_macro`.
    pub(crate) fn host(probe: &CargoProbe, triple: &str, rustflags: &[String]) -> Result<Self, DevError> {
        let mut args = vec!["-", "--crate-name", "___", "--print=cfg"];
        args.extend(rustflags.iter().map(String::as_str));
        let mut cfg = TargetCfg::parse(&probe.run(&args)?);
        cfg.flags.remove("proc_macro");
        cfg.complete = true;
        Ok(Self {
            triple: triple.to_owned(),
            cfg,
        })
    }

    /// Complete host facts from a triple and `rustc --print cfg` text, for
    /// tests.
    #[cfg(test)]
    pub(crate) fn from_parts(triple: &str, cfg: &str) -> Self {
        let mut cfg = TargetCfg::parse(cfg);
        cfg.complete = true;
        Self {
            triple: triple.to_owned(),
            cfg,
        }
    }
}

/// A tested target lane's triple, through the preflight its lanes need: the
/// toolchain holds the triple's standard library, its cargo runs a cross
/// target's doctests, and where the triple's runtime libraries live. Only
/// [`SweepTarget::resolve`] makes one, so holding one means the lane passed.
#[derive(Debug, Clone)]
pub(crate) struct SweepTarget {
    facts: TargetFacts,
    /// `rustc --print target-libdir --target <triple>`: the runtime libraries
    /// a binary built for the triple loads from.
    libdir: String,
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
        let facts = TargetFacts::query_with(root, triple, env, &rustc)?;
        let libdir = probe(
            root,
            env,
            &rustc,
            &["--print", "target-libdir", "--target", triple],
        )?
        .trim()
        .to_owned();
        Ok(Self { facts, libdir })
    }

    pub(crate) fn triple(&self) -> &str {
        self.facts.triple()
    }

    pub(crate) fn facts(&self) -> &TargetFacts {
        &self.facts
    }

    pub(crate) fn libdir(&self) -> &str {
        &self.libdir
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
/// A configured rustc wrapper: the first non-empty of the `env_keys`, else
/// `build.KEY` from the config chain, resolved as [`compiler`] resolves
/// `build.rustc`. An empty value means none, as cargo reads it.
fn wrapper(
    root: &Path,
    env: EffectiveEnv<'_>,
    files: &[(PathBuf, toml::Table)],
    env_keys: &[&str],
    key: &str,
) -> Option<String> {
    for name in env_keys {
        if let Some(value) = env.get_os(name).and_then(|v| v.into_string().ok()) {
            return (!value.is_empty()).then(|| absolute_if_path(root, &value));
        }
    }
    let (path, value) = files.iter().find_map(|(path, doc)| {
        let value = doc.get("build")?.as_table()?.get(key)?.as_str()?;
        Some((path, value.to_owned()))
    })?;
    if value.is_empty() {
        return None;
    }
    Some(if value.contains('/') {
        let anchor = path.parent().and_then(Path::parent).unwrap_or(root);
        anchor.join(value).display().to_string()
    } else {
        value
    })
}

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
    fn target_spec_is_a_bare_triple_or_one_opening_with_host_arch() {
        for ok in [
            "x86_64-unknown-linux-musl",
            "riscv64gc-unknown-linux-gnu",
            "{host_arch}-unknown-linux-musl",
        ] {
            assert_eq!(TargetSpec::parse(ok).unwrap().configured(), ok);
        }
        assert!(
            !TargetSpec::parse("x86_64-unknown-linux-musl")
                .unwrap()
                .is_template()
        );
        assert!(
            TargetSpec::parse("{host_arch}-unknown-linux-musl")
                .unwrap()
                .is_template()
        );
        for bad in [
            "",
            "x86_64 linux",
            "my-target.json",
            "{host_arch}",
            "{host_arch}-",
            "{host_arch}unknown-linux-musl",
            "x86_64-{host_arch}",
            "{arch}-unknown-linux-musl",
            "{host_arch}-{host_arch}",
        ] {
            assert!(TargetSpec::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn a_literal_target_resolves_to_itself_without_asking_the_host() {
        // No compiler exists at this root; a literal never runs one.
        let spec = TargetSpec::parse("x86_64-unknown-linux-musl").unwrap();
        let env = EffectiveEnv::new(&[("RUSTC", "/nonexistent/rustc")]);
        assert_eq!(
            spec.resolve(Path::new("/nonexistent"), env, &[]).unwrap(),
            "x86_64-unknown-linux-musl"
        );
    }

    // The difference is judged on the four execution keys only, and a
    // compiler that cannot answer leaves it unsaid rather than failing.
    #[cfg(unix)]
    #[test]
    fn differing_host_compares_the_execution_keys_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::test_scratch::scratch("target_platform", "differing_host");
        let rustc = root.join("fake-rustc");
        std::fs::write(
            &rustc,
            "#!/bin/sh\n\
             case \"$*\" in\n\
             -v*) printf 'release: 1.99.0\\nhost: x86_64-unknown-linux-gnu\\n' ;;\n\
             '--print cfg') printf 'target_arch=\"x86_64\"\\ntarget_os=\"linux\"\\ntarget_env=\"gnu\"\\ntarget_pointer_width=\"64\"\\ntarget_endian=\"little\"\\n' ;;\n\
             *aarch64*) printf 'target_arch=\"aarch64\"\\ntarget_os=\"linux\"\\ntarget_env=\"musl\"\\ntarget_pointer_width=\"64\"\\ntarget_endian=\"little\"\\n' ;;\n\
             *) printf 'target_arch=\"x86_64\"\\ntarget_os=\"linux\"\\ntarget_env=\"musl\"\\ntarget_pointer_width=\"64\"\\ntarget_endian=\"little\"\\n' ;;\n\
             esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&rustc, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rustc = rustc.display().to_string();
        let env = [("RUSTC", rustc.as_str())];
        let env = EffectiveEnv::new(&env);
        // Only target_env differs: no line.
        assert_eq!(
            differing_host(&root, "x86_64-unknown-linux-musl", env, &[]),
            None
        );
        assert_eq!(
            differing_host(&root, "aarch64-unknown-linux-musl", env, &[]).as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        let missing = root.join("missing-rustc").display().to_string();
        let missing = [("RUSTC", missing.as_str())];
        assert_eq!(
            differing_host(
                &root,
                "aarch64-unknown-linux-musl",
                EffectiveEnv::new(&missing),
                &[]
            ),
            None
        );
    }

    // A triple whose standard library is not in the sysroot still has its
    // cfg facts read; only a tested lane's preflight refuses it.
    #[cfg(unix)]
    #[test]
    fn target_facts_refuse_nothing_the_preflight_refuses() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::test_scratch::scratch("target_platform", "facts_without_std");
        let sysroot = root.join("sysroot");
        std::fs::create_dir_all(&sysroot).unwrap();
        let rustc = root.join("fake-rustc");
        std::fs::write(
            &rustc,
            format!(
                "#!/bin/sh\n\
                 case \"$*\" in\n\
                 '--print sysroot') printf '%s\\n' '{}' ;;\n\
                 *) printf 'target_arch=\"riscv64\"\\ntarget_os=\"linux\"\\nunix\\n' ;;\n\
                 esac\n",
                sysroot.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&rustc, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rustc = rustc.display().to_string();
        let env = [("RUSTC", rustc.as_str())];
        let env = EffectiveEnv::new(&env);
        let triple = "riscv64gc-unknown-linux-gnu";
        let facts = TargetFacts::query(&root, triple, env, &[]).unwrap();
        assert_eq!(facts.triple(), triple);
        assert_eq!(
            facts.cfg().key_matches("target_arch", "riscv64"),
            Some(true)
        );
        assert_eq!(facts.cfg().flag("unix"), Some(true));
        let refused = SweepTarget::resolve(&root, triple, env, &[]).unwrap_err();
        assert!(
            refused.to_string().contains("is not installed"),
            "{refused}"
        );
    }

    #[test]
    fn cargo_minor_reads_release_and_nightly_spellings() {
        assert_eq!(cargo_minor("cargo 1.89.0 (abc 2025-06-01)\n"), Some(89));
        assert_eq!(
            cargo_minor("cargo 1.101.0-nightly (e9bbadf68 2026-10-12)"),
            Some(101)
        );
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
