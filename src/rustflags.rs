//! Where a project's extra rustc flags actually come from, and how to add to
//! them without discarding them.
//!
//! Cargo does not merge rustflags across kinds of source. It picks exactly one,
//! first match wins:
//!
//! 1. `CARGO_ENCODED_RUSTFLAGS`
//! 2. `RUSTFLAGS`
//! 3. every *matching* `target.<triple>.rustflags` / `target.<cfg>.rustflags`,
//!    joined together
//! 4. `build.rustflags`
//!
//! That rule is why exporting `RUSTFLAGS` to add one `-A` is destructive: it
//! promotes brokkr's flags to source 2 and the project's `-Dwarnings`, its
//! `-fuse-ld=lld`, its `relocation-model=pic` all vanish at once. Within a
//! single source cargo *does* merge across config files, joining arrays with
//! higher-precedence entries placed later - and `--config` on the command line
//! is the highest-precedence config source there is. So a flag injected with
//! `--config` lands in the same layer the project already uses, appended after
//! the project's own entries, which is exactly what a lint allow needs (rustc
//! resolves conflicting lint levels last-wins).
//!
//! The catch, and the reason this module exists rather than a one-line
//! `--config` in the caller: injecting at the *wrong* layer is silently
//! destructive in one direction. Adding `target."cfg(all())".rustflags` to a
//! project whose flags live in `build.rustflags` creates a source-3 match,
//! which *demotes* source 4 and drops the project's flags entirely - the same
//! failure as the `RUSTFLAGS` export, just better hidden. Adding
//! `build.rustflags` when a target table matches is merely inert. So the sink
//! is chosen by inspecting the config chain, and an unrecognised `cfg(...)`
//! resolves to the inert direction, never the destructive one.
//!
//! The chain is the whole chain: every `.cargo/config{.toml}` from the build
//! root upwards, plus the cargo home's, with their includes
//! ([`read_config_files_under`] has the walk). A user-level
//! `[target.<host-triple>] rustflags` is common (linker choice, `target-cpu`),
//! and it decides the winning layer for every project on that machine.
//!
//! Rustdoc's flags follow the identical rule with the key spelled
//! `rustdocflags` (`CARGO_ENCODED_RUSTDOCFLAGS`, `RUSTDOCFLAGS`, the target
//! tables, `build.rustdocflags`) - cargo resolves both through one function
//! parametrised by the kind - so every decision here takes a [`FlagKind`].
//! Brokkr adds to rustdocflags twice: the rustdoc phase's lint-only `--check`
//! (`rustdoc_check.rs`), and the `--cfg` / `--check-cfg` entries of a sweep's
//! `rustflags` ([`cfg_args`]), which cargo never hands to rustdoc on its own.
//!
//! The target layer is chosen only when it is live for every configured
//! triple; an ambient cross triple decides only platform-free selectors.
//! Each directory's file is the one cargo's own probe picks
//! ([`selected_config_file`], shared by `guard`, `crit`'s stamp and
//! `strip_rustflags`). The module also owns the launch-side reads of the same
//! chain: [`EffectiveEnv`] (a launch's env over brokkr's own),
//! [`CargoLaunchConfig`] (one parse for runners, `build.target`, rustdoc,
//! `build-dir` and `[env]`) and [`effective_rustflags`] (the flags a host
//! build gets and the source that won, for build provenance).

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// Which of cargo's two extra-flag families a decision is about. Cargo reads
/// both by the same four-source rule; only the names differ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum FlagKind {
    /// `rustflags`, handed to rustc.
    #[default]
    Rustc,
    /// `rustdocflags`, handed to rustdoc: `cargo doc` and the doctests of
    /// `cargo test`.
    Rustdoc,
}

impl FlagKind {
    /// The config key, in `build.<key>` and `target.<selector>.<key>`.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Rustc => "rustflags",
            Self::Rustdoc => "rustdocflags",
        }
    }

    fn plain_env(self) -> &'static str {
        match self {
            Self::Rustc => "RUSTFLAGS",
            Self::Rustdoc => "RUSTDOCFLAGS",
        }
    }

    fn encoded_env(self) -> &'static str {
        match self {
            Self::Rustc => "CARGO_ENCODED_RUSTFLAGS",
            Self::Rustdoc => "CARGO_ENCODED_RUSTDOCFLAGS",
        }
    }

    /// `CARGO_TARGET_<TRIPLE>_<KIND>`, the environment spelling of
    /// `target.<triple>.<key>`.
    fn target_env_var(self, triple: &str) -> String {
        format!(
            "CARGO_TARGET_{}_{}",
            triple.to_uppercase().replace('-', "_"),
            self.plain_env()
        )
    }
}

/// Cargo's two environment sources of one flag kind and their precedence.
/// Keep the raw values so presence, including a set-but-empty or non-Unicode
/// value, cannot be confused with absence by a caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RustflagsEnv {
    kind: FlagKind,
    encoded: Option<std::ffi::OsString>,
    plain: Option<std::ffi::OsString>,
}

impl RustflagsEnv {
    /// Resolve the rustflags values one cargo process will inherit. Explicit
    /// overrides take precedence over brokkr's environment, just as they do
    /// for the other cargo launch settings.
    pub(crate) fn from(env: EffectiveEnv<'_>) -> Self {
        Self::for_kind(FlagKind::Rustc, env)
    }

    /// [`Self::from`] for either flag kind.
    pub(crate) fn for_kind(kind: FlagKind, env: EffectiveEnv<'_>) -> Self {
        Self {
            kind,
            encoded: env.get_os(kind.encoded_env()),
            plain: env.get_os(kind.plain_env()),
        }
    }

    pub(crate) fn process() -> Self {
        Self::from(EffectiveEnv::new(&[]))
    }

    /// The source Cargo reads, applying its documented encoded-env priority.
    fn active(&self) -> Option<(&'static str, &std::ffi::OsString)> {
        self.encoded
            .as_ref()
            .map(|value| (self.kind.encoded_env(), value))
            .or_else(|| {
                self.plain
                    .as_ref()
                    .map(|value| (self.kind.plain_env(), value))
            })
    }

    /// Append UTF-8 brokkr flags to the one source cargo reads. A non-Unicode
    /// inherited value cannot be safely copied through the string-based env
    /// interface used by cargo launchers, so refuse instead of falling back
    /// to the lower-priority source or silently replacing project flags.
    ///
    /// The plain form is split on spaces by cargo, so an addition that would
    /// not survive that split - one holding a space, an empty one, one with
    /// surrounding whitespace (`--check-cfg=cfg(mode, values("a"))`) - moves
    /// the value to the encoded form instead: the plain value decoded as cargo
    /// decodes it, then the additions, each argument kept whole. The plain
    /// variable is left set; cargo ignores it once the encoded one is.
    pub(crate) fn compose(
        &self,
        additions: &[String],
    ) -> Result<Option<(String, String)>, crate::error::DevError> {
        if additions.is_empty() {
            return Ok(None);
        }
        let empty = std::ffi::OsString::new();
        let (key, existing) = self.active().unwrap_or((self.kind.plain_env(), &empty));
        let existing = existing.to_str().ok_or_else(|| {
            crate::error::DevError::Config(format!(
                "{key} is set to a non-UTF-8 value; brokkr cannot append {} without changing it",
                self.kind.key()
            ))
        })?;
        let encoded = self.kind.encoded_env();
        if key == encoded {
            let mut parts: Vec<String> = if existing.is_empty() {
                Vec::new()
            } else {
                existing.split('\u{1f}').map(str::to_owned).collect()
            };
            parts.extend(additions.iter().cloned());
            return Ok(Some((key.to_owned(), parts.join("\u{1f}"))));
        }
        if !additions.iter().all(|a| survives_plain_split(a)) {
            let mut parts: Vec<String> = existing
                .split(' ')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            parts.extend(additions.iter().cloned());
            return Ok(Some((encoded.to_owned(), parts.join("\u{1f}"))));
        }
        let mut value = existing.to_owned();
        for flag in additions {
            if !value.trim().is_empty() {
                value.push(' ');
            }
            value.push_str(flag);
        }
        Ok(Some((key.to_owned(), value)))
    }

    /// Describe the precedence decision used by `cargo`, including a lower
    /// priority `RUSTFLAGS` value that is present but shadowed.
    pub(crate) fn invocation_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some((key, value)) = self.active() {
            let description = match value.to_str() {
                Some(v) if v.trim().is_empty() => "set but empty in the environment",
                Some(_) => "inherited from the environment",
                None => "inherited as a non-UTF-8 value",
            };
            notes.push(format!("{key} {description}"));
        }
        if self.encoded.is_some() && self.plain.is_some() {
            notes.push("RUSTFLAGS is shadowed by CARGO_ENCODED_RUSTFLAGS".into());
        }
        notes
    }

    /// Include Cargo's config-shaped rustflags environment sources in the
    /// invocation notice, identifying target variables through the same
    /// target/config view as [`sink`].
    pub(crate) fn invocation_notes_for(&self, build_root: &Path) -> Vec<String> {
        let mut notes = self.invocation_notes();
        match read_config_files_under(build_root, EffectiveEnv::new(&[])) {
            Ok(files) => {
                match target_triples_for_sink(build_root, &files, EffectiveEnv::new(&[])) {
                    Some(triples) => {
                        for triple in triples {
                            let key = target_rustflags_env_var(&triple);
                            if std::env::var_os(&key).is_some() {
                                notes.push(format!("{key} is set for target {triple}"));
                            }
                        }
                    }
                    None => notes.push(
                        "Cargo config's build.target could not be modelled for rustflags sources"
                            .into(),
                    ),
                }
            }
            Err(_) => {
                notes
                    .push("Cargo config could not be fully inspected for rustflags sources".into());
            }
        }
        if std::env::var_os("CARGO_BUILD_RUSTFLAGS").is_some() {
            notes.push("CARGO_BUILD_RUSTFLAGS is set for build.rustflags".into());
        }
        notes
    }
}

/// Whether one argument comes back unchanged from cargo's decoding of a plain
/// `RUSTFLAGS` / `RUSTDOCFLAGS` value: split on spaces, each piece trimmed,
/// empty pieces dropped.
fn survives_plain_split(arg: &str) -> bool {
    !arg.is_empty() && !arg.contains(' ') && arg.trim() == arg
}

/// The `--cfg` and `--check-cfg` arguments of a rustflags list, in order and
/// verbatim, both spellings (`--cfg x`, `--cfg=x`). These are what rustdoc must
/// see to resolve `cfg` as the sweep's rustc build does; every other rustc flag
/// stays rustc's. The list is walked as options and their operands, so the
/// operand of another option (`-C`, `--cap-lints`, ...) is never read as an
/// option of its own. A trailing `--cfg` with no operand is carried as it is,
/// for rustdoc to reject as rustc does, never quietly repaired.
pub(crate) fn cfg_args(rustflags: &[String]) -> Vec<String> {
    /// rustc options that take their operand as the next argument.
    const TAKES_OPERAND: &[&str] = &[
        "-A",
        "-W",
        "-D",
        "-F",
        "-C",
        "-Z",
        "-L",
        "-l",
        "-o",
        "--allow",
        "--warn",
        "--deny",
        "--forbid",
        "--force-warn",
        "--cap-lints",
        "--codegen",
        "--crate-type",
        "--crate-name",
        "--edition",
        "--emit",
        "--print",
        "--target",
        "--extern",
        "--sysroot",
        "--error-format",
        "--json",
        "--color",
        "--remap-path-prefix",
        "--explain",
        "--out-dir",
        "--diagnostic-width",
    ];
    let mut out = Vec::new();
    let mut i = 0;
    while i < rustflags.len() {
        let arg = rustflags[i].as_str();
        if arg == "--cfg" || arg == "--check-cfg" {
            out.extend(rustflags[i..].iter().take(2).cloned());
            i += 2;
        } else if arg.starts_with("--cfg=") || arg.starts_with("--check-cfg=") {
            out.push(arg.to_owned());
            i += 1;
        } else if TAKES_OPERAND.contains(&arg) {
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// The environment Cargo receives, with the caller's explicit pairs taking
/// precedence over brokkr's inherited process environment. A present empty
/// value remains present: Cargo's config environment loader reads it instead
/// of treating it as unset, though a setting's parser may reject that value.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EffectiveEnv<'a> {
    overrides: &'a [(&'a str, &'a str)],
}

impl<'a> EffectiveEnv<'a> {
    pub(crate) fn new(overrides: &'a [(&'a str, &'a str)]) -> Self {
        Self { overrides }
    }

    /// The final value Cargo sees. The last explicit pair wins, followed by
    /// the process environment when no pair sets this key.
    pub(crate) fn get_os(self, key: &str) -> Option<std::ffi::OsString> {
        self.overrides
            .iter()
            .rev()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| std::ffi::OsString::from(*value))
            .or_else(|| std::env::var_os(key))
    }

    pub(crate) fn contains(self, key: &str) -> bool {
        self.get_os(key).is_some()
    }

    pub(crate) fn pairs(self) -> &'a [(&'a str, &'a str)] {
        self.overrides
    }

    fn source(self, key: &str) -> Option<Source> {
        self.get_os(key).map(|value| Source {
            origin: SourceOrigin::Environment(value),
            key: key.to_owned(),
        })
    }
}

