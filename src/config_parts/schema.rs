use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
    /// `[gremlins]` config: directories the gremlin scanner skips. Used to
    /// silence vendored / third-party material (reference manuals, imported
    /// docs) that legitimately carries gremlin characters. `None` when the
    /// project has no `[gremlins]` section. See [`GremlinsConfig`].
    pub gremlins: Option<GremlinsConfig>,
    /// `[style]` config: opt-in native Rust style checks run by `brokkr
    /// check`. `None` when the project has no `[style]` section. See
    /// [`StyleConfig`].
    pub style: Option<StyleConfig>,
    /// `[header]` config: a required file header with a current-year check.
    /// `None` when the project has no `[header]` section. See [`HeaderConfig`].
    pub header: Option<HeaderConfig>,
    /// `[[textlint]]` rules: declarative forbid-a-pattern line checks run by
    /// `brokkr check`. Empty when the project defines no `[[textlint]]`
    /// entries. See [`TextlintRule`].
    pub textlint: Vec<TextlintRule>,
    /// `[manifest]` config: native structural `Cargo.toml` conventions
    /// (dependency ordering, ...) run by `brokkr check`. `None` when the
    /// project has no `[manifest]` section. See [`ManifestConfig`].
    pub manifest: Option<ManifestConfig>,
    /// Top-level `disable_toolchain = true`: move the project's
    /// `rust-toolchain.toml` (or legacy `rust-toolchain`) aside for the
    /// duration of every brokkr command, so rustup ignores the pin and falls
    /// back to its default. For driving a foreign checkout whose pinned
    /// toolchain we don't have or don't want. `false` by default. See
    /// [`crate::toolchain`].
    pub disable_toolchain: bool,
}

/// `[gremlins]` section: tuning for the `brokkr check` gremlin scanner.
///
/// Knobs:
/// - `disable` - skip the gremlin phase entirely (scan and `--fix-gremlins`).
///   The escape hatch for driving a foreign checkout whose Unicode we don't
///   want to police.
/// - `exclude` - directories (relative to the project root) skipped by both
///   the scan and the `--fix-gremlins` rewrite. Intended for vendored content
///   from an outside source that ships its own typographic punctuation, BOMs,
///   and other characters the scanner would otherwise flag. Matching is by
///   path prefix on the git-relative path: `docs/manual` excludes
///   `docs/manual` and everything under `docs/manual/`, but not a sibling
///   `docs/manual-extra`.
/// - `allow` - codepoints to un-ban: the scan skips them and `--fix-gremlins`
///   leaves them alone, even though they are in the built-in banned set.
/// - `ban` - codepoints to flag beyond the built-in set. Scan-only: brokkr
///   has no ASCII mapping for an arbitrary codepoint, so `--fix-gremlins`
///   does not rewrite them.
///
/// The parsed `allow`/`ban` sets are built once at config load from
/// `U+XXXX` strings; see `parse_gremlins` in the parser.
#[derive(Debug, Clone, Default)]
pub struct GremlinsConfig {
    /// Skip the whole gremlin phase when true.
    pub disable: bool,
    /// Directories (project-root-relative) to skip when scanning for
    /// gremlins. Empty / unset means scan everything.
    pub exclude: Vec<String>,
    /// Codepoints removed from the effective banned set (scan skips, fix
    /// leaves alone). Singletons and ranges.
    pub allow: CodepointSet,
    /// Codepoints added to the banned set beyond the built-in list. Detected
    /// by the scan; not auto-rewritten by `--fix-gremlins`. Singletons and
    /// ranges.
    pub ban: CodepointSet,
}

/// A set of Unicode codepoints: individual codepoints plus inclusive ranges.
/// Backs the `[gremlins]` `allow`/`ban` lists, which accept both `U+XXXX`
/// singletons and `U+AAAA..U+BBBB` ranges (both ends inclusive). Ranges are
/// kept as ranges rather than expanded, so banning a whole plane costs a
/// bounds check, not a million-entry set.
#[derive(Debug, Clone, Default)]
pub struct CodepointSet {
    pub singles: HashSet<char>,
    pub ranges: Vec<std::ops::RangeInclusive<u32>>,
}

impl CodepointSet {
    /// Whether `c` is covered by a listed singleton or range.
    pub fn contains(&self, c: char) -> bool {
        if self.singles.contains(&c) {
            return true;
        }
        let cp = c as u32;
        self.ranges.iter().any(|r| r.contains(&cp))
    }
}

