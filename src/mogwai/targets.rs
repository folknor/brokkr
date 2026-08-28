//! The `[mogwai.targets.*]` registry: a name -> a cargo example plus features.

use crate::build::BuildConfig;
use crate::config::{DevConfig, MogwaiConfig};
use crate::error::DevError;

/// The `[mogwai]` section, or a message saying how to add it.
pub(crate) fn config(dev_config: &DevConfig) -> Result<&MogwaiConfig, DevError> {
    dev_config.mogwai.as_ref().ok_or_else(|| {
        DevError::Config(
            "no [mogwai] section in brokkr.toml\n  add:\n\n  [mogwai]\n  \
             package = \"mogwai-cli\"\n  bin = \"mogwai\""
                .to_owned(),
        )
    })
}

/// What to build for this invocation.
#[derive(Debug)]
pub(crate) struct Resolved {
    /// The name rows are filed under: the target name, or the bin name for a
    /// CLI invocation.
    pub(crate) name: String,
    pub(crate) build: BuildConfig,
}

/// Resolve an invocation to a build spec.
///
/// `None` is the CLI surface - the registered bin, which needs no entry of its
/// own. A name resolves against `[mogwai.targets.*]`.
///
/// Features come from the registration rather than the call site because
/// `--hotpath` and `--alloc` are inert without the feature that compiles the
/// instrumentation in. A target that has to be *remembered* to be built with
/// `--features hotpath` is a target that records profile-less rows, which is
/// what the predecessor did.
pub(crate) fn resolve(
    cfg: &MogwaiConfig,
    target: Option<&str>,
    extra_features: &[String],
) -> Result<Resolved, DevError> {
    let Some(target) = target else {
        let name = cfg.bin_target().ok_or_else(|| {
            DevError::Config(
                "[mogwai] names neither `package` nor `bin`, so there is no \
                 CLI to run.\n  add `package = \"...\"` (and `bin` if the \
                 binary is named differently)."
                    .to_owned(),
            )
        })?;
        return Ok(Resolved {
            name: name.to_owned(),
            build: BuildConfig {
                package: cfg.package.clone(),
                bin: Some(name.to_owned()),
                example: None,
                features: extra_features.to_vec(),
                default_features: true,
                profile: "release",
            },
        });
    };

    let entry = cfg.target(target)?;
    // Registered features first, mode and call-site ones appended: the
    // registration is what makes the target buildable at all, so an addition
    // adds an arm rather than replacing the shape.
    //
    // Deduped because the overlap is the common case, not the exception - a
    // target registers `hotpath` and `--hotpath` asks for it again. Cargo
    // tolerates a repeat, but the recorded feature string is read by humans
    // comparing two rows, and `hotpath,hotpath` reads as a mistake.
    let mut features = entry.features.clone();
    for feature in extra_features {
        if !features.contains(feature) {
            features.push(feature.clone());
        }
    }

    Ok(Resolved {
        name: target.to_owned(),
        build: BuildConfig {
            package: entry.package.clone().or_else(|| cfg.package.clone()),
            bin: None,
            example: Some(entry.example.clone()),
            features,
            default_features: true,
            profile: "release",
        },
    })
}

