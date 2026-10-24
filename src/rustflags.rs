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
//! root upwards, plus `$CARGO_HOME/config{.toml}`. A user-level
//! `[target.<host-triple>] rustflags` is common (linker choice, `target-cpu`),
//! and it decides the winning layer for every project on that machine.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// Cargo's two environment rustflags sources and their precedence. Keep the
/// raw values so presence, including a set-but-empty or non-Unicode value,
/// cannot be confused with absence by a caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RustflagsEnv {
    encoded: Option<std::ffi::OsString>,
    plain: Option<std::ffi::OsString>,
}

impl RustflagsEnv {
    /// Resolve the values one cargo process will inherit. Explicit overrides
    /// take precedence over brokkr's environment, just as they do for the
    /// other cargo launch settings.
    pub(crate) fn from(env: EffectiveEnv<'_>) -> Self {
        Self {
            encoded: env.get_os("CARGO_ENCODED_RUSTFLAGS"),
            plain: env.get_os("RUSTFLAGS"),
        }
    }

    pub(crate) fn process() -> Self {
        Self::from(EffectiveEnv::new(&[]))
    }

    /// The source Cargo reads, applying its documented encoded-env priority.
    fn active(&self) -> Option<(&'static str, &std::ffi::OsString)> {
        self.encoded
            .as_ref()
            .map(|value| ("CARGO_ENCODED_RUSTFLAGS", value))
            .or_else(|| self.plain.as_ref().map(|value| ("RUSTFLAGS", value)))
    }