impl GremlinsConfig {
    /// True when `rel` (a project-root-relative path) falls under any
    /// excluded directory. A bare match on the directory itself counts,
    /// as does anything beneath it; sibling paths sharing a prefix do not.
    pub fn is_excluded(&self, rel: &Path) -> bool {
        self.exclude.iter().any(|dir| {
            let dir = Path::new(dir.trim_end_matches('/'));
            rel == dir || rel.starts_with(dir)
        })
    }
}

/// `[style]` section: opt-in native Rust style checks for `brokkr check`.
///
/// Every knob defaults to `false`, so a project that omits `[style]` (or lists
/// it empty) runs no style checks and sees no behaviour change. Currently one
/// rule; the section exists to grow more.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StyleConfig {
    /// Require a blank line above `if`/`match`/`for`/`while`/`loop`/`spawn`
    /// constructs, honouring the exemption ladder in [`crate::style`].
    pub rust_blank_line_above_control_flow: bool,
}

impl StyleConfig {
    /// True when no rule in the section is enabled - the phase can short out.
    pub fn is_empty(&self) -> bool {
        !self.rust_blank_line_above_control_flow
    }
}

/// `[header]` section: a required file header whose year must be current.
///
/// A file matching `paths` (and not `exempt`) must contain `pattern`, with
/// `{year}` expanded to the current UTC year. A missing header and a stale
/// year both fail the same check. See [`crate::header`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderConfig {
    /// Globs for files that must carry the header (e.g. `crates/**/*.rs`).
    pub paths: Vec<String>,
    /// The required header text. `{year}` expands to the current UTC year.
    pub pattern: String,
    /// Globs for files excused from the check. Empty by default.
    #[serde(default)]
    pub exempt: Vec<String>,
}

/// One `[[textlint]]` rule: forbid a regex `pattern` on lines of files matching
/// `paths`, with bounded predicates and inline/regex exceptions. See
/// [`crate::textlint`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextlintRule {
    /// Human label surfaced in violation output.
    pub name: String,
    /// The forbidden pattern (a linear-time `regex`). A match is a violation.
    pub pattern: String,
    /// Globs for the files this rule scans.
    pub paths: Vec<String>,
    /// Globs for files excused from this rule (checked after `paths`). Use it
    /// to skip docs that deliberately show the forbidden pattern, e.g. a style
    /// guide. Empty by default.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Message shown for each violation.
    pub message: String,
    /// Inline escape hatch: a line containing this literal substring (typically
    /// an author's `// allow-...` comment) is skipped. Optional.
    #[serde(default)]
    pub allow_marker: Option<String>,
    /// Widen `allow_marker` to also suppress a match when the marker appears on
    /// one of the N lines *above* it (0 = same line only, the default). For
    /// markers a rustfmt-wrapped construct pushes off the offending line, e.g.
    /// `// log-period-ok` within 3 lines. Requires `allow_marker`.
    #[serde(default)]
    pub allow_marker_above: usize,
    /// Lines matching any of these regexes are exempt. Optional.
    #[serde(default)]
    pub except: Vec<String>,
    /// Region predicate: only consider lines while the last-seen TOML section
    /// header (`[section]`) equals this. Optional; for `Cargo.toml` rules.
    #[serde(default)]
    pub in_toml_section: Option<String>,
    /// Line predicate: only consider markdown table rows (trimmed line starts
    /// with `|`). Off by default.
    #[serde(default)]
    pub table_row_only: bool,
    /// Region predicate: once a line in a file matches this regex, every
    /// *following* line in that file is exempt (the matching line itself is
    /// still checked). Expresses "ignore everything after `#[cfg(test)]`" for
    /// rules that should not fire inside test modules. Optional.
    #[serde(default)]
    pub skip_after: Option<String>,
    /// File-scope precondition: the rule only fires in files where at least one
    /// line matches this regex. Expresses "flag bare `Instant::now()` only in
    /// files that import `Instant`" - a cheap stand-in for import-awareness.
    /// Optional.
    #[serde(default)]
    pub only_if_file_matches: Option<String>,
    /// Lexical region the `pattern` is scoped to: `code`, `string`, or
    /// `comment`. Rust files only (tokenized with `rustc_lexer`). Off = match
    /// the whole line. `code` never flags a pattern quoted in a comment/string;
    /// `string` targets message text (e.g. a `", got"` phrasing rule). Only
    /// `pattern` is scoped - `allow_marker`, `except`, and the reported line
    /// stay the physical line. Optional.
    #[serde(default)]
    pub region: Option<String>,
    /// Match `pattern` against whole `use ...;` statements instead of physical
    /// lines: a rustfmt-wrapped import is reconstructed onto one line (comments
    /// stripped) before matching, so patterns like `use tracing::.*warn` catch
    /// a multi-line `use` block. The violation is reported at the `use` line,
    /// and `allow_marker` matches on any physical line of the statement.
    /// Rust-only. Off by default.
    #[serde(default)]
    pub join_wrapped_use: bool,
}

