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

use std::path::{Path, PathBuf};

/// The one place brokkr can add flags for this invocation without displacing
/// the project's own. Determined by [`sink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// Source 1 is live: append to `CARGO_ENCODED_RUSTFLAGS` (0x1f-separated).
    EncodedEnv,
    /// Source 2 is live: append to `RUSTFLAGS` (space-separated).
    Env,
    /// Source 3 is live: `--config target."cfg(all())".rustflags=[..]`, which
    /// joins the matching target entries and lands last.
    TargetConfig,
    /// Source 4 (or nothing at all): `--config build.rustflags=[..]`.
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
pub fn sink(build_root: &Path, sweep_sets_rustflags: bool) -> Sink {
    if std::env::var_os("CARGO_ENCODED_RUSTFLAGS").is_some() {
        return Sink::EncodedEnv;
    }
    if sweep_sets_rustflags
        || std::env::var("RUSTFLAGS").is_ok_and(|v| !v.trim().is_empty())
    {
        return Sink::Env;
    }
    let triple = host_triple();
    for path in config_paths(build_root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml::Table>() else {
            continue;
        };
        if has_matching_target_rustflags(&doc, triple.as_deref()) {
            return Sink::TargetConfig;
        }
    }
    Sink::BuildConfig
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

/// Every `.cargo/config{.toml}` cargo would read for a build at `build_root`:
/// the ancestor walk, then `$CARGO_HOME`. Order does not matter here - the
/// question is only whether *any* of them contributes a matching target entry.
fn config_paths(build_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in build_root.ancestors() {
        push_config(&mut out, &dir.join(".cargo"));
    }
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    if let Some(home) = home {
        push_config(&mut out, &home);
    }
    out
}

/// Cargo accepts both spellings and prefers `config.toml`; only the first found
/// in a directory is read.
fn push_config(out: &mut Vec<PathBuf>, dir: &Path) {
    for name in ["config.toml", "config"] {
        let path = dir.join(name);
        if path.exists() {
            out.push(path);
            return;
        }
    }
}

/// Does this config document contribute a `target.*.rustflags` that applies to
/// the host?
///
/// `triple` is `None` when the host triple could not be read, in which case a
/// bare `[target.<triple>]` table cannot be confirmed to match and is treated
/// as not matching - the inert direction.
fn has_matching_target_rustflags(doc: &toml::Table, triple: Option<&str>) -> bool {
    let Some(targets) = doc.get("target").and_then(toml::Value::as_table) else {
        return false;
    };
    for (selector, table) in targets {
        let has_flags = table
            .as_table()
            .and_then(|t| t.get("rustflags"))
            .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()));
        if !has_flags {
            continue;
        }
        let matches = match selector.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')')) {
            Some(expr) => eval_cfg(expr) == Some(true),
            None => triple.is_some_and(|t| t == selector),
        };
        if matches {
            return true;
        }
    }
    false
}

/// `rustc -vV`'s host triple. Read once per process; a failure here degrades
/// the bare-triple check to "no match", never to a wrong match.
fn host_triple() -> Option<String> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let out = std::process::Command::new("rustc").arg("-vV").output().ok()?;
            if !out.status.success() {
                return None;
            }
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|l| l.strip_prefix("host: "))
                .map(|h| h.trim().to_owned())
        })
        .clone()
}

/// Evaluate a cargo `cfg(...)` selector body against the host.
///
/// `Some(true)` / `Some(false)` are confident answers; `None` means the
/// expression uses something this evaluator does not model, and the caller must
/// take the non-destructive branch. brokkr runs on the host it builds for, so
/// the host's values are brokkr's own `cfg!` values - no triple parsing beyond
/// the `target_env` / `target_vendor` fields, which the consts do not expose.
fn eval_cfg(expr: &str) -> Option<bool> {
    let (value, rest) = parse_expr(expr.trim())?;
    rest.trim().is_empty().then_some(value)
}

/// Parse one predicate and return it with the unconsumed remainder.
fn parse_expr(s: &str) -> Option<(bool, &str)> {
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
        let (args, after) = parse_args(inner)?;
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
        return Some((cfg_key(ident)? == value, &after[1..]));
    }

    // Bare flag: `unix`, `windows`.
    Some((cfg_flag(ident)?, rest))
}

/// Parse a comma-separated argument list up to the closing paren, returning the
/// evaluated arguments and the remainder after the paren. An argument that
/// cannot be evaluated poisons the whole expression.
fn parse_args(mut s: &str) -> Option<(Vec<bool>, &str)> {
    let mut args = Vec::new();
    loop {
        s = s.trim_start();
        if let Some(after) = s.strip_prefix(')') {
            return Some((args, after));
        }
        let (value, rest) = parse_expr(s)?;
        args.push(value);
        s = rest.trim_start();
        if let Some(next) = s.strip_prefix(',') {
            s = next;
        }
    }
}

/// The host's value for a `key = "value"` cfg key, or `None` if unmodelled.
fn cfg_key(key: &str) -> Option<String> {
    let v = match key {
        "target_os" => std::env::consts::OS.to_string(),
        "target_arch" => std::env::consts::ARCH.to_string(),
        "target_family" => std::env::consts::FAMILY.to_string(),
        "target_pointer_width" => usize::BITS.to_string(),
        "target_endian" => {
            if cfg!(target_endian = "little") { "little".into() } else { "big".into() }
        }
        "target_env" => triple_field(2)?,
        "target_vendor" => triple_field(1)?,
        _ => return None,
    };
    Some(v)
}

/// A dash-separated field of the host triple (`arch-vendor-os[-env]`), used for
/// the two cfg keys `std::env::consts` does not carry. A triple short of the
/// requested field yields the empty string, which is the correct answer for
/// `target_env` on e.g. `x86_64-apple-darwin`.
fn triple_field(index: usize) -> Option<String> {
    let triple = host_triple()?;
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
        assert_eq!(eval_cfg("target_os = \"definitely-not-an-os\""), Some(false));
    }

    #[test]
    fn cfg_nested_combinators() {
        let os = std::env::consts::OS;
        assert_eq!(
            eval_cfg(&format!("all(not(target_os = \"nope\"), any(target_os = \"{os}\"))")),
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
