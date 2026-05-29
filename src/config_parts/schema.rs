use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::error::DevError;
use crate::project::Project;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct DevConfig {
    pub hosts: HashMap<String, HostConfig>,
    pub litehtml: Option<LitehtmlConfig>,
    pub sluggrs: Option<SluggrsConfig>,
    pub ratatoskr: Option<RatatoskrConfig>,
    pub piners: Option<PinersConfig>,
    /// Static Cargo package dependency boundary rules enforced by
    /// `brokkr check` before clippy/tests. Empty when the project does
    /// not define any `[[dependency_rule]]` entries.
    pub dependency_rules: Vec<DependencyRule>,
    /// Each `[[check]]` entry in `brokkr.toml`. One entry = one
    /// (clippy + test) sweep with the entry's feature flags. Empty
    /// when the file has no `[[check]]` arrays - in that case
    /// `brokkr check` falls back to today's single
    /// `--all-features` sweep for backward compatibility with
    /// projects that haven't configured anything.
    pub check: Vec<CheckEntry>,
    pub test: Option<TestConfig>,
    /// Env var names / globs to capture into `run_kv` on every measured
    /// run (as `env.<NAME>` pairs). Supports exact names (`MALLOC_CONF`)
    /// and `PREFIX_*` globs (`PBFHOGG_*`). Empty by default.
    pub capture_env: Vec<String>,
}

/// One `[[dependency_rule]]` entry: a direct Cargo dependency that must
/// not exist.
///
/// `from` names one or more workspace packages. `forbid` names package
/// dependencies that are illegal for those packages. Both accept either
/// a single string or an array of strings in TOML.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyRule {
    /// Optional human label surfaced in violation output.
    pub name: Option<String>,
    /// Workspace package(s) whose direct dependency list is checked.
    #[serde(deserialize_with = "string_or_vec")]
    pub from: Vec<String>,
    /// Package names that may not appear in `from`'s direct dependencies.
    #[serde(deserialize_with = "string_or_vec")]
    pub forbid: Vec<String>,
}

fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrVec;

    impl<'de> Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a string or an array of strings")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(vec![value.to_owned()])
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(vec![value])
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                out.push(value);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

/// One `[[check]]` entry: a single named cargo invocation shape that
/// both `cargo clippy` and `cargo test` execute against.
///
/// `features` is an explicit list - the previous `features = "all"`
/// sentinel was removed because it silently broadens the test sweep
/// every time a new feature lands in `Cargo.toml` (the bug class that
/// motivated the explicit-list refactor). Enumerate features by name
/// or use `--features` on the CLI for one-shot runs.
///
/// `build_packages` rebuilds each listed cargo package with the same
/// feature flags before the test phase, so `tests/cli_*.rs`
/// `CliInvoker` calls hit a binary built for the sweep's feature set
/// (request 2: CLI binary feature parity).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckEntry {
    pub name: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub no_default_features: bool,
    #[serde(default)]
    pub build_packages: Vec<String>,
}

impl CheckEntry {
    /// Translate `features` / `no_default_features` into the cargo
    /// argv fragment used by both `cargo clippy` and `cargo test`.
    /// Skipped entirely when no flags are set so the cargo defaults
    /// (the package's default feature set) apply.
    pub fn cargo_feature_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.no_default_features {
            args.push("--no-default-features".into());
        }
        if !self.features.is_empty() {
            args.push("--features".into());
            args.push(self.features.join(","));
        }
        args
    }
}

/// `[test]` section.
///
/// - `default_package` is the cargo package `brokkr test` should pass to
///   `cargo test -p <pkg>` when the user doesn't supply `-p`. Required for
///   multi-crate workspaces (e.g. ratatoskr) where there's no single
///   "obvious" package; optional for single-crate projects that have a
///   built-in default via `Project::cli_package()`. An explicit CLI `-p`
///   always wins; TOML `default_package` wins over `cli_package()`.
/// - `default_profile` is the named [`ProfileDef`] used by `brokkr check`
///   when no `--profile` is passed. Without it, bare `brokkr check` runs
///   every `[[check]]` entry with no libtest filters; that's slow but
///   predictable. With it, the inner-loop signal is whatever profile the
///   project chose (typically a fast `tier1`).
/// - `[test.profiles.*]` declares named test selections that layer
///   libtest-level filters (`only` / `skip` / `tests` /
///   `include_ignored` / `test_threads` / `env`) on top of one or more
///   `[[check]]` entries (referenced by name in the profile's
///   `sweeps` field). Profiles can chain via `extends`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TestConfig {
    pub default_package: Option<String>,
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileDef>,
}

