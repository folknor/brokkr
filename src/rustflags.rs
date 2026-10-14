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

use std::collections::HashSet;
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
        let paths = config_paths(build_root);
        for triple in target_triples_for_sink(build_root, &paths) {
            let key = target_rustflags_env_var(&triple);
            if std::env::var_os(&key).is_some() {
                notes.push(format!("{key} is set for target {triple}"));
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
    /// `force = true`: overrides an inherited environment value; without it
    /// the entry only fills a hole.
    pub(crate) force: bool,
}

/// The cargo config facts shared by check's direct lanes, serial shim, and
/// build concurrency decision. Paths are ordered from highest to lowest
/// precedence, and each file is parsed once for this discovery.
#[derive(Debug, Clone, Default)]
pub(crate) struct CargoLaunchConfig {
    pub(crate) runner: Option<Source>,
    pub(crate) build_target: Option<Source>,
    pub(crate) rustdoc: Option<Source>,
    pub(crate) build_dir: bool,
    pub(crate) env: Vec<ConfigEnvEntry>,
}

impl CargoLaunchConfig {
    pub(crate) fn discover(root: &Path, env: EffectiveEnv<'_>) -> Self {
        let files: Vec<(PathBuf, toml::Table)> = config_paths_under(root, env)
            .into_iter()
            .filter_map(|path| {
                let text = std::fs::read_to_string(&path).ok()?;
                let doc = text.parse::<toml::Table>().ok()?;
                Some((path, doc))
            })
            .collect();

        let host = host_triple_for(root);
        let runner_var = host.as_deref().map(runner_env_var);
        let runner = runner_var
            .as_deref()
            .and_then(|key| env.source(key))
            .or_else(|| config_runner_source(&files, host.as_deref()));
        let build_target = env
            .source("CARGO_BUILD_TARGET")
            .or_else(|| config_source(&files, "build", "target"));
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
        let config_env = files
            .iter()
            .flat_map(|(path, doc)| config_env_entries(path, doc))
            .collect();

        Self {
            runner,
            build_target,
            rustdoc,
            build_dir,
            env: config_env,
        }
    }
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

fn config_runner_source(files: &[(PathBuf, toml::Table)], host: Option<&str>) -> Option<Source> {
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
                eval_cfg_for_host(expr, host) != Some(false)
            } else {
                host == Some(selector.as_str())
            };
            applies.then(|| Source {
                origin: SourceOrigin::Config(path.clone()),
                key: format!("target.{selector}.runner"),
            })
        })
    })
}

fn config_env_entries(path: &Path, doc: &toml::Table) -> Vec<ConfigEnvEntry> {
    let Some(entries) = doc.get("env").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    // <dir>/.cargo/config.toml -> <dir>.
    let anchor = path.parent().and_then(Path::parent);
    entries
        .iter()
        .filter_map(|(key, value)| match value {
            toml::Value::String(value) => Some(ConfigEnvEntry {
                key: key.clone(),
                value: value.clone(),
                force: false,
            }),
            toml::Value::Table(table) => {
                let value = table.get("value")?.as_str()?;
                let relative = table
                    .get("relative")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false);
                let value = match (relative, anchor) {
                    (true, Some(dir)) => dir.join(value).display().to_string(),
                    _ => value.to_owned(),
                };
                Some(ConfigEnvEntry {
                    key: key.clone(),
                    value,
                    force: table
                        .get("force")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false),
                })
            }
            _ => None,
        })
        .collect()
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
/// Only the process environment is inspected, not a sweep's `env` table, and
/// that is sufficient rather than an oversight: `RUSTFLAGS` and
/// `CARGO_ENCODED_RUSTFLAGS` are reserved keys that config loading refuses in
/// both a `[[check]]` entry's `env` and a profile's `env`, unconditionally
/// (`reject_reserved_sweep_env`). No sweep can reach a compiling phase carrying
/// either, so the process env plus `sweep_sets_rustflags` is the whole set of
/// ways source 1/2 can be live.
///
/// Presence, not content, decides both env sources, because that is cargo's
/// rule: it reads `RUSTFLAGS` whenever the variable is set, and a set-but-empty
/// value is source 2 with no flags - it still shadows every config-file
/// rustflags table. Treating an empty `RUSTFLAGS` as unset would pick a
/// `--config` sink cargo never reads, and every allow injected there would be
/// silently inert.
pub fn sink(build_root: &Path, sweep_sets_rustflags: bool) -> Sink {
    let env = RustflagsEnv::process();
    if let Some(sink) = env_sink(sweep_sets_rustflags, &env) {
        return sink;
    }
    let paths = config_paths(build_root);
    let triples = target_triples_for_sink(build_root, &paths);
    // Cargo treats the target-triple environment spelling as a matching
    // `target.<triple>.rustflags` entry. Add brokkr's flags to the target
    // source when it is live, so the higher-priority source-3 rule does not
    // make an injected `build.rustflags` inert.
    if triples
        .iter()
        .any(|triple| std::env::var_os(target_rustflags_env_var(triple)).is_some())
    {
        return Sink::TargetConfig;
    }
    let host = host_triple_for(build_root);
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml::Table>() else {
            continue;
        };
        if has_matching_target_rustflags(&doc, host.as_deref()) {
            return Sink::TargetConfig;
        }
    }
    Sink::BuildConfig
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

