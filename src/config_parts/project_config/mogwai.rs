use crate::error::DevError;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Mogwai-specific configuration from `[mogwai]` in brokkr.toml.
///
/// Drives `brokkr mogwai`. The registry holds TARGETS and their feature shapes,
/// and nothing else - no workloads, no frozen invocations.
///
/// The predecessor registered a name per hand-written argv, which could only
/// address the argv-shaped surfaces. Mogwai's measurable set is the product of
/// commands, presets, windows, seeds, cells and flag axes, plus every
/// library-level loop that has no command line at all; that product cannot be
/// enumerated, and everything it could not hold got pushed into a second
/// "layer" that was an escape hatch with a name rather than an architecture.
///
/// pbfhogg is the precedent: ~25 commands over several flag axes and ten
/// datasets, and it registers ZERO workloads. It registers inputs. Invocations
/// are composed at the call site and captured verbatim, and pairing rows is a
/// query rather than a name lookup. What a registry must hold is only what
/// cannot be recovered from the invocation: which cargo target a name means,
/// and which features it needs to be built with.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MogwaiConfig {
    /// The cargo package holding the CLI (`-p`). `None` builds the workspace
    /// default.
    #[serde(default)]
    pub package: Option<String>,
    /// The bin target to build and run. Defaults to `package` when absent.
    ///
    /// Registered ONCE. Every argv-shaped surface goes through this binary, so
    /// the argv is composed at the call site rather than enumerated here.
    #[serde(default)]
    pub bin: Option<String>,
    /// Harness targets, keyed by the name `brokkr mogwai <NAME>` takes.
    ///
    /// These are the surfaces with no command line - the matching loop, the
    /// tick sources, the arrival draw, the screen's projection - where the
    /// harness itself is the addressable thing. They are the majority of the
    /// eventual measurable surface, which is why they are what the registry
    /// holds.
    #[serde(default)]
    pub targets: BTreeMap<String, MogwaiTarget>,
}

/// One harness target: which cargo example, and the features it needs.
///
/// Registering a target is the work you were going to do the moment you wanted
/// to optimize the surface anyway. It carries no invocation: a harness takes an
/// argv exactly like the bin does, because every surface here is config-shaped
/// (preset, window, seed, cell) and an argument-free harness would need a new
/// registry entry per shape - the enumeration trap again at one remove.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MogwaiTarget {
    /// The cargo package owning the example (`-p`). Falls back to
    /// `[mogwai] package`.
    #[serde(default)]
    pub package: Option<String>,
    /// The `[[example]]` target name, as registered in that crate's
    /// `Cargo.toml`. Required.
    pub example: String,
    /// Cargo features to build the example with.
    ///
    /// The reason a feature list belongs in the registry rather than at the
    /// call site: `--hotpath` and `--alloc` are useless without the feature
    /// that compiles the instrumentation in, and the predecessor recorded
    /// profile-less rows for exactly that reason - it named a bin with no
    /// feature shape attached. A target plus its features is the fix.
    #[serde(default)]
    pub features: Vec<String>,
}

impl MogwaiConfig {
    /// The bin target to build: explicit `bin`, else the package name.
    pub fn bin_target(&self) -> Option<&str> {
        self.bin.as_deref().or(self.package.as_deref())
    }

    /// Look up a harness target by name, with an error listing the registered
    /// ones.
    pub fn target(&self, name: &str) -> Result<&MogwaiTarget, DevError> {
        self.targets.get(name).ok_or_else(|| {
            let known: Vec<&str> = self.targets.keys().map(String::as_str).collect();
            let known = if known.is_empty() {
                "none registered".to_owned()
            } else {
                known.join(", ")
            };
            DevError::Config(format!(
                "unknown target {name:?}\n  registered: {known}\n  \
                 add it as [mogwai.targets.{name}] in brokkr.toml, or drop the \
                 name to run the CLI: brokkr mogwai -- <args>"
            ))
        })
    }
}

/// Parse the required `[mogwai]` section.
///
/// Almost nothing to validate, which is the point. The registry holds cargo
/// coordinates, and cargo rejects a target that does not exist far better than
/// a pre-check here could. The predecessor's validation - measurement flags
/// frozen into an argv, lineage pointers, corpus tokens agreeing in both
/// directions - existed to defend a promise the registry no longer makes.
pub(crate) fn parse(table: &toml::map::Map<String, toml::Value>) -> Result<MogwaiConfig, DevError> {
    let value = table.get("mogwai").ok_or_else(|| {
        DevError::Config(
            "missing required [mogwai] section in brokkr.toml\n  add:\n\n  [mogwai]\n  \
             package = \"mogwai-cli\"\n  bin = \"mogwai\""
                .into(),
        )
    })?;
    let config: MogwaiConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("mogwai: {e}")))?;

    for (name, target) in &config.targets {
        if target.example.trim().is_empty() {
            return Err(DevError::Config(format!(
                "[mogwai.targets.{name}].example is empty - name the \
                 [[example]] target as registered in that crate's Cargo.toml."
            )));
        }
    }
    Ok(config)
}