/// Where a cargo setting came from, kept with the key for a precise refusal.
#[derive(Debug, Clone)]
pub(crate) struct Source {
    pub(crate) origin: SourceOrigin,
    pub(crate) key: String,
}

#[derive(Debug, Clone)]
pub(crate) enum SourceOrigin {
    Environment(std::ffi::OsString),
    Config(PathBuf),
}

#[derive(Debug, Clone)]
pub(crate) struct ConfigEnvEntry {
    pub(crate) key: String,
    pub(crate) value: String,
    /// `force = true`: overrides an inherited environment value; without it
    /// the entry only fills a hole.
    pub(crate) force: bool,
}

/// The cargo config facts shared by check's direct lanes, serial shim, and
/// build concurrency decision. Paths are ordered from highest to lowest
/// precedence, and each file is parsed once for this discovery.
#[derive(Debug, Clone, Default)]
pub(crate) struct CargoLaunchConfig {
    /// The host's runner: what cargo runs host binaries (a proc-macro's test
    /// harness, or every test of a host sweep) through.
    pub(crate) runner: Option<Source>,
    /// A target sweep's runner, possibly applying: the triple's own table or
    /// env spelling, or a `cfg(...)` table not proven false for it.
    pub(crate) target_runner: Option<Source>,
    /// An ambient build target. Always `None` for a target sweep, whose
    /// explicit `--target` overrides it.
    pub(crate) build_target: Option<Source>,
    pub(crate) rustdoc: Option<Source>,
    pub(crate) build_dir: bool,
    pub(crate) env: Vec<ConfigEnvEntry>,
}

impl CargoLaunchConfig {
    pub(crate) fn discover(
        root: &Path,
        env: EffectiveEnv<'_>,
    ) -> Result<Self, crate::error::DevError> {
        Self::discover_for(root, env, None)
    }

    /// The launch facts for a sweep that may carry a `target`.
    pub(crate) fn discover_for(
        root: &Path,
        env: EffectiveEnv<'_>,
        target: Option<&crate::target_platform::TargetFacts>,
    ) -> Result<Self, crate::error::DevError> {
        let files = read_config_files_under(root, env)?;

        let host = host_triple_for(root);
        let runner_var = host.as_deref().map(runner_env_var);
        let runner = runner_var
            .as_deref()
            .and_then(|key| env.source(key))
            .or_else(|| config_runner_source(&files, Platform::Host(host.as_deref())));
        let target_runner = target.and_then(|t| {
            env.source(&runner_env_var(t.triple()))
                .or_else(|| config_runner_source(&files, Platform::Target(t)))
        });
        let build_target = if target.is_some() {
            None
        } else {
            env.source("CARGO_BUILD_TARGET")
                .or_else(|| config_source(&files, "build", "target"))
        };
        let rustdoc = env
            .source("RUSTDOC")
            .or_else(|| env.source("CARGO_BUILD_RUSTDOC"))
            .or_else(|| config_source(&files, "build", "rustdoc"));
        let build_dir = env.contains("CARGO_BUILD_BUILD_DIR")
            || files.iter().any(|(_, doc)| {
                doc.get("build")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|build| build.contains_key("build-dir"))
            });
        let config_env = merged_config_env(&files)?;

        Ok(Self {
            runner,
            target_runner,
            build_target,
            rustdoc,
            build_dir,
            env: config_env,
        })
    }
}

/// Read the exact config documents used for launch reconstruction. Config
/// locations that are absent are ordinary; once Cargo would select a file, a
/// read or TOML error means this snapshot cannot safely omit any settings.
///
/// The walk mirrors cargo's merged loader (1.97 and later), so the snapshot
/// holds what cargo merges, repeats included:
///
/// - the walk starts at the physical path of `build_root` ([`physical_cwd`]),
///   the directory cargo's `current_dir()` reports, and a relative
///   `CARGO_HOME` resolves against it;
/// - every ancestor `.cargo` contributes its selected file; the cargo home is
///   skipped only when its directory, canonical or as spelled, is one of the
///   ancestor `.cargo` directories (cargo compares directories, not files);
/// - each root file has its own include set, so a file reached from two roots
///   is read twice, as cargo reads it twice and appends its arrays twice;
/// - an include is the lexically normalized join of the reached path's parent
///   and the include string, never a canonical path: two symlinked spellings of
///   one file are two documents whose relative includes resolve against
///   different directories, exactly as cargo resolves them;
/// - within one root a path reached a second time refuses, cycle or diamond,
///   because cargo's include set is never cleared and cargo fails it too.
///
/// Cargo 1.94 to 1.96 opened the joined include path without normalizing it,
/// so `link/../child.toml` through a symlinked `link` names a different file
/// on those versions; brokkr follows the current loader.
pub(crate) fn read_config_files_under(
    build_root: &Path,
    env: EffectiveEnv<'_>,
) -> Result<Vec<(PathBuf, toml::Table)>, crate::error::DevError> {
    let cwd = physical_cwd(build_root)?;
    let home = cargo_home(&cwd, env)?;
    read_config_files_from(&cwd, &home)
}

/// The working directory a cargo child launched in `dir` sees: cargo reads
/// `current_dir()`, which is the kernel's physical path, so a symlinked build
/// root walks the ancestors of its target, not of its spelling. A directory
/// that cannot be resolved is one cargo cannot be launched in.
pub(crate) fn physical_cwd(dir: &Path) -> Result<PathBuf, crate::error::DevError> {
    dir.canonicalize().map_err(|error| {
        crate::error::DevError::Config(format!(
            "could not resolve the cargo working directory {}: {error}",
            crate::output::path(dir)
        ))
    })
}

/// The cargo home a cargo child rooted at `cwd` uses: a nonempty
/// `CARGO_HOME`, else `.cargo` under a nonempty `HOME`, else under the
/// account's passwd home, as std's `home_dir` falls back. A relative value is
/// resolved against `cwd`, the child's working directory. Cargo cannot start
/// without a home, so neither source yielding one is an error.
pub(crate) fn cargo_home(
    cwd: &Path,
    env: EffectiveEnv<'_>,
) -> Result<PathBuf, crate::error::DevError> {
    if let Some(value) = env.get_os("CARGO_HOME").filter(|value| !value.is_empty()) {
        return Ok(cwd.join(PathBuf::from(value)));
    }
    let home = env
        .get_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(crate::user_dirs::account_home)
        .ok_or_else(|| {
            crate::error::DevError::Config(
                "no cargo home: CARGO_HOME and HOME are unset and the account has no home directory"
                    .into(),
            )
        })?;
    Ok(cwd.join(home).join(".cargo"))
}

/// [`read_config_files_under`] with the ancestor walk starting at
/// `search_root` and an already resolved cargo `home`, for a cargo that
/// reloads config from another directory while keeping its context's home
/// (`cargo install --path`).
pub(crate) fn read_config_files_from(
    search_root: &Path,
    home: &Path,
) -> Result<Vec<(PathBuf, toml::Table)>, crate::error::DevError> {
    let mut roots = Vec::new();
    let mut seen_dirs = HashSet::new();
    for dir in search_root.ancestors() {
        let config_root = dir.join(".cargo");
        if let Some(path) = selected_config_file(&config_root) {
            roots.push(path);
        }
        seen_dirs.insert(config_root.canonicalize().unwrap_or(config_root));
    }
    let canonical_home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
    if !seen_dirs.contains(&canonical_home)
        && !seen_dirs.contains(home)
        && let Some(path) = selected_config_file(home)
    {
        roots.push(path);
    }

    let mut files = Vec::new();
    for path in roots {
        push_config_document(&path, &mut files, &mut HashSet::new())?;
    }
    Ok(files)
}

/// Read a cargo config file passed explicitly through `--config`, including
/// every config file it includes, into `out`. `seen` is the include set:
/// cargo shares one across all file-valued `--config` arguments and their
/// descendants, so the caller passes the same set for each.
pub(crate) fn read_config_file_into(
    path: &Path,
    out: &mut Vec<(PathBuf, toml::Table)>,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), crate::error::DevError> {
    push_config_document(path, out, seen)
}

/// The files an inline `--config` assignment includes (`include=[...]`),
/// anchored at cargo's `cwd` and walked with a fresh include set, as cargo
/// walks each inline argument. `label` names the argument in errors.
pub(crate) fn read_inline_includes(
    label: &Path,
    doc: &toml::Table,
    cwd: &Path,
) -> Result<Vec<(PathBuf, toml::Table)>, crate::error::DevError> {
    let mut out = Vec::new();
    push_includes(label, doc, cwd, &mut out, &mut HashSet::new())?;
    Ok(out)
}

/// The config file cargo reads in one `.cargo`-shaped directory, by cargo's
/// own probe (`get_file_path`): the extensionless legacy `config` when it can
/// be opened, else `config.toml` when it exists. The probe is cargo's, not a
/// regular-file test: a directory at either spelling is selected and then
/// fails to read, as cargo fails; an unopenable legacy file is passed over,
/// as cargo passes over it; and opening a FIFO at `config` blocks, as it
/// blocks cargo. Callers that write the file apply their own checks.
pub(crate) fn selected_config_file(dir: &Path) -> Option<PathBuf> {
    let legacy = dir.join("config");
    if std::fs::File::open(&legacy).is_ok() {
        return Some(legacy);
    }
    let modern = dir.join("config.toml");
    modern.exists().then_some(modern)
}

/// Load `path` (as reached, never canonicalized) and its includes into `out`,
/// highest precedence first. `seen` is the root's include set: every path
/// loaded under this root, never cleared, as in cargo.
fn push_config_document(
    path: &Path,
    out: &mut Vec<(PathBuf, toml::Table)>,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), crate::error::DevError> {
    if !seen.insert(path.to_owned()) {
        return Err(crate::error::DevError::Config(format!(
            "Cargo config include cycle reaches {} (cargo refuses a file included twice under one config root)",
            crate::output::path(path)
        )));
    }

    let text = std::fs::read_to_string(path).map_err(|error| config_read_error(path, &error))?;
    let doc = text.parse::<toml::Table>().map_err(|error| {
        crate::error::DevError::Config(format!(
            "could not parse Cargo config {}: {error}",
            crate::output::path(path)
        ))
    })?;

    out.push((path.to_owned(), doc.clone()));
    let anchor = path.parent().unwrap_or_else(|| Path::new("."));
    push_includes(path, &doc, anchor, out, seen)
}

