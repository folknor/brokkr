use std::collections::BTreeMap;

use crate::config::HarnessConfig;
use crate::error::DevError;
use serde::Deserialize;

/// The top-level key of the pbfhogg project section.
pub(crate) const SECTION: &str = "pbfhogg";

/// The key whose presence makes the `pbfhogg` table the project section.
const CLAIM_KEY: &str = "library";

/// Pbfhogg-specific configuration from `[pbfhogg]` in brokkr.toml.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PbfhoggConfig {
    /// `[pbfhogg.library]`: `brokkr library`, pbfhogg used as a library
    /// through its harness. See `docs/commands/library.md`.
    pub library: Option<LibraryConfig>,
}

/// `[pbfhogg.library]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryConfig {
    /// `[pbfhogg.library.harness]`: the harness member's build.
    pub harness: HarnessConfig,
    /// `[pbfhogg.library.require]`: capability id to its one-line meaning,
    /// what `brokkr library --all` certifies. Absent makes `--all` refuse;
    /// empty requires no capability.
    #[serde(default)]
    pub require: Option<BTreeMap<String, String>>,
}

/// Longest meaning of a required capability, in bytes.
const MEANING_LIMIT: usize = 200;

/// Each required capability's id by the protocol's capability grammar, and
/// its meaning one line of text.
fn check_require(require: &BTreeMap<String, String>) -> Result<(), DevError> {
    for (id, meaning) in require {
        if !crate::pbfhogg::library::is_capability(id) {
            return Err(DevError::Config(format!(
                "[pbfhogg.library.require]: {id:?} is not a capability id (an identifier \
                 ending .v<N>, such as cancel.pre-decision.v1; see `brokkr man library`)"
            )));
        }
        if meaning.is_empty() || meaning.len() > MEANING_LIMIT {
            return Err(DevError::Config(format!(
                "[pbfhogg.library.require] {id}: its meaning must be 1 to {MEANING_LIMIT} bytes, \
                 is {}",
                meaning.len()
            )));
        }
        if meaning.chars().any(char::is_control) {
            return Err(DevError::Config(format!(
                "[pbfhogg.library.require] {id}: its meaning is one line of text, without \
                 control characters"
            )));
        }
    }
    Ok(())
}

/// Whether the top-level `value` named `pbfhogg` is the pbfhogg project
/// section rather than a hostname section. `pbfhogg` was never a reserved
/// key, so a `[pbfhogg]` table has always loaded as a host; it is claimed
/// only when it carries `library`, which no host table can (the host schema
/// refuses unknown fields), so the claim changes no config that loads.
/// Applied to the composed table, by the host parser and by
/// [`super::ProjectConfig::parse`] alike.
pub(crate) fn claims(value: &toml::Value) -> bool {
    value.as_table().is_some_and(|t| t.contains_key(CLAIM_KEY))
}

/// Whether the composed root `table` holds the claimed section.
pub(crate) fn present(table: &toml::Table) -> bool {
    table.get(SECTION).is_some_and(claims)
}

/// Parse the claimed `[pbfhogg]` section, if the root table holds one.
pub(crate) fn parse(table: &toml::Table) -> Result<PbfhoggConfig, DevError> {
    let Some(value) = table.get(SECTION).filter(|value| claims(value)) else {
        return Ok(PbfhoggConfig::default());
    };
    if let Some(section) = value.as_table() {
        let stray: Vec<&str> = section
            .keys()
            .map(String::as_str)
            .filter(|key| *key != CLAIM_KEY)
            .collect();
        if !stray.is_empty() {
            return Err(DevError::Config(format!(
                "[pbfhogg] holds `library`, so it is the pbfhogg project section, and {} cannot \
                 share it; host settings for a machine named pbfhogg go in [host.pbfhogg]",
                stray
                    .iter()
                    .map(|key| format!("`{key}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let config: PbfhoggConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("pbfhogg: {e}")))?;
    if let Some(require) = config.library.as_ref().and_then(|l| l.require.as_ref()) {
        check_require(require)?;
    }
    Ok(config)
}