/// `[manifest]` section: native structural `Cargo.toml` conventions checked by
/// `brokkr check` on the `[style]` model - discrete named toggles, not a rule
/// DSL. Inert unless at least one check is enabled. Each check reads a manifest
/// with `toml_edit` (comment- and order-preserving), so it can see structure a
/// value-only parse discards (blank-line groups, key order).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ManifestConfig {
    /// Globs for the manifests to check. Empty = `["**/Cargo.toml"]`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Globs for manifests excused from every check. Empty by default.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Require dependency keys to be sorted within each blank-line-separated
    /// group of a `[dependencies]` / `[dev-dependencies]` /
    /// `[build-dependencies]` / `[workspace.dependencies]` table (target-cfg
    /// variants included). Off by default.
    #[serde(default)]
    pub sort_dependencies: bool,
    /// Required relative order of top-level sections. Only sections present and
    /// named here are constrained (others may appear anywhere); a listed
    /// section appearing before an earlier-listed one is a violation. Empty =
    /// off. `cargo-fuzz = true` crates are exempt.
    #[serde(default)]
    pub section_order: Vec<String>,
    /// Required relative order of `[lib] crate-type` entries (e.g.
    /// `["rlib", "staticlib", "cdylib"]`). Only listed values are constrained.
    /// Empty = off. `cargo-fuzz = true` crates are exempt.
    #[serde(default)]
    pub crate_type_order: Vec<String>,
    /// Required relative order of `[package]` keys (e.g. `["name", "version",
    /// "edition", ...]`). Only listed keys are constrained. Empty = off.
    /// `cargo-fuzz = true` crates are exempt.
    #[serde(default)]
    pub package_field_order: Vec<String>,
    /// When a `[lib]` or `[[bin]]` target is present, require `[lints] workspace
    /// = true`. Off by default. `cargo-fuzz = true` crates are exempt.
    #[serde(default)]
    pub lints_workspace_required: bool,
    /// Require every `[[bin]]` to set `doc = false`. Off by default.
    #[serde(default)]
    pub bin_doc_false: bool,
    /// Require every `[[bin]]` to set `test = false`. Off by default.
    #[serde(default)]
    pub bin_test_false: bool,
    /// Require every `[[example]]` to set `doc = false`. Off by default.
    #[serde(default)]
    pub example_doc_false: bool,
    /// Require `[package.metadata.cargo-machete] ignored` entries to each name a
    /// declared dependency (in any dependency table). Off by default.
    #[serde(default)]
    pub cargo_machete_ignored_declared: bool,
    /// `[[manifest.version_align]]` groups: sets of crates whose version
    /// requirements must agree at a chosen granularity. Empty by default.
    #[serde(default)]
    pub version_align: Vec<VersionAlign>,
}

/// One `[[manifest.version_align]]` group.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct VersionAlign {
    /// Crate names whose versions must agree. Absent crates are skipped, so a
    /// group only fires when two or more of them are actually present.
    #[serde(default)]
    pub crates: Vec<String>,
    /// `"major"` or `"minor"` (default). How much of the version must match.
    #[serde(default)]
    pub granularity: String,
}