/// Load the files `doc` (reached as `path`) includes, resolved against
/// `anchor`, into `out`, later includes first.
fn push_includes(
    path: &Path,
    doc: &toml::Table,
    anchor: &Path,
    out: &mut Vec<(PathBuf, toml::Table)>,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), crate::error::DevError> {
    let includes = match doc.get("include") {
        None => &[][..],
        Some(toml::Value::Array(includes)) => includes.as_slice(),
        Some(_) => {
            return Err(crate::error::DevError::Config(format!(
                "Cargo config {} has a non-array `include`; brokkr cannot resolve its launch settings",
                crate::output::path(path)
            )));
        }
    };
    // Later includes override earlier ones, so visit them first while keeping
    // this high-precedence file at the front of the resulting snapshot.
    for include in includes.iter().rev() {
        let (include_path, optional) = match include {
            toml::Value::String(include_path) => (include_path.as_str(), false),
            toml::Value::Table(table) => {
                let include_path =
                    table
                        .get("path")
                        .and_then(toml::Value::as_str)
                        .ok_or_else(|| {
                            crate::error::DevError::Config(format!(
                                "Cargo config {} has an include table without a string `path`",
                                crate::output::path(path)
                            ))
                        })?;
                let optional = match table.get("optional") {
                    None => false,
                    Some(toml::Value::Boolean(value)) => *value,
                    Some(_) => {
                        return Err(crate::error::DevError::Config(format!(
                            "Cargo config {} has a non-boolean include `optional` value",
                            crate::output::path(path)
                        )));
                    }
                };
                (include_path, optional)
            }
            _ => {
                return Err(crate::error::DevError::Config(format!(
                    "Cargo config {} has an invalid include entry",
                    crate::output::path(path)
                )));
            }
        };
        // Cargo's checks, on the include string as written.
        if Path::new(include_path).extension() != Some(std::ffi::OsStr::new("toml")) {
            return Err(crate::error::DevError::Config(format!(
                "Cargo config {} includes `{include_path}`, which does not end in `.toml`",
                crate::output::path(path)
            )));
        }
        if include_path.contains(['*', '?', '[', ']', '{', '}']) {
            return Err(crate::error::DevError::Config(format!(
                "Cargo config {} includes `{include_path}`, a glob or template path cargo refuses",
                crate::output::path(path)
            )));
        }
        let target = lexical_normalize(&anchor.join(include_path));
        // Cargo skips an optional include whose target does not exist,
        // following symlinks; any other read failure refuses.
        if optional && !target.exists() {
            continue;
        }
        push_config_document(&target, out, seen)?;
    }
    Ok(())
}

/// Cargo's `paths::normalize_path`: `.` dropped and `..` popping its textual
/// parent, without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn config_read_error(path: &Path, error: &std::io::Error) -> crate::error::DevError {
    crate::error::DevError::Config(format!(
        "could not read Cargo config {}: {error}",
        crate::output::path(path)
    ))
}

/// Paths for a Cargo invocation rooted at `build_root`, in high-to-low
/// precedence order. Discovery is fallible so a caller cannot quietly omit an
/// unreadable or malformed selected config (or required include).
pub(crate) fn config_paths_under(
    build_root: &Path,
    env: EffectiveEnv<'_>,
) -> Result<Vec<PathBuf>, crate::error::DevError> {
    read_config_files_under(build_root, env)
        .map(|files| files.into_iter().map(|(path, _)| path).collect())
}

/// Cargo's documented env spelling for `[target.<triple>].runner`.
pub(crate) fn runner_env_var(triple: &str) -> String {
    format!(
        "CARGO_TARGET_{}_RUNNER",
        triple.to_uppercase().replace('-', "_")
    )
}

fn config_source(files: &[(PathBuf, toml::Table)], table: &str, key: &str) -> Option<Source> {
    files.iter().find_map(|(path, doc)| {
        doc.get(table)
            .and_then(toml::Value::as_table)
            .filter(|values| values.contains_key(key))
            .map(|_| Source {
                origin: SourceOrigin::Config(path.clone()),
                key: format!("{table}.{key}"),
            })
    })
}

fn config_runner_source(
    files: &[(PathBuf, toml::Table)],
    platform: Platform<'_>,
) -> Option<Source> {
    files.iter().find_map(|(path, doc)| {
        let targets = doc.get("target")?.as_table()?;
        targets.iter().find_map(|(selector, value)| {
            let table = value.as_table()?;
            if !table.contains_key("runner") {
                return None;
            }
            let applies = if let Some(expr) = selector
                .strip_prefix("cfg(")
                .and_then(|s| s.strip_suffix(')'))
            {
                // Unknown -> true: replacing a runner that might apply is the
                // destructive direction.
                eval_cfg_on(expr, platform) != Some(false)
            } else {
                platform.triple() == Some(selector.as_str())
            };
            applies.then(|| Source {
                origin: SourceOrigin::Config(path.clone()),
                key: format!("target.{selector}.runner"),
            })
        })
    })
}

/// Merge cargo's merge rules for one config node: tables merge key by key,
/// arrays append the higher layer after the lower, scalars replace, and a
/// table or array meeting another kind is an error, as cargo's merge refuses
/// it. These are the rules brokkr's `[env]` projection needs; cargo's own
/// refusal of TOML types it does not model (floats, datetimes) is not
/// reproduced.
fn merge_config_value(
    lower: &mut toml::Value,
    higher: toml::Value,
    key: &str,
    path: &Path,
) -> Result<(), crate::error::DevError> {
    match (lower, higher) {
        (toml::Value::Table(lower), toml::Value::Table(higher)) => {
            for (k, v) in higher {
                let child = format!("{key}.{k}");
                match lower.get_mut(&k) {
                    Some(existing) => merge_config_value(existing, v, &child, path)?,
                    None => {
                        lower.insert(k, v);
                    }
                }
            }
        }
        (toml::Value::Array(lower), toml::Value::Array(higher)) => lower.extend(higher),
        (lower @ (toml::Value::Table(_) | toml::Value::Array(_)), higher)
        | (lower, higher @ (toml::Value::Table(_) | toml::Value::Array(_))) => {
            return Err(crate::error::DevError::Config(format!(
                "Cargo config {} sets `{key}` as a {}, which a lower config sets as a {}; cargo refuses to merge them",
                crate::output::path(path),
                higher.type_str(),
                lower.type_str()
            )));
        }
        (lower, higher) => *lower = higher,
    }
    Ok(())
}

/// The `[env]` table cargo applies: every document's `[env]` merged lowest
/// precedence first by cargo's merge rules, so a higher table that only sets
/// `force` keeps a lower `value`, then decoded. A `relative` value anchors at
/// the document that supplied `value` (cargo 1.98 and later; 1.97 anchored at
/// the entry's table).
fn merged_config_env(
    files: &[(PathBuf, toml::Table)],
) -> Result<Vec<ConfigEnvEntry>, crate::error::DevError> {
    let mut merged = toml::Table::new();
    let mut origin: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for (path, doc) in files.iter().rev() {
        let Some(value) = doc.get("env") else {
            continue;
        };
        let entries = value.as_table().ok_or_else(|| {
            crate::error::DevError::Config(format!(
                "Cargo config {} has a non-table `env`; brokkr cannot reconstruct its launch environment",
                crate::output::path(path)
            ))
        })?;
        for (key, value) in entries {
            let supplies_value = match value {
                toml::Value::Table(table) => table.contains_key("value"),
                _ => true,
            };
            match merged.get_mut(key) {
                Some(existing) => {
                    merge_config_value(existing, value.clone(), &format!("env.{key}"), path)?;
                }
                None => {
                    merged.insert(key.clone(), value.clone());
                }
            }
            if supplies_value {
                origin.insert(key.clone(), path.clone());
            }
        }
    }
    let mut out = Vec::new();
    for (key, value) in &merged {
        let Some(path) = origin.get(key) else {
            return Err(crate::error::DevError::Config(format!(
                "Cargo config sets `[env]` `{key}` without a `value` anywhere in the chain"
            )));
        };
        out.push(config_env_entry(path, key, value)?);
    }
    Ok(out)
}

/// One merged `[env]` entry, its `value` supplied by `path`.
fn config_env_entry(
    path: &Path,
    key: &str,
    value: &toml::Value,
) -> Result<ConfigEnvEntry, crate::error::DevError> {
    // <dir>/.cargo/config.toml -> <dir>.
    let anchor = path.parent().and_then(Path::parent);
    {
        let (value, force, relative) = match value {
            toml::Value::String(value) => (value.clone(), false, false),
            toml::Value::Table(table) => {
                let value = table
                    .get("value")
                    .and_then(toml::Value::as_str)
                    .ok_or_else(|| {
                        crate::error::DevError::Config(format!(
                            "Cargo config {} has an `[env]` table for `{key}` without a string `value`",
                            crate::output::path(path)
                        ))
                    })?;
                let force = match table.get("force") {
                    None => false,
                    Some(toml::Value::Boolean(value)) => *value,
                    Some(_) => {
                        return Err(crate::error::DevError::Config(format!(
                            "Cargo config {} has a non-boolean `[env]` force value for `{key}`",
                            crate::output::path(path)
                        )));
                    }
                };
                let relative = match table.get("relative") {
                    None => false,
                    Some(toml::Value::Boolean(value)) => *value,
                    Some(_) => {
                        return Err(crate::error::DevError::Config(format!(
                            "Cargo config {} has a non-boolean `[env]` relative value for `{key}`",
                            crate::output::path(path)
                        )));
                    }
                };
                (value.to_owned(), force, relative)
            }
            _ => {
                return Err(crate::error::DevError::Config(format!(
                    "Cargo config {} has an unsupported `[env]` value for `{key}`",
                    crate::output::path(path)
                )));
            }
        };
        let value = match (relative, anchor) {
            (true, Some(dir)) => dir.join(value).display().to_string(),
            _ => value,
        };
        Ok(ConfigEnvEntry {
            key: key.to_owned(),
            value,
            force,
        })
    }
}

/// The one place brokkr can add flags for this invocation without displacing
/// the project's own. Determined by [`sink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// Source 1 is live: append to `CARGO_ENCODED_RUSTFLAGS` (0x1f-separated).
    EncodedEnv,
    /// Source 2 is live: append to `RUSTFLAGS` (space-separated).
    Env,
    /// Source 3 is live: `--config target."cfg(all())".rustflags=[..]`, which
    /// joins the matching target entries and lands last. A matching
    /// `CARGO_TARGET_<triple>_RUSTFLAGS` is the environment form of this source.
    TargetConfig,
    /// Source 4 (or nothing at all): `--config build.rustflags=[..]`. This
    /// includes `CARGO_BUILD_RUSTFLAGS`, the environment form of this key.
    BuildConfig,
}

impl Sink {
    /// Human fragment for the notice line, so a run that suppressed a lint says
    /// *how* it did it - an inert `BuildConfig` injection is otherwise a silent
    /// no-op that looks like a brokkr bug.
    pub fn describe(self) -> &'static str {
        match self {
            Self::EncodedEnv => "CARGO_ENCODED_RUSTFLAGS",
            Self::Env => "RUSTFLAGS",
            Self::TargetConfig => "--config target.\"cfg(all())\".rustflags",
            Self::BuildConfig => "--config build.rustflags",
        }
    }

    /// Whether this sink is carried by the environment rather than by cargo's
    /// command line. The two are plumbed differently at the call site.
    pub fn is_env(self) -> bool {
        matches!(self, Self::EncodedEnv | Self::Env)
    }
}

/// Decide the rustflags sink for a cargo invocation rooted at `build_root`:
/// [`decide`] for [`FlagKind::Rustc`], keeping only the layer.
pub(crate) fn sink(
    build_root: &Path,
    sweep_sets_rustflags: bool,
    target: Option<&crate::target_platform::TargetFacts>,
    cargo_env: EffectiveEnv<'_>,
) -> Sink {
    decide(
        FlagKind::Rustc,
        build_root,
        sweep_sets_rustflags,
        target,
        cargo_env,
    )
    .sink
}

/// Where brokkr's flags of one kind go, and whether that answer is proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SinkDecision {
    pub(crate) sink: Sink,
    /// A config file that sets the sink's `--config` key as a string. Cargo
    /// refuses to merge brokkr's array into it, so placing flags there is
    /// refused ([`SinkDecision::refuse_string_destination`]). Only set for
    /// a config sink over a chain that was read.
    pub(crate) string_destination: Option<PathBuf>,
    /// False when the layer was chosen on a guess: some flagged target table
    /// could not be judged (an unmodelled `cfg(...)`, a bare triple with no
    /// host triple, a value of an unexpected shape), the configured triples
    /// disagree about the live layer, or the config chain could not be read.
    /// The guess is always [`Sink::BuildConfig`], the direction that can be
    /// inert but never demotes the project's own flags.
    pub(crate) certain: bool,
}

