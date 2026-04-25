use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::Deserialize;

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

/// Load `brokkr.toml` from the project root directory.
///
/// Returns both the detected `Project` and the parsed `DevConfig`.
/// This is the **single code path** that reads and parses `brokkr.toml`.
pub fn load(project_root: &Path) -> Result<(Project, DevConfig), DevError> {
    let path = project_root.join("brokkr.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| DevError::Config(format!("{}: {e}", path.display())))?;

    let root: toml::Value = toml::from_str(&text)?;

    let table = root
        .as_table()
        .ok_or_else(|| DevError::Config("brokkr.toml root is not a table".into()))?;

    let project_str = table
        .get("project")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DevError::Config("brokkr.toml missing required 'project' field".into()))?;

    let project = match project_str {
        "pbfhogg" => Project::Pbfhogg,
        "elivagar" => Project::Elivagar,
        "nidhogg" => Project::Nidhogg,
        "brokkr" => Project::Brokkr,
        "litehtml-rs" => Project::Litehtml,
        "sluggrs" => Project::Sluggrs,
        other => Project::Other(Box::leak(other.to_owned().into_boxed_str())),
    };

    let litehtml = parse_litehtml(table)?;
    let sluggrs = parse_sluggrs(table)?;
    let check = parse_check(table)?;
    let test = parse_test(table)?;
    validate_check_against_test(&check, test.as_ref())?;
    let capture_env = parse_capture_env(table)?;
    let hosts = parse_hosts(table)?;
    validate_datasets(&hosts)?;

    Ok((
        project,
        DevConfig {
            hosts,
            litehtml,
            sluggrs,
            check,
            test,
            capture_env,
        },
    ))
}

/// Parse the optional top-level `capture_env = ["PBFHOGG*", "MALLOC_CONF"]`
/// list. Each entry is either an exact env var name or a `PREFIX*` glob;
/// `*` is only supported as the final character.
///
/// Validated eagerly to catch three footguns before they silently do
/// the wrong thing: bare `"*"` (would match *every* env var, including
/// PATH, SSH_AUTH_SOCK, and any API tokens - those would then land in
/// the results DB); empty strings; and patterns with `*` anywhere
/// other than the tail (like `"FOO*BAR"`, which today is treated as an
/// exact name and silently matches nothing).
fn parse_capture_env(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<String>, DevError> {
    let Some(value) = table.get("capture_env") else {
        return Ok(Vec::new());
    };
    let arr = value.as_array().ok_or_else(|| {
        DevError::Config("capture_env must be an array of strings".into())
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let raw = entry.as_str().ok_or_else(|| {
            DevError::Config(format!(
                "capture_env entries must be strings (got {entry})"
            ))
        })?;
        let s = raw.trim();
        if s.is_empty() {
            return Err(DevError::Config(
                "capture_env contains an empty string".into(),
            ));
        }
        if s == "*" {
            return Err(DevError::Config(
                "capture_env pattern '*' would capture every env var \
                 (PATH, credentials, …) into results.db - refusing. \
                 List the specific prefixes you want."
                    .into(),
            ));
        }
        // `*` is only legal as the last character. `FOO*BAR` and `*FOO`
        // are rejected rather than silently treated as exact names that
        // never match.
        let star_count = s.matches('*').count();
        if star_count > 0 && !s.ends_with('*') {
            return Err(DevError::Config(format!(
                "capture_env pattern {s:?}: '*' is only supported as the \
                 trailing character (got '*' elsewhere)"
            )));
        }
        if star_count > 1 {
            return Err(DevError::Config(format!(
                "capture_env pattern {s:?}: only a single trailing '*' \
                 is supported"
            )));
        }
        out.push(s.to_owned());
    }
    Ok(out)
}

/// Every top-level key that is a table and is not `project` is
/// treated as a hostname section.
fn parse_hosts(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<HashMap<String, HostConfig>, DevError> {
    let mut out = HashMap::new();
    for (key, value) in table {
        if key == "project"
            || key == "litehtml"
            || key == "sluggrs"
            || key == "check"
            || key == "test"
            || key == "capture_env"
        {
            continue;
        }
        if !value.is_table() {
            return Err(DevError::Config(format!(
                "unknown key '{key}' in brokkr.toml"
            )));
        }
        let hc: HostConfig = value
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| DevError::Config(format!("{key}: {e}")))?;
        out.insert(key.clone(), hc);
    }
    Ok(out)
}

/// Parse the optional `[litehtml]` section from the root table.
fn parse_litehtml(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<LitehtmlConfig>, DevError> {
    let Some(value) = table.get("litehtml") else {
        return Ok(None);
    };
    let config: LitehtmlConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("litehtml: {e}")))?;
    Ok(Some(config))
}

/// Parse the optional `[sluggrs]` section from the root table.
fn parse_sluggrs(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<SluggrsConfig>, DevError> {
    let Some(value) = table.get("sluggrs") else {
        return Ok(None);
    };
    let config: SluggrsConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("sluggrs: {e}")))?;
    Ok(Some(config))
}