/// A named test selection layered onto one or more `[[check]]` entries.
///
/// All collection fields are optional `Option<Vec<_>>` so `extends`
/// merging can distinguish "child does not specify, inherit parent"
/// (None) from "child explicitly empty, override parent" (Some(vec![])).
///
/// The `extends` field walks up to one parent profile; cycles are
/// rejected at resolve time. Field merge semantics: child wins where
/// `Some`, parent fills in where the child is `None`. Collections are
/// **replaced**, not concatenated - this matches the hand-off doc's
/// example where `sort` extends `tier1` but ships its own `skip` list.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProfileDef {
    /// Human-readable description (parsed for completeness; brokkr does
    /// not surface it today, but a future `brokkr profiles` listing
    /// command will use it).
    #[allow(dead_code)]
    pub description: Option<String>,
    pub extends: Option<String>,
    /// Names of `[[check]]` entries to execute. Each is one cargo test
    /// invocation with the entry's feature flags. Empty / unset after
    /// `extends` resolution is an error.
    pub sweeps: Option<Vec<String>>,
    /// `--test <name>` for each entry. Limits cargo to specific test
    /// binaries (the `tests/<name>.rs` files).
    pub tests: Option<Vec<String>>,
    /// Positional substring filter passed to libtest. Tests whose name
    /// contains any of these strings run; everything else is filtered
    /// out by libtest. Use module path prefixes (e.g. `tier2::`) to
    /// match every test inside a module.
    pub only: Option<Vec<String>>,
    /// `--skip <substring>` for each entry. Cumulative with libtest's
    /// own `--skip`.
    pub skip: Option<Vec<String>>,
    /// `--include-ignored` (run `#[ignore]`d tests too).
    pub include_ignored: Option<bool>,
    /// `--test-threads=N`. Required for serial-only tests that touch
    /// process-global state (fault-injection hooks, etc.).
    pub test_threads: Option<u32>,
    /// Extra environment variables exported to the test process.
    pub env: Option<BTreeMap<String, String>>,
}

/// A single PBF file entry (one variant like raw, indexed, locations).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct PbfEntry {
    pub file: String,
    #[serde(alias = "sha256")]
    pub xxhash: Option<String>,
    pub seq: Option<u64>,
}

/// A single OSC diff file entry, keyed by sequence number.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OscEntry {
    pub file: String,
    #[serde(alias = "sha256")]
    pub xxhash: Option<String>,
}

/// A PMTiles archive entry, keyed by variant name (e.g. "elivagar").
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PmtilesEntry {
    pub file: String,
    #[serde(alias = "sha256")]
    pub xxhash: Option<String>,
}

/// A historical snapshot of a dataset - a different point-in-time capture
/// of the same region. Snapshots are first-class for the `diff-snapshots`
/// command and any future operation that takes a pair of snapshot refs.
///
/// Snapshots are NOT variants in the `pbf.raw` / `pbf.indexed` sense - those
/// are transforms of one PBF. A snapshot is a different PBF (e.g. a different
/// weekly planet dump). Each snapshot can carry its own pbf variants and its
/// own OSC chain anchored at its own replication seq.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct Snapshot {
    pub download_date: Option<String>,
    pub seq: Option<u64>,
    /// PBF variants keyed by name (e.g. "raw", "indexed").
    #[serde(default)]
    pub pbf: HashMap<String, PbfEntry>,
    /// OSC files keyed by sequence number. Stored but not consumed by any
    /// current command - pre-positioned for future `apply-changes --snapshot`
    /// / `merge-changes --snapshot` style commands.
    #[serde(default)]
    pub osc: HashMap<String, OscEntry>,
}

