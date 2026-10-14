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

    /// Differential-lint corpus config (`[piners.lint]`). Drives
    /// `brokkr lint-corpus` / `brokkr lint-results`. Independent of the
    /// trade corpus above; shares only `corpus_root` for snippet
    /// resolution. See `docs/commands/lint-corpus.md`.
    pub lint: Option<LintConfig>,

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

/// `[piners.lint]` - the differential-lint corpus.
///
/// brokkr cargo-builds `package`/`binary` from the dirty tree (the
/// validator under test) and invokes `<bin> <subcommand> <file> --format
/// json` per probe; `pine_lint_bin` is the pre-installed external partner.
/// Snippet paths in `lints.toml` resolve under `[piners] corpus_root`. See
/// `docs/commands/lint-corpus.md`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LintConfig {
    /// Cargo package brokkr builds from the dirty tree (`cargo build
    /// --package`). The validator under test.
    pub package: String,

    /// Which `[[bin]]` inside `package` to invoke. Defaults to `package`.
    #[serde(default)]
    pub binary: Option<String>,

    /// The validate subcommand on that binary. Defaults to `validate`.
    #[serde(default)]
    pub subcommand: Option<String>,

    /// Cargo features for the validator build. Empty = cargo defaults.
    #[serde(default)]
    pub features: Vec<String>,

    /// Build the validator with the dev profile by default. CLI
    /// `--debug`/`--release` still override per-invocation.
    #[serde(default)]
    pub debug: Option<bool>,

    /// Registry directory holding `lints.toml` + `<keyword>.toml`, resolved
    /// relative to `brokkr.toml`. Defaults to `corpus-lint-registry`.
    pub registry_dir: Option<PathBuf>,

    /// Directory `--reseed` walks for `.pine` snippets (recursively, keyed by
    /// file stem), resolved relative to `brokkr.toml`. Must live under
    /// `[piners] corpus_root`. Defaults to `registry_dir`.
    pub snippets_dir: Option<PathBuf>,

    /// The external pine-lint validator. Defaults to `pine-lint` on PATH.
    pub pine_lint_bin: Option<String>,
}

impl LintConfig {
    /// The validate subcommand, defaulting to `validate`.
    pub fn subcommand(&self) -> &str {
        self.subcommand.as_deref().unwrap_or("validate")
    }

    /// Registry directory, defaulting to `corpus-lint-registry`.
    pub fn registry_dir(&self) -> &Path {
        self.registry_dir
            .as_deref()
            .unwrap_or_else(|| Path::new("corpus-lint-registry"))
    }

    /// Snippet directory `--reseed` walks, defaulting to [`Self::registry_dir`].
    pub fn snippets_dir(&self) -> &Path {
        self.snippets_dir
            .as_deref()
            .unwrap_or_else(|| self.registry_dir())
    }

    /// The external pine-lint binary, defaulting to `pine-lint`.
    pub fn pine_lint_bin(&self) -> &str {
        self.pine_lint_bin.as_deref().unwrap_or("pine-lint")
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