/// Parse the optional `[[check]]` array of tables.
///
/// Rejects:
/// - the legacy `[check]` singular table form (with `consumer_features`),
///   pointing the user at the migration path;
/// - duplicate `name` values across entries;
/// - empty `name` strings.
///
/// Returns an empty `Vec` when no `[[check]]` arrays are configured;
/// callers fall back to today's single `--all-features` sweep in that
/// case.
fn parse_check(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<CheckEntry>, DevError> {
    let Some(value) = table.get("check") else {
        return Ok(Vec::new());
    };

    // [check] (singular table) is the old shape; reject loudly so a
    // stale brokkr.toml doesn't silently fall through to "no [[check]]
    // configured" behaviour and start running the wrong sweeps.
    if value.is_table() {
        return Err(DevError::Config(
            "[check] (table form) is no longer supported. Migrate to \
             one or more `[[check]]` array-of-table entries with \
             `name`, `features`, optional `no_default_features`, and \
             optional `build_packages`. See CLAUDE.md for examples."
                .into(),
        ));
    }

    let entries: Vec<CheckEntry> = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[[check]]: {e}")))?;

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for entry in &entries {
        if entry.name.trim().is_empty() {
            return Err(DevError::Config(
                "[[check]] entry has empty `name` - every entry needs a label \
                 used by output and by `[test.profiles].sweeps` references."
                    .into(),
            ));
        }
        if !seen.insert(entry.name.as_str()) {
            return Err(DevError::Config(format!(
                "[[check]] has duplicate name '{}' - each entry must have a unique name.",
                entry.name
            )));
        }
    }
    Ok(entries)
}

/// Parse the optional `[test]` section from the root table.
///
/// Also detects the previous `[test.sweeps.*]` shape (folded into
/// `[[check]]` with this redesign) and `[check].consumer_features`
/// fragments smuggled inside `[test]`, redirecting to the new shape.
fn parse_test(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<TestConfig>, DevError> {
    let Some(value) = table.get("test") else {
        return Ok(None);
    };
    if let Some(t) = value.as_table()
        && t.contains_key("sweeps")
    {
        return Err(DevError::Config(
            "[test.sweeps] is no longer supported. Sweeps are now declared \
             as `[[check]]` array-of-table entries that profiles reference \
             by name in `[test.profiles.<name>].sweeps`."
                .into(),
        ));
    }
    let config: TestConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("test: {e}")))?;
    Ok(Some(config))
}

/// Cross-check that every sweep name referenced by a profile resolves
/// to a `[[check]]` entry. Catches typos at parse time instead of at
/// `brokkr check --profile` time.
fn validate_check_against_test(
    check: &[CheckEntry],
    test: Option<&TestConfig>,
) -> Result<(), DevError> {
    let Some(t) = test else {
        return Ok(());
    };
    if t.profiles.is_empty() {
        return Ok(());
    }
    let names: BTreeSet<&str> = check.iter().map(|e| e.name.as_str()).collect();
    for (profile_name, def) in &t.profiles {
        let Some(sweeps) = &def.sweeps else {
            continue;
        };
        for sweep in sweeps {
            if !names.contains(sweep.as_str()) {
                return Err(DevError::Config(format!(
                    "[test.profiles.{profile_name}] references sweep '{sweep}', \
                     but no `[[check]]` entry with that name exists."
                )));
            }
        }
    }
    Ok(())
}

