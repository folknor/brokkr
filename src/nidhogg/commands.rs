//! Nidhogg measurable command definitions.
//!
//! Each nidhogg command that can be measured (wall-clock, hotpath, alloc) is
//! defined here.  Unlike pbfhogg commands, nidhogg commands have fundamentally
//! different execution patterns: Api does HTTP against a running server,
//! Ingest runs an external binary with per-run scratch cleanup, and Tiles
//! manages a full server lifecycle.  The enum captures identity, capabilities,
//! and metadata - dispatch handles execution.

use crate::db::KvPair;
use crate::error::DevError;

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

#[allow(dead_code)] // methods prepared for future dispatch unification
impl NidhoggCommand {
    /// The command ID string used in CLI and result DB.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Api { .. } => "api",
            Self::Ingest => "nid-ingest",
            Self::Tiles { .. } => "tiles",
        }
    }

    /// Whether this command supports hotpath profiling.
    ///
    /// Only Ingest supports hotpath - the current hotpath implementation
    /// always runs `nidhogg ingest` with hotpath features enabled.
    /// Api talks to a running server (no binary instrumentation) and
    /// Tiles manages a server lifecycle (hotpath not wired).
    pub fn supports_hotpath(&self) -> bool {
        match self {
            Self::Ingest => true,
            Self::Api { .. } | Self::Tiles { .. } => false,
        }
    }

    /// The cargo package name to build, if this command needs a build.
    ///
    /// Api doesn't build anything (it talks to an already-running server).
    pub fn package(&self) -> Option<&'static str> {
        match self {
            Self::Api { .. } => None,
            Self::Ingest | Self::Tiles { .. } => Some("nidhogg"),
        }
    }

    /// The result command label for the DB - the bare subcommand id. The
    /// measurement mode (`bench`/`hotpath`/`alloc`) is recorded in the
    /// `variant` column.
    pub fn result_command(&self) -> &'static str {
        match self {
            Self::Api { .. } => "api",
            Self::Ingest => "ingest",
            Self::Tiles { .. } => "tiles",
        }
    }

    /// Build metadata key-value pairs for the result DB.
    ///
    /// Post-v13 this is reserved for runtime observations. Axis-like
    /// fields (query name, uring flag) are already captured in the
    /// `brokkr_args` column, so we don't mirror them here.
    pub fn metadata(&self) -> Vec<KvPair> {
        Vec::new()
    }

    /// Whether this command needs a binary build before execution.
    ///
    /// Api talks to an already-running server via HTTP - no build needed.
    /// Ingest and Tiles both need the nidhogg binary.
    pub fn needs_build(&self) -> bool {
        match self {
            Self::Api { .. } => false,
            Self::Ingest | Self::Tiles { .. } => true,
        }
    }

    /// Whether this command requires an already-running nidhogg server.
    ///
    /// Api requires an external server to be running.
    /// Ingest runs standalone. Tiles manages its own server lifecycle.
    pub fn needs_server(&self) -> bool {
        match self {
            Self::Api { .. } => true,
            Self::Ingest | Self::Tiles { .. } => false,
        }
    }

    /// Build the argument vector for commands that run an external binary.
    ///
    /// Only Ingest supports this - Api/Tiles have custom lifecycles.
    /// `pbf_str` is the resolved PBF path, `output_dir` is the scratch
    /// output directory for ingested data.
    pub fn build_args(&self, pbf_str: &str, output_dir: &str) -> Result<Vec<String>, DevError> {
        match self {
            Self::Ingest => Ok(vec![
                "ingest".into(),
                pbf_str.into(),
                output_dir.into(),
            ]),
            _ => Err(DevError::Config(format!(
                "build_args not supported for nidhogg command '{}'",
                self.id(),
            ))),
        }
    }

    /// Scratch output directory name for commands that produce output.
    pub fn scratch_output_dir(&self) -> Option<&'static str> {
        match self {
            Self::Ingest => Some("bench-ingest-output"),
            Self::Api { .. } | Self::Tiles { .. } => None,
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
    fn only_ingest_supports_hotpath() {
        assert!(!NidhoggCommand::Api { query: None }.supports_hotpath());
        assert!(NidhoggCommand::Ingest.supports_hotpath());
        assert!(
            !NidhoggCommand::Tiles {
                tiles_variant: None,
                uring: false,
            }
            .supports_hotpath()
        );
    }

    #[test]
    fn package_none_for_api() {
        assert_eq!(NidhoggCommand::Api { query: None }.package(), None);
        assert_eq!(NidhoggCommand::Ingest.package(), Some("nidhogg"));
        assert_eq!(
            NidhoggCommand::Tiles {
                tiles_variant: None,
                uring: false,
            }
            .package(),
            Some("nidhogg")
        );
    }

    #[test]
    fn result_command_labels() {
        assert_eq!(
            NidhoggCommand::Api { query: None }.result_command(),
            "api"
        );
        assert_eq!(NidhoggCommand::Ingest.result_command(), "ingest");
        assert_eq!(
            NidhoggCommand::Tiles {
                tiles_variant: None,
                uring: false,
            }
            .result_command(),
            "tiles"
        );
    }

    #[test]
    fn metadata_is_empty_post_v13() {
        // Axis-like fields (query, uring) live in brokkr_args/cli_args after
        // v13; nidhogg's metadata() now returns no runtime observations.
        assert!(
            NidhoggCommand::Api {
                query: Some("bbox-small".into())
            }
            .metadata()
            .is_empty()
        );
        assert!(NidhoggCommand::Ingest.metadata().is_empty());
        assert!(
            NidhoggCommand::Tiles {
                tiles_variant: None,
                uring: true,
            }
            .metadata()
            .is_empty()
        );
    }

    #[test]
    fn api_needs_server_not_build() {
        let cmd = NidhoggCommand::Api { query: None };
        assert!(!cmd.needs_build());
        assert!(cmd.needs_server());
    }

    #[test]
    fn ingest_needs_build_not_server() {
        assert!(NidhoggCommand::Ingest.needs_build());
        assert!(!NidhoggCommand::Ingest.needs_server());
    }

    #[test]
    fn tiles_needs_build_not_server() {
        let cmd = NidhoggCommand::Tiles {
            tiles_variant: None,
            uring: false,
        };
        assert!(cmd.needs_build());
        assert!(!cmd.needs_server());
    }
}
