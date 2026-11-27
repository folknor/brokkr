//! The preflight of a launch through cargo: whether brokkr's
//! `target.TRIPLE.runner` override is sure to be the runner cargo uses, and
//! the user's own runner, resolved as cargo resolves it, for the helper to
//! run after it.
//!
//! The override is an exact-target key passed with `--config`, cargo's
//! highest config layer. `target.*.runner` is one of cargo's non-mergeable
//! keys: a higher layer's value replaces a lower one's instead of joining it.
//! So the override replaces a user's exact-target runner from a config file,
//! and cargo prefers any exact-target runner over every `cfg()` runner. The
//! user's runner, exact or `cfg()`, is then brokkr's to run, after the helper.
//! Replacing a string with a list (or the reverse) is cargo's merge error, so
//! the override takes the form of the value it replaces.
//!
//! What blocks, because the override could miss or cargo would refuse:
//!
//! - `CARGO_TARGET_TRIPLE_RUNNER`: the environment layer, whose interplay with
//!   a `--config` list brokkr does not model;
//! - `target-applies-to-host` in a config file: when false, cargo reads a host
//!   artifact's runner from `[host]` and ignores the override;
//! - a configured build target (`build.target`, `CARGO_BUILD_TARGET`): the
//!   artifact is the target's, whose runner key the preflight does not model;
//! - a runner whose layers mix a string and a list, cargo's merge error;
//! - two matching `cfg()` selectors, cargo's own error, which the override
//!   would mask; a `cfg()` selector the host facts cannot decide; and a
//!   `cfg()` runner when the build's rustflags cannot be read, since they can
//!   change which selector matches.
//!
//! The host and its cfg come from the compiler cargo would use, with the
//! build's effective rustflags (`target_platform::TargetFacts::host`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::rustflags::{self, EffectiveEnv, Platform};
use crate::target_platform::TargetFacts;

/// The outcome of the preflight.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Preflight {
    /// The override wins. `runner` is the user's runner to run after the
    /// helper (empty when none applies); `string` says the override must be
    /// written as a string, because the value it replaces is one.
    Ready {
        triple: String,
        runner: Vec<OsString>,
        string: bool,
    },
    /// It cannot be proven to win; the reason names the configuration.
    Blocked(String),
}

/// Decide for a cargo invocation run in `build_root` with the environment
/// `env` adds to brokkr's own.
pub(crate) fn preflight(build_root: &Path, env: EffectiveEnv<'_>) -> Result<Preflight, DevError> {
    let files = rustflags::read_config_files_under(build_root, env)?;
    let config_env = rustflags::CargoLaunchConfig::discover(build_root, env)?.env;
    let probe = crate::target_platform::CargoProbe::new(build_root, env, &files, &config_env);
    let triple = probe.host_triple()?;
    // The cfg probe runs only when a cfg() runner needs it: it costs a
    // compiler spawn, and its flags must be readable.
    let mut facts = || -> Result<TargetFacts, String> {
        // Cargo resolves cfg()-selected rustflags against a probe of its own
        // and probes again with them; brokkr's resolver matches them on its
        // own host facts instead, so with any it cannot claim cargo's cfg.
        if let Some(path) = files.iter().find_map(|(path, doc)| {
            let targets = doc.get("target")?.as_table()?;
            targets
                .iter()
                .any(|(selector, table)| {
                    selector.starts_with("cfg(")
                        && table.as_table().is_some_and(|t| t.contains_key("rustflags"))
                })
                .then_some(path)
        }) {
            return Err(format!(
                "{} sets rustflags under a `cfg()` selector, which brokkr cannot resolve as \
                 cargo does",
                crate::output::path(path)
            ));
        }
        let Some(flags) = rustflags::effective_rustflags(build_root, env).flags else {
            return Err("the build's rustflags, which decide whether it matches, cannot be read".into());
        };
        TargetFacts::host(&probe, &triple, &flags).map_err(|e| e.to_string())
    };
    Ok(decide(&triple, &files, env, &mut facts))
}