impl SinkDecision {
    /// Refuse placing nonempty `flags` of `kind` at a config key a file sets
    /// as a string: cargo fails to merge `--config key=[...]` over it, so the
    /// invocation could not have run. Writing a string instead would take
    /// `--config` precedence over env contributions to the key and reorder
    /// the project's own flags.
    pub(crate) fn refuse_string_destination(
        &self,
        kind: FlagKind,
        flags: &[String],
    ) -> Result<(), crate::error::DevError> {
        let Some(path) = self
            .string_destination
            .as_ref()
            .filter(|_| !flags.is_empty())
        else {
            return Ok(());
        };
        let key = match self.sink {
            Sink::TargetConfig => format!("target.\"cfg(all())\".{}", kind.key()),
            _ => format!("build.{}", kind.key()),
        };
        Err(crate::error::DevError::Refused(format!(
            "{} sets `{key}` as a string; brokkr adds its flags there as `--config {key}=[...]`, \
             which cargo refuses to merge with a string - write it as an array",
            crate::output::path(path)
        )))
    }
}

/// Decide the sink for flags of `kind` on a cargo invocation rooted at
/// `build_root`.
///
/// `sweep_sets_env` is brokkr's own doing: a `[[check]]` entry carrying
/// `rustflags` is exported as an env var by `sweep_runtime_env`, which makes
/// source 1/2 live for that sweep regardless of what the config chain says.
/// Brokkr never exports rustdocflags on its own, so rustdoc callers pass
/// `false`.
///
/// Sources 1 and 2 are read under `cargo_env`, the environment cargo will
/// actually see. For rustflags that is the process environment in practice:
/// `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` are reserved keys that config
/// loading refuses in both a `[[check]]` entry's `env` and a profile's `env`,
/// unconditionally (`reject_reserved_sweep_env`). The rustdoc keys are not
/// reserved, so a sweep's `env` can make them live, and reading `cargo_env`
/// is what sees it.
///
/// Presence, not content, decides both env sources, because that is cargo's
/// rule: it reads `RUSTFLAGS` whenever the variable is set, and a set-but-empty
/// value is source 2 with no flags - it still shadows every config-file
/// rustflags table. Treating an empty `RUSTFLAGS` as unset would pick a
/// `--config` sink cargo never reads, and every allow injected there would be
/// silently inert.
///
/// Source 3 is judged per platform the invocation builds for. `target` is a
/// `[[check]]` sweep's `target`: cargo then builds for that triple whatever
/// `build.target` says, judged with rustc's facts for it. Without one, the
/// triples are `build.target` / `CARGO_BUILD_TARGET`, else the host; a
/// `cfg(...)` selector is judged with the host's values only for the host
/// triple, and for any other triple only where it needs no platform fact
/// (`cfg(all())`). Cargo picks the source per platform but brokkr's
/// `--config` is one key for the whole invocation, so `TargetConfig` is chosen
/// only when the target source is live for every triple; live for none is
/// `BuildConfig`; anything else is an uncertain `BuildConfig`, inert for the
/// triples whose target source is live rather than destructive for the rest.
/// `cargo_env` also selects the config chain (`CARGO_HOME`) and carries the
/// target-table env spellings.
pub(crate) fn decide(
    kind: FlagKind,
    build_root: &Path,
    sweep_sets_env: bool,
    target: Option<&crate::target_platform::TargetFacts>,
    cargo_env: EffectiveEnv<'_>,
) -> SinkDecision {
    let env = RustflagsEnv::for_kind(kind, cargo_env);
    if let Some(sink) = env_sink(sweep_sets_env, &env) {
        return SinkDecision {
            sink,
            certain: true,
            string_destination: None,
        };
    }
    let files = match read_config_files_under(build_root, cargo_env) {
        Ok(files) => files,
        // BuildConfig is the non-destructive fallback: an unseen target table
        // may leave brokkr's flags inert, while a speculative TargetConfig
        // could demote the project's build flags. Cargo itself rejects a
        // selected unreadable or malformed config before compiling.
        Err(_) => {
            return SinkDecision {
                sink: Sink::BuildConfig,
                certain: false,
                string_destination: None,
            };
        }
    };
    let decided = decide_config_layer(kind, build_root, target, cargo_env, &files);
    SinkDecision {
        string_destination: string_destination(kind, decided.sink, &files),
        ..decided
    }
}

/// The file holding a string at the `--config` key `sink` writes, which
/// brokkr's array value cannot merge with: cargo refuses a string meeting an
/// array even from `--config`, so emitting it fails the invocation.
fn string_destination(
    kind: FlagKind,
    sink: Sink,
    files: &[(PathBuf, toml::Table)],
) -> Option<PathBuf> {
    files.iter().find_map(|(path, doc)| {
        let table = match sink {
            Sink::TargetConfig => doc
                .get("target")?
                .as_table()?
                .get("cfg(all())")?
                .as_table()?,
            Sink::BuildConfig => doc.get("build")?.as_table()?,
            Sink::EncodedEnv | Sink::Env => return None,
        };
        matches!(table.get(kind.key()), Some(toml::Value::String(_))).then(|| path.clone())
    })
}

/// [`decide`]'s config-layer half, over an already read chain.
fn decide_config_layer(
    kind: FlagKind,
    build_root: &Path,
    target: Option<&crate::target_platform::TargetFacts>,
    cargo_env: EffectiveEnv<'_>,
    files: &[(PathBuf, toml::Table)],
) -> SinkDecision {
    let host = host_triple_for(build_root);
    let triples = match target {
        Some(t) => vec![t.triple().to_owned()],
        None => match target_triples_for_sink(build_root, files, cargo_env) {
            Some(triples) => triples,
            None => {
                return SinkDecision {
                    sink: Sink::BuildConfig,
                    certain: false,
                    string_destination: None,
                };
            }
        },
    };
    let answers: Vec<Option<bool>> = if triples.is_empty() {
        // No host triple could be read: the host's cfg values still decide
        // `cfg(...)` tables, but no bare triple table can be confirmed.
        vec![triple_source_live(
            kind,
            files,
            None,
            Platform::Host(None),
            cargo_env,
        )]
    } else {
        triples
            .iter()
            .map(|triple| {
                let platform = match target {
                    Some(t) => Platform::Target(t),
                    None if host.as_deref() == Some(triple.as_str()) => {
                        Platform::Host(host.as_deref())
                    }
                    None => Platform::Unknown(triple),
                };
                triple_source_live(kind, files, Some(triple), platform, cargo_env)
            })
            .collect()
    };
    if answers.iter().all(|a| *a == Some(true)) {
        SinkDecision {
            sink: Sink::TargetConfig,
            certain: true,
            string_destination: None,
        }
    } else {
        SinkDecision {
            sink: Sink::BuildConfig,
            certain: answers.iter().all(|a| *a == Some(false)),
            string_destination: None,
        }
    }
}

/// Whether source 3 is live for one platform, three-valued: `Some(true)` when
/// some contribution is proven - the triple key or a `cfg(...)` that evaluates
/// true having non-empty flags once merged across the chain as cargo merges
/// them (a string replaces lower layers, an array appends), or a non-empty
/// `CARGO_TARGET_<triple>_<KIND>` - `Some(false)` when none can contribute,
/// `None` when nothing is proven but something could not be judged. An
/// undecided table cannot cancel a proven one, and a lower layer's flags a
/// higher layer replaced cannot prove anything. A set-but-empty target env
/// variable proves nothing: unlike the two top-level env sources, an empty
/// target source falls through to `build.<key>`.
fn triple_source_live(
    kind: FlagKind,
    files: &[(PathBuf, toml::Table)],
    triple: Option<&str>,
    platform: Platform<'_>,
    cargo_env: EffectiveEnv<'_>,
) -> Option<bool> {
    let flags_of = |selector: &str| {
        let selector = selector.to_owned();
        merged_config_flags(files, move |doc: &toml::Table| {
            doc.get("target")?
                .as_table()?
                .get(&selector)?
                .as_table()?
                .get(kind.key())
                .cloned()
        })
    };
    // `Some(true)` non-empty, `Some(false)` absent or empty, `None` unreadable.
    let non_empty = |merged: Option<Option<Vec<String>>>| match merged {
        Some(Some(flags)) => Some(!flags.is_empty()),
        Some(None) => Some(false),
        None => None,
    };
    let mut undecided = false;
    let selectors: BTreeSet<&str> = files
        .iter()
        .filter_map(|(_, doc)| doc.get("target")?.as_table())
        .flat_map(|targets| targets.keys().map(String::as_str))
        .collect();
    for selector in selectors {
        let flagged = non_empty(flags_of(selector));
        if flagged == Some(false) {
            continue;
        }
        let matches = match selector
            .strip_prefix("cfg(")
            .and_then(|s| s.strip_suffix(')'))
        {
            Some(expr) => eval_cfg_on(expr, platform),
            None => triple.map(|t| t == selector),
        };
        match (matches, flagged) {
            (Some(true), Some(true)) => return Some(true),
            (Some(false), _) => {}
            _ => undecided = true,
        }
    }
    if let Some(t) = triple {
        match cargo_env
            .get_os(&kind.target_env_var(t))
            .map(std::ffi::OsString::into_string)
        {
            Some(Ok(v)) if v.split_whitespace().next().is_some() => return Some(true),
            Some(Err(_)) => undecided = true,
            _ => {}
        }
    }
    if undecided { None } else { Some(false) }
}

/// Resolve the environment-backed sink independently from process and config
/// discovery so precedence and empty-value behavior can be tested directly.
fn env_sink(sweep_sets_env: bool, env: &RustflagsEnv) -> Option<Sink> {
    match env.active().map(|(key, _)| key) {
        Some(key) if key == env.kind.encoded_env() => Some(Sink::EncodedEnv),
        Some(_) => Some(Sink::Env),
        None if sweep_sets_env => Some(Sink::Env),
        None => None,
    }
}

/// Where one cargo invocation's added flags of a kind ended up: an env pair
/// to set (the live env source's value with the additions appended) or
/// `--config` arguments, and whether the layer is proven ([`SinkDecision`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Placement {
    pub(crate) env: Option<(String, String)>,
    pub(crate) args: Vec<String>,
    pub(crate) certain: bool,
}

/// Place `additions` of `kind` for one cargo invocation without displacing
/// the flags that invocation already gets: appended to the live env source,
/// else `--config` at the live config layer. The env value is composed from
/// `cargo_env`, the same environment the decision read, so selection and
/// construction cannot disagree about which source is live.
pub(crate) fn place(
    kind: FlagKind,
    build_root: &Path,
    target: Option<&crate::target_platform::TargetFacts>,
    cargo_env: EffectiveEnv<'_>,
    additions: &[String],
) -> Result<Placement, crate::error::DevError> {
    let decision = decide(kind, build_root, false, target, cargo_env);
    if decision.sink.is_env() {
        return Ok(Placement {
            env: RustflagsEnv::for_kind(kind, cargo_env).compose(additions)?,
            args: Vec::new(),
            certain: decision.certain,
        });
    }
    decision.refuse_string_destination(kind, additions)?;
    Ok(Placement {
        env: None,
        args: config_args_for(kind, decision.sink, additions),
        certain: decision.certain,
    })
}

/// The rustflags cargo applies to a host build rooted at `build_root`, and the
/// source that supplied them - cargo's precedence, first match wins:
/// `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, every matching `target.<triple>` /
/// `target.<cfg>` table joined (with `CARGO_TARGET_<triple>_RUSTFLAGS`), then
/// `build.rustflags` (with `CARGO_BUILD_RUSTFLAGS`). A target source that comes
/// to no flags falls through to `build.rustflags`; a set env source, empty or
/// not, wins outright. Each key merges across the config chain as
/// cargo merges config values (an array appends, a string replaces), and the
/// target source takes the triple key first, then the matching cfg keys.
///
/// `None` flags with source `undecided` when the answer cannot be read with
/// confidence: an unreadable config chain, a flagged `cfg(...)` table this
/// evaluator does not model, or a non-Unicode env value. A recorded guess
/// would be worse than the admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveRustflags {
    pub(crate) source: &'static str,
    pub(crate) flags: Option<Vec<String>>,
}

