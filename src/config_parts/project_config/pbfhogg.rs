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
    value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("pbfhogg: {e}")))
}
