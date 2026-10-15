use crate::config::HarnessConfig;
use crate::error::DevError;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Piners-specific configuration from `[piners]` in brokkr.toml.
///
/// Drives `brokkr corpus`, the parity-corpus runner. `[piners.harness]`
/// (reusing [`HarnessConfig`]) describes the binary brokkr builds once
/// and invokes with the resolved probe manifest. The flat fields locate
/// the corpus tree and the piners-owned pin/keyword registry. OHLCV feeds
/// are *not* config: they are hash-pinned registry content (`[feeds]` in
/// `pins.toml` - the feed is part of a probe's oracle identity). See
/// `docs/commands/corpus.md`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinersConfig {
    /// Build spec for the corpus harness binary. Required to actually run
    /// probes; `--verify-only` works without it.
    pub harness: Option<HarnessConfig>,

    /// Root of the piners-owned corpus tree (vendor submodules +
    /// first-party probe dirs), resolved relative to `brokkr.toml`.
    /// Pinned probe and feed paths in `pins.toml` resolve under here.
    /// Defaults to `corpus`.
    pub corpus_root: Option<PathBuf>,

    /// Directory holding the piners-owned registry: `pins.toml` (the
    /// canonical id -> path+xxh128 universe plus the `[feeds]` and
    /// `[probe_config]` tables) and one `*.toml` per keyword (id lists). Resolved relative
    /// to `brokkr.toml`. Defaults to `corpus-registry`.
    pub registry_dir: Option<PathBuf>,
}

impl PinersConfig {
    /// Corpus tree root, defaulting to `corpus`.
    pub fn corpus_root(&self) -> &Path {
        self.corpus_root
            .as_deref()
            .unwrap_or_else(|| Path::new("corpus"))
    }

    /// Registry directory, defaulting to `corpus-registry`.
    pub fn registry_dir(&self) -> &Path {
        self.registry_dir
            .as_deref()
            .unwrap_or_else(|| Path::new("corpus-registry"))
    }
}

/// Parse the optional `[piners]` section from the root table.
pub(crate) fn parse(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<PinersConfig>, DevError> {
    let Some(value) = table.get("piners") else {
        return Ok(None);
    };
    let config: PinersConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("piners: {e}")))?;
    Ok(Some(config))
}