pub(crate) fn effective_rustflags(build_root: &Path, env: EffectiveEnv<'_>) -> EffectiveRustflags {
    let undecided = EffectiveRustflags {
        source: "undecided",
        flags: None,
    };
    let rustflags_env = RustflagsEnv::from(env);
    if let Some((key, value)) = rustflags_env.active() {
        let Some(value) = value.to_str() else {
            return undecided;
        };
        let flags = if key == "CARGO_ENCODED_RUSTFLAGS" {
            value
                .split('\u{1f}')
                .filter(|f| !f.is_empty())
                .map(str::to_owned)
                .collect()
        } else {
            // Cargo's split: literal spaces, each piece trimmed, empties
            // dropped. A tab or non-breaking space inside a piece stays.
            value
                .split(' ')
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let source = if key == "CARGO_ENCODED_RUSTFLAGS" {
            "env:CARGO_ENCODED_RUSTFLAGS"
        } else {
            "env:RUSTFLAGS"
        };
        return EffectiveRustflags {
            source,
            flags: Some(flags),
        };
    }
    let Ok(files) = read_config_files_under(build_root, env) else {
        return undecided;
    };
    let host = host_triple_for(build_root);
    let Some(triples) = target_triples_for_sink(build_root, &files, env) else {
        return undecided;
    };
    let [triple] = triples.as_slice() else {
        // Several configured targets build several ways; no one answer.
        return undecided;
    };
    // The cfg evaluator answers for the host; a configured cross target's
    // `cfg(...)` tables are not the host's to judge.
    if host.as_deref() != Some(triple.as_str()) {
        return undecided;
    }
    let platform = Platform::Host(host.as_deref());
    match target_table_flags(&files, triple, platform, env) {
        None => return undecided,
        Some(Some(flags)) => {
            return EffectiveRustflags {
                source: "target",
                flags: Some(flags),
            };
        }
        Some(None) => {}
    }
    match build_table_flags(&files, env) {
        None => undecided,
        Some(Some(flags)) => EffectiveRustflags {
            source: "build",
            flags: Some(flags),
        },
        Some(None) => EffectiveRustflags {
            source: "none",
            flags: Some(Vec::new()),
        },
    }
}

/// Source 3, as cargo builds it: the `target.<triple>` key (config merged,
/// then its `CARGO_TARGET_<triple>_RUSTFLAGS` spelling), then every matching
/// `target.cfg(...)` key in key order, each merged across the chain on its
/// own. `None` is undecided; `Some(None)` is no flags at all - an empty
/// result falls through to `build.rustflags`, as cargo's does.
fn target_table_flags(
    files: &[(PathBuf, toml::Table)],
    triple: &str,
    platform: Platform<'_>,
    env: EffectiveEnv<'_>,
) -> Option<Option<Vec<String>>> {
    let target_key = |selector: &str| {
        let selector = selector.to_owned();
        move |doc: &toml::Table| -> Option<toml::Value> {
            doc.get("target")?
                .as_table()?
                .get(&selector)?
                .as_table()?
                .get("rustflags")
                .cloned()
        }
    };
    let mut flags: Vec<String> =
        merged_config_flags(files, target_key(triple))?.unwrap_or_default();
    if let Some(value) = env.get_os(&target_rustflags_env_var(triple)) {
        flags.extend(value.to_str()?.split_whitespace().map(str::to_owned));
    }
    let cfg_selectors: BTreeSet<&str> = files
        .iter()
        .filter_map(|(_, doc)| doc.get("target")?.as_table())
        .flat_map(|targets| targets.keys().map(String::as_str))
        .filter(|selector| selector.starts_with("cfg("))
        .collect();
    for selector in cfg_selectors {
        let Some(cfg_flags) = merged_config_flags(files, target_key(selector))? else {
            continue;
        };
        let expr = selector
            .strip_prefix("cfg(")
            .and_then(|s| s.strip_suffix(')'))?;
        if eval_cfg_on(expr, platform)? {
            flags.extend(cfg_flags);
        }
    }
    Some((!flags.is_empty()).then_some(flags))
}

/// Source 4: `build.rustflags` merged across the chain, then
/// `CARGO_BUILD_RUSTFLAGS`. `None` is undecided; `Some(None)` is no flags.
fn build_table_flags(
    files: &[(PathBuf, toml::Table)],
    env: EffectiveEnv<'_>,
) -> Option<Option<Vec<String>>> {
    let mut flags: Vec<String> = merged_config_flags(files, |doc: &toml::Table| {
        doc.get("build")?.as_table()?.get("rustflags").cloned()
    })?
    .unwrap_or_default();
    if let Some(value) = env.get_os("CARGO_BUILD_RUSTFLAGS") {
        flags.extend(value.to_str()?.split_whitespace().map(str::to_owned));
    }
    Some((!flags.is_empty()).then_some(flags))
}

/// One rustflags key merged across the config chain as cargo merges config
/// values: lowest precedence first, an array appending to what lies below, a
/// string replacing it. `None` is undecided: a value of another shape, or a
/// string and an array for one key, which cargo refuses to merge.
/// `Some(None)` is a key no file sets.
fn merged_config_flags(
    files: &[(PathBuf, toml::Table)],
    get: impl Fn(&toml::Table) -> Option<toml::Value>,
) -> Option<Option<Vec<String>>> {
    let mut merged: Option<Vec<String>> = None;
    let mut was_string: Option<bool> = None;
    for (_, doc) in files.iter().rev() {
        let Some(value) = get(doc) else {
            continue;
        };
        let is_string = matches!(value, toml::Value::String(_));
        if was_string.is_some_and(|was| was != is_string) {
            return None;
        }
        was_string = Some(is_string);
        match value {
            toml::Value::String(s) => {
                merged = Some(s.split_whitespace().map(str::to_owned).collect());
            }
            toml::Value::Array(items) => {
                let items: Vec<String> = items
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<_>>()?;
                merged.get_or_insert_with(Vec::new).extend(items);
            }
            _ => return None,
        }
    }
    Some(merged)
}

/// Split `allow_flags` into the share that rides the environment and the share
/// that rides cargo's command line - exactly one of which is ever non-empty.
///
/// The two always travel together (the same sink decision selects both), and
/// both `check`'s test phase and `brokkr test` need the pair, so the split
/// lives here rather than being re-derived at each call site.
pub(crate) fn plumbing<'a>(
    build_root: &Path,
    sweep_sets_rustflags: bool,
    target: Option<&crate::target_platform::TargetFacts>,
    cargo_env: EffectiveEnv<'_>,
    allow_flags: &'a [String],
) -> Result<(&'a [String], Vec<String>), crate::error::DevError> {
    let decision = decide(
        FlagKind::Rustc,
        build_root,
        sweep_sets_rustflags,
        target,
        cargo_env,
    );
    if decision.sink.is_env() {
        Ok((allow_flags, Vec::new()))
    } else {
        decision.refuse_string_destination(FlagKind::Rustc, allow_flags)?;
        Ok((&[], config_args(decision.sink, allow_flags)))
    }
}

/// The `--config` arguments for `flags`, or empty for an env sink (where the
/// caller appends to the env value instead).
pub fn config_args(sink: Sink, flags: &[String]) -> Vec<String> {
    config_args_for(FlagKind::Rustc, sink, flags)
}

/// [`config_args`] for either flag kind.
pub(crate) fn config_args_for(kind: FlagKind, sink: Sink, flags: &[String]) -> Vec<String> {
    if flags.is_empty() || sink.is_env() {
        return Vec::new();
    }
    let key = match sink {
        Sink::TargetConfig => format!("target.\"cfg(all())\".{}", kind.key()),
        _ => format!("build.{}", kind.key()),
    };
    let list = flags
        .iter()
        .map(|f| toml_string(f))
        .collect::<Vec<_>>()
        .join(",");
    vec!["--config".into(), format!("{key}=[{list}]")]
}

/// Serialize a TOML string value through `toml_edit`'s basic-string writer.
fn toml_string(s: &str) -> String {
    toml_edit::Value::from(s).to_string()
}

fn target_rustflags_env_var(triple: &str) -> String {
    FlagKind::Rustc.target_env_var(triple)
}

/// Cargo's configured target names drive `CARGO_TARGET_<triple>_RUSTFLAGS`.
/// Respect `CARGO_BUILD_TARGET` or `build.target`; if neither is present,
/// Cargo builds for the host. `build.target` merges across the chain as cargo
/// merges it (an array appends, a string replaces); `None` is a definition
/// brokkr cannot model - a string and an array for the key, which cargo
/// refuses, or a value of another shape - and callers take their uncertain
/// side rather than guess the host.
fn target_triples_for_sink(
    build_root: &Path,
    files: &[(PathBuf, toml::Table)],
    cargo_env: EffectiveEnv<'_>,
) -> Option<Vec<String>> {
    let host = host_triple_for(build_root);
    if let Some(targets) = cargo_env
        .get_os("CARGO_BUILD_TARGET")
        .and_then(|value| value.into_string().ok())
        .and_then(|value| parse_target_values(&value, host.as_deref()))
    {
        return Some(targets);
    }
    let mut merged: Option<Vec<String>> = None;
    let mut was_string: Option<bool> = None;
    for (_, doc) in files.iter().rev() {
        let Some(value) = doc
            .get("build")
            .and_then(toml::Value::as_table)
            .and_then(|build| build.get("target"))
        else {
            continue;
        };
        let is_string = matches!(value, toml::Value::String(_));
        if was_string.is_some_and(|was| was != is_string) {
            return None;
        }
        was_string = Some(is_string);
        match value {
            toml::Value::String(value) => {
                merged = Some(parse_target_values(value, host.as_deref()).unwrap_or_default());
            }
            toml::Value::Array(values) => {
                let targets = values
                    .iter()
                    .map(|value| value.as_str())
                    .collect::<Option<Vec<_>>>()?;
                merged.get_or_insert_with(Vec::new).extend(
                    targets
                        .into_iter()
                        .filter_map(|value| target_key_triple(value, host.as_deref())),
                );
            }
            _ => return None,
        }
    }
    Some(
        merged
            .filter(|targets| !targets.is_empty())
            .unwrap_or_else(|| host.into_iter().collect()),
    )
}

fn parse_target_values(value: &str, host: Option<&str>) -> Option<Vec<String>> {
    let targets: Vec<String> = value
        .split(',')
        .filter_map(|value| target_key_triple(value, host))
        .collect();
    (!targets.is_empty()).then_some(targets)
}

fn target_key_triple(value: &str, host: Option<&str>) -> Option<String> {
    let target = value.trim();
    if target.is_empty() {
        return None;
    }
    if target == "host-tuple" {
        return host.map(str::to_owned);
    }
    let path = Path::new(target);
    Some(
        path.file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or(target)
            .to_owned(),
    )
}

/// Whether [`sink`]'s answer for a plain sweep (one exporting no `rustflags`)
/// might be inert: it fell through to `build.rustflags` on a guess
/// ([`SinkDecision::certain`]). If an undecided target table does match,
/// cargo reads it instead and the injected flags do nothing. The fall-through
/// is the deliberate safe direction; this is how the caller finds out it was
/// taken on a guess.
pub fn sink_may_be_inert(build_root: &Path) -> bool {
    !decide(
        FlagKind::Rustc,
        build_root,
        false,
        None,
        EffectiveEnv::new(&[]),
    )
    .certain
}

/// The rustc host selected from the current working directory. A failure here
/// degrades the bare-triple check to "no match", never to a wrong match.
pub(crate) fn host_triple() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|root| host_triple_for(&root))
}

