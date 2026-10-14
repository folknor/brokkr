//! Geofabrik and planet source selection, aliases and URLs.

use super::layout::seq_path;
use crate::{config::Dataset, error::DevError};

pub(super) const PLANET_ORIGIN: &str = "planet.openstreetmap.org";
pub(super) const PLANET_BASE_URL: &str = "https://planet.openstreetmap.org";

/// Where to download PBF and OSC files from.
pub(super) enum DownloadSource {
    /// Geofabrik regional extract. Path is e.g. `europe/denmark`.
    Geofabrik { path: String },
    /// Full planet from planet.openstreetmap.org.
    Planet,
}

impl DownloadSource {
    /// URL for the latest PBF.
    pub(super) fn pbf_url(&self) -> String {
        match self {
            Self::Geofabrik { path } => {
                format!("https://download.geofabrik.de/{path}-latest.osm.pbf")
            }
            Self::Planet => {
                format!("{PLANET_BASE_URL}/pbf/planet-latest.osm.pbf")
            }
        }
    }

    /// URL for an OSC diff at the given sequence number.
    pub(super) fn osc_url(&self, seq: u64) -> String {
        let sp = seq_path(seq);
        match self {
            Self::Geofabrik { path } => {
                format!("https://download.geofabrik.de/{path}-updates/{sp}.osc.gz")
            }
            Self::Planet => {
                format!("{PLANET_BASE_URL}/replication/day/{sp}.osc.gz")
            }
        }
    }

    /// Origin string for `brokkr.toml`.
    pub(super) fn origin(&self) -> &'static str {
        match self {
            Self::Geofabrik { .. } => "Geofabrik",
            Self::Planet => PLANET_ORIGIN,
        }
    }

    /// Display name for log messages.
    pub(super) fn display_name(&self) -> String {
        match self {
            Self::Geofabrik { path } => format!("Geofabrik: {path}"),
            Self::Planet => PLANET_ORIGIN.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Region resolution
// ---------------------------------------------------------------------------

/// Short aliases for commonly used regions.
pub(super) const ALIASES: &[(&str, &str)] = &[
    ("malta", "europe/malta"),
    (
        "greater-london",
        "europe/united-kingdom/england/greater-london",
    ),
    ("switzerland", "europe/switzerland"),
    ("norway", "europe/norway"),
    ("japan", "asia/japan"),
    ("denmark", "europe/denmark"),
    ("germany", "europe/germany"),
    ("north-america", "north-america"),
    ("europe", "europe"),
];

/// Resolved download target.
pub(super) struct ResolvedDownload {
    pub(super) source: DownloadSource,
    /// Dataset key for `brokkr.toml` (e.g. `denmark`, `planet`).
    pub(super) dataset_key: String,
}

/// Resolve a region argument into a download source and dataset key.
///
/// Resolution order:
/// 1. If an existing dataset has `origin = "planet.openstreetmap.org"`, use planet source.
/// 2. `"planet"` -> planet source.
/// 3. Short alias -> Geofabrik source.
/// 4. Path containing `/` -> direct Geofabrik path.
/// 5. Error with suggestions.
pub(super) fn resolve(name: &str, dataset: Option<&Dataset>) -> Result<ResolvedDownload, DevError> {
    // If the dataset already exists, let its origin override.
    if let Some(ds) = dataset
        && ds.origin.as_deref() == Some(PLANET_ORIGIN)
    {
        return Ok(ResolvedDownload {
            source: DownloadSource::Planet,
            dataset_key: name.to_string(),
        });
    }

    // "planet" keyword.
    if name == "planet" {
        return Ok(ResolvedDownload {
            source: DownloadSource::Planet,
            dataset_key: "planet".into(),
        });
    }

    // Short aliases.
    for &(alias, path) in ALIASES {
        if alias == name {
            return Ok(ResolvedDownload {
                source: DownloadSource::Geofabrik { path: path.into() },
                dataset_key: name.to_string(),
            });
        }
    }

    // Direct Geofabrik path.
    if name.contains('/') {
        let trimmed = name.trim_end_matches('/');
        let dataset_key = trimmed.rsplit('/').next().unwrap_or(trimmed).to_string();
        if dataset_key.is_empty() {
            return Err(DevError::Config(format!(
                "cannot derive dataset key from path '{name}'"
            )));
        }
        return Ok(ResolvedDownload {
            source: DownloadSource::Geofabrik {
                path: trimmed.into(),
            },
            dataset_key,
        });
    }

    let alias_list: Vec<&str> = ALIASES.iter().map(|&(n, _)| n).collect();
    Err(DevError::Config(format!(
        "unknown region '{name}'. use 'planet', a Geofabrik path (e.g. europe/france), \
         or one of: {}",
        alias_list.join(", ")
    )))
}