/// A dataset with structured PBF variants, multiple OSC entries, and zero
/// or more historical snapshots.
///
/// The top-level `pbf` and `osc` tables are the dataset's "primary" snapshot
/// (referenced as `base` by the `diff-snapshots` command). Additional
/// snapshots live under `snapshot.<key>` with their own pbf/osc tables.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct Dataset {
    pub origin: Option<String>,
    pub download_date: Option<String>,
    pub bbox: Option<String>,
    pub data_dir: Option<String>,
    /// PBF variants keyed by name (e.g. "raw", "indexed", "locations").
    /// These are the variants of the dataset's primary (legacy / `base`) snapshot.
    #[serde(default)]
    pub pbf: HashMap<String, PbfEntry>,
    /// OSC files keyed by sequence number, anchored to the primary PBF.
    #[serde(default)]
    pub osc: HashMap<String, OscEntry>,
    /// PMTiles archives keyed by variant name (e.g. "elivagar").
    #[serde(default)]
    pub pmtiles: HashMap<String, PmtilesEntry>,
    /// Additional historical snapshots keyed by snapshot name (e.g. a date
    /// like "20260411"). The reserved name "base" is rejected at parse time
    /// because it's the CLI sentinel for the legacy top-level data.
    #[serde(default)]
    pub snapshot: HashMap<String, Snapshot>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct HostConfig {
    pub data: Option<String>,
    pub scratch: Option<String>,
    pub target: Option<String>,
    pub port: Option<u16>,
    pub drives: Option<DriveConfig>,
    /// Cargo features to enable by default for all build commands on this host.
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub datasets: HashMap<String, Dataset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DriveConfig {
    pub source: Option<String>,
    pub data: Option<String>,
    pub scratch: Option<String>,
    pub target: Option<String>,
}

/// Litehtml visual reference testing configuration from `[litehtml]` in brokkr.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LitehtmlConfig {
    pub viewport_width: u32,
    pub mode: String,
    pub pixel_diff_threshold: f64,
    pub element_match_threshold: f64,
    pub fallback_aspect_ratio: Option<f64>,
    #[serde(rename = "fixture")]
    pub fixtures: Vec<LitehtmlFixture>,
}

/// A single litehtml test fixture entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct LitehtmlFixture {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub viewport_width: Option<u32>,
    pub mode: Option<String>,
    pub pixel_diff_threshold: Option<f64>,
    pub element_match_threshold: Option<f64>,
    pub expected: String,
    #[serde(default)]
    pub waive_element_threshold: bool,
    pub notes: Option<String>,
}

impl LitehtmlFixture {
    pub fn resolved_pixel_threshold(&self, config: &LitehtmlConfig) -> f64 {
        self.pixel_diff_threshold
            .unwrap_or(config.pixel_diff_threshold)
    }

    pub fn resolved_element_threshold(&self, config: &LitehtmlConfig) -> f64 {
        self.element_match_threshold
            .unwrap_or(config.element_match_threshold)
    }
}

impl LitehtmlConfig {
    pub fn fixtures_for_suite(&self, suite: &str) -> Vec<&LitehtmlFixture> {
        self.fixtures
            .iter()
            .filter(|f| f.tags.iter().any(|t| t == suite))
            .collect()
    }

    pub fn fixture_by_id(&self, id: &str) -> Option<&LitehtmlFixture> {
        self.fixtures.iter().find(|f| f.id == id)
    }
}

/// Sluggrs visual snapshot testing configuration from `[sluggrs]` in brokkr.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SluggrsConfig {
    pub width: u32,
    pub height: u32,
    pub pixel_diff_threshold: f64,
    #[serde(rename = "snapshot")]
    pub snapshots: Vec<SluggrsSnapshot>,
}

/// A single sluggrs snapshot definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct SluggrsSnapshot {
    pub id: String,
    pub description: String,
    pub fonts: Vec<String>,
    #[serde(default)]
    pub optional_fonts: Option<Vec<String>>,
}

impl SluggrsConfig {
    pub fn snapshot_by_id(&self, id: &str) -> Option<&SluggrsSnapshot> {
        self.snapshots.iter().find(|s| s.id == id)
    }
}

/// Ratatoskr-specific configuration from `[ratatoskr]` in brokkr.toml.
///
/// Holds two clusters of fields. `[ratatoskr.harness]` (the nested table)
/// drives `service-test`/`service-suite` builds (plan 1). The flat fields
/// drive plan-3 sync orchestration: where sæhrimnir's binary and fixtures
/// live, which env-var names ratatoskr's `test-helpers` feature reads to
/// pick up the mock endpoints, and where sync-test scripts live. All flat
/// fields are optional - a project that only wires plan 1 omits them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RatatoskrConfig {
    pub harness: Option<HarnessConfig>,

    /// Path to sæhrimnir's compiled binary (plan 3). Resolved relative
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
    /// per-protocol mock endpoints. Consumed by `sync-smoke` /
    /// `sync-bench` when exporting endpoints to the harness binary -
    /// `mock-serve` doesn't need them. Brokkr does not hardcode the
    /// spellings so they stay in sync with whatever ratatoskr's
    /// account-config code expects; missing field = "not exposed in
    /// this checkout."
    pub test_endpoint_env_jmap: Option<String>,
    pub test_endpoint_env_imap: Option<String>,
    pub test_endpoint_env_smtp: Option<String>,
    pub test_endpoint_env_graph: Option<String>,
    pub test_endpoint_env_gmail: Option<String>,
    pub test_endpoint_env_caldav: Option<String>,
    pub test_endpoint_env_people: Option<String>,
    pub test_endpoint_env_gcal: Option<String>,

    /// Where sync-test scripts live. Defaults to
    /// `crates/app/tests/sync-harness` when unset. Consumed by
    /// `sync-list`, `sync-smoke`, and `sync-bench`.
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
    /// Path to the sync-bench script this gate applies to. Used to
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