pub(crate) fn host_triple_for(root: &Path) -> Option<String> {
    crate::cargo::rustc::load(root)
        .ok()
        .map(|info| info.host.clone())
}

/// Evaluate a cargo `cfg(...)` selector body against the host.
///
/// `Some(true)` / `Some(false)` are confident answers; `None` means the
/// expression uses something this evaluator does not model, and the caller must
/// take the non-destructive branch. brokkr runs on the host it builds for, so
/// the host's values are brokkr's own `cfg!` values - no triple parsing beyond
/// the `target_env` / `target_vendor` fields, which the consts do not expose.
#[cfg(test)]
pub(crate) fn eval_cfg(expr: &str) -> Option<bool> {
    let host = std::env::current_dir()
        .ok()
        .and_then(|root| host_triple_for(&root));
    eval_cfg_for_host(expr, host.as_deref())
}

/// Evaluate a cargo `cfg(...)` selector against a known host triple.
pub(crate) fn eval_cfg_for_host(expr: &str, host: Option<&str>) -> Option<bool> {
    eval_cfg_on(expr, Platform::Host(host))
}

/// The platform a cargo `[target.*]` selector is matched against.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Platform<'a> {
    /// The host: brokkr's own `cfg!` values plus the host triple (`None` when
    /// it could not be read).
    Host(Option<&'a str>),
    /// A `[[check]]` sweep's `target`, with rustc's facts for it.
    Target(&'a crate::target_platform::TargetFacts),
    /// A configured triple brokkr has no facts for (an ambient `build.target`
    /// other than the host): every cfg atom is undecided, so only selectors
    /// that need none (`cfg(all())`) are judged.
    Unknown(&'a str),
}

impl<'a> Platform<'a> {
    /// The triple a bare `[target.<triple>]` selector must equal.
    fn triple(self) -> Option<&'a str> {
        match self {
            Self::Host(host) => host,
            Self::Target(target) => Some(target.triple()),
            Self::Unknown(triple) => Some(triple),
        }
    }

    /// Whether `key = "value"` holds, or `None` when unmodelled.
    fn key_matches(self, key: &str, value: &str) -> Option<bool> {
        match self {
            Self::Host(host) => Some(cfg_key(key, host)? == value),
            Self::Target(target) => target.cfg().key_matches(key, value),
            Self::Unknown(_) => None,
        }
    }

    /// Whether bare `name` holds, or `None` when unmodelled.
    fn flag(self, name: &str) -> Option<bool> {
        match self {
            Self::Host(_) => cfg_flag(name),
            Self::Target(target) => target.cfg().flag(name),
            Self::Unknown(_) => None,
        }
    }
}

/// Evaluate a cargo `cfg(...)` selector body against `platform`. Any atom the
/// platform does not decide makes the whole expression undecided (`None`).
pub(crate) fn eval_cfg_on(expr: &str, platform: Platform<'_>) -> Option<bool> {
    let (value, rest) = parse_expr(expr.trim(), platform)?;
    rest.trim().is_empty().then_some(value)
}

/// Parse one predicate and return it with the unconsumed remainder.
fn parse_expr<'a>(s: &'a str, host: Platform<'_>) -> Option<(bool, &'a str)> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    let (ident, rest) = s.split_at(end);
    if ident.is_empty() {
        return None;
    }
    let rest = rest.trim_start();

    if let Some(inner) = rest.strip_prefix('(') {
        let (args, after) = parse_args(inner, host)?;
        let value = match ident {
            // `all()` of nothing is vacuously true - that is `cfg(all())`, the
            // idiomatic "every target" selector this module also injects into.
            "all" => args.iter().all(|a| *a),
            "any" => args.iter().any(|a| *a),
            // Exactly one operand, as cargo's grammar requires.
            "not" => match args.as_slice() {
                [one] => !one,
                _ => return None,
            },
            _ => return None,
        };
        return Some((value, after));
    }

    if let Some(after_eq) = rest.strip_prefix('=') {
        let after_eq = after_eq.trim_start();
        let quoted = after_eq.strip_prefix('"')?;
        let close = quoted.find('"')?;
        let (value, after) = quoted.split_at(close);
        return Some((host.key_matches(ident, value)?, &after[1..]));
    }

    // The boolean literals of cargo's cfg grammar, never flag names.
    match ident {
        "true" => return Some((true, rest)),
        "false" => return Some((false, rest)),
        _ => {}
    }
    // Bare flag: `unix`, `windows`.
    Some((host.flag(ident)?, rest))
}

/// Parse a comma-separated argument list up to the closing paren, returning the
/// evaluated arguments and the remainder after the paren. An argument that
/// cannot be evaluated poisons the whole expression.
fn parse_args<'a>(mut s: &'a str, host: Platform<'_>) -> Option<(Vec<bool>, &'a str)> {
    let mut args = Vec::new();
    loop {
        s = s.trim_start();
        if let Some(after) = s.strip_prefix(')') {
            return Some((args, after));
        }
        let (value, rest) = parse_expr(s, host)?;
        args.push(value);
        s = rest.trim_start();
        // Operands are comma-separated; two side by side are malformed, as
        // cargo parses them, never read as a list.
        if let Some(next) = s.strip_prefix(',') {
            s = next;
        } else if !s.starts_with(')') {
            return None;
        }
    }
}

/// The host's value for a `key = "value"` cfg key, or `None` if unmodelled.
fn cfg_key(key: &str, host: Option<&str>) -> Option<String> {
    let v = match key {
        "target_os" => std::env::consts::OS.to_string(),
        "target_arch" => std::env::consts::ARCH.to_string(),
        "target_family" => std::env::consts::FAMILY.to_string(),
        "target_pointer_width" => usize::BITS.to_string(),
        "target_endian" => {
            if cfg!(target_endian = "little") {
                "little".into()
            } else {
                "big".into()
            }
        }
        "target_env" => triple_field(2, host)?,
        "target_vendor" => triple_field(1, host)?,
        _ => return None,
    };
    Some(v)
}

/// A dash-separated field of the host triple (`arch-vendor-os[-env]`), used for
/// the two cfg keys `std::env::consts` does not carry. A triple short of the
/// requested field yields the empty string, which is the correct answer for
/// `target_env` on e.g. `x86_64-apple-darwin`.
fn triple_field(index: usize, host: Option<&str>) -> Option<String> {
    let triple = host?;
    let parts: Vec<&str> = triple.split('-').collect();
    // Field 2 is the OS; `target_env` is field 3 when present.
    let idx = if index == 2 { 3 } else { index };
    Some(parts.get(idx).unwrap_or(&"").to_string())
}

