//! Typed dataset selection and input-validation failures.

use crate::error::DevError;
use std::fmt;

#[derive(Debug)]
pub(crate) enum ResolveError {
    UnknownDataset(String),
    UnknownEntry {
        dataset: String,
        kind: String,
        key: String,
        available: Vec<String>,
    },
    EmptySelection {
        scope: String,
        kind: String,
    },
    AmbiguousSelection {
        scope: String,
        kind: String,
        flag: String,
        available: Vec<String>,
    },
    UnknownSnapshot {
        dataset: String,
        key: String,
        available: Vec<String>,
    },
    MissingOsc {
        scope: String,
        seq: u64,
        range: crate::config::OscRange,
    },
    MissingDataDir(String),
    MissingDirectory(std::path::PathBuf),
    MissingFile {
        kind: String,
        path: std::path::PathBuf,
    },
    MissingBbox(String),
    InvalidSnapshot(String),
    SnapshotVariant {
        dataset: String,
        snapshot: String,
        variant: String,
        available: Vec<String>,
    },
    Verification(DevError),
    Io(std::io::Error),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDataset(dataset) => write!(f, "unknown dataset: {dataset}"),
            Self::UnknownEntry {
                dataset,
                kind,
                key,
                available,
            } => write!(
                f,
                "dataset '{dataset}' has no {kind} '{key}' (available: {})",
                if available.is_empty() {
                    "none".to_owned()
                } else {
                    available.join(", ")
                }
            ),
            Self::EmptySelection { scope, kind } => write!(f, "{scope} has no {kind} configured"),
            Self::AmbiguousSelection {
                scope,
                kind,
                flag,
                available,
            } => write!(
                f,
                "{scope} has multiple {kind} entries - use {flag} to select (available: {})",
                available.join(", ")
            ),
            Self::UnknownSnapshot {
                dataset,
                key,
                available,
            } => write!(
                f,
                "dataset '{dataset}' has no snapshot '{key}' (available: {})",
                available.join(", ")
            ),
            Self::MissingOsc { scope, seq, range } => write!(
                f,
                "{scope} missing osc.{seq} (required by --osc-range {range})"
            ),
            Self::MissingDataDir(dataset) => {
                write!(f, "dataset '{dataset}' has no data_dir configured")
            }
            Self::MissingDirectory(path) => {
                write!(f, "data directory not found: {}", path.display())
            }
            Self::MissingFile { kind, path } => write!(
                f,
                "{} file not found: {}",
                kind.to_uppercase(),
                path.display()
            ),
            Self::MissingBbox(dataset) => {
                write!(f, "dataset '{dataset}' has no bbox configured (use --bbox)")
            }
            Self::InvalidSnapshot(message) => f.write_str(message),
            Self::SnapshotVariant {
                dataset,
                snapshot: key,
                variant,
                available,
            } => {
                let available_str = if available.is_empty() {
                    "none".to_owned()
                } else {
                    available.join(", ")
                };
                let suggestion_variant = available.first().map_or("raw", String::as_str);
                write!(
                    f,
                    "dataset '{dataset}' has no snapshot.{key}.pbf variant '{variant}'\n  \
                     \u{2514}\u{2500} available variants on this snapshot: {available_str}\n  \
                     \u{2514}\u{2500} pass `--variant {suggestion_variant}` to resolve both sides with the available variant,\n     \
                     or register the {variant} variant via `brokkr download {dataset} --as-snapshot {key}`\n     \
                     (which auto-generates pbf.indexed from pbf.raw)."
                )
            }
            Self::Verification(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Verification(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DevError> for ResolveError {
    fn from(error: DevError) -> Self {
        Self::Verification(error)
    }
}

impl From<std::io::Error> for ResolveError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ResolveError> for DevError {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::Verification(error) => error,
            ResolveError::Io(error) => Self::Io(error),
            error => Self::Config(error.to_string()),
        }
    }
}
