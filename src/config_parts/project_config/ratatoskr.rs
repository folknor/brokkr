use crate::config::HarnessConfig;
use crate::error::DevError;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How a protocol's mock endpoint is written into the harness environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndpointFormat {
    /// `http://host:port`, for the JSON-over-HTTP protocols.
    HttpOrigin,
    /// `host:port`, for the stream protocols.
    HostPort,
}

/// The protocol registry: the one place the ratatoskr protocol list is
/// spelled. Each row gives the variant, its lowercase name (frontmatter,
/// CLI output), its sæhrimnir sentinel name, its endpoint shape, and the
/// `[ratatoskr]` key naming the env var that carries its endpoint. The
/// macro emits [`Protocol`] and the serde-facing [`RawRatatoskrConfig`],
/// whose fields keep the TOML keys, their order and their types exactly.
macro_rules! ratatoskr_protocols {
    ($( $variant:ident => $name:literal, $sentinel:literal, $format:ident, $key:ident; )*) => {
        /// A protocol exposed by sæhrimnir's readiness sentinel.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum Protocol {
            $( $variant, )*
        }

        impl Protocol {
            pub(crate) const COUNT: usize = [$( Self::$variant ),*].len();
            const ALL: [Self; Self::COUNT] = [$( Self::$variant ),*];

            pub(crate) fn all() -> impl Iterator<Item = Self> {
                Self::ALL.into_iter()
            }

            pub(crate) fn index(self) -> usize {
                self as usize
            }

            pub(crate) fn name(self) -> &'static str {
                match self {
                    $( Self::$variant => $name, )*
                }
            }

            pub(crate) fn sentinel_name(self) -> &'static str {
                match self {
                    $( Self::$variant => $sentinel, )*
                }
            }

            pub(crate) fn endpoint_format(self) -> EndpointFormat {
                match self {
                    $( Self::$variant => EndpointFormat::$format, )*
                }
            }
        }

        /// Serde shape of `[ratatoskr]`, converted into `RatatoskrConfig` by
        /// `From`. A type error names the struct through `expecting`, not
        /// its Rust name, so the message reads as before.
        #[derive(Deserialize)]
        #[serde(
            rename = "RatatoskrConfig",
            expecting = "struct RatatoskrConfig",
            deny_unknown_fields
        )]
        struct RawRatatoskrConfig {
            harness: Option<HarnessConfig>,
            mock_server_binary: Option<PathBuf>,
            fixtures_dir: Option<PathBuf>,
            $( $key: Option<String>, )*
            sync_script_dir: Option<PathBuf>,
            #[serde(default)]
            gate: BTreeMap<String, GateConfig>,
        }

        impl From<RawRatatoskrConfig> for RatatoskrConfig {
            fn from(raw: RawRatatoskrConfig) -> Self {
                let mut test_endpoint_env = EndpointEnvs::default();
                $( test_endpoint_env.set(Protocol::$variant, raw.$key); )*
                Self {
                    harness: raw.harness,
                    mock_server_binary: raw.mock_server_binary,
                    fixtures_dir: raw.fixtures_dir,
                    test_endpoint_env,
                    sync_script_dir: raw.sync_script_dir,
                    gate: raw.gate,
                }
            }
        }
    };
}

ratatoskr_protocols! {
    Jmap => "jmap", "JMAP", HttpOrigin, test_endpoint_env_jmap;
    Imap => "imap", "IMAP", HostPort, test_endpoint_env_imap;
    Smtp => "smtp", "SMTP", HostPort, test_endpoint_env_smtp;
    Graph => "graph", "GRAPH", HttpOrigin, test_endpoint_env_graph;
    Gmail => "gmail", "GMAIL", HttpOrigin, test_endpoint_env_gmail;
    Caldav => "caldav", "CALDAV", HttpOrigin, test_endpoint_env_caldav;
    Carddav => "carddav", "CARDDAV", HttpOrigin, test_endpoint_env_carddav;
    People => "people", "PEOPLE", HttpOrigin, test_endpoint_env_people;
    Gcal => "gcal", "GCAL", HttpOrigin, test_endpoint_env_gcal;
}

/// Per-protocol env-var names from the `test_endpoint_env_<protocol>` keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointEnvs([Option<String>; Protocol::COUNT]);

impl EndpointEnvs {
    /// The configured env-var name for `protocol`, if its key is set.
    pub(crate) fn get(&self, protocol: Protocol) -> Option<&str> {
        self.0[protocol.index()].as_deref()
    }

    pub(crate) fn set(&mut self, protocol: Protocol, name: Option<String>) {
        self.0[protocol.index()] = name;
    }
}