/// One `[[dependency_rule]]` entry: a direct Cargo dependency that must
/// not exist.
///
/// `from` names one or more workspace packages, or the wildcard `"*"` for
/// every workspace package. `forbid` names package dependencies that are
/// illegal for those packages - typically an external crate you want to keep
/// out of a boundary (e.g. `forbid = "tokio"` for sync core crates). `except`
/// removes packages from the `from` set, which pairs with `from = "*"` to
/// express "no crate may depend on X, except these". `from`, `forbid`, and
/// `except` each accept a single string or an array of strings.
///
/// `kinds` and `optional` scope *which* occurrence of a forbidden dependency
/// trips the rule - both filter the same present-dep match (absence is never a
/// violation), so they express manifest conventions like "tokio is fine as a
/// dev-dependency" or "tokio must be optional":
///
/// - `kinds` restricts the rule to specific dependency kinds (`normal` / `dev`
///   / `build`). Unset = every kind (the historical behavior). `kinds =
///   ["normal"]` means a forbidden crate is only a violation as a regular
///   dependency - a `[dev-dependencies]` entry is allowed. Kept an explicit
///   opt-in rather than a default so a rule never silently stops flagging a
///   dev-dep it used to catch.
/// - `optional` (when set) restricts the rule to deps whose `optional` flag
///   equals it. `optional = false` matches only non-optional deps, i.e. "if
///   this crate is present it must be `optional = true`, or it's a violation".
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyRule {
    /// Optional human label surfaced in violation output.
    pub name: Option<String>,
    /// Workspace package(s) whose direct dependency list is checked, or `"*"`
    /// for all workspace packages.
    #[serde(deserialize_with = "string_or_vec")]
    pub from: Vec<String>,
    /// Package names that may not appear in `from`'s direct dependencies.
    #[serde(deserialize_with = "string_or_vec")]
    pub forbid: Vec<String>,
    /// Workspace packages to drop from the `from` set (mainly for
    /// `from = "*"`). Empty by default.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub except: Vec<String>,
    /// Dependency kinds the rule applies to (`normal` / `dev` / `build`). Empty
    /// = all kinds. Validated when the phase runs. Single string or array.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub kinds: Vec<String>,
    /// When set, only match deps whose `optional` flag equals this. `false`
    /// expresses "must be optional". Unset = match regardless.
    #[serde(default)]
    pub optional: Option<bool>,
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
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckEntry {
    pub name: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub no_default_features: bool,
    #[serde(default)]
    pub build_packages: Vec<String>,
    /// Packages to scope the sweep's `cargo clippy` / `cargo test`
    /// invocation to, emitted as `-p <pkg>` per entry. Required to use
    /// `features` in a virtual workspace (one with no root package):
    /// cargo rejects `--features` at the root, so the sweep must name the
    /// package(s) the features belong to. Distinct from `build_packages`,
    /// which only pre-builds CLI binaries for the test phase.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Packages to omit from the **test phase** of this sweep, emitted as
    /// `cargo test --workspace --exclude <pkg>`. Clippy is left workspace-wide
    /// (unscoped) - only tests are trimmed. For workspace members that can't
    /// link a test binary in this environment (e.g. a crate whose test
    /// executable needs a system library absent on the build host), which
    /// would otherwise fail the whole test phase. Mutually exclusive with
    /// `packages` (you can't both select `-p` and exclude from `--workspace`).
    #[serde(default)]
    pub test_exclude_packages: Vec<String>,
    /// Environment variables exported to every cargo subprocess this sweep
    /// runs - clippy, the test-phase pre-build, and the test run. Lets a
    /// sweep pin a build-affecting var (e.g. a codegen toggle) so
    /// `brokkr check` is reproducible without the caller exporting it by
    /// hand. Merged under any profile `env`, with the entry winning on a
    /// key collision.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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
/// - `debug` flips `brokkr test`'s cargo profile from release (the
///   default) to dev, so projects whose tests aren't profile-sensitive can
///   pin the faster-compiling build without typing `--debug` every time.
///   The CLI wins: `--debug` forces dev, `--release` forces release, and
///   only when neither is passed does this field decide.
/// - `doctests` decides whether the test phase runs doctests. It defaults
///   to `false` because every brokkr-managed project runs its CI under
///   cargo-nextest, which never executes doctests - so a `brokkr check`
///   that ran them would fail (or pass) on a signal CI cannot see, breaking
///   CI parity. With the default, brokkr scopes `cargo test` to `--tests`
///   (lib + bins + integration, no doctests) unless the sweep already
///   carries an explicit target selector (`--test <name>`), which excludes
///   doctests on its own. Set `doctests = true` to opt a project back in to
///   the full `cargo test` default (doctests included).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TestConfig {
    pub default_package: Option<String>,
    pub default_profile: Option<String>,
    #[serde(default)]
    pub debug: bool,
    #[serde(default)]
    pub doctests: bool,
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
///
/// `locations_on_ways` and `force_sorted` are assertions about *this file*,
/// not about a pipeline config: one says the PBF carries node coordinates
/// embedded in ways, the other that its nodes are monotonic. Elivagar reads
/// both from the PBF header and the flags only force what the header fails to
/// declare, so they belong with the variant that has the property rather than
/// in a `[<host>.tilegen.<name>]` block - a block would otherwise have to know
/// which variant it was about to be run against.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct PbfEntry {
    pub file: String,
    #[serde(alias = "sha256")]
    pub xxhash: Option<String>,
    pub seq: Option<u64>,
    /// Force `--locations-on-ways` when the PBF header does not declare it.
    #[serde(default)]
    pub locations_on_ways: bool,
    /// Force the compact node store without the PBF sort header.
    #[serde(default)]
    pub force_sorted: bool,
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

/// The blessed reference archive for a dataset: a gate-passing (incl. human
/// QA) PMTiles output that `regress` diffs the current build against. Written
/// by `brokkr bless` and lives under `data/blessed/` (gitignored); the repo
/// carries only this registration. `commit` is the source commit the archive
/// was built at (derivable from the filename but kept for provenance).
/// Singular per dataset - one pmtiles variant exists today; a map waits for a
/// second one.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct BlessedEntry {
    pub file: String,
    pub commit: String,
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
    /// The blessed reference archive for `regress` (singular). Written by
    /// `brokkr bless`.
    #[serde(default)]
    pub blessed: Option<BlessedEntry>,
    /// Additional historical snapshots keyed by snapshot name (e.g. a date
    /// like "20260411"). The reserved name "base" is rejected at parse time
    /// because it's the CLI sentinel for the legacy top-level data.
    #[serde(default)]
    pub snapshot: HashMap<String, Snapshot>,
}

/// One ocean input, in elivagar's `--ocean` spec spelling.
///
/// Shapefiles name the zoom band they serve; the artifact is a bare path. The
/// band is part of the statement rather than implied by the flag name, so the
/// recorded invocation says what coverage it claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OceanSpec {
    /// `z0-z14:<file.shp>` - one shapefile serves every zoom.
    ShapefileAll(String),
    /// `z0-z7:<file.shp>` - the pre-generalized low-zoom shapefile.
    ShapefileLow(String),
    /// `z8-z14:<file.shp>` - the full-resolution shapefile.
    ShapefileHigh(String),
    /// `<file.pmtiles>` - the precomputed world-ocean artifact.
    Artifact(String),
}

