use crate::config::HarnessConfig;
use crate::error::DevError;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Ratatoskr-specific configuration from `[ratatoskr]` in brokkr.toml.
///
/// `[ratatoskr.harness]` (the nested table) is the harness build both
/// `service` and `sync` run. The flat fields say where sæhrimnir's binary and
/// fixtures live, which env-var names ratatoskr's `test-helpers` feature reads
/// to pick up the mock endpoints (exported by `service` and `sync` alike), and
/// where sync-test scripts live. All flat fields are optional; a command that
/// needs one refuses when it is missing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// Env-var names ratatoskr's `test-helpers` reads to pick up the
    /// per-protocol mock endpoints. Consumed by `service` and `sync` when
    /// exporting endpoints to the harness binary - `mock-serve` doesn't
    /// need them. Brokkr does not hardcode the
    /// spellings so they stay in sync with whatever ratatoskr's
    /// account-config code expects; missing field = "not exposed in
    /// this checkout."
    pub test_endpoint_env_jmap: Option<String>,
    pub test_endpoint_env_imap: Option<String>,
    pub test_endpoint_env_smtp: Option<String>,
    pub test_endpoint_env_graph: Option<String>,
    pub test_endpoint_env_gmail: Option<String>,
    pub test_endpoint_env_caldav: Option<String>,
    pub test_endpoint_env_carddav: Option<String>,
    pub test_endpoint_env_people: Option<String>,
    pub test_endpoint_env_gcal: Option<String>,

    /// Where sync-test scripts live. Defaults to
    /// `crates/app/tests/sync-harness` when unset. Consumed by
    /// every shape of `sync`.
    pub sync_script_dir: Option<PathBuf>,

    /// Named `[ratatoskr.gate.<name>]` blocks. Each gate pins a
    /// per-hostname baseline UUID (looked up in `.brokkr/ratatoskr/gate.db`)
    /// plus a set of metric rules. See `docs/commands/ratatoskr-gate.md`.
    #[serde(default)]
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

/// One metric's threshold rules. All fields are optional; multiple
/// fields stack as logical AND. See `docs/commands/ratatoskr-gate.md`.
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