/// The host's value for a bare cfg flag, or `None` if unmodelled.
fn cfg_flag(name: &str) -> Option<bool> {
    match name {
        "unix" => Some(cfg!(unix)),
        "windows" => Some(cfg!(windows)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn encoded_rustflags_wins_by_presence_and_keeps_an_empty_value() {
        let overrides = [
            ("CARGO_ENCODED_RUSTFLAGS", ""),
            ("RUSTFLAGS", "-D warnings"),
        ];
        let env = RustflagsEnv::from(EffectiveEnv::new(&overrides));
        assert_eq!(
            env.compose(&["-A unused".into()]).unwrap(),
            Some(("CARGO_ENCODED_RUSTFLAGS".into(), "-A unused".into()))
        );
        assert!(
            env.invocation_notes()
                .iter()
                .any(|line| line.contains("CARGO_ENCODED_RUSTFLAGS set but empty"))
        );
    }

    #[test]
    fn sink_selection_uses_injected_rustflags_presence() {
        let encoded_empty = RustflagsEnv {
            kind: FlagKind::Rustc,
            encoded: Some("".into()),
            plain: Some("-D warnings".into()),
        };
        assert_eq!(env_sink(false, &encoded_empty), Some(Sink::EncodedEnv));

        let plain_empty = RustflagsEnv {
            kind: FlagKind::Rustc,
            encoded: None,
            plain: Some("".into()),
        };
        assert_eq!(env_sink(false, &plain_empty), Some(Sink::Env));
        assert_eq!(env_sink(true, &RustflagsEnv::default()), Some(Sink::Env));
        assert_eq!(env_sink(false, &RustflagsEnv::default()), None);
    }

    #[test]
    fn the_target_source_is_decided_on_merged_flags() {
        let doc = |text: &str| (PathBuf::from("c"), text.parse::<toml::Table>().unwrap());
        let host = Platform::Host(Some("x86_64-unknown-linux-gnu"));
        let none: [(&str, &str); 0] = [];
        let live = |files: &[(PathBuf, toml::Table)]| {
            triple_source_live(
                FlagKind::Rustc,
                files,
                Some("x86_64-unknown-linux-gnu"),
                host,
                EffectiveEnv::new(&none),
            ) == Some(true)
        };

        // A string value counts, not only a non-empty array.
        assert!(live(&[doc(
            "[target.'cfg(all())']\nrustflags = \"-Dwarnings\"\n"
        )]));
        // A higher layer's empty string replaces the lower array: nothing left.
        assert!(!live(&[
            doc("[target.'cfg(all())']\nrustflags = \"\"\n"),
            doc("[target.'cfg(all())']\nrustflags = [\"-Dwarnings\"]\n"),
        ]));
        // A table brokkr cannot evaluate does not cancel one it can prove.
        assert!(live(&[doc(
            "[target.'cfg(all())']\nrustflags = [\"-Dwarnings\"]\n\
             [target.'cfg(target_has_atomic = \"128\")']\nrustflags = [\"-Cx\"]\n"
        )]));
        assert!(!live(&[doc("[build]\nrustflags = [\"-Cx\"]\n")]));
        // Replacement and an undecided table together: the replaced lower
        // layer's flags prove nothing (the review's counterexample).
        assert!(!live(&[
            doc("[build]\nrustflags = [\"-Ctarget-cpu=native\"]\n\
                 [target.'cfg(all())']\nrustflags = \"\"\n\
                 [target.'cfg(all(windows, target_has_atomic = \"128\"))']\nrustflags = [\"-Cx\"]\n"),
            doc("[target.'cfg(all())']\nrustflags = [\"-Dwarnings\"]\n"),
        ]));
        // The triple key counts like a matching cfg.
        assert!(live(&[doc(
            "[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-Cx\"]\n"
        )]));
    }

    #[test]
    fn config_snapshot_includes_cargo_documents_in_precedence_order() {
        let root = crate::test_scratch::scratch("rustflags", "cargo_config_includes")
            .canonicalize()
            .unwrap();
        let cargo_dir = root.join(".cargo");
        std::fs::create_dir_all(&cargo_dir).unwrap();
        std::fs::write(
            cargo_dir.join("config.toml"),
            "include = [\"first.toml\", \"second.toml\"]\n",
        )
        .unwrap();
        std::fs::write(cargo_dir.join("first.toml"), "[build]\nrustflags = []\n").unwrap();
        std::fs::write(cargo_dir.join("second.toml"), "[build]\nrustflags = []\n").unwrap();

        let cargo_home = [("CARGO_HOME", "empty-cargo-home")];
        let files = read_config_files_under(&root, EffectiveEnv::new(&cargo_home)).unwrap();
        let paths: Vec<PathBuf> = files.iter().map(|(path, _)| path.clone()).collect();
        assert_eq!(paths.first(), Some(&cargo_dir.join("config.toml")));
        assert_eq!(paths.get(1), Some(&cargo_dir.join("second.toml")));
        assert_eq!(paths.get(2), Some(&cargo_dir.join("first.toml")));
    }

    /// Cargo's probe: the legacy file when it opens, else `config.toml` when
    /// it exists, a directory at either spelling included.
    #[test]
    fn config_selection_follows_cargos_probe() {
        let both = crate::test_scratch::scratch("rustflags", "select_both");
        std::fs::write(both.join("config"), "").unwrap();
        std::fs::write(both.join("config.toml"), "").unwrap();
        assert_eq!(selected_config_file(&both), Some(both.join("config")));

        let modern = crate::test_scratch::scratch("rustflags", "select_modern");
        std::fs::write(modern.join("config.toml"), "").unwrap();
        assert_eq!(
            selected_config_file(&modern),
            Some(modern.join("config.toml"))
        );

        let neither = crate::test_scratch::scratch("rustflags", "select_neither");
        assert_eq!(selected_config_file(&neither), None);

        // A directory is selected and then fails to read, as cargo fails.
        let root = crate::test_scratch::scratch("rustflags", "select_directory");
        std::fs::create_dir_all(root.join(".cargo/config.toml")).unwrap();
        assert_eq!(
            selected_config_file(&root.join(".cargo")),
            Some(root.join(".cargo/config.toml"))
        );
        let cargo_home = [("CARGO_HOME", "empty-cargo-home")];
        assert!(read_config_files_under(&root, EffectiveEnv::new(&cargo_home)).is_err());
    }

    /// `[env]` merges per key across the chain: a higher table that only
    /// sets `force` keeps the lower `value`, which anchors `relative` at the
    /// file that supplied it; a string meeting a table refuses.
    #[test]
    fn config_env_merges_per_key_as_cargo_merges() {
        let root = crate::test_scratch::scratch("rustflags", "env_merge");
        let inner = root.join("inner");
        std::fs::create_dir_all(inner.join(".cargo")).unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[env]\nFOO = { value = \"data\", relative = true }\nBAR = \"low\"\n",
        )
        .unwrap();
        std::fs::write(
            inner.join(".cargo/config.toml"),
            "[env]\nFOO = { force = true }\nBAR = \"high\"\n",
        )
        .unwrap();
        let files =
            read_config_files_under(&inner, EffectiveEnv::new(&[("CARGO_HOME", "h")])).unwrap();
        let env = merged_config_env(&files).unwrap();
        let foo = env.iter().find(|e| e.key == "FOO").unwrap();
        assert!(foo.force);
        let physical = root.canonicalize().unwrap();
        assert_eq!(foo.value, physical.join("data").display().to_string());
        let bar = env.iter().find(|e| e.key == "BAR").unwrap();
        assert_eq!(bar.value, "high");
        assert_eq!(env.len(), 2);

        std::fs::write(inner.join(".cargo/config.toml"), "[env]\nFOO = \"plain\"\n").unwrap();
        let files =
            read_config_files_under(&inner, EffectiveEnv::new(&[("CARGO_HOME", "h")])).unwrap();
        assert!(merged_config_env(&files).is_err());
    }

    /// A string at the key brokkr's `--config` array would merge into is
    /// refused when flags would be placed there, and a string/array mix for
    /// one rustflags key is undecided.
    #[test]
    fn a_string_destination_refuses_config_placement() {
        let files = vec![(
            PathBuf::from("/p/.cargo/config.toml"),
            doc("[build]\nrustflags = \"-Dwarnings\"\n"),
        )];
        let found = string_destination(FlagKind::Rustc, Sink::BuildConfig, &files);
        assert_eq!(found, Some(PathBuf::from("/p/.cargo/config.toml")));
        assert_eq!(
            string_destination(FlagKind::Rustc, Sink::TargetConfig, &files),
            None
        );
        let decision = SinkDecision {
            sink: Sink::BuildConfig,
            certain: true,
            string_destination: found,
        };
        assert!(
            decision
                .refuse_string_destination(FlagKind::Rustc, &[])
                .is_ok()
        );
        assert!(
            decision
                .refuse_string_destination(FlagKind::Rustc, &["-Aunused".into()])
                .is_err()
        );

        let mixed = vec![
            (
                PathBuf::from("/p/a/.cargo/config.toml"),
                doc("[build]\nrustflags = [\"-Dx\"]\n"),
            ),
            (
                PathBuf::from("/p/.cargo/config.toml"),
                doc("[build]\nrustflags = \"-Dy\"\n"),
            ),
        ];
        assert_eq!(
            merged_config_flags(&mixed, |d: &toml::Table| {
                d.get("build")?.as_table()?.get("rustflags").cloned()
            }),
            None
        );
    }

    /// `build.target` merges across the chain; a string/array mix is
    /// unmodelled rather than the host.
    #[test]
    fn build_target_merges_across_the_chain() {
        let root = crate::test_scratch::scratch("rustflags", "build_target_merge");
        let none = EffectiveEnv::new(&[]);
        let arrays = vec![
            (
                PathBuf::from("/a"),
                doc("[build]\ntarget = [\"x86_64-unknown-none\"]\n"),
            ),
            (
                PathBuf::from("/b"),
                doc("[build]\ntarget = [\"wasm32-unknown-unknown\"]\n"),
            ),
        ];
        assert_eq!(
            target_triples_for_sink(&root, &arrays, none),
            Some(vec![
                "wasm32-unknown-unknown".into(),
                "x86_64-unknown-none".into()
            ])
        );
        let mixed = vec![
            (
                PathBuf::from("/a"),
                doc("[build]\ntarget = \"x86_64-unknown-none\"\n"),
            ),
            (
                PathBuf::from("/b"),
                doc("[build]\ntarget = [\"wasm32-unknown-unknown\"]\n"),
            ),
        ];
        assert_eq!(target_triples_for_sink(&root, &mixed, none), None);
    }

    /// Two symlinked spellings of one file are two documents, each resolving
    /// its relative include against the directory it was reached through.
    #[cfg(unix)]
    #[test]
    fn symlinked_includes_resolve_children_against_the_reached_path() {
        let root = crate::test_scratch::scratch("rustflags", "symlinked_includes")
            .canonicalize()
            .unwrap();
        let cargo_dir = root.join(".cargo");
        let shared = root.join("shared");
        for dir in [
            &cargo_dir,
            &cargo_dir.join("a"),
            &cargo_dir.join("b"),
            &shared,
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(
            cargo_dir.join("config.toml"),
            "include = [\"a/common.toml\", \"b/common.toml\"]\n",
        )
        .unwrap();
        std::fs::write(
            shared.join("common.toml"),
            "include = [{ path = \"child.toml\", optional = true }]\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(shared.join("common.toml"), cargo_dir.join("a/common.toml"))
            .unwrap();
        std::os::unix::fs::symlink(shared.join("common.toml"), cargo_dir.join("b/common.toml"))
            .unwrap();
        std::fs::write(cargo_dir.join("b/child.toml"), "[build]\ntarget = \"x\"\n").unwrap();

        let cargo_home = [("CARGO_HOME", "empty-cargo-home")];
        let files = read_config_files_under(&root, EffectiveEnv::new(&cargo_home)).unwrap();
        // Ancestors above the scratch root may hold real configs; only the
        // scratch root's own chain is under test.
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|(path, _)| path.clone())
            .filter(|path| path.starts_with(&root))
            .collect();
        assert_eq!(
            paths,
            vec![
                cargo_dir.join("config.toml"),
                cargo_dir.join("b/common.toml"),
                cargo_dir.join("b/child.toml"),
                cargo_dir.join("a/common.toml"),
            ]
        );
    }

    /// Cargo's include set is per root and never cleared: one file included
    /// twice under a root refuses, while a file reached from two roots is
    /// read once per root.
    #[test]
    fn include_repeats_refuse_within_a_root_and_repeat_across_roots() {
        let root = crate::test_scratch::scratch("rustflags", "include_repeats")
            .canonicalize()
            .unwrap();
        let inner = root.join("inner");
        std::fs::create_dir_all(inner.join(".cargo")).unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(root.join(".cargo/shared.toml"), "[build]\nrustflags = []\n").unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "include = [\"shared.toml\"]\n",
        )
        .unwrap();
        std::fs::write(
            inner.join(".cargo/config.toml"),
            "include = [\"../../.cargo/shared.toml\"]\n",
        )
        .unwrap();
        let cargo_home = [("CARGO_HOME", "empty-cargo-home")];
        let files = read_config_files_under(&inner, EffectiveEnv::new(&cargo_home)).unwrap();
        let shared = files
            .iter()
            .filter(|(path, _)| *path == root.join(".cargo/shared.toml"))
            .count();
        assert_eq!(shared, 2, "{files:?}");

        std::fs::write(
            root.join(".cargo/config.toml"),
            "include = [\"shared.toml\", \"./shared.toml\"]\n",
        )
        .unwrap();
        assert!(read_config_files_under(&root, EffectiveEnv::new(&cargo_home)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_encoded_rustflags_refuses_instead_of_falling_through() {
        use std::os::unix::ffi::OsStringExt;

        let env = RustflagsEnv {
            kind: FlagKind::Rustc,
            encoded: Some(std::ffi::OsString::from_vec(vec![0xff])),
            plain: Some("-D warnings".into()),
        };
        assert!(matches!(
            env.compose(&["-A unused".into()]),
            Err(crate::error::DevError::Config(message))
                if message.contains("CARGO_ENCODED_RUSTFLAGS")
        ));
    }

    #[test]
    fn effective_rustflags_follow_cargos_precedence() {
        let root = crate::test_scratch::scratch("rustflags", "effective");
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        let home = ("CARGO_HOME", "no-cargo-home");
        // The environment wins outright, whatever the config says.
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"-Cbuild\"]\n",
        )
        .unwrap();
        let env = [home, ("RUSTFLAGS", "-Cfrom-env  -Cmore")];
        let got = effective_rustflags(&root, EffectiveEnv::new(&env));
        assert_eq!(got.source, "env:RUSTFLAGS");
        assert_eq!(got.flags.unwrap(), ["-Cfrom-env", "-Cmore"]);
    }

    /// The config sources over an explicit chain (highest precedence first),
    /// independent of whatever the host's own config files hold.
    #[test]
    fn config_rustflags_sources_join_and_demote() {
        let chain = |docs: &[&str]| -> Vec<(PathBuf, toml::Table)> {
            docs.iter()
                .enumerate()
                .map(|(i, d)| (PathBuf::from(format!("/c{i}")), d.parse().unwrap()))
                .collect()
        };
        let none: [(&str, &str); 0] = [];
        let env = EffectiveEnv::new(&none);
        let host = Platform::Host(Some("x86_64-unknown-linux-gnu"));
        let triple = "x86_64-unknown-linux-gnu";
        // Arrays from two files join, lowest precedence first...
        let files = chain(&[
            "[build]\nrustflags = [\"-Chigh\"]\n",
            "[build]\nrustflags = [\"-Clow\"]\n",
        ]);
        assert_eq!(
            build_table_flags(&files, env),
            Some(Some(vec!["-Clow".into(), "-Chigh".into()]))
        );
        assert_eq!(target_table_flags(&files, triple, host, env), Some(None));
        // ...while a string replaces a string below it...
        let files = chain(&[
            "[build]\nrustflags = \"-Ctarget-cpu=x86-64\"\n",
            "[build]\nrustflags = \"-Ctarget-cpu=native\"\n",
        ]);
        assert_eq!(
            build_table_flags(&files, env),
            Some(Some(vec!["-Ctarget-cpu=x86-64".into()]))
        );
        // ...and a string over an array is a merge cargo refuses: undecided.
        let files = chain(&[
            "[build]\nrustflags = \"-Ctarget-cpu=x86-64\"\n",
            "[build]\nrustflags = [\"-Ctarget-cpu=native\"]\n",
        ]);
        assert_eq!(build_table_flags(&files, env), None);
        // The triple key comes first, merged across files, then the matching
        // cfg keys; a bare triple must equal the platform's.
        let files = chain(&[
            "[target.'cfg(all())']\nrustflags = [\"-Ccfg\"]\n\
             [target.x86_64-unknown-linux-gnu]\nrustflags = [\"-Ctriple-high\"]\n",
            "[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-Ctriple-low\"]\n\
             [target.aarch64-unknown-linux-gnu]\nrustflags = [\"-Cother\"]\n",
        ]);
        assert_eq!(
            target_table_flags(&files, triple, host, env),
            Some(Some(vec![
                "-Ctriple-low".into(),
                "-Ctriple-high".into(),
                "-Ccfg".into()
            ]))
        );
        // An empty target result falls through to `build.rustflags`.
        let files = chain(&[
            "[target.x86_64-unknown-linux-gnu]\nrustflags = []\n[build]\nrustflags = [\"-Cbuild\"]\n",
        ]);
        assert_eq!(target_table_flags(&files, triple, host, env), Some(None));
        assert_eq!(
            build_table_flags(&files, env),
            Some(Some(vec!["-Cbuild".into()]))
        );
        // A flagged table this evaluator cannot judge is an admission, not a guess.
        let files =
            chain(&["[target.'cfg(target_has_atomic = \"128\")']\nrustflags = [\"-Cx\"]\n"]);
        assert_eq!(
            target_table_flags(&files, "x86_64-unknown-linux-gnu", host, env),
            None
        );
    }

    #[test]
    fn cfg_all_of_nothing_is_true() {
        // `cfg(all())` is the idiomatic every-target selector; it must resolve
        // confidently, since it is also the key brokkr injects into.
        assert_eq!(eval_cfg("all()"), Some(true));
    }

    #[test]
    fn cfg_any_of_nothing_is_false() {
        assert_eq!(eval_cfg("any()"), Some(false));
    }

    #[test]
    fn cfg_matches_host_os() {
        let os = std::env::consts::OS;
        assert_eq!(eval_cfg(&format!("target_os = \"{os}\"")), Some(true));
        assert_eq!(
            eval_cfg("target_os = \"definitely-not-an-os\""),
            Some(false)
        );
    }

    #[test]
    fn cfg_nested_combinators() {
        let os = std::env::consts::OS;
        assert_eq!(
            eval_cfg(&format!(
                "all(not(target_os = \"nope\"), any(target_os = \"{os}\"))"
            )),
            Some(true)
        );
    }

    #[test]
    fn unmodelled_cfg_is_unknown_not_false() {
        // The distinction matters: "unknown" must reach the caller so it can
        // pick the inert sink. Collapsing it to `false` would be indistinguishable
        // from a confident no-match.
        assert_eq!(eval_cfg("target_has_atomic = \"128\""), None);
        assert_eq!(eval_cfg("all(unix, target_has_atomic = \"128\")"), None);
    }

    #[test]
    fn malformed_cfg_is_unknown() {
        assert_eq!(eval_cfg("all(unix"), None);
        assert_eq!(eval_cfg("unix)"), None);
        assert_eq!(eval_cfg(""), None);
    }

    fn doc(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    /// Whether this one document makes source 3 provably live for a host
    /// whose triple is `triple` (`None`: the host triple could not be read).
    fn has_matching_target_rustflags(doc: &toml::Table, triple: Option<&str>) -> bool {
        let none: [(&str, &str); 0] = [];
        triple_source_live(
            FlagKind::Rustc,
            &[(PathBuf::from("c"), doc.clone())],
            triple,
            Platform::Host(triple),
            EffectiveEnv::new(&none),
        ) == Some(true)
    }

    #[test]
    fn a_triple_with_no_facts_judges_only_platform_free_selectors() {
        // An ambient cross `build.target`: the host's values must not decide a
        // cfg table for it, but `cfg(all())` holds on every platform.
        let none: [(&str, &str); 0] = [];
        let env = EffectiveEnv::new(&none);
        let live = |text: &str| {
            triple_source_live(
                FlagKind::Rustdoc,
                &[(PathBuf::from("c"), doc(text))],
                Some("aarch64-pc-windows-msvc"),
                Platform::Unknown("aarch64-pc-windows-msvc"),
                env,
            )
        };
        assert_eq!(
            live("[target.'cfg(unix)']\nrustdocflags = [\"-x\"]\n"),
            None
        );
        assert_eq!(
            live("[target.'cfg(all())']\nrustdocflags = [\"-x\"]\n"),
            Some(true)
        );
        assert_eq!(
            live("[target.aarch64-pc-windows-msvc]\nrustdocflags = [\"-x\"]\n"),
            Some(true)
        );
        // The rustc key is not the rustdoc one.
        assert_eq!(
            live("[target.'cfg(all())']\nrustflags = [\"-x\"]\n"),
            Some(false)
        );
    }

    #[test]
    fn an_empty_target_env_variable_proves_nothing() {
        // Cargo falls through to `build.<key>` when the joined target source is
        // empty, so injecting at the target layer would demote build flags.
        let triple = "x86_64-unknown-linux-gnu";
        let var = FlagKind::Rustdoc.target_env_var(triple);
        assert_eq!(var, "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTDOCFLAGS");
        let live = |value: &str| {
            let pairs = [(var.as_str(), value)];
            triple_source_live(
                FlagKind::Rustdoc,
                &[],
                Some(triple),
                Platform::Host(Some(triple)),
                EffectiveEnv::new(&pairs),
            )
        };
        assert_eq!(live("  "), Some(false));
        assert_eq!(live("--cfg x"), Some(true));
    }

    #[test]
    fn rustdoc_env_sources_are_appended_to_by_presence() {
        // The placement the rustdoc phase's `--check` used to make on its own:
        // an env source that is set is appended to, empty or not, and the
        // encoded form outranks the plain one across override and process.
        let compose = |pairs: &[(&str, &str)], additions: &[&str]| {
            let additions: Vec<String> = additions.iter().map(|s| (*s).to_owned()).collect();
            RustflagsEnv::for_kind(FlagKind::Rustdoc, EffectiveEnv::new(pairs))
                .compose(&additions)
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            compose(
                &[("RUSTDOCFLAGS", "--cfg docsrs")],
                &["-Zunstable-options", "--check"]
            ),
            (
                "RUSTDOCFLAGS".into(),
                "--cfg docsrs -Zunstable-options --check".into()
            )
        );
        assert_eq!(
            compose(&[("RUSTDOCFLAGS", "")], &["--cfg", "hooks"]),
            ("RUSTDOCFLAGS".into(), "--cfg hooks".into())
        );
        assert_eq!(
            compose(
                &[
                    ("RUSTDOCFLAGS", "-Dwarnings"),
                    ("CARGO_ENCODED_RUSTDOCFLAGS", "--cfg\u{1f}x")
                ],
                &["--check"]
            ),
            (
                "CARGO_ENCODED_RUSTDOCFLAGS".into(),
                "--cfg\u{1f}x\u{1f}--check".into()
            )
        );
        let env = RustflagsEnv::for_kind(
            FlagKind::Rustdoc,
            EffectiveEnv::new(&[("RUSTDOCFLAGS", "x")]),
        );
        assert_eq!(env_sink(false, &env), Some(Sink::Env));
    }

    #[test]
    fn an_argument_the_plain_split_would_break_moves_to_the_encoded_form() {
        let pairs = [("RUSTDOCFLAGS", " --cfg  docsrs ")];
        let got = RustflagsEnv::for_kind(FlagKind::Rustdoc, EffectiveEnv::new(&pairs))
            .compose(&["--check-cfg=cfg(mode, values(\"a\"))".into()])
            .unwrap();
        assert_eq!(
            got,
            Some((
                "CARGO_ENCODED_RUSTDOCFLAGS".into(),
                "--cfg\u{1f}docsrs\u{1f}--check-cfg=cfg(mode, values(\"a\"))".into()
            ))
        );
        assert!(survives_plain_split("--cfg=x"));
        assert!(!survives_plain_split(""));
        assert!(!survives_plain_split("a b"));
        assert!(!survives_plain_split("\tx"));
    }

    #[test]
    fn cfg_args_take_cfg_options_and_nothing_else() {
        let flags: Vec<String> = [
            "--cfg",
            "pbfhogg_test_hooks",
            "-C",
            "--cfg",
            "--check-cfg=cfg(x)",
            "-Dwarnings",
            "--cfg=madsim",
            "--check-cfg",
            "cfg(y)",
            "--cfg",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        // `-C --cfg` is `-C`'s operand, not a cfg; the trailing bare `--cfg`
        // is carried for rustdoc to reject, not dropped.
        assert_eq!(
            cfg_args(&flags),
            [
                "--cfg",
                "pbfhogg_test_hooks",
                "--check-cfg=cfg(x)",
                "--cfg=madsim",
                "--check-cfg",
                "cfg(y)",
                "--cfg",
            ]
        );
        assert!(cfg_args(&["-Ctarget-cpu=native".to_owned()]).is_empty());
    }

    #[test]
    fn rustdoc_config_args_name_the_rustdoc_key() {
        let flags = vec!["--cfg".to_string(), "x".to_string()];
        assert_eq!(
            config_args_for(FlagKind::Rustdoc, Sink::BuildConfig, &flags),
            ["--config", "build.rustdocflags=[\"--cfg\",\"x\"]"]
        );
        assert_eq!(
            config_args_for(FlagKind::Rustdoc, Sink::TargetConfig, &flags),
            [
                "--config",
                "target.\"cfg(all())\".rustdocflags=[\"--cfg\",\"x\"]"
            ]
        );
    }

    #[test]
    fn bare_triple_matches_only_the_host() {
        let d = doc("[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-Ctarget-cpu=native\"]\n");
        assert!(has_matching_target_rustflags(
            &d,
            Some("x86_64-unknown-linux-gnu")
        ));
        assert!(!has_matching_target_rustflags(
            &d,
            Some("aarch64-apple-darwin")
        ));
    }

    #[test]
    fn unknown_host_triple_does_not_match_a_bare_triple() {
        // Safe direction: without a host triple we cannot confirm the match, so
        // we must not claim the target layer wins.
        let d = doc("[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-x\"]\n");
        assert!(!has_matching_target_rustflags(&d, None));
    }

    #[test]
    fn target_table_without_rustflags_does_not_count() {
        // A `linker`-only table contributes no flags, so it does not make
        // source 3 win; `build.rustflags` still applies.
        let d = doc("[target.x86_64-unknown-linux-gnu]\nlinker = \"clang\"\n");
        assert!(!has_matching_target_rustflags(
            &d,
            Some("x86_64-unknown-linux-gnu")
        ));
    }

    #[test]
    fn empty_rustflags_array_does_not_count() {
        let d = doc("[target.\"cfg(all())\"]\nrustflags = []\n");
        assert!(!has_matching_target_rustflags(&d, Some("x")));
    }

    #[test]
    fn cfg_selector_is_evaluated() {
        let d = doc("[target.\"cfg(all())\"]\nrustflags = [\"-Dwarnings\"]\n");
        assert!(has_matching_target_rustflags(&d, Some("x")));

        let d = doc("[target.\"cfg(target_os = \\\"plan9\\\")\"]\nrustflags = [\"-x\"]\n");
        assert!(!has_matching_target_rustflags(&d, Some("x")));
    }

    #[test]
    fn no_target_section_at_all() {
        let d = doc("[build]\nrustflags = [\"-Dwarnings\"]\n");
        assert!(!has_matching_target_rustflags(&d, Some("x")));
    }

    #[test]
    fn config_args_quote_and_join() {
        let flags = vec!["-A".to_string(), "deprecated".to_string()];
        assert_eq!(
            config_args(Sink::TargetConfig, &flags),
            vec![
                "--config".to_string(),
                "target.\"cfg(all())\".rustflags=[\"-A\",\"deprecated\"]".to_string()
            ]
        );
        assert_eq!(
            config_args(Sink::BuildConfig, &flags),
            vec![
                "--config".to_string(),
                "build.rustflags=[\"-A\",\"deprecated\"]".to_string()
            ]
        );
    }

    #[test]
    fn config_args_empty_for_env_sinks_and_empty_flags() {
        let flags = vec!["-A".to_string()];
        assert!(config_args(Sink::Env, &flags).is_empty());
        assert!(config_args(Sink::EncodedEnv, &flags).is_empty());
        assert!(config_args(Sink::TargetConfig, &[]).is_empty());
    }

    #[test]
    fn quoting_escapes_a_string_terminator() {
        // Not a privilege boundary (it is the user's own brokkr.toml), but a
        // lint name with a quote in it must not be able to end the TOML string
        // and have the remainder parsed as config.
        // The quoting style is the writer's choice; what matters is that the
        // expression parses back to exactly the original string.
        for original in ["a\"b", "a\\b"] {
            let expr = format!("v = {}", super::toml_string(original));
            let parsed = expr.parse::<toml::Table>().unwrap();
            assert_eq!(parsed["v"].as_str(), Some(original), "{expr}");
        }
    }
}