impl OceanSpec {
    /// Parse one brokkr.toml `ocean` entry.
    ///
    /// Paths are bare (relative to the host's `data` dir) here; the `data/`
    /// prefix in elivagar's own docs belongs to raw shell invocations. Every
    /// other path in brokkr.toml resolves against `data`, and a `data/`-
    /// prefixed one would silently break on a host whose data dir is not
    /// literally `data`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.split_once(':') {
            Some(("z0-z14", f)) => Ok(Self::ShapefileAll(f.to_owned())),
            Some(("z0-z7", f)) => Ok(Self::ShapefileLow(f.to_owned())),
            Some(("z8-z14", f)) => Ok(Self::ShapefileHigh(f.to_owned())),
            Some((band, _)) => Err(format!(
                "unknown zoom band '{band}': elivagar implements one split, at \
                 z7/z8, so the only accepted bands are z0-z14, z0-z7 and z8-z14"
            )),
            None if raw.ends_with(".pmtiles") => Ok(Self::Artifact(raw.to_owned())),
            None if raw.ends_with(".shp") => Err(format!(
                "shapefile '{raw}' needs a zoom band prefix (z0-z14:, z0-z7: or z8-z14:)"
            )),
            None => Err(format!(
                "'{raw}' is neither a zoom-banded shapefile nor a .pmtiles artifact"
            )),
        }
    }

    /// The bare path this spec names.
    pub fn file(&self) -> &str {
        match self {
            Self::ShapefileAll(f)
            | Self::ShapefileLow(f)
            | Self::ShapefileHigh(f)
            | Self::Artifact(f) => f,
        }
    }

    /// Re-render as an elivagar `--ocean` value, with `file` substituted.
    pub fn render(&self, file: &str) -> String {
        match self {
            Self::ShapefileAll(_) => format!("z0-z14:{file}"),
            Self::ShapefileLow(_) => format!("z0-z7:{file}"),
            Self::ShapefileHigh(_) => format!("z8-z14:{file}"),
            Self::Artifact(_) => file.to_owned(),
        }
    }
}