/// [`preflight`] on an already-read config chain, highest precedence first,
/// with `facts` probing the host's cfg on first need.
fn decide(
    triple: &str,
    files: &[(PathBuf, toml::Table)],
    env: EffectiveEnv<'_>,
    facts: &mut dyn FnMut() -> Result<TargetFacts, String>,
) -> Preflight {
    let mut host: Option<TargetFacts> = None;
    let exact_var = rustflags::runner_env_var(triple);
    if env.get_os(&exact_var).is_some() {
        return Preflight::Blocked(format!("{exact_var} is set"));
    }
    if env.get_os("CARGO_BUILD_TARGET").is_some() {
        return Preflight::Blocked("CARGO_BUILD_TARGET is set".into());
    }
    for (path, doc) in files {
        let shown = crate::output::path(path);
        if doc.contains_key("target-applies-to-host") {
            return Preflight::Blocked(format!("{shown} sets `target-applies-to-host`"));
        }
        if doc
            .get("build")
            .and_then(toml::Value::as_table)
            .is_some_and(|build| build.contains_key("target"))
        {
            return Preflight::Blocked(format!("{shown} sets `build.target`"));
        }
    }

    // An exact-target runner wins over every cfg() one; the override replaces
    // it and the helper runs it.
    let exact = runner_layers(files, triple);
    if !exact.is_empty() {
        return ready(triple, &format!("target.{triple}.runner"), &exact);
    }

    let mut matching: Vec<(&str, Vec<(&Path, &toml::Value)>)> = Vec::new();
    for (path, doc) in files {
        let Some(targets) = doc.get("target").and_then(toml::Value::as_table) else {
            continue;
        };
        for (selector, table) in targets {
            if table.as_table().and_then(|t| t.get("runner")).is_none() {
                continue;
            }
            let Some(expr) = selector.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')')) else {
                continue;
            };
            if matching.iter().any(|(s, _)| *s == selector) {
                continue;
            }
            if host.is_none() {
                match facts() {
                    Ok(f) => host = Some(f),
                    Err(why) => {
                        return Preflight::Blocked(format!(
                            "{} has a runner under `target.'{selector}'`, and the host's cfg \
                             cannot be probed: {why}",
                            crate::output::path(path)
                        ));
                    }
                }
            }
            let Some(facts) = host.as_ref() else {
                continue;
            };
            match rustflags::eval_cfg_on(expr, Platform::Target(facts)) {
                Some(false) => {}
                Some(true) => matching.push((selector, runner_layers(files, selector))),
                None => {
                    return Preflight::Blocked(format!(
                        "{} has a runner under `target.'{selector}'`, a selector brokkr \
                         cannot evaluate",
                        crate::output::path(path)
                    ));
                }
            }
        }
    }
    match matching.as_slice() {
        [] => Preflight::Ready {
            triple: triple.to_owned(),
            runner: Vec::new(),
            string: false,
        },
        [(selector, layers)] => ready(triple, &format!("target.'{selector}'.runner"), layers),
        [(first, _), (second, _), ..] => Preflight::Blocked(format!(
            "two `cfg()` runners match this host, `{first}` and `{second}`, which cargo refuses"
        )),
    }
}

/// Every config layer's `target.SELECTOR.runner`, highest precedence first.
fn runner_layers<'a>(files: &'a [(PathBuf, toml::Table)], selector: &str) -> Vec<(&'a Path, &'a toml::Value)> {
    files
        .iter()
        .filter_map(|(path, doc)| {
            let runner = doc.get("target")?.as_table()?.get(selector)?.as_table()?.get("runner")?;
            Some((path.as_path(), runner))
        })
        .collect()
}