/// Render the registry as the bare `brokkr mogwai` index.
///
/// Lists the harness targets and names the CLI form, because the CLI surface is
/// not in the registry and would otherwise look unavailable.
pub(crate) fn format_index(cfg: &MogwaiConfig) -> String {
    let mut out = String::new();

    match cfg.bin_target() {
        Some(bin) => out.push_str(&format!(
            "  CLI      brokkr mogwai -- <args>   ({bin})\n"
        )),
        None => out.push_str("  CLI      unavailable - [mogwai] names no package or bin\n"),
    }

    if cfg.targets.is_empty() {
        out.push_str("  targets  none registered - add [mogwai.targets.<name>]\n");
        return out;
    }

    out.push_str("  targets\n");
    let width = cfg.targets.keys().map(String::len).max().unwrap_or(0);
    for (name, target) in &cfg.targets {
        let pkg = target.package.as_deref().or(cfg.package.as_deref());
        let coords = match pkg {
            Some(pkg) => format!("{pkg}/{}", target.example),
            None => target.example.clone(),
        };
        let features = if target.features.is_empty() {
            String::new()
        } else {
            format!("  [{}]", target.features.join(", "))
        };
        out.push_str(&format!("    {name:<width$}  {coords}{features}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    const BASE: &str = "project = \"mogwai\"\n\n[mogwai]\npackage = \"mogwai-cli\"\n\
                        bin = \"mogwai\"\n\n";

    /// A fresh scratch dir for one test. `test_name` must be unique within
    /// this module - see `crate::test_scratch`.
    fn tmpdir(test_name: &str) -> PathBuf {
        crate::test_scratch::scratch("mogwai-targets", test_name)
    }

    /// Build a `DevConfig` from `toml_src`, with `brokkr.toml` written into
    /// the caller's OWN scratch dir.
    ///
    /// Takes the dir rather than allocating one: a helper that allocated its
    /// own would have to name it, and any fixed name is shared by every
    /// caller. Writing into the caller's root also matches how the config is
    /// really found - `project_root` and the directory holding `brokkr.toml`
    /// are the same place in every non-test path.
    fn config_with(dir: &Path, toml_src: &str) -> DevConfig {
        fs::write(dir.join("brokkr.toml"), toml_src).unwrap();
        crate::config::load(dir).unwrap().1
    }

    #[test]
    fn no_target_resolves_the_registered_bin() {
        let dir = tmpdir("no_target_resolves_the_registered_bin");
        let cfg = config_with(&dir, BASE);
        let resolved = resolve(config(&cfg).unwrap(), None, &[]).unwrap();
        // The CLI surface needs no registration - that is the whole point.
        assert_eq!(resolved.name, "mogwai");
        assert_eq!(resolved.build.bin.as_deref(), Some("mogwai"));
        assert!(resolved.build.example.is_none());
    }

    #[test]
    fn a_target_resolves_to_its_example_and_features() {
        let dir = tmpdir("a_target_resolves_to_its_example_and_features");
        let cfg = config_with(&dir, &format!(
            "{BASE}[mogwai.targets.screen_projection]\n\
             package = \"mogwai-lab\"\n\
             example = \"screen_projection_bench\"\n\
             features = [\"hotpath\"]\n"
        ));
        let resolved = resolve(config(&cfg).unwrap(), Some("screen_projection"), &[]).unwrap();
        assert_eq!(resolved.name, "screen_projection");
        assert_eq!(resolved.build.package.as_deref(), Some("mogwai-lab"));
        assert_eq!(
            resolved.build.example.as_deref(),
            Some("screen_projection_bench")
        );
        // Carried from the registration, not the call site: an instrumented
        // mode is inert without it, which is how profile-less rows happen.
        assert_eq!(resolved.build.features, vec!["hotpath"]);
    }

    #[test]
    fn call_site_features_append_to_the_registered_ones() {
        let dir = tmpdir("call_site_features_append_to_the_registered_ones");
        let cfg = config_with(&dir, &format!(
            "{BASE}[mogwai.targets.walk]\nexample = \"arrival_walk_bench\"\n\
             features = [\"hotpath\"]\n"
        ));
        let extra = vec![String::from("hotpath-alloc")];
        let resolved = resolve(config(&cfg).unwrap(), Some("walk"), &extra).unwrap();
        assert_eq!(resolved.build.features, vec!["hotpath", "hotpath-alloc"]);
    }

    #[test]
    fn a_feature_already_registered_is_not_repeated() {
        // `--hotpath` asks for the feature the target already registers, which
        // is the common case rather than an edge one.
        let dir = tmpdir("a_feature_already_registered_is_not_repeated");
        let cfg = config_with(&dir, &format!(
            "{BASE}[mogwai.targets.walk]\nexample = \"arrival_walk_bench\"\n\
             features = [\"hotpath\"]\n"
        ));
        let extra = vec![String::from("hotpath")];
        let resolved = resolve(config(&cfg).unwrap(), Some("walk"), &extra).unwrap();
        assert_eq!(resolved.build.features, vec!["hotpath"]);
    }

    #[test]
    fn a_target_without_a_package_falls_back_to_the_mogwai_one() {
        let dir = tmpdir("a_target_without_a_package_falls_back_to_the_mogwai_one");
        let cfg = config_with(&dir, &format!(
            "{BASE}[mogwai.targets.walk]\nexample = \"arrival_walk_bench\"\n"
        ));
        let resolved = resolve(config(&cfg).unwrap(), Some("walk"), &[]).unwrap();
        assert_eq!(resolved.build.package.as_deref(), Some("mogwai-cli"));
    }

    #[test]
    fn an_unknown_target_lists_the_registered_ones_and_the_cli_form() {
        let dir = tmpdir("an_unknown_target_lists_the_registered_ones_and_the_cli_form");
        let cfg = config_with(&dir, &format!(
            "{BASE}[mogwai.targets.walk]\nexample = \"arrival_walk_bench\"\n"
        ));
        let err = resolve(config(&cfg).unwrap(), Some("nope"), &[]).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("walk"), "{msg}");
        // A name that is not a target is very often an argv-shaped surface
        // someone expected to be registered; say where those go.
        assert!(msg.contains("brokkr mogwai --"), "{msg}");
    }

    #[test]
    fn the_index_names_the_cli_form_and_every_target() {
        let dir = tmpdir("the_index_names_the_cli_form_and_every_target");
        let cfg = config_with(&dir, &format!(
            "{BASE}[mogwai.targets.walk]\nexample = \"arrival_walk_bench\"\n\
             features = [\"hotpath\"]\n"
        ));
        let index = format_index(config(&cfg).unwrap());
        // The CLI surface is not in the registry, so the index has to say it
        // exists or it looks unavailable.
        assert!(index.contains("brokkr mogwai -- <args>"), "{index}");
        assert!(index.contains("arrival_walk_bench"), "{index}");
        assert!(index.contains("hotpath"), "{index}");
    }

    #[test]
    fn an_empty_registry_still_offers_the_cli() {
        let dir = tmpdir("an_empty_registry_still_offers_the_cli");
        let cfg = config_with(&dir, BASE);
        let index = format_index(config(&cfg).unwrap());
        assert!(index.contains("none registered"), "{index}");
        assert!(index.contains("brokkr mogwai -- <args>"), "{index}");
    }
}