/// A named elivagar tilegen contract: `[<host>.tilegen.<name>]`.
///
/// This is the whole of what a tilegen run is configured by. Nothing is
/// inferred from the filesystem and there are no override flags - a run's
/// behaviour is a function of the named block and its input, and of nothing
/// else. brokkr used to auto-detect the ocean shapefiles from `data/`, which
/// meant two runs of the same binary on the same PBF could produce different
/// ocean geometry with nothing in the recorded invocation saying which; on
/// 2026-07-14 a denmark archive was blessed as the regress baseline while
/// `ocean-tiles.pmtiles` was absent, and every gate passed.
///
/// Ordering matters here: `BTreeMap` (not `HashMap`) keeps the expanded argv
/// byte-identical across runs of the same block, which is what lets
/// `brokkr results --grep` select an arm off `cli_args`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct TilegenConfig {
    /// Ocean inputs. Absent or empty means no ocean - the same statement
    /// elivagar's removed `--no-ocean` used to make.
    #[serde(default)]
    pub ocean: Vec<String>,
    /// Gzip level 0-10. A *base*: elivagar clamps low zooms up and caps
    /// z13/z14 down, per `config.tile.compression_policy` in provenance.
    pub compression_level: Option<u32>,
    pub tile_format: Option<String>,
    pub tile_compression: Option<String>,
    pub compress_sort_chunks: Option<String>,
    #[serde(default)]
    pub in_memory: bool,
    /// `-j`. A host tuning knob, not part of the comparability contract -
    /// `reference/metadata.md` bars thread counts from the provenance block
    /// because same-commit builds must be byte-identical.
    pub threads: Option<u32>,
    /// Sizes accept `256M`, `1G`, or raw bytes.
    pub sort_budget: Option<String>,
    pub way_budget: Option<String>,
    pub assemble_budget: Option<String>,
    /// Polygon layers getting shared-edge seam reconciliation, layer -> maxzoom.
    #[serde(default)]
    pub seam_reconcile_layers: BTreeMap<String, u32>,
    /// Default fanout cap for all polygon layers. 0 or absent = uncapped.
    pub fanout_cap_default: Option<u32>,
    /// Per-layer fanout caps; take precedence over the default.
    #[serde(default)]
    pub fanout_caps: BTreeMap<String, u32>,
    pub polygon_simplify_factor: Option<f64>,
    /// Expert debugging only; may cause severe IO/RSS degradation.
    #[serde(default)]
    pub allow_unsafe_flat_index: bool,
}