    /// Append UTF-8 brokkr flags to the one source cargo reads. A non-Unicode
    /// inherited value cannot be safely copied through the string-based env
    /// interface used by cargo launchers, so refuse instead of falling back
    /// to the lower-priority source or silently replacing project flags.
    pub(crate) fn compose(
        &self,
        additions: &[String],
    ) -> Result<Option<(String, String)>, crate::error::DevError> {
        if additions.is_empty() {
            return Ok(None);
        }
        let empty = std::ffi::OsString::new();
        let (key, existing) = self.active().unwrap_or(("RUSTFLAGS", &empty));
        let existing = existing.to_str().ok_or_else(|| {
            crate::error::DevError::Config(format!(
                "{key} is set to a non-UTF-8 value; brokkr cannot append rustflags without changing it"
            ))
        })?;
        let value = if key == "CARGO_ENCODED_RUSTFLAGS" {
            let mut parts: Vec<String> = if existing.is_empty() {
                Vec::new()
            } else {
                existing.split('\u{1f}').map(str::to_owned).collect()
            };
            parts.extend(additions.iter().cloned());
            parts.join("\u{1f}")
        } else {
            let mut value = existing.to_owned();
            for flag in additions {
                if !value.trim().is_empty() {
                    value.push(' ');
                }
                value.push_str(flag);
            }
            value
        };
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
                for triple in target_triples_for_sink(build_root, &files, EffectiveEnv::new(&[])) {
                    let key = target_rustflags_env_var(&triple);
                    if std::env::var_os(&key).is_some() {
                        notes.push(format!("{key} is set for target {triple}"));
                    }
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
    pub(crate) source: PathBuf,
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
        target: Option<&crate::target_platform::SweepTarget>,
    ) -> Result<Self, crate::error::DevError> {
        let files = read_config_files_under(root, env)?;

        let host = host_triple_for(root);
        let runner_var = host.as_deref().map(runner_env_var);
        let runner = runner_var
            .as_deref()
            .and_then(|key| env.source(key))
            .or_else(|| config_runner_source(&files, Platform::Host(host.as_deref())));
        let target_runner = target.and_then(|t| {
            env.source(&runner_env_var(&t.triple))
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
        let mut config_env = Vec::new();
        for (path, doc) in &files {
            config_env.extend(config_env_entries(path, doc)?);
        }

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
pub(crate) fn read_config_files_under(
    build_root: &Path,
    env: EffectiveEnv<'_>,
) -> Result<Vec<(PathBuf, toml::Table)>, crate::error::DevError> {
    let mut roots = Vec::new();
    let mut seen_roots = HashSet::new();
    for dir in build_root.ancestors() {
        push_existing_config(&mut roots, &dir.join(".cargo"), &mut seen_roots)?;
    }
    // Cargo treats an empty CARGO_HOME as unset and resolves a relative value
    // against the cargo process's cwd, which is build_root here.
    let home = match env.get_os("CARGO_HOME").filter(|value| !value.is_empty()) {
        Some(value) => Some(build_root.join(PathBuf::from(value))),
        None => crate::user_dirs::home().map(|home| home.join(".cargo")),
    };
    if let Some(home) = home {
        push_existing_config(&mut roots, &home, &mut seen_roots)?;
    }

    let mut files = Vec::new();
    let mut loaded = HashSet::new();
    let mut active = HashSet::new();
    for path in roots {
        push_config_document(&path, false, &mut files, &mut loaded, &mut active)?;
    }
    Ok(files)
}

/// Read a cargo config file passed explicitly through `--config`, including
/// every config file it includes.
pub(crate) fn read_config_file(
    path: &Path,
) -> Result<Vec<(PathBuf, toml::Table)>, crate::error::DevError> {
    let mut files = Vec::new();
    let mut loaded = HashSet::new();
    let mut active = HashSet::new();
    push_config_document(path, false, &mut files, &mut loaded, &mut active)?;
    Ok(files)
}

/// The config file cargo reads in one `.cargo`-shaped directory: the
/// extensionless legacy `config` when it exists, else `config.toml`.
pub(crate) fn selected_config_file(dir: &Path) -> Result<Option<PathBuf>, crate::error::DevError> {
    for path in [dir.join("config"), dir.join("config.toml")] {
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => return Ok(Some(path)),
            Ok(_) => {}
            // No file at one of Cargo's search locations is ordinary; an
            // inaccessible location could hide the selected legacy file and
            // must not be treated as that ordinary absence.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(config_read_error(&path, &error)),
        }
    }
    Ok(None)
}

fn push_existing_config(
    out: &mut Vec<PathBuf>,
    dir: &Path,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), crate::error::DevError> {
    if let Some(path) = selected_config_file(dir)? {
        let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.insert(identity) {
            out.push(path);
        }
    }
    Ok(())
}

fn push_config_document(
    path: &Path,
    optional: bool,
    out: &mut Vec<(PathBuf, toml::Table)>,
    loaded: &mut HashSet<PathBuf>,
    active: &mut HashSet<PathBuf>,
) -> Result<(), crate::error::DevError> {
    let identity = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    if active.contains(&identity) {
        return Err(crate::error::DevError::Config(format!(
            "Cargo config include cycle reaches {}",
            path.display()
        )));
    }
    // A file can be found both in the ancestor chain and through an include.
    // Cargo config discovery treats that path as one document in this view.
    if !loaded.insert(identity.clone()) {
        return Ok(());
    }

    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // Cargo's optional include contract permits a missing file only.
        // Permission, decoding, and every other read error leave the config
        // chain incomplete and refuse reconstruction.
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => {
            loaded.remove(&identity);
            return Ok(());
        }
        Err(error) => return Err(config_read_error(path, &error)),
    };
    let doc = text.parse::<toml::Table>().map_err(|error| {
        crate::error::DevError::Config(format!(
            "could not parse Cargo config {}: {error}",
            path.display()
        ))
    })?;

    active.insert(identity.clone());
    out.push((path.to_owned(), doc.clone()));
    let includes = match doc.get("include") {
        None => &[][..],
        Some(toml::Value::Array(includes)) => includes.as_slice(),
        Some(_) => {
            return Err(crate::error::DevError::Config(format!(
                "Cargo config {} has a non-array `include`; brokkr cannot resolve its launch settings",
                path.display()
            )));
        }
    };
    // Later includes override earlier ones, so visit them first while keeping
    // this high-precedence file at the front of the resulting snapshot.
    let anchor = path.parent().unwrap_or_else(|| Path::new("."));
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
                                path.display()
                            ))
                        })?;
                let optional = match table.get("optional") {
                    None => false,
                    Some(toml::Value::Boolean(value)) => *value,
                    Some(_) => {
                        return Err(crate::error::DevError::Config(format!(
                            "Cargo config {} has a non-boolean include `optional` value",
                            path.display()
                        )));
                    }
                };
                (include_path, optional)
            }
            _ => {
                return Err(crate::error::DevError::Config(format!(
                    "Cargo config {} has an invalid include entry",
                    path.display()
                )));
            }
        };
        if !include_path.ends_with(".toml") {
            return Err(crate::error::DevError::Config(format!(
                "Cargo config {} includes `{include_path}`, which does not end in `.toml`",
                path.display()
            )));
        }
        push_config_document(&anchor.join(include_path), optional, out, loaded, active)?;
    }
    active.remove(&identity);
    Ok(())
}