/// Validate all datasets across all hosts for empty file names and snapshot
/// key constraints.
fn validate_datasets(hosts: &HashMap<String, HostConfig>) -> Result<(), DevError> {
    for (host, hc) in hosts {
        for (ds_name, ds) in &hc.datasets {
            for (variant, entry) in &ds.pbf {
                if entry.file.is_empty() {
                    return Err(DevError::Config(format!(
                        "{host}.datasets.{ds_name}.pbf.{variant}: file name is empty"
                    )));
                }
            }
            for (seq, entry) in &ds.osc {
                if entry.file.is_empty() {
                    return Err(DevError::Config(format!(
                        "{host}.datasets.{ds_name}.osc.{seq}: file name is empty"
                    )));
                }
            }
            for (variant, entry) in &ds.pmtiles {
                if entry.file.is_empty() {
                    return Err(DevError::Config(format!(
                        "{host}.datasets.{ds_name}.pmtiles.{variant}: file name is empty"
                    )));
                }
            }
            for (snap_key, snap) in &ds.snapshot {
                validate_snapshot_key(snap_key).map_err(|e| {
                    DevError::Config(format!(
                        "{host}.datasets.{ds_name}.snapshot.{snap_key}: {e}"
                    ))
                })?;
                for (variant, entry) in &snap.pbf {
                    if entry.file.is_empty() {
                        return Err(DevError::Config(format!(
                            "{host}.datasets.{ds_name}.snapshot.{snap_key}.pbf.{variant}: file name is empty"
                        )));
                    }
                }
                for (seq, entry) in &snap.osc {
                    if entry.file.is_empty() {
                        return Err(DevError::Config(format!(
                            "{host}.datasets.{ds_name}.snapshot.{snap_key}.osc.{seq}: file name is empty"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Validate a snapshot key matches `[a-zA-Z0-9_-]+` and is not the reserved
/// sentinel `base` (which the CLI uses to refer to the dataset's legacy
/// top-level pbf/osc data).
pub(crate) fn validate_snapshot_key(key: &str) -> Result<(), String> {
    if key == "base" {
        return Err(
            "'base' is a reserved snapshot name (CLI sentinel for the dataset's primary data)"
                .into(),
        );
    }
    if key.is_empty() {
        return Err("snapshot key must not be empty".into());
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "snapshot key '{key}' must match [a-zA-Z0-9_-]+ (no spaces, dots, or other special characters)"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Host features
// ---------------------------------------------------------------------------

/// Walk brokkr's own environment and return an `env.<NAME> = <value>`
/// [`crate::db::KvPair`] for every variable that matches one of the
/// `capture_env` patterns in `config`. Each pattern is either an exact
/// name (`MALLOC_CONF`) or a `PREFIX*` glob; the trailing `*` is the
/// only supported wildcard. Patterns are validated at
/// `parse_capture_env` time, so a pattern reaching this point is
/// known-good. Returns an empty vec when `capture_env` is empty.
///
/// The capture runs on brokkr's inherited env, so a user invocation like
/// `PBFHOGG_USE_NEW_PATH=1 brokkr apply-changes --bench` records that
/// var without any per-command plumbing.
pub fn captured_env_pairs(config: &DevConfig) -> Vec<crate::db::KvPair> {
    if config.capture_env.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<crate::db::KvPair> = Vec::new();
    for (name, value) in std::env::vars() {
        if matches_capture(&name, &config.capture_env) {
            out.push(crate::db::KvPair::text(format!("env.{name}"), value));
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

fn matches_capture(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        if let Some(prefix) = p.strip_suffix('*') {
            name.starts_with(prefix)
        } else {
            name == p
        }
    })
}

/// Return the default cargo features configured for the current host.
pub fn host_features(config: &DevConfig) -> Vec<String> {
    let Ok(name) = hostname() else {
        return Vec::new();
    };
    config
        .hosts
        .get(&name)
        .map(|h| h.features.clone())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Hostname
// ---------------------------------------------------------------------------

/// Get the current hostname via `libc::gethostname()`. Cached for the
/// life of the process - the hostname doesn't change under us and the
/// FFI call gets hit from the hot path (harness bootstrap, history
/// init, host-feature resolution).
pub fn hostname() -> Result<String, DevError> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Result<String, String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let mut buf = [0u8; 256];
            let ret = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
            if ret != 0 {
                return Err("gethostname failed".to_owned());
            }
            let len = buf
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| "hostname not null-terminated".to_owned())?;
            String::from_utf8(buf[..len].to_vec())
                .map_err(|e| format!("hostname is not utf-8: {e}"))
        })
        .clone()
        .map_err(DevError::Config)
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve host-specific paths from config, with defaults for unknown hosts.
///
/// - `project_root`: the root of the project
/// - `target_dir`: from cargo metadata (resolved elsewhere)
pub fn resolve_paths(
    config: &DevConfig,
    hostname: &str,
    project_root: &Path,
    target_dir: &Path,
) -> ResolvedPaths {
    let host = config.hosts.get(hostname);

    let data_rel = host.and_then(|h| h.data.as_deref()).unwrap_or("data");

    let scratch_rel = host
        .and_then(|h| h.scratch.as_deref())
        .unwrap_or("data/scratch");

    let data_dir = resolve_relative(project_root, data_rel);
    let scratch_dir = resolve_relative(project_root, scratch_rel);

    let target_dir = match host.and_then(|h| h.target.as_deref()) {
        Some(t) => resolve_relative(project_root, t),
        None => target_dir.to_path_buf(),
    };

    let drives = host.and_then(|h| h.drives.clone());

    let features = host.map(|h| h.features.clone()).unwrap_or_default();

    let datasets = host.map(|h| h.datasets.clone()).unwrap_or_default();

    ResolvedPaths {
        hostname: hostname.to_owned(),
        data_dir,
        scratch_dir,
        target_dir,
        drives,
        features,
        datasets,
    }
}

/// Collect every dataset key configured across every host section. The
/// results DB is shared across hosts, so the `brokkr results` view
/// should recognize dataset names from rows that originated on a
/// different machine too. Keys are returned deduped.
pub fn all_dataset_keys(config: &DevConfig) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for host in config.hosts.values() {
        for key in host.datasets.keys() {
            keys.insert(key.clone());
        }
    }
    keys.into_iter().collect()
}

/// Resolve a potentially relative path against a base directory.
/// Absolute paths are returned as-is.
fn resolve_relative(base: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

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

    fn make_config(hosts: HashMap<String, HostConfig>) -> DevConfig {
        DevConfig {
            hosts,
            litehtml: None,
            sluggrs: None,
            check: Vec::new(),
            test: None,
            capture_env: Vec::new(),
        }
    }

    #[test]
    fn capture_env_matcher() {
        let patterns = vec!["PBFHOGG*".to_owned(), "MALLOC_CONF".to_owned()];
        assert!(matches_capture("PBFHOGG_USE_NEW_PATH", &patterns));
        assert!(matches_capture("PBFHOGG", &patterns));
        assert!(matches_capture("MALLOC_CONF", &patterns));
        assert!(!matches_capture("MALLOC_ARENA_MAX", &patterns));
        assert!(!matches_capture("PATH", &patterns));
        assert!(!matches_capture("XPBFHOGG", &patterns));
    }

    #[test]
    fn capture_env_parse_array() {
        let text = r#"
project = "pbfhogg"
capture_env = ["PBFHOGG*", "MALLOC_CONF"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        let got = parse_capture_env(table).unwrap();
        assert_eq!(got, vec!["PBFHOGG*", "MALLOC_CONF"]);
    }

    #[test]
    fn capture_env_absent_ok() {
        let text = r#"project = "pbfhogg""#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).unwrap().is_empty());
    }

    #[test]
    fn capture_env_rejects_non_array() {
        let text = r#"
project = "pbfhogg"
capture_env = "oops"
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_rejects_bare_star() {
        // `"*"` would capture every env var into results.db, including
        // PATH, SSH_AUTH_SOCK, and any API tokens. Validation is the
        // safety net.
        let text = r#"
project = "pbfhogg"
capture_env = ["*"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        let err = parse_capture_env(table).unwrap_err();
        assert!(matches!(err, DevError::Config(_)));
    }

    #[test]
    fn capture_env_rejects_middle_star() {
        // `"FOO*BAR"` would today be treated as an exact name (matches
        // nothing) - reject it loudly rather than silently no-op.
        let text = r#"
project = "pbfhogg"
capture_env = ["FOO*BAR"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_rejects_leading_star() {
        let text = r#"
project = "pbfhogg"
capture_env = ["*FOO"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_rejects_empty_string() {
        let text = r#"
project = "pbfhogg"
capture_env = [""]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_rejects_multiple_stars() {
        let text = r#"
project = "pbfhogg"
capture_env = ["FOO**"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_trims_whitespace() {
        // Leading/trailing whitespace used to be accepted literally,
        // so " PBFHOGG*" silently never matched. Trim eagerly.
        let text = r#"
project = "pbfhogg"
capture_env = ["  PBFHOGG*  ", "MALLOC_CONF"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        let got = parse_capture_env(table).unwrap();
        assert_eq!(got, vec!["PBFHOGG*", "MALLOC_CONF"]);
    }

    fn empty_dataset() -> Dataset {
        Dataset {
            origin: None,
            download_date: None,
            bbox: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
            snapshot: HashMap::new(),
        }
    }

    // -------------------------------------------------------------------
    // resolve_paths
    // -------------------------------------------------------------------

    #[test]
    fn host_datasets_resolved() {
        let mut pbf = HashMap::new();
        pbf.insert(
            "indexed".into(),
            PbfEntry {
                file: "dk-indexed.osm.pbf".into(),
                xxhash: None,
                seq: Some(4704),
            },
        );
        let mut host_ds = HashMap::new();
        host_ds.insert(
            "dk".into(),
            Dataset {
                bbox: Some("1,2,3,4".into()),
                pbf,
                ..empty_dataset()
            },
        );
        let mut hosts = HashMap::new();
        hosts.insert(
            "myhost".into(),
            HostConfig {
                data: None,
                scratch: None,
                target: None,
                port: None,
                drives: None,
                features: Vec::new(),
                datasets: host_ds,
            },
        );
        let config = make_config(hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.pbf.get("indexed").unwrap().file, "dk-indexed.osm.pbf");
        assert_eq!(dk.bbox.as_deref(), Some("1,2,3,4"));
    }

    #[test]
    fn unknown_host_gets_empty_datasets() {
        let config = make_config(HashMap::new());
        let resolved = resolve_paths(&config, "unknown", Path::new("/proj"), Path::new("/target"));
        assert!(resolved.datasets.is_empty());
    }

    #[test]
    fn multiple_pbf_variants() {
        let mut pbf = HashMap::new();
        pbf.insert(
            "raw".into(),
            PbfEntry {
                file: "dk-raw.osm.pbf".into(),
                xxhash: Some("aaa".into()),
                seq: Some(4704),
            },
        );
        pbf.insert(
            "indexed".into(),
            PbfEntry {
                file: "dk-indexed.osm.pbf".into(),
                xxhash: Some("bbb".into()),
                seq: None,
            },
        );
        pbf.insert(
            "locations".into(),
            PbfEntry {
                file: "dk-locations.osm.pbf".into(),
                xxhash: None,
                seq: None,
            },
        );
        let mut host_ds = HashMap::new();
        host_ds.insert(
            "dk".into(),
            Dataset {
                pbf,
                ..empty_dataset()
            },
        );
        let mut hosts = HashMap::new();
        hosts.insert(
            "myhost".into(),
            HostConfig {
                data: None,
                scratch: None,
                target: None,
                port: None,
                drives: None,
                features: Vec::new(),
                datasets: host_ds,
            },
        );
        let config = make_config(hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.pbf.len(), 3);
        assert_eq!(dk.pbf.get("raw").unwrap().xxhash.as_deref(), Some("aaa"));
        assert_eq!(
            dk.pbf.get("indexed").unwrap().xxhash.as_deref(),
            Some("bbb")
        );
    }

    #[test]
    fn multiple_osc_entries() {
        let mut osc = HashMap::new();
        osc.insert(
            "4705".into(),
            OscEntry {
                file: "dk-4705.osc.gz".into(),
                xxhash: Some("ccc".into()),
            },
        );
        osc.insert(
            "4706".into(),
            OscEntry {
                file: "dk-4706.osc.gz".into(),
                xxhash: None,
            },
        );
        let mut host_ds = HashMap::new();
        host_ds.insert(
            "dk".into(),
            Dataset {
                osc,
                ..empty_dataset()
            },
        );
        let mut hosts = HashMap::new();
        hosts.insert(
            "myhost".into(),
            HostConfig {
                data: None,
                scratch: None,
                target: None,
                port: None,
                drives: None,
                features: Vec::new(),
                datasets: host_ds,
            },
        );
        let config = make_config(hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.osc.len(), 2);
        assert_eq!(dk.osc.get("4705").unwrap().file, "dk-4705.osc.gz");
    }

    // -------------------------------------------------------------------
    // TOML parsing
    // -------------------------------------------------------------------

    #[test]
    fn parse_nested_dataset_from_toml() {
        let toml_str = r#"
project = "pbfhogg"

[myhost.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "8.0,54.5,13.0,58.0"

[myhost.datasets.denmark.pbf.raw]
file = "dk-raw.osm.pbf"
sha256 = "aaa"
seq = 4704

[myhost.datasets.denmark.pbf.indexed]
file = "dk-indexed.osm.pbf"
sha256 = "bbb"

[myhost.datasets.denmark.osc.4705]
file = "dk-4705.osc.gz"
sha256 = "ccc"
"#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        let host = hosts.get("myhost").unwrap();
        let dk = host.datasets.get("denmark").unwrap();
        assert_eq!(dk.origin.as_deref(), Some("Geofabrik"));
        assert_eq!(dk.download_date.as_deref(), Some("2026-02-20"));
        assert_eq!(dk.bbox.as_deref(), Some("8.0,54.5,13.0,58.0"));
        assert_eq!(dk.pbf.get("raw").unwrap().file, "dk-raw.osm.pbf");
        assert_eq!(dk.pbf.get("raw").unwrap().seq, Some(4704));
        assert_eq!(
            dk.pbf.get("indexed").unwrap().xxhash.as_deref(),
            Some("bbb")
        );
        assert_eq!(dk.osc.get("4705").unwrap().file, "dk-4705.osc.gz");
    }

    #[test]
    fn parse_dataset_with_snapshot_table() {
        let toml_str = r#"
project = "pbfhogg"

[myhost.datasets.planet]
origin = "planet.openstreetmap.org"

[myhost.datasets.planet.pbf.raw]
file = "planet-base.osm.pbf"

[myhost.datasets.planet.snapshot.20260411]
download_date = "2026-04-11"
seq = 4969

[myhost.datasets.planet.snapshot.20260411.pbf.raw]
file = "planet-20260411.osm.pbf"
xxhash = "deadbeef"

[myhost.datasets.planet.snapshot.20260411.pbf.indexed]
file = "planet-20260411-with-indexdata.osm.pbf"
xxhash = "feedface"
"#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        let host = hosts.get("myhost").unwrap();
        let planet = host.datasets.get("planet").unwrap();
        assert_eq!(planet.pbf.get("raw").unwrap().file, "planet-base.osm.pbf");
        let snap = planet.snapshot.get("20260411").unwrap();
        assert_eq!(snap.download_date.as_deref(), Some("2026-04-11"));
        assert_eq!(snap.seq, Some(4969));
        assert_eq!(snap.pbf.get("raw").unwrap().file, "planet-20260411.osm.pbf");
        assert_eq!(snap.pbf.get("raw").unwrap().xxhash.as_deref(), Some("deadbeef"));
        assert_eq!(
            snap.pbf.get("indexed").unwrap().file,
            "planet-20260411-with-indexdata.osm.pbf"
        );
    }

    #[test]
    fn snapshot_named_base_is_rejected() {
        let mut hc = HostConfig {
            data: None,
            scratch: None,
            target: None,
            port: None,
            drives: None,
            features: Vec::new(),
            datasets: HashMap::new(),
        };
        let mut ds = Dataset {
            origin: None,
            download_date: None,
            bbox: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
            snapshot: HashMap::new(),
        };
        ds.snapshot.insert(
            "base".into(),
            Snapshot {
                download_date: None,
                seq: None,
                pbf: HashMap::new(),
                osc: HashMap::new(),
            },
        );
        hc.datasets.insert("planet".into(), ds);
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), hc);

        let err = validate_datasets(&hosts).unwrap_err().to_string();
        assert!(err.contains("'base' is a reserved snapshot name"), "got: {err}");
    }

    #[test]
    fn snapshot_key_with_invalid_chars_rejected() {
        let mut hc = HostConfig {
            data: None,
            scratch: None,
            target: None,
            port: None,
            drives: None,
            features: Vec::new(),
            datasets: HashMap::new(),
        };
        let mut ds = Dataset {
            origin: None,
            download_date: None,
            bbox: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
            snapshot: HashMap::new(),
        };
        ds.snapshot.insert(
            "bad key".into(),
            Snapshot {
                download_date: None,
                seq: None,
                pbf: HashMap::new(),
                osc: HashMap::new(),
            },
        );
        hc.datasets.insert("planet".into(), ds);
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), hc);

        let err = validate_datasets(&hosts).unwrap_err().to_string();
        assert!(err.contains("[a-zA-Z0-9_-]+"), "got: {err}");
    }

    #[test]
    fn both_sha256_and_xxhash_is_rejected() {
        let toml_str = r#"
project = "pbfhogg"

[myhost.datasets.dk.pbf.raw]
file = "test.pbf"
sha256 = "aaa"
xxhash = "bbb"
"#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let result = parse_hosts(table);
        assert!(
            result.is_err(),
            "should reject entry with both sha256 and xxhash"
        );
    }

    #[test]
    fn parse_no_host_section() {
        let toml_str = r#"project = "pbfhogg""#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        assert!(hosts.is_empty());
    }

    #[test]
    fn parse_pmtiles_entries() {
        let toml_str = r#"
project = "nidhogg"

[myhost.datasets.denmark.pmtiles.elivagar]
file = "denmark-elivagar.pmtiles"
sha256 = "ddd"
"#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        let host = hosts.get("myhost").unwrap();
        let dk = host.datasets.get("denmark").unwrap();
        assert_eq!(dk.pmtiles.len(), 1);
        assert_eq!(
            dk.pmtiles.get("elivagar").unwrap().file,
            "denmark-elivagar.pmtiles"
        );
        assert_eq!(
            dk.pmtiles.get("elivagar").unwrap().xxhash.as_deref(),
            Some("ddd")
        );
    }

    // -------------------------------------------------------------------
    // [[check]] parsing
    // -------------------------------------------------------------------

    fn root_table(text: &str) -> toml::map::Map<String, toml::Value> {
        let v: toml::Value = toml::from_str(text).unwrap();
        v.as_table().unwrap().clone()
    }

    #[test]
    fn parse_check_returns_empty_when_absent() {
        let table = root_table(r#"project = "pbfhogg""#);
        let check = parse_check(&table).unwrap();
        assert!(check.is_empty());
    }

    #[test]
    fn parse_check_array_of_tables() {
        let table = root_table(
            r#"
project = "pbfhogg"

[[check]]
name = "all"
features = ["test-hooks", "linux-direct-io"]

[[check]]
name = "consumer"
no_default_features = true
features = ["commands"]
build_packages = ["pbfhogg-cli"]
"#,
        );
        let check = parse_check(&table).unwrap();
        assert_eq!(check.len(), 2);
        assert_eq!(check[0].name, "all");
        assert_eq!(check[0].features, vec!["test-hooks", "linux-direct-io"]);
        assert!(!check[0].no_default_features);
        assert!(check[0].build_packages.is_empty());

        assert_eq!(check[1].name, "consumer");
        assert_eq!(check[1].features, vec!["commands"]);
        assert!(check[1].no_default_features);
        assert_eq!(check[1].build_packages, vec!["pbfhogg-cli"]);
    }

    #[test]
    fn parse_check_rejects_legacy_table_form() {
        // The previous shape was `[check]\nconsumer_features = [...]`.
        // Detect the singular table and error loudly so a stale config
        // doesn't silently fall through.
        let table = root_table(
            r#"
project = "pbfhogg"
[check]
consumer_features = ["commands"]
"#,
        );
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("[[check]]"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_duplicate_names() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
features = ["a"]
[[check]]
name = "all"
features = ["b"]
"#,
        );
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("duplicate name 'all'"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_empty_name() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = ""
features = ["a"]
"#,
        );
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("empty `name`"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_features_all_sentinel() {
        // The `features = "all"` shorthand is gone - explicit lists only.
        // serde rejects with a type-mismatch error, which is loud enough
        // (the user sees "expected sequence" pointing at the offending line).
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "everything"
features = "all"
"#,
        );
        assert!(parse_check(&table).is_err());
    }

    #[test]
    fn parse_test_rejects_legacy_sweeps_section() {
        let table = root_table(
            r#"
project = "pbfhogg"

[test]

[test.sweeps.all]
features = ["a"]
"#,
        );
        let err = parse_test(&table).unwrap_err().to_string();
        assert!(err.contains("[test.sweeps]"), "got: {err}");
    }

    #[test]
    fn validate_check_against_test_catches_dangling_sweep_reference() {
        let check = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
        }];
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "tier1".into(),
            ProfileDef {
                description: None,
                extends: None,
                sweeps: Some(vec!["all".into(), "consumer".into()]),
                tests: None,
                only: None,
                skip: None,
                include_ignored: None,
                test_threads: None,
                env: None,
            },
        );
        let test = TestConfig {
            default_package: None,
            default_profile: None,
            profiles,
        };
        let err = validate_check_against_test(&check, Some(&test))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'consumer'"), "got: {err}");
    }

    #[test]
    fn check_entry_cargo_feature_args_shapes() {
        // No flags → no args at all (use cargo defaults).
        let bare = CheckEntry {
            name: "bare".into(),
            features: Vec::new(),
            no_default_features: false,
            build_packages: Vec::new(),
        };
        assert!(bare.cargo_feature_args().is_empty());

        // --features only.
        let feats = CheckEntry {
            name: "f".into(),
            features: vec!["a".into(), "b".into()],
            no_default_features: false,
            build_packages: Vec::new(),
        };
        assert_eq!(feats.cargo_feature_args(), vec!["--features", "a,b"]);

        // --no-default-features only.
        let nd = CheckEntry {
            name: "nd".into(),
            features: Vec::new(),
            no_default_features: true,
            build_packages: Vec::new(),
        };
        assert_eq!(nd.cargo_feature_args(), vec!["--no-default-features"]);

        // Both.
        let consumer = CheckEntry {
            name: "consumer".into(),
            features: vec!["commands".into()],
            no_default_features: true,
            build_packages: vec!["pbfhogg-cli".into()],
        };
        assert_eq!(
            consumer.cargo_feature_args(),
            vec!["--no-default-features", "--features", "commands"]
        );
    }
}