impl TilegenConfig {
    /// Parse the `ocean` entries into specs.
    pub fn ocean_specs(&self) -> Result<Vec<OceanSpec>, String> {
        self.ocean.iter().map(|s| OceanSpec::parse(s)).collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct HostConfig {
    pub data: Option<String>,
    pub scratch: Option<String>,
    /// Durable, commit-addressable tilegen output store (map-data projects).
    /// Defaults to `data/tilegen`. Kept SEPARATE from `scratch` on purpose:
    /// elivagar wipes its `--tmp-dir` (`<data>/tilegen_tmp`) at every run
    /// start, and on some hosts `scratch` is configured to that same dir, so
    /// renamed `<dataset>-<commit>.pmtiles` archives written into scratch were
    /// destroyed by the next run. `output` is never wiped by a run; retention
    /// bounds its growth instead.
    pub output: Option<String>,
    pub target: Option<String>,
    pub port: Option<u16>,
    pub drives: Option<DriveConfig>,
    /// Cargo features to enable by default for all build commands on this host.
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub datasets: HashMap<String, Dataset>,
    /// Named elivagar tilegen contracts (map-data projects). `tilegen` selects
    /// `default`; there is no override flag, and no block is an error rather
    /// than an implicit bare run.
    #[serde(default)]
    pub tilegen: HashMap<String, TilegenConfig>,
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
/// the corpus tree and the piners-owned pin/keyword registry. OHLCV feeds
/// are *not* config: they are hash-pinned registry content (`[feeds]` in
/// `pins.toml` - the feed is part of a probe's oracle identity). See
/// `docs/commands/corpus.md`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinersConfig {
    /// Build spec for the corpus harness binary. Required to actually run
    /// probes; `--verify-only` works without it.
    pub harness: Option<HarnessConfig>,

    /// Differential-lint corpus config (`[piners.lint]`). Drives
    /// `brokkr lint-corpus` / `brokkr lint-results`. Independent of the
    /// trade corpus above; shares only `corpus_root` for snippet
    /// resolution. See `docs/commands/lint-corpus.md`.
    pub lint: Option<LintConfig>,

    /// Root of the piners-owned corpus tree (vendor submodules +
    /// first-party probe dirs), resolved relative to `brokkr.toml`.
    /// Pinned probe and feed paths in `pins.toml` resolve under here.
    /// Defaults to `corpus`.
    pub corpus_root: Option<PathBuf>,

    /// Directory holding the piners-owned registry: `pins.toml` (the
    /// canonical id -> path+xxh128 universe plus the `[feeds]`/`[roots]`
    /// tables) and one `*.toml` per keyword (id lists). Resolved relative
    /// to `brokkr.toml`. Defaults to `corpus-registry`.
    pub registry_dir: Option<PathBuf>,
}

impl PinersConfig {
    /// Corpus tree root, defaulting to `corpus`.
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

/// `[piners.lint]` - the differential-lint corpus.
///
/// brokkr cargo-builds `package`/`binary` from the dirty tree (the
/// validator under test) and invokes `<bin> <subcommand> <file> --format
/// json` per probe; `pine_lint_bin` is the pre-installed external partner.
/// Snippet paths in `lints.toml` resolve under `[piners] corpus_root`. See
/// `docs/commands/lint-corpus.md`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LintConfig {
    /// Cargo package brokkr builds from the dirty tree (`cargo build
    /// --package`). The validator under test.
    pub package: String,

    /// Which `[[bin]]` inside `package` to invoke. Defaults to `package`.
    #[serde(default)]
    pub binary: Option<String>,

    /// The validate subcommand on that binary. Defaults to `validate`.
    #[serde(default)]
    pub subcommand: Option<String>,

    /// Cargo features for the validator build. Empty = cargo defaults.
    #[serde(default)]
    pub features: Vec<String>,

    /// Build the validator with the dev profile by default. CLI
    /// `--debug`/`--release` still override per-invocation.
    #[serde(default)]
    pub debug: Option<bool>,

    /// Registry directory holding `lints.toml` + `<keyword>.toml`, resolved
    /// relative to `brokkr.toml`. Defaults to `corpus-lint-registry`.
    pub registry_dir: Option<PathBuf>,

    /// Directory `--reseed` walks for `.pine` snippets (recursively, keyed by
    /// file stem), resolved relative to `brokkr.toml`. Must live under
    /// `[piners] corpus_root`. Defaults to `registry_dir`.
    pub snippets_dir: Option<PathBuf>,

    /// The external pine-lint validator. Defaults to `pine-lint` on PATH.
    pub pine_lint_bin: Option<String>,
}

impl LintConfig {
    /// The validate subcommand, defaulting to `validate`.
    pub fn subcommand(&self) -> &str {
        self.subcommand.as_deref().unwrap_or("validate")
    }

    /// Registry directory, defaulting to `corpus-lint-registry`.
    pub fn registry_dir(&self) -> &Path {
        self.registry_dir
            .as_deref()
            .unwrap_or_else(|| Path::new("corpus-lint-registry"))
    }

    /// Snippet directory `--reseed` walks, defaulting to [`Self::registry_dir`].
    pub fn snippets_dir(&self) -> &Path {
        self.snippets_dir.as_deref().unwrap_or_else(|| self.registry_dir())
    }

    /// The external pine-lint binary, defaulting to `pine-lint`.
    pub fn pine_lint_bin(&self) -> &str {
        self.pine_lint_bin.as_deref().unwrap_or("pine-lint")
    }
}

#[allow(dead_code)]
pub struct ResolvedPaths {
    pub hostname: String,
    pub data_dir: PathBuf,
    pub scratch_dir: PathBuf,
    /// Durable tilegen output store (default `<root>/data/tilegen`). Distinct
    /// from `scratch_dir` so run-to-run tmp wipes never destroy archives that
    /// `--commit` resolution and `regress`/`bless` depend on.
    pub output_dir: PathBuf,
    pub target_dir: PathBuf,
    pub drives: Option<DriveConfig>,
    pub features: Vec<String>,
    pub datasets: HashMap<String, Dataset>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