/// Ratatoskr-specific configuration from `[ratatoskr]` in brokkr.toml.
///
/// `[ratatoskr.harness]` (the nested table) is the harness build both
/// `service` and `sync` run. The flat fields say where sæhrimnir's binary and
/// fixtures live, which env-var names ratatoskr's harness runtime reads
/// to pick up the mock endpoints (exported by `service` and `sync` alike), and
/// where sync-test scripts live. All flat fields are optional; a command that
/// needs one refuses when it is missing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "RawRatatoskrConfig")]
pub struct RatatoskrConfig {
    pub harness: Option<HarnessConfig>,

    /// Path to sæhrimnir's compiled binary. Resolved relative
    /// to `brokkr.toml`. Auto-build of sæhrimnir is not yet wired -
    /// today the binary must already exist at this path.
    pub mock_server_binary: Option<PathBuf>,

    /// Directory holding sæhrimnir fixture files. Resolved relative to
    /// `brokkr.toml`. Sync-test script frontmatter (and `mock-serve
    /// --fixture`) references fixtures by name; resolution prefers the
    /// `.toml` or `.lua` file with the matching stem and refuses if
    /// both exist for the same stem. The hatch when both legitimately
    /// coexist is to write the name with its extension - see
    /// `crate::ratatoskr::saehrimnir::resolve_fixture`.
    pub fixtures_dir: Option<PathBuf>,

    /// Env-var names ratatoskr's harness runtime reads to pick up the
    /// per-protocol mock endpoints, from the `test_endpoint_env_<protocol>`
    /// keys. Consumed by `service` and `sync` when exporting endpoints to
    /// the harness binary - `mock-serve` doesn't need them. Brokkr does not
    /// hardcode the spellings so they stay in sync with whatever
    /// ratatoskr's account-config code expects; an unset key = "not
    /// exposed in this checkout."
    pub test_endpoint_env: EndpointEnvs,

    /// Where sync-test scripts live. Defaults to
    /// `crates/app/tests/sync-harness` when unset. Consumed by
    /// every shape of `sync`.
    pub sync_script_dir: Option<PathBuf>,

    /// Named `[ratatoskr.gate.<name>]` blocks. Each gate pins a
    /// per-hostname baseline UUID (looked up in `.brokkr/ratatoskr/gate.db`)
    /// plus a set of metric rules. See `docs/commands/ratatoskr-gate.md`.
    pub gate: BTreeMap<String, GateConfig>,
}

/// One `[ratatoskr.gate.<name>]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    /// Path to the sync script this gate applies to. Used to
    /// validate that the looked-up baseline row matches the current
    /// invocation.
    pub script: PathBuf,

    /// Optional human-readable label, recorded for context when a
    /// developer reads the config; not consumed by the gate logic.
    #[serde(default)]
    pub baseline_label: Option<String>,

    /// Per-hostname pinned baseline UUIDs. Lookup is by libc hostname.
    /// Missing entry for the current host is a hard error at gate time.
    #[serde(default)]
    pub baseline: BTreeMap<String, String>,

    /// Per-metric rules. Keys use dotted namespacing: bare
    /// (`elapsed_ms`/`exit_code`/`success`), `sidecar.*`, or `meta.*`.
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricRule>,
}

/// Raw TOML shape for one metric's threshold rules. The predicates remain
/// independent so multiple checks can stack as logical AND; making this a
/// one-of enum would reject that supported configuration. This is not the
/// validated runtime form: `validate_gates` compiles every metric when the
/// config loads, and rejects any metric with no effective predicate.
/// See `docs/commands/ratatoskr-gate.md`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MetricRule {
    /// Hard upper cap: current value must be `<= max`.
    #[serde(default)]
    pub max: Option<f64>,
    /// Hard lower floor: current value must be `>= min`.
    #[serde(default)]
    pub min: Option<f64>,
    /// Relative upper bound: current `<=` baseline `*` factor.
    #[serde(default)]
    pub max_relative: Option<f64>,
    /// Relative lower bound: current `>=` baseline `*` factor.
    #[serde(default)]
    pub min_relative: Option<f64>,
    /// Maximum allowed delta vs baseline: `current - baseline <= max_delta`.
    #[serde(default)]
    pub max_delta: Option<f64>,
    /// Literal equality with the given scalar. Numbers compare as f64;
    /// strings compare as Text via the JSON-blob path.
    #[serde(default)]
    pub equal: Option<toml::Value>,
    /// Current value must equal the baseline row's value exactly.
    #[serde(default)]
    pub equal_to_baseline: Option<bool>,
}