/// A Ready outcome running the runner these layers resolve to, or Blocked
/// with the reason it cannot be resolved.
fn ready(triple: &str, key: &str, layers: &[(&Path, &toml::Value)]) -> Preflight {
    match resolve(layers) {
        Ok((runner, string)) => Preflight::Ready {
            triple: triple.to_owned(),
            runner,
            string,
        },
        Err(why) => Preflight::Blocked(format!("`{key}`: {why}")),
    }
}

/// One runner after cargo's merge: the highest layer's value replaces the
/// rest, and a string meeting a list in any two layers is cargo's merge
/// error. A string splits on ASCII whitespace. The program is made absolute
/// against the defining file's project (its directory above `.cargo`) when
/// it names a path, as cargo resolves it; a bare name stays bare, for the
/// helper to find on the launch environment's `PATH`, as cargo's spawn does.
/// The bool is whether the value is a string.
fn resolve(layers: &[(&Path, &toml::Value)]) -> Result<(Vec<OsString>, bool), String> {
    let Some(&(path, value)) = layers.first() else {
        return Err("it is not set".into());
    };
    let is_string = |v: &toml::Value| v.is_str();
    if layers.iter().any(|(_, v)| is_string(v) != is_string(value)) {
        return Err("its config layers mix a string and a list, which cargo cannot merge".into());
    }
    let words: Vec<String> = match value {
        // ASCII whitespace, as cargo splits it: a non-breaking space is part
        // of a word.
        toml::Value::String(s) => s.split_ascii_whitespace().map(str::to_owned).collect(),
        toml::Value::Array(items) => {
            let mut words = Vec::new();
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err("a list entry is not a string".into());
                };
                words.push(s.to_owned());
            }
            words
        }
        _ => return Err("it is neither a string nor a list".into()),
    };
    let Some((program, rest)) = words.split_first() else {
        return Err("it is empty".into());
    };
    let program = if program.contains('/') && !Path::new(program).is_absolute() {
        let root = path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| format!("cannot tell what `{program}` is relative to"))?;
        root.join(program).into_os_string()
    } else {
        OsString::from(program)
    };
    let mut out = vec![program];
    out.extend(rest.iter().map(OsString::from));
    Ok((out, value.is_str()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const HOST: &str = "x86_64-unknown-linux-gnu";

    fn host() -> TargetFacts {
        TargetFacts::from_parts(
            HOST,
            "target_arch=\"x86_64\"\ntarget_os=\"linux\"\ntarget_env=\"gnu\"\nunix\n\
             target_feature=\"sse2\"\n",
        )
    }

    fn file(path: &str, text: &str) -> (PathBuf, toml::Table) {
        (PathBuf::from(path), text.parse().unwrap())
    }

    fn run(files: &[(PathBuf, toml::Table)]) -> Preflight {
        decide(HOST, files, EffectiveEnv::new(&[]), &mut || Ok(host()))
    }

    fn blocked(p: &Preflight) -> &str {
        match p {
            Preflight::Blocked(why) => why,
            Preflight::Ready { .. } => "",
        }
    }

    fn ready_with(runner: &[&str], string: bool) -> Preflight {
        Preflight::Ready {
            triple: HOST.into(),
            runner: runner.iter().map(OsString::from).collect(),
            string,
        }
    }

    #[test]
    fn nothing_configured_is_ready_with_no_runner() {
        assert_eq!(run(&[]), ready_with(&[], false));
    }

    #[test]
    fn an_exact_runner_is_replaced_by_the_override_and_run_after_it() {
        // The highest layer replaces the rest: runner lists do not merge.
        let high = file("/p/.cargo/config.toml", &format!("[target.{HOST}]\nrunner = [\"wrap\", \"--project\"]\n"));
        let low = file("/home/u/.cargo/config.toml", &format!("[target.{HOST}]\nrunner = [\"tools/w\", \"--user\"]\n"));
        assert_eq!(run(&[high, low]), ready_with(&["wrap", "--project"], false));
        // A string stays a string, so the override can be written as one.
        let string = file("/p/.cargo/config.toml", &format!("[target.{HOST}]\nrunner = \"tools/qemu -L /s\"\n"));
        assert_eq!(run(&[string]), ready_with(&["/p/tools/qemu", "-L", "/s"], true));
        // It wins over a matching cfg() runner, as in cargo.
        let both = file(
            "/p/.cargo/config.toml",
            &format!("[target.{HOST}]\nrunner = [\"exact\"]\n[target.'cfg(unix)']\nrunner = [\"cfg\"]\n"),
        );
        assert_eq!(run(&[both]), ready_with(&["exact"], false));
    }

    #[test]
    fn what_defeats_the_override_blocks() {
        let var = rustflags::runner_env_var(HOST);
        let env = [(var.as_str(), "wrap")];
        assert!(blocked(&decide(HOST, &[], EffectiveEnv::new(&env), &mut || Ok(host()))).contains(&var));

        let host_cfg = file("/p/.cargo/config.toml", "target-applies-to-host = false\n");
        assert!(blocked(&run(&[host_cfg])).contains("target-applies-to-host"));

        let target = file("/p/.cargo/config.toml", "[build]\ntarget = \"aarch64-unknown-linux-gnu\"\n");
        assert!(blocked(&run(&[target])).contains("build.target"));

        let two = file(
            "/p/.cargo/config.toml",
            "[target.'cfg(unix)']\nrunner = \"a\"\n[target.'cfg(target_os = \"linux\")']\nrunner = \"b\"\n",
        );
        assert!(blocked(&run(&[two])).contains("two"));

        let mixed_high = file("/p/.cargo/config.toml", "[target.'cfg(unix)']\nrunner = \"x\"\n");
        let mixed_low = file("/h/.cargo/config.toml", "[target.'cfg(unix)']\nrunner = [\"y\"]\n");
        assert!(blocked(&run(&[mixed_high, mixed_low])).contains("mix"));

        let cfg = file("/p/.cargo/config.toml", "[target.'cfg(unix)']\nrunner = \"x\"\n");
        let unprobed = decide(HOST, &[cfg], EffectiveEnv::new(&[]), &mut || Err("rustflags unreadable".into()));
        assert!(blocked(&unprobed).contains("rustflags unreadable"));

        // No cfg() runner: the probe is never needed, so it never blocks.
        let plain = file("/p/.cargo/config.toml", "[build]\njobs = 2\n");
        assert_eq!(
            decide(HOST, &[plain], EffectiveEnv::new(&[]), &mut || Err("must not run".into())),
            ready_with(&[], false)
        );
    }

    #[test]
    fn a_cfg_runner_matches_on_the_compilers_facts() {
        // target_feature comes from rustc's report, rustflags included.
        let feature = file("/p/.cargo/config.toml", "[target.'cfg(target_feature = \"sse2\")']\nrunner = \"w\"\n");
        assert_eq!(run(&[feature]), ready_with(&["w"], true));
        let windows = file("/p/.cargo/config.toml", "[target.'cfg(windows)']\nrunner = \"wine\"\n");
        assert_eq!(run(&[windows]), ready_with(&[], false));
        // `true` and `false` are literals in cargo's grammar, not flags.
        let always = file("/p/.cargo/config.toml", "[target.'cfg(true)']\nrunner = \"always\"\n");
        assert_eq!(run(&[always]), ready_with(&["always"], true));
        let never = file("/p/.cargo/config.toml", "[target.'cfg(not(true))']\nrunner = \"never\"\n");
        assert_eq!(run(&[never]), ready_with(&[], false));
        // A malformed selector is never read as a match: it blocks.
        for bad in ["any(windows unix)", "not(windows, unix)"] {
            let text = format!("[target.'cfg({bad})']\nrunner = \"r\"\n");
            assert!(!blocked(&run(&[file("/p/.cargo/config.toml", &text)])).is_empty(), "{bad}");
        }
    }
}
