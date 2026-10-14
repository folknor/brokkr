//! Nidhogg measurable command definitions.
//!
//! Each nidhogg command that can be measured (wall-clock, hotpath, alloc) is
//! defined here.  Unlike pbfhogg commands, nidhogg commands have fundamentally
//! different execution patterns: Api does HTTP against a running server,
//! Ingest runs the binary with per-run scratch cleanup, and Tiles manages a
//! full server lifecycle. The enum owns command identity, build package and
//! the ingest argv; the plan supplies the other lifecycle details.

use crate::error::DevError;
use crate::project::Project;

// ---------------------------------------------------------------------------
// NidhoggCommand - the unified command enum
// ---------------------------------------------------------------------------

/// Every measurable nidhogg command.
///
/// - `Api` - queries a running nidhogg server via HTTP (no build needed).
/// - `Ingest` - builds nidhogg, runs `nidhogg ingest <pbf> <output_dir>`.
/// - `Tiles` - builds nidhogg, manages server lifecycle (start → requests → stop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NidhoggCommand {
    /// API query benchmark: fires spatial queries at a running server.
    /// The optional `query` filters to a single named query.
    Api { query: Option<String> },

    /// Ingest benchmark: builds nidhogg and runs `nidhogg ingest` externally.
    /// Needs per-run scratch cleanup between runs.
    Ingest,

    /// Tile serving lifecycle benchmark: builds nidhogg, starts a server,
    /// fires tile requests, sends SIGTERM, captures shutdown KV pairs.
    Tiles {
        tiles_variant: Option<String>,
        uring: bool,
    },
}

impl NidhoggCommand {
    /// The command ID string used in CLI and result DB.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Api { .. } => "api",
            Self::Ingest => "nid-ingest",
            Self::Tiles { .. } => "tiles",
        }
    }

    /// The cargo package name to build, if this command needs a build.
    ///
    /// Api doesn't build anything (it talks to an already-running server).
    pub fn package(&self) -> Option<&'static str> {
        match self {
            Self::Api { .. } => None,
            Self::Ingest | Self::Tiles { .. } => Project::Nidhogg.cli_package(),
        }
    }

    /// Build the argument vector for commands that run an external binary.
    ///
    /// Only Ingest supports this - Api/Tiles have custom lifecycles.
    /// `pbf_str` is the resolved PBF path, `output_dir` is the scratch
    /// output directory for ingested data.
    pub fn build_args(&self, pbf_str: &str, output_dir: &str) -> Result<Vec<String>, DevError> {
        match self {
            Self::Ingest => Ok(vec!["ingest".into(), pbf_str.into(), output_dir.into()]),
            _ => Err(DevError::Config(format!(
                "build_args not supported for nidhogg command '{}'",
                self.id(),
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;

    #[test]
    fn api_id() {
        let cmd = NidhoggCommand::Api { query: None };
        assert_eq!(cmd.id(), "api");
    }

    #[test]
    fn ingest_id() {
        assert_eq!(NidhoggCommand::Ingest.id(), "nid-ingest");
    }

    #[test]
    fn tiles_id() {
        let cmd = NidhoggCommand::Tiles {
            tiles_variant: None,
            uring: false,
        };
        assert_eq!(cmd.id(), "tiles");
    }

    #[test]
    fn package_matches_build_requirements() {
        assert_eq!(NidhoggCommand::Api { query: None }.package(), None);
        assert_eq!(
            NidhoggCommand::Ingest.package(),
            Project::Nidhogg.cli_package()
        );
        assert_eq!(
            NidhoggCommand::Tiles {
                tiles_variant: None,
                uring: false,
            }
            .package(),
            Project::Nidhogg.cli_package()
        );
    }
}