/// Parse the required `[ratatoskr]` section from the root table.
///
/// Rejects the pre-decoupling `[ratatoskr.harness].sweep` field with a
/// migration message - that field is gone; orchestration builds are now
/// fully described by `package`/`binary`/`features` directly under
/// `[ratatoskr.harness]` and never reference `[[check]]`. See
/// `docs/projects/ratatoskr.md`.
pub(crate) fn parse(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<RatatoskrConfig, DevError> {
    let value = table.get("ratatoskr").ok_or_else(|| {
        DevError::Config("missing required [ratatoskr] section in brokkr.toml".into())
    })?;
    if let Some(harness) = value.as_table().and_then(|t| t.get("harness"))
        && let Some(ht) = harness.as_table()
        && ht.contains_key("sweep")
    {
        return Err(DevError::Config(
            "[ratatoskr.harness].sweep is no longer supported. The harness \
             build spec is now self-contained: drop the `[[check]]` entry \
             that the sweep referenced, then under `[ratatoskr.harness]` \
             declare `package = \"<crate>\"` (required), optional \
             `binary = \"<bin>\"` (defaults to `package`), optional \
             `features = [...]`, and optional `debug = true`. See \
             docs/projects/ratatoskr.md."
                .into(),
        ));
    }
    let config: RatatoskrConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("ratatoskr: {e}")))?;
    validate_gates(&config)?;
    Ok(config)
}

/// Gate blocks are checked when the config loads, not when a gated run
/// reaches them: the reserved `all` name, every metric rule, and every
/// baseline pin. An empty rule set stays legal here - it is what
/// `--as-baseline` records against - and is refused only by an evaluating run.
/// `MetricRule` intentionally remains a raw set of optional conjunctive
/// predicates; compiling it here turns each metric into a nonempty executable
/// rule set before any gate can run.
fn validate_gates(config: &RatatoskrConfig) -> Result<(), DevError> {
    let reserved = crate::ratatoskr::sync::GATE_ALL;
    if config.gate.contains_key(reserved) {
        return Err(DevError::Config(format!(
            "`[ratatoskr.gate.{reserved}]` collides with the reserved `--gate {reserved}` \
             sweep value - rename that gate"
        )));
    }
    for (name, gate) in &config.gate {
        crate::ratatoskr::gate::validate_rules(name, gate)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::unwrap_in_result)]
    use super::*;

    fn parse_str(body: &str) -> Result<RatatoskrConfig, DevError> {
        let table: toml::Table = toml::from_str(body).unwrap();
        parse(&table)
    }

    /// The TOML keys are the config contract, so they are spelled out here
    /// rather than derived from the registry they feed.
    const KEYS: [(&str, Protocol); Protocol::COUNT] = [
        ("test_endpoint_env_jmap", Protocol::Jmap),
        ("test_endpoint_env_imap", Protocol::Imap),
        ("test_endpoint_env_smtp", Protocol::Smtp),
        ("test_endpoint_env_graph", Protocol::Graph),
        ("test_endpoint_env_gmail", Protocol::Gmail),
        ("test_endpoint_env_caldav", Protocol::Caldav),
        ("test_endpoint_env_carddav", Protocol::Carddav),
        ("test_endpoint_env_people", Protocol::People),
        ("test_endpoint_env_gcal", Protocol::Gcal),
    ];

    #[test]
    fn every_endpoint_key_lands_on_its_protocol() {
        for (key, protocol) in KEYS {
            let cfg = parse_str(&format!("[ratatoskr]\n{key} = \"VAR\"\n")).unwrap();
            for other in Protocol::all() {
                let expected = (other == protocol).then_some("VAR");
                assert_eq!(cfg.test_endpoint_env.get(other), expected, "{key}");
            }
        }
    }

    #[test]
    fn absent_endpoint_keys_are_unset() {
        let cfg = parse_str("[ratatoskr]\n").unwrap();
        assert_eq!(cfg.test_endpoint_env, EndpointEnvs::default());
        assert!(Protocol::all().all(|p| cfg.test_endpoint_env.get(p).is_none()));
    }

    #[test]
    fn endpoint_key_refusals_are_unchanged() {
        let err = parse_str("[ratatoskr]\ntest_endpoint_env_jmap = 5\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid type: integer `5`"), "got: {err}");
        assert!(err.contains("expected a string"), "got: {err}");

        let err = parse_str("[ratatoskr]\ntest_endpoint_env_pop3 = \"X\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `test_endpoint_env_pop3`"), "got: {err}");
        for (key, _) in KEYS {
            assert!(err.contains(&format!("`{key}`")), "{key} missing from: {err}");
        }

        let err = parse_str("ratatoskr = 5\n").unwrap_err().to_string();
        assert!(err.contains("struct RatatoskrConfig"), "got: {err}");
    }
}