/// Split `allow_flags` into the share that rides the environment and the share
/// that rides cargo's command line - exactly one of which is ever non-empty.
///
/// The two always travel together (the same sink decision selects both), and
/// both `check`'s test phase and `brokkr test` need the pair, so the split
/// lives here rather than being re-derived at each call site.
pub fn plumbing<'a>(
    build_root: &Path,
    sweep_sets_rustflags: bool,
    allow_flags: &'a [String],
) -> (&'a [String], Vec<String>) {
    let sink = sink(build_root, sweep_sets_rustflags);
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
        .map(|f| toml_string(f))
        .collect::<Vec<_>>()
        .join(",");
    vec!["--config".into(), format!("{key}=[{list}]")]
}

/// Minimal TOML basic-string quoting. Lint names and `-A` are plain ASCII, but
/// the value is assembled into a config expression cargo parses, so anything
/// that could end the string early is escaped rather than trusted.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Every Cargo config document cargo would read for a build at `build_root`:
/// the ancestor walk, then `$CARGO_HOME`, with each document's includes in
/// their Cargo precedence order. Callers can then inspect one flattened view
/// without silently missing a target rustflags entry stored in an include.
pub(crate) fn config_paths(build_root: &Path) -> Vec<PathBuf> {
    config_paths_under(build_root, EffectiveEnv::new(&[]))
}

/// [`config_paths`] for a cargo launched with `env` over brokkr's own
/// environment: a `CARGO_HOME` there moves the home cargo reads, so the chain
/// must follow it (relative to `build_root`, where that cargo starts) or a
/// runner or target configured in the other home goes unseen.
pub(crate) fn config_paths_under(build_root: &Path, env: EffectiveEnv<'_>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for dir in build_root.ancestors() {
        push_config(&mut out, &dir.join(".cargo"), &mut seen);
    }
    // The value cargo sees: the sweep's (even an empty one) over brokkr's own.
    // Then cargo's rule - empty means unset, so `$HOME/.cargo`, and a relative
    // value is taken against where that cargo starts.
    let home = match env.get_os("CARGO_HOME").filter(|v| !v.is_empty()) {
        Some(v) => Some(build_root.join(PathBuf::from(v))),
        None => crate::user_dirs::home().map(|h| h.join(".cargo")),
    };
    if let Some(home) = home {
        push_config(&mut out, &home, &mut seen);
    }
    out
}

/// Cargo accepts both spellings but reads only one per directory - the
/// extensionless `config` when both exist (`user_dirs::cargo_config_file`).
fn push_config(out: &mut Vec<PathBuf>, dir: &Path, seen: &mut HashSet<PathBuf>) {
    if let Some(path) = crate::user_dirs::cargo_config_file(dir) {
        push_config_with_includes(out, &path, seen);
    }
}

