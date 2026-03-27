//! Nidhogg measurable command definitions.
//!
//! Each nidhogg command that can be measured (wall-clock, hotpath, alloc) is
//! defined here.  Unlike pbfhogg commands, nidhogg commands have fundamentally
//! different execution patterns: Api does HTTP against a running server,
//! Ingest runs an external binary with per-run scratch cleanup, and Tiles
//! manages a full server lifecycle.  The enum captures identity, capabilities,
//! and metadata — dispatch handles execution.

use crate::db::KvPair;

// ---------------------------------------------------------------------------
// NidhoggCommand — the unified command enum
// ---------------------------------------------------------------------------

/// Every measurable nidhogg command.
///
/// - `Api` — queries a running nidhogg server via HTTP (no build needed).
/// - `Ingest` — builds nidhogg, runs `nidhogg ingest <pbf> <output_dir>`.
/// - `Tiles` — builds nidhogg, manages server lifecycle (start → requests → stop).
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

    /// Whether this command supports hotpath profiling.
    ///
    /// Only Ingest supports hotpath — the current hotpath implementation
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

    /// The result command label for the DB.
    pub fn result_command(&self) -> &'static str {
        match self {
            Self::Api { .. } => "bench api",
            Self::Ingest => "bench ingest",
            Self::Tiles { .. } => "bench tiles",
        }
    }

    /// The result variant label for the DB.
    ///
    /// Api uses the query name as the variant (each query is stored
    /// separately). Ingest and Tiles have no variant.
    pub fn result_variant(&self) -> Option<String> {
        match self {
            Self::Api { query: Some(q) } => Some(q.clone()),
            Self::Api { query: None } => None,
            Self::Ingest => None,
            Self::Tiles { .. } => None,
        }
    }

    /// Build metadata key-value pairs for the result DB.
    ///
    /// Tiles carries the uring flag. Api carries query and port info
    /// (port is populated by the dispatch layer, not here — we only
    /// include what's known from the enum fields).
    pub fn metadata(&self) -> Vec<KvPair> {
        match self {
            Self::Api { query: Some(q) } => {
                vec![KvPair::text("meta.query", q)]
            }
            Self::Api { query: None } => vec![],
            Self::Ingest => vec![],
            Self::Tiles { uring, .. } => {
                vec![KvPair::text("meta.uring", uring.to_string())]
            }
        }
    }

    /// Whether this command needs a binary build before execution.
    ///
    /// Api talks to an already-running server via HTTP — no build needed.
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
        assert!(!NidhoggCommand::Tiles {
            tiles_variant: None,
            uring: false,
        }
        .supports_hotpath());
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
            "bench api"
        );
        assert_eq!(NidhoggCommand::Ingest.result_command(), "bench ingest");
        assert_eq!(
            NidhoggCommand::Tiles {
                tiles_variant: None,
                uring: false,
            }
            .result_command(),
            "bench tiles"
        );
    }

    #[test]
    fn api_variant_from_query() {
        let cmd = NidhoggCommand::Api {
            query: Some("bbox-small".into()),
        };
        assert_eq!(cmd.result_variant(), Some("bbox-small".into()));

        let cmd_no_query = NidhoggCommand::Api { query: None };
        assert_eq!(cmd_no_query.result_variant(), None);
    }

    #[test]
    fn ingest_and_tiles_have_no_variant() {
        assert_eq!(NidhoggCommand::Ingest.result_variant(), None);
        assert_eq!(
            NidhoggCommand::Tiles {
                tiles_variant: Some("elivagar".into()),
                uring: true,
            }
            .result_variant(),
            None
        );
    }

    #[test]
    fn tiles_metadata_includes_uring() {
        let cmd = NidhoggCommand::Tiles {
            tiles_variant: None,
            uring: true,
        };
        let meta = cmd.metadata();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].key, "meta.uring");
    }

    #[test]
    fn api_metadata_includes_query() {
        let cmd = NidhoggCommand::Api {
            query: Some("bbox-small".into()),
        };
        let meta = cmd.metadata();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].key, "meta.query");
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