fn config_read_error(path: &Path, error: &std::io::Error) -> crate::error::DevError {
    crate::error::DevError::Config(format!(
        "could not read Cargo config {}: {error}",
        path.display()
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

fn config_runner_source(files: &[(PathBuf, toml::Table)], platform: Platform<'_>) -> Option<Source> {
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

fn config_env_entries(
    path: &Path,
    doc: &toml::Table,
) -> Result<Vec<ConfigEnvEntry>, crate::error::DevError> {
    let Some(value) = doc.get("env") else {
        return Ok(Vec::new());
    };
    let entries = value.as_table().ok_or_else(|| {
        crate::error::DevError::Config(format!(
            "Cargo config {} has a non-table `env`; brokkr cannot reconstruct its launch environment",
            path.display()
        ))
    })?;
    // <dir>/.cargo/config.toml -> <dir>.
    let anchor = path.parent().and_then(Path::parent);
    let mut out = Vec::new();
    for (key, value) in entries {
        let (value, force, relative) = match value {
            toml::Value::String(value) => (value.clone(), false, false),
            toml::Value::Table(table) => {
                let value = table
                    .get("value")
                    .and_then(toml::Value::as_str)
                    .ok_or_else(|| {
                        crate::error::DevError::Config(format!(
                            "Cargo config {} has an `[env]` table for `{key}` without a string `value`",
                            path.display()
                        ))
                    })?;
                let force = match table.get("force") {
                    None => false,
                    Some(toml::Value::Boolean(value)) => *value,
                    Some(_) => {
                        return Err(crate::error::DevError::Config(format!(
                            "Cargo config {} has a non-boolean `[env]` force value for `{key}`",
                            path.display()
                        )));
                    }
                };
                let relative = match table.get("relative") {
                    None => false,
                    Some(toml::Value::Boolean(value)) => *value,
                    Some(_) => {
                        return Err(crate::error::DevError::Config(format!(
                            "Cargo config {} has a non-boolean `[env]` relative value for `{key}`",
                            path.display()
                        )));
                    }
                };
                (value.to_owned(), force, relative)
            }
            _ => {
                return Err(crate::error::DevError::Config(format!(
                    "Cargo config {} has an unsupported `[env]` value for `{key}`",
                    path.display()
                )));
            }
        };
        let value = match (relative, anchor) {
            (true, Some(dir)) => dir.join(value).display().to_string(),
            _ => value,
        };
        out.push(ConfigEnvEntry {
            key: key.clone(),
            value,
            source: path.to_owned(),
            force,
        });
    }
    Ok(out)
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

/// Decide the sink for a cargo invocation rooted at `build_root`.
///
/// `sweep_sets_rustflags` is brokkr's own doing: a `[[check]]` entry carrying
/// `rustflags` is exported as an env var by `sweep_runtime_env`, which makes
/// source 1/2 live for that sweep regardless of what the config chain says.
///
/// For sources 1 and 2 only the process environment is inspected, not a
/// sweep's `env` table, and that is sufficient rather than an oversight:
/// `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` are reserved keys that config
/// loading refuses in both a `[[check]]` entry's `env` and a profile's `env`,
/// unconditionally (`reject_reserved_sweep_env`). No sweep can reach a
/// compiling phase carrying either, so the process env plus
/// `sweep_sets_rustflags` is the whole set of ways source 1/2 can be live.
/// Sources 3 and 4 are read under `cargo_env`, which a sweep's `env` can move.
///
/// Presence, not content, decides both env sources, because that is cargo's
/// rule: it reads `RUSTFLAGS` whenever the variable is set, and a set-but-empty
/// value is source 2 with no flags - it still shadows every config-file
/// rustflags table. Treating an empty `RUSTFLAGS` as unset would pick a
/// `--config` sink cargo never reads, and every allow injected there would be
/// silently inert.
///
/// `target` is a `[[check]]` sweep's `target`: cargo then builds for that
/// triple whatever `build.target` says, so its tables are the ones matched.
/// `cargo_env` is the environment the build runs under (a sweep's `env` over
/// brokkr's): it selects the config chain (`CARGO_HOME`) and carries the
/// target-table env spellings.
pub(crate) fn sink(
    build_root: &Path,
    sweep_sets_rustflags: bool,
    target: Option<&crate::target_platform::SweepTarget>,
    cargo_env: EffectiveEnv<'_>,
) -> Sink {
    let env = RustflagsEnv::process();
    if let Some(sink) = env_sink(sweep_sets_rustflags, &env) {
        return sink;
    }
    let files = match read_config_files_under(build_root, cargo_env) {
        Ok(files) => files,
        // BuildConfig is the non-destructive fallback: an unseen target table
        // may leave brokkr's allows inert, while a speculative TargetConfig
        // could demote the project's build flags. Cargo itself rejects a
        // selected unreadable or malformed config before compiling.
        Err(_) => return Sink::BuildConfig,
    };
    let triples = match target {
        Some(t) => vec![t.triple.clone()],
        None => target_triples_for_sink(build_root, &files, cargo_env),
    };
    // Cargo treats the target-triple environment spelling as a matching
    // `target.<triple>.rustflags` entry. Add brokkr's flags to the target
    // source when it is live, so the higher-priority source-3 rule does not
    // make an injected `build.rustflags` inert.
    if triples
        .iter()
        .any(|triple| cargo_env.contains(&target_rustflags_env_var(triple)))
    {
        return Sink::TargetConfig;
    }
    let host = host_triple_for(build_root);
    let platform = match target {
        Some(t) => Platform::Target(t),
        None => Platform::Host(host.as_deref()),
    };
    if target_source_live(&files, &triples, platform) {
        Sink::TargetConfig
    } else {
        Sink::BuildConfig
    }
}

/// Whether source 3 is provably live: some selector known to match - the
/// triple key, or a `cfg(...)` that evaluates true - has non-empty flags once
/// merged across the chain as cargo merges them (a string replaces lower
/// layers, an array appends). A selector whose `cfg` cannot be evaluated
/// proves nothing either way and is passed over - the inert direction - so
/// an undecided table cannot cancel a proven one, and a lower layer's flags
/// a higher layer replaced cannot prove anything.
fn target_source_live(
    files: &[(PathBuf, toml::Table)],
    triples: &[String],
    platform: Platform<'_>,
) -> bool {
    let flags_of = |selector: &str| {
        let selector = selector.to_owned();
        merged_config_flags(files, move |doc: &toml::Table| {
            doc.get("target")?.as_table()?.get(&selector)?.as_table()?.get("rustflags").cloned()
        })
    };
    let non_empty = |merged: Option<Option<Vec<String>>>| matches!(merged, Some(Some(flags)) if !flags.is_empty());
    let cfg_selectors: BTreeSet<&str> = files
        .iter()
        .filter_map(|(_, doc)| doc.get("target")?.as_table())
        .flat_map(|targets| targets.keys().map(String::as_str))
        .filter(|selector| selector.starts_with("cfg("))
        .collect();
    let cfg_live = cfg_selectors.into_iter().any(|selector| {
        let matches = selector
            .strip_prefix("cfg(")
            .and_then(|s| s.strip_suffix(')'))
            .and_then(|expr| eval_cfg_on(expr, platform));
        matches == Some(true) && non_empty(flags_of(selector))
    });
    cfg_live || triples.iter().any(|triple| non_empty(flags_of(triple)))
}

/// Resolve the environment-backed sink independently from process and config
/// discovery so precedence and empty-value behavior can be tested directly.
fn env_sink(sweep_sets_rustflags: bool, env: &RustflagsEnv) -> Option<Sink> {
    match env.active().map(|(key, _)| key) {
        Some("CARGO_ENCODED_RUSTFLAGS") => Some(Sink::EncodedEnv),
        Some("RUSTFLAGS") => Some(Sink::Env),
        None if sweep_sets_rustflags => Some(Sink::Env),
        None => None,
        Some(_) => unreachable!("RustflagsEnv::active only returns known keys"),
    }
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
            value.split('\u{1f}').filter(|f| !f.is_empty()).map(str::to_owned).collect()
        } else {
            value.split_whitespace().map(str::to_owned).collect()
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
    let triples = target_triples_for_sink(build_root, &files, env);
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
            doc.get("target")?.as_table()?.get(&selector)?.as_table()?.get("rustflags").cloned()
        }
    };
    let mut flags: Vec<String> = merged_config_flags(files, target_key(triple))?.unwrap_or_default();
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
        let expr = selector.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')'))?;
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
/// string replacing it. `None` is undecided (a value of another shape);
/// `Some(None)` is a key no file sets.
fn merged_config_flags(
    files: &[(PathBuf, toml::Table)],
    get: impl Fn(&toml::Table) -> Option<toml::Value>,
) -> Option<Option<Vec<String>>> {
    let mut merged: Option<Vec<String>> = None;
    for (_, doc) in files.iter().rev() {
        let Some(value) = get(doc) else {
            continue;
        };
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
    target: Option<&crate::target_platform::SweepTarget>,
    cargo_env: EffectiveEnv<'_>,
    allow_flags: &'a [String],
) -> (&'a [String], Vec<String>) {
    let sink = sink(build_root, sweep_sets_rustflags, target, cargo_env);
    if sink.is_env() {
        (allow_flags, Vec::new())
    } else {
        (&[], config_args(sink, allow_flags))
    }
}

/// The `--config` arguments for `flags`, or empty for an env sink (where the
/// caller appends to the env value instead).
pub fn config_args(sink: Sink, flags: &[String]) -> Vec<String> {
    if flags.is_empty() || sink.is_env() {
        return Vec::new();
    }
    let key = match sink {
        Sink::TargetConfig => "target.\"cfg(all())\".rustflags",
        _ => "build.rustflags",
    };
    let list = flags
        .iter()
        .map(|f| crate::piners::toml_writer::toml_string(f))
        .collect::<Vec<_>>()
        .join(",");
    vec!["--config".into(), format!("{key}=[{list}]")]
}

fn target_rustflags_env_var(triple: &str) -> String {
    format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        triple.to_uppercase().replace('-', "_")
    )
}

/// Cargo's configured target names drive `CARGO_TARGET_<triple>_RUSTFLAGS`.
/// Respect `CARGO_BUILD_TARGET` or `build.target`; if neither is present,
/// Cargo builds for the host.
fn target_triples_for_sink(
    build_root: &Path,
    files: &[(PathBuf, toml::Table)],
    cargo_env: EffectiveEnv<'_>,
) -> Vec<String> {
    let host = host_triple_for(build_root);
    let configured = cargo_env
        .get_os("CARGO_BUILD_TARGET")
        .and_then(|value| value.into_string().ok())
        .and_then(|value| parse_target_values(&value, host.as_deref()))
        .or_else(|| {
            files.iter().find_map(|(_, doc)| {
                let value = doc.get("build")?.as_table()?.get("target")?;
                match value {
                    toml::Value::String(value) => parse_target_values(value, host.as_deref()),
                    toml::Value::Array(values) => {
                        let targets: Vec<String> = values
                            .iter()
                            .filter_map(toml::Value::as_str)
                            .filter_map(|value| target_key_triple(value, host.as_deref()))
                            .collect();
                        (!targets.is_empty()).then_some(targets)
                    }
                    _ => None,
                }
            })
        });

    configured.unwrap_or_else(|| host.into_iter().collect())
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

/// Does this config document contribute a `target.*.rustflags` that applies to
/// the host?
///
/// `triple` is `None` when the host triple could not be read, in which case a
/// bare `[target.<triple>]` table cannot be confirmed to match and is treated
/// as not matching - the inert direction.
#[cfg(test)]
fn has_matching_target_rustflags(doc: &toml::Table, triple: Option<&str>) -> bool {
    target_rustflags_match(doc, Platform::Host(triple)) == Some(true)
}

/// Whether some flagged `target.*` table applies, three-valued: `Some(true)` when
/// some flagged target table confidently matches the platform, `Some(false)`
/// when none can, and `None` when none matched confidently but at least one
/// could not be decided (an unmodelled `cfg(...)`, or a bare triple with no
/// host triple to compare against).
fn target_rustflags_match(doc: &toml::Table, platform: Platform<'_>) -> Option<bool> {
    let Some(targets) = doc.get("target").and_then(toml::Value::as_table) else {
        return Some(false);
    };
    let mut undecided = false;
    for (selector, table) in targets {
        let has_flags = table
            .as_table()
            .and_then(|t| t.get("rustflags"))
            .is_some_and(|v| {
                v.as_array().is_some_and(|a| !a.is_empty())
                    || v.as_str().is_some_and(|s| !s.trim().is_empty())
            });
        if !has_flags {
            continue;
        }
        let matches = match selector
            .strip_prefix("cfg(")
            .and_then(|s| s.strip_suffix(')'))
        {
            Some(expr) => eval_cfg_on(expr, platform),
            None => platform.triple().map(|t| t == selector),
        };
        match matches {
            Some(true) => return Some(true),
            Some(false) => {}
            None => undecided = true,
        }
    }
    if undecided { None } else { Some(false) }
}

/// Whether [`sink`]'s answer for a plain sweep (one exporting no `rustflags`)
/// might be inert: it fell through to `build.rustflags`, but some flagged
/// `target.*` table in the chain could not be decided. If that table does
/// match the host, cargo reads it instead and the injected flags do nothing.
/// The fall-through is the deliberate safe direction; this is how the caller
/// finds out it was taken on a guess.
pub fn sink_may_be_inert(build_root: &Path) -> bool {
    if sink(build_root, false, None, EffectiveEnv::new(&[])) != Sink::BuildConfig {
        return false;
    }
    let triple = host_triple_for(build_root);
    match read_config_files_under(build_root, EffectiveEnv::new(&[])) {
        Ok(files) => files.iter().any(|(_, doc)| {
            target_rustflags_match(doc, Platform::Host(triple.as_deref())).is_none()
        }),
        // The notice cannot establish that its chosen layer is live when part
        // of the config chain was unreadable, so report the uncertainty.
        Err(_) => true,
    }
}

/// The rustc host selected from the current working directory. A failure here
/// degrades the bare-triple check to "no match", never to a wrong match.
pub(crate) fn host_triple() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|root| host_triple_for(&root))
}

fn host_triple_for(root: &Path) -> Option<String> {
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
    Target(&'a crate::target_platform::SweepTarget),
}

impl<'a> Platform<'a> {
    /// The triple a bare `[target.<triple>]` selector must equal.
    fn triple(self) -> Option<&'a str> {
        match self {
            Self::Host(host) => host,
            Self::Target(target) => Some(&target.triple),
        }
    }

    /// Whether `key = "value"` holds, or `None` when unmodelled.
    fn key_matches(self, key: &str, value: &str) -> Option<bool> {
        match self {
            Self::Host(host) => Some(cfg_key(key, host)? == value),
            Self::Target(target) => target.cfg.key_matches(key, value),
        }
    }

    /// Whether bare `name` holds, or `None` when unmodelled.
    fn flag(self, name: &str) -> Option<bool> {
        match self {
            Self::Host(_) => cfg_flag(name),
            Self::Target(target) => target.cfg.flag(name),
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
            "not" => !args.first()?,
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
        if let Some(next) = s.strip_prefix(',') {
            s = next;
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
            encoded: Some("".into()),
            plain: Some("-D warnings".into()),
        };
        assert_eq!(env_sink(false, &encoded_empty), Some(Sink::EncodedEnv));

        let plain_empty = RustflagsEnv {
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
        let triples = vec!["x86_64-unknown-linux-gnu".to_owned()];
        let live = |files: &[(PathBuf, toml::Table)]| target_source_live(files, &triples, host);

        // A string value counts, not only a non-empty array.
        assert!(live(&[doc("[target.'cfg(all())']\nrustflags = \"-Dwarnings\"\n")]));
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
            doc(
                "[build]\nrustflags = [\"-Ctarget-cpu=native\"]\n\
                 [target.'cfg(all())']\nrustflags = \"\"\n\
                 [target.'cfg(all(windows, target_has_atomic = \"128\"))']\nrustflags = [\"-Cx\"]\n"
            ),
            doc("[target.'cfg(all())']\nrustflags = [\"-Dwarnings\"]\n"),
        ]));
        // The triple key counts like a matching cfg.
        assert!(live(&[doc("[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-Cx\"]\n")]));
    }

    #[test]
    fn config_snapshot_includes_cargo_documents_in_precedence_order() {
        let root = crate::test_scratch::scratch("rustflags", "cargo_config_includes");
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

    #[cfg(unix)]
    #[test]
    fn non_unicode_encoded_rustflags_refuses_instead_of_falling_through() {
        use std::os::unix::ffi::OsStringExt;

        let env = RustflagsEnv {
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
        std::fs::write(root.join(".cargo/config.toml"), "[build]\nrustflags = [\"-Cbuild\"]\n").unwrap();
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
        let files = chain(&["[build]\nrustflags = [\"-Chigh\"]\n", "[build]\nrustflags = [\"-Clow\"]\n"]);
        assert_eq!(build_table_flags(&files, env), Some(Some(vec!["-Clow".into(), "-Chigh".into()])));
        assert_eq!(target_table_flags(&files, triple, host, env), Some(None));
        // ...while a string replaces whatever lies below it.
        let files = chain(&[
            "[build]\nrustflags = \"-Ctarget-cpu=x86-64\"\n",
            "[build]\nrustflags = [\"-Ctarget-cpu=native\"]\n",
        ]);
        assert_eq!(build_table_flags(&files, env), Some(Some(vec!["-Ctarget-cpu=x86-64".into()])));
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
            Some(Some(vec!["-Ctriple-low".into(), "-Ctriple-high".into(), "-Ccfg".into()]))
        );
        // An empty target result falls through to `build.rustflags`.
        let files = chain(&[
            "[target.x86_64-unknown-linux-gnu]\nrustflags = []\n[build]\nrustflags = [\"-Cbuild\"]\n",
        ]);
        assert_eq!(target_table_flags(&files, triple, host, env), Some(None));
        assert_eq!(build_table_flags(&files, env), Some(Some(vec!["-Cbuild".into()])));
        // A flagged table this evaluator cannot judge is an admission, not a guess.
        let files = chain(&["[target.'cfg(target_has_atomic = \"128\")']\nrustflags = [\"-Cx\"]\n"]);
        assert_eq!(target_table_flags(&files, "x86_64-unknown-linux-gnu", host, env), None);
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
            let expr = format!("v = {}", crate::piners::toml_writer::toml_string(original));
            let parsed = expr.parse::<toml::Table>().unwrap();
            assert_eq!(parsed["v"].as_str(), Some(original), "{expr}");
        }
    }
}