/// Append one Cargo config and its included documents from highest to lowest
/// precedence. Cargo loads includes left to right, then overlays the including
/// document, so the root comes first and the includes are traversed in reverse.
fn push_config_with_includes(out: &mut Vec<PathBuf>, path: &Path, seen: &mut HashSet<PathBuf>) {
    let identity = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    if !seen.insert(identity) {
        return;
    }

    let doc = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok());
    out.push(path.to_owned());
    if let Some(includes) = doc
        .as_ref()
        .and_then(|doc| doc.get("include"))
        .and_then(toml::Value::as_array)
    {
        let anchor = path
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."));
        for include in includes.iter().rev() {
            let (include_path, optional) = match include {
                toml::Value::String(path) => (Some(path.as_str()), false),
                toml::Value::Table(table) => (
                    table.get("path").and_then(toml::Value::as_str),
                    table
                        .get("optional")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false),
                ),
                _ => (None, false),
            };
            let Some(include_path) = include_path.filter(|path| path.ends_with(".toml")) else {
                continue;
            };
            let included = anchor.join(include_path);
            if optional && !included.exists() {
                continue;
            }
            push_config_with_includes(out, &included, seen);
        }
    }
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
fn target_triples_for_sink(build_root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let host = host_triple_for(build_root);
    let configured = std::env::var_os("CARGO_BUILD_TARGET")
        .and_then(|value| value.into_string().ok())
        .and_then(|value| parse_target_values(&value, host.as_deref()))
        .or_else(|| {
            paths.iter().find_map(|path| {
                let text = std::fs::read_to_string(path).ok()?;
                let doc = text.parse::<toml::Table>().ok()?;
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
fn has_matching_target_rustflags(doc: &toml::Table, triple: Option<&str>) -> bool {
    target_rustflags_match(doc, triple) == Some(true)
}

/// Three-valued form of [`has_matching_target_rustflags`]: `Some(true)` when
/// some flagged target table confidently matches the host, `Some(false)` when
/// none can, and `None` when none matched confidently but at least one could
/// not be decided (an unmodelled `cfg(...)`, or a bare triple with no host
/// triple to compare against).
fn target_rustflags_match(doc: &toml::Table, triple: Option<&str>) -> Option<bool> {
    let Some(targets) = doc.get("target").and_then(toml::Value::as_table) else {
        return Some(false);
    };
    let mut undecided = false;
    for (selector, table) in targets {
        let has_flags = table
            .as_table()
            .and_then(|t| t.get("rustflags"))
            .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()));
        if !has_flags {
            continue;
        }
        let matches = match selector
            .strip_prefix("cfg(")
            .and_then(|s| s.strip_suffix(')'))
        {
            Some(expr) => eval_cfg_for_host(expr, triple),
            None => triple.map(|t| t == selector),
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
    if sink(build_root, false) != Sink::BuildConfig {
        return false;
    }
    let triple = host_triple_for(build_root);
    config_paths(build_root).iter().any(|path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .is_some_and(|doc| target_rustflags_match(&doc, triple.as_deref()).is_none())
    })
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
    let (value, rest) = parse_expr(expr.trim(), host)?;
    rest.trim().is_empty().then_some(value)
}

/// Parse one predicate and return it with the unconsumed remainder.
fn parse_expr<'a>(s: &'a str, host: Option<&str>) -> Option<(bool, &'a str)> {
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
        return Some((cfg_key(ident, host)? == value, &after[1..]));
    }

    // Bare flag: `unix`, `windows`.
    Some((cfg_flag(ident)?, rest))
}

/// Parse a comma-separated argument list up to the closing paren, returning the
/// evaluated arguments and the remainder after the paren. An argument that
/// cannot be evaluated poisons the whole expression.
fn parse_args<'a>(mut s: &'a str, host: Option<&str>) -> Option<(Vec<bool>, &'a str)> {
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
    fn config_paths_include_cargo_documents_in_precedence_order() {
        let root = crate::test_scratch::scratch("rustflags", "cargo_config_includes");
        let cargo_dir = root.join(".cargo");
        std::fs::create_dir_all(&cargo_dir).unwrap();
        std::fs::write(
            cargo_dir.join("config.toml"),
            "include = [\"first.toml\", \"second.toml\"]\n",
        )
        .unwrap();
        std::fs::write(root.join("first.toml"), "[build]\nrustflags = []\n").unwrap();
        std::fs::write(root.join("second.toml"), "[build]\nrustflags = []\n").unwrap();

        let paths = config_paths(&root);
        assert_eq!(paths.first(), Some(&cargo_dir.join("config.toml")));
        assert_eq!(paths.get(1), Some(&root.join("second.toml")));
        assert_eq!(paths.get(2), Some(&root.join("first.toml")));
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
        assert_eq!(toml_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(toml_string("a\\b"), "\"a\\\\b\"");
    }
}