/// `[ratatoskr.harness]` - self-contained build spec for ratatoskr's
/// orchestration commands (`service-test`, `service-suite`,
/// `mock-serve`, `sync-smoke`, `sync-bench`). Decoupled from `[[check]]`
/// so that the everyday `brokkr check` pass doesn't get conflated with
/// "which features must the spawned binary have." See
/// `docs/projects/ratatoskr.md`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// Cargo package to build. Passed straight to `cargo build --package`.
    pub package: String,

    /// Which `[[bin]]` inside `package` to spawn. Defaults to `package`
    /// (the common case). Override only when the orchestration target
    /// is a non-default bin inside a multi-binary package.
    #[serde(default)]
    pub binary: Option<String>,

    /// Cargo features to activate for the harness build. Empty = cargo
    /// defaults. There is no `no_default_features` here: orchestration
    /// builds are not in the `brokkr check` sweep matrix, so the
    /// no-default-features story is irrelevant.
    #[serde(default)]
    pub features: Vec<String>,

    /// When true, orchestration commands build the harness with the dev
    /// profile by default. The CLI `--debug` / `--release` flags still
    /// override per-invocation.
    #[serde(default)]
    pub debug: Option<bool>,
}

impl HarnessConfig {
    /// Resolved binary name (defaults to `package` when unset). Not
    /// consumed by production code today - `cargo build` already takes
    /// `--package` and `--bin` separately - but exercised by parse tests
    /// to lock the defaulting rule into the schema.
    #[cfg(test)]
    pub fn binary_name(&self) -> &str {
        self.binary.as_deref().unwrap_or(&self.package)
    }
}

/// Piners-specific configuration from `[piners]` in brokkr.toml.
///
/// Drives `brokkr corpus`, the parity-corpus runner. `[piners.harness]`
/// (reusing [`HarnessConfig`]) describes the binary brokkr builds once
/// and invokes with the resolved probe manifest. The flat fields locate
/// the read-only corpus submodule, the piners-owned pin/keyword registry,
/// and the shared OHLCV feeds the probes run against. See
/// `docs/commands/corpus.md`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinersConfig {
    /// Build spec for the corpus harness binary. Required to actually run
    /// probes; `--verify-only` works without it.
    pub harness: Option<HarnessConfig>,

    /// Root of the read-only corpus submodule, resolved relative to
    /// `brokkr.toml`. Pinned probe paths in `pins.toml` resolve under
    /// here. Defaults to `corpus`.
    pub corpus_root: Option<PathBuf>,

    /// Directory holding the piners-owned registry: `pins.toml` (the
    /// canonical id -> path+xxh128 universe) plus one `*.toml` per keyword
    /// (id lists). Resolved relative to `brokkr.toml`. Defaults to
    /// `corpus-registry`.
    pub registry_dir: Option<PathBuf>,

    /// Shared OHLCV feed paths the probes run against, keyed by an
    /// arbitrary label (e.g. timeframe). Resolved relative to
    /// `brokkr.toml` and passed through to the harness in the manifest.
    /// Not hash-gated - only `strategy.pine` and `tv_trades.csv` are
    /// pinned oracles.
    #[serde(default)]
    pub feeds: BTreeMap<String, PathBuf>,
}

impl PinersConfig {
    /// Corpus submodule root, defaulting to `corpus`.
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

#[allow(dead_code)]
pub struct ResolvedPaths {
    pub hostname: String,
    pub data_dir: PathBuf,
    pub scratch_dir: PathBuf,
    pub target_dir: PathBuf,
    pub drives: Option<DriveConfig>,
    pub features: Vec<String>,
    pub datasets: HashMap<String, Dataset>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

