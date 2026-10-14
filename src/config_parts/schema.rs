use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::preflight::{FileDigest, Xxh128};

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
    /// `[[quarantine]]` entries: the justification ledger for coverage
    /// accounting. Under a `certifies =
    /// "complete"` profile, every (sweep, test) pair that no lane runs must
    /// match one of these or the check fails as orphaned. Empty when the
    /// project declares none.
    pub quarantine: Vec<QuarantineEntry>,
    /// Env var names / globs to capture into `run_kv` on every measured
    /// run (as `env.<NAME>` pairs). Supports exact names (`MALLOC_CONF`)
    /// and `PREFIX_*` globs (`PBFHOGG_*`). Empty by default.
    pub capture_env: Vec<String>,
    /// `[gremlins]` config: directories the gremlin scanner skips. Used to
    /// silence vendored / third-party material (reference manuals, imported
    /// docs) that legitimately carries gremlin characters. `None` when the
    /// project has no `[gremlins]` section. See [`GremlinsConfig`].
    pub gremlins: Option<GremlinsConfig>,
    /// `[header]` config: required text anywhere in selected files, with a current-year check.
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
    /// `[rustdoc]` config: its presence turns on `brokkr check`'s `rustdoc`
    /// phase. `None` when the project has no `[rustdoc]` section. See
    /// [`RustdocConfig`].
    pub rustdoc: Option<RustdocConfig>,
    /// `[[script_check]]` gates: run a command and assert its output matches a
    /// sentinel, for pre-commit checks brokkr's native phases can't express.
    /// Empty when the project defines no `[[script_check]]` entries. See
    /// [`ScriptCheck`] and [`crate::script_check`].
    pub script_checks: Vec<ScriptCheck>,
    /// `[deps]` config: tuning for the `brokkr deps` audit. `None` when the
    /// project has no `[deps]` section. See [`DepsConfig`].
    pub deps: Option<DepsConfig>,
    /// `[lints]` config: lint suppressions consumed by *both* build phases
    /// (`brokkr check`'s clippy and test phases, and `brokkr clippy`). `None`
    /// when the project has neither `[lints]` nor its `[clippy]` alias. See
    /// [`LintsConfig`].
    pub lints: Option<LintsConfig>,
    /// `[bin]` config: shared defaults for `brokkr run` and `brokkr install`.
    /// `None` when the project has no `[bin]` section (discovery still
    /// works; the section only curates ambiguity). See [`BinConfig`].
    pub bin: Option<BinConfig>,
    /// Top-level `disable_toolchain = true`: move the project's
    /// `rust-toolchain.toml` (or legacy `rust-toolchain`) aside for the
    /// duration of every brokkr command, so rustup ignores the pin and falls
    /// back to its default. For driving a foreign checkout whose pinned
    /// toolchain we don't have or don't want. `false` by default. See
    /// [`crate::toolchain`].
    pub disable_toolchain: bool,
    /// `[cargo_env]`: variable name -> shell command whose output brokkr sets
    /// on every cargo it launches. See [`crate::cargo_env`].
    pub cargo_env: std::collections::BTreeMap<String, String>,
}

/// `[bin]` section: shared defaults for `brokkr run` and `brokkr install`.
///
/// Both commands discover the workspace's runnables (bin targets, and for
/// `run` also examples) from cargo metadata; this section exists to curate
/// what discovery leaves ambiguous, not to enable the commands.
///
/// - `default` - the target name bare `brokkr run` launches. Without it, a
///   sole runnable runs and several runnables list.
/// - `install` - package names `brokkr install` installs (`cargo install
///   --path <pkg dir>` each). Without it, the workspace's sole bin-carrying
///   package installs; several is an error naming this key.
/// - `debug` - default to the dev profile for `run` and `--debug` for
///   `install`. Unset means release for both (matching `brokkr test`);
///   `--debug`/`--release` on the CLI override either way.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BinConfig {
    /// Target name bare `brokkr run` launches.
    pub default: Option<String>,
    /// Package names `brokkr install` installs.
    pub install: Vec<String>,
    /// Default both commands to the dev profile.
    pub debug: bool,
    /// When `brokkr check` runs the install-feature phase (compile the
    /// `install` packages under package-mode feature unification, the way
    /// `cargo install` resolves them). Unset means `gate`. Only meaningful
    /// alongside `install` - the parser refuses the key without it.
    pub install_feature_check: Option<InstallFeatureCheck>,
}

/// When the install-feature phase runs. `gate` (the default) means only
/// under a `certifies = "complete"` profile - the pre-landing run - because
/// package-mode resolution deliberately compiles duplicate variants of
/// shared dependencies, which is not edit-loop priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallFeatureCheck {
    Off,
    Gate,
    Always,
}

/// `[lints]` section: lint suppressions, applied to every phase that compiles.
///
/// Spelled `[clippy]` historically, and that spelling still parses (the two are
/// unioned). The rename is the honest name for what the section does: a lint
/// that stops a *build* is not a clippy-phase concern. A suppression that
/// cleared clippy but not the test build left `brokkr check` green on one phase
/// and red on the other for the identical diagnostic, with no key that reached
/// the second - so both phases now read this one section.
///
/// - `allow` - lint names suppressed on every sweep. The clippy phase passes
///   them as `-A <lint>` after `--cap-lints=warn`; the test phase, which has no
///   rustc passthrough of its own, routes them through
///   [`crate::rustflags`]. The escape hatch for driving a project whose own
///   tooling runs a *pinned* toolchain: under `disable_toolchain` brokkr builds
///   on the host's (newer) compiler, which surfaces lints upstream's CI cannot
///   see and upstream code cannot be expected to satisfy - and under a project
///   `-Dwarnings` those arrive as hard compile errors. Suppression happens at
///   the compiler, so the any-diagnostic-fails rule needs no carve-outs; each
///   phase announces the allowed lints up front so a narrowed gate never reads
///   as a full one. Accepts both `clippy::` lints and plain rustc names.
/// - `allow_exact` - sited suppressions, `"lint@path"` entries filtered on
///   brokkr's side of the pipe. `allow` acts at the compiler CLI level, and a
///   source site carrying its own lint-level attribute can defeat it (the
///   `#[expect]`-sibling interaction documented in `docs/commands/check.md`).
///   An `allow_exact` entry instead drops matching diagnostics at JSON
///   ingestion - after clippy has spoken, before the pass/fail decision - so
///   it holds regardless of attribute games at the site. Deliberately narrow:
///   one lint in one file (every occurrence in that file), never
///   workspace-wide, and no `-A` is injected for it.
///
///   **In the test phase the file scoping cannot be honoured.** A lint that
///   fails the build fails it during compilation, before any diagnostic reaches
///   brokkr to be filtered, and `-A` has no path-scoped form. So the test phase
///   takes the *lint name* of each entry and suppresses it across that build,
///   and says so on the notice line. The narrow guarantee still holds where it
///   can be enforced (the clippy phase); the alternative was an `allow_exact`
///   that silently does nothing about the failure it was written for.
#[derive(Debug, Clone, Default)]
pub struct LintsConfig {
    /// Parsed lint names to suppress (`-A <lint>`) on every sweep, both phases.
    pub allow: Vec<check::LintName>,
    /// Sited suppressions (`"lint@path"`), filtered at diagnostic ingestion in
    /// the clippy phase, degraded to a build-wide `-A <lint>` in the test phase.
    pub allow_exact: Vec<SitedAllow>,
}

impl LintsConfig {
    /// Union of two parsed sections, for the `[lints]` / `[clippy]` alias.
    pub fn merge(&mut self, other: Self) {
        self.allow.extend(other.allow);
        self.allow_exact.extend(other.allow_exact);
    }
}

/// One `[lints] allow_exact` entry: a lint suppressed in one file only.
///
/// Parsed from the `"lint@path"` string form; the split and both halves are
/// validated during configuration loading, so a constructed value is always
/// well-formed. `path` is relative to the tree cargo compiles in (the build
/// root) - exactly the path clippy's own diagnostics carry.
#[derive(Debug, Clone)]
pub struct SitedAllow {
    /// Parsed lint name (`clippy::`-qualified or plain rustc name).
    pub lint: check::LintName,
    /// Build-root-relative file the suppression is scoped to.
    pub path: String,
}

impl SitedAllow {
    /// Parse the `"lint@path"` form, rejecting anything that isn't a bare
    /// lint name plus a relative path - the same flag-smuggling guards as
    /// `allow`, and an explicit rejection of the `@`-less form so a blanket
    /// allow can't land in the sited key by accident.
    pub fn parse(s: &str) -> Result<Self, String> {
        let Some((lint, path)) = s.split_once('@') else {
            return Err(format!(
                "allow_exact entry '{s}' has no '@' - the form is \
                 \"lint@path\" (for a workspace-wide suppression use `allow`)."
            ));
        };
        if lint.is_empty() || lint.starts_with('-') || lint.chars().any(char::is_whitespace) {
            return Err(format!(
                "allow_exact entry '{s}' does not start with a bare lint name \
                 (e.g. \"clippy::unused_async@src/lib.rs\")."
            ));
        }
        if path.is_empty()
            || Path::new(path).is_absolute()
            || path.chars().any(char::is_whitespace)
            || path.contains('@')
        {
            return Err(format!(
                "allow_exact entry '{s}' does not end with a single relative \
                 path (e.g. \"clippy::unused_async@src/lib.rs\")."
            ));
        }
        Ok(Self {
            lint: check::LintName::parse(lint),
            path: path.to_string(),
        })
    }
}

impl std::fmt::Display for SitedAllow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.lint.as_str(), self.path)
    }
}

impl<'de> Deserialize<'de> for SitedAllow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
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

/// `[header]` section: required text anywhere in selected files, with a current-year check.
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

/// A context-window gate on a `[[textlint]]` match: filter an already-produced
/// pattern hit by the raw physical lines around it. `lines` is the window depth
/// N (must be >= 1); `pattern` is a regex matched against each raw line in the
/// window (`is_match` per line, no region filtering, no `use`-joining).
///
/// All four gate fields (`except_above` / `except_below` / `require_above` /
/// `require_below`) have the same behavior: the match is suppressed iff the
/// pattern is found in the window; the names differ only to document intent.
///
/// Because the check is per-line, a context pattern that only matches a full
/// single-line attribute can miss a rustfmt-wrapped one
/// (`#[cfg(\n    not(madsim)\n)]`). Write context patterns fragment-tolerant
/// (match `madsim` or `cfg\(test\)`, not the whole attribute) so a wrapped
/// attribute still suppresses within the window. This is the same per-line
/// blindness as the upstream awk hooks, so parity holds.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextWindow {
    /// Window depth: how many physical lines above/below the match to scan.
    /// Must be >= 1 (validated when the rule compiles).
    pub lines: usize,
    /// Regex matched against each raw physical line in the window.
    pub pattern: String,
}

/// How a [`ScriptCheck`]'s captured output is compared to its `expect`
/// sentinel. See [`crate::script_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchMode {
    /// The whole captured stream (trimmed) must equal `expect` (trimmed).
    /// Suits quiet-on-success lints that print only the sentinel.
    Exact,
    /// The last non-empty line (trimmed) must equal `expect` (trimmed).
    /// The default - tolerates progress output above a final verdict line.
    #[default]
    LastLine,
    /// The captured stream must contain `expect` as a substring.
    Contains,
}

/// Which captured stream(s) a [`ScriptCheck`] matches against. See
/// [`crate::script_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stream {
    /// Match against stdout only (the default).
    #[default]
    Stdout,
    /// Match against stderr only.
    Stderr,
    /// Match against stdout and stderr concatenated (stdout, a newline, then
    /// stderr). For tools that split progress and results across the two.
    Both,
}

/// Where in the `brokkr check` pipeline a [`ScriptCheck`] runs. One value per
/// entry - an entry runs once, at the point it names. See
/// [`crate::script_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// With the other convention phases, before clippy builds anything (the
    /// default, and where every script-check ran before `stage` existed).
    /// Right for cheap source-level gates: they fail before the run has spent
    /// any time compiling.
    #[default]
    PreClippy,
    /// After clippy, before the test phase. For gates that want a
    /// clippy-clean tree but shouldn't wait on the test suite.
    PreTest,
    /// After the test phase and the coverage audit. Skipped entirely when the
    /// test phase failed - the tests fail fast, so a post-test gate would be
    /// judging a half-run tree.
    PostTest,
    /// Started with the convention phases and run concurrently with them, with
    /// clippy, rustdoc and the prepare phase's builds, and with every other
    /// background entry; joined before any test binary is listed or run. For a
    /// slow gate that reads nothing the run writes and writes nothing the run
    /// reads - the project's word, since brokkr cannot tell from a command.
    Background,
}

/// The shape a [`ScriptCheck`]'s output is declared to have, which decides how
/// a failure is rendered. Declared, never sniffed: guessing wrong hides the one
/// line that mattered, and a bespoke analyser is free to print the word `error`
/// in prose. See [`crate::script_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Diagnostics {
    /// Unstructured output with no parseable levels (the default, and what
    /// every entry written before this key had). A failure prints the captured
    /// streams verbatim.
    #[default]
    Opaque,
    /// rustc-shaped output: `error:` / `warning:` blocks at column zero, each
    /// running until the next such line. A failure prints the `error` blocks
    /// alone, then a line counting the warnings it did not print.
    Rustc,
}

/// One `[[script_check]]` entry: run `command` (via `sh -c`) and pass iff its
/// output matches the `expect` sentinel per `match`/`stream`. A gate for
/// pre-commit checks that brokkr's native phases can't express. Asserting on a
/// success sentinel (not the exit code) catches a check silently stubbed to
/// exit 0. See [`crate::script_check`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptCheck {
    /// Human label surfaced in phase output.
    pub name: String,
    /// The command line, run as `sh -c "<command>"` with cwd = the code tree.
    pub command: String,
    /// The success sentinel the output must match.
    pub expect: String,
    /// How `expect` is matched against the output. Defaults to `last-line`.
    #[serde(rename = "match", default)]
    pub match_mode: MatchMode,
    /// Which captured stream(s) to match. Defaults to `stdout`.
    #[serde(default)]
    pub stream: Stream,
    /// Where in the check pipeline this entry runs. Defaults to `pre-clippy`.
    #[serde(default)]
    pub stage: Stage,
    /// The output shape a failure is rendered against. Defaults to `opaque`.
    #[serde(default)]
    pub diagnostics: Diagnostics,
}

/// One `[[textlint]]` rule: forbid a regex `pattern` on lines of files matching
/// `paths`, with bounded predicates and inline/regex exceptions. See
/// [`crate::textlint`].
///
/// Every field but `name`/`pattern`/`message` can also come from a shared
/// `[textlint_preset.<name>]` block named by the rule's `preset` key - a
/// parse-time merge, so by the time this struct exists the preset is already
/// folded in and there is no runtime notion of a preset.
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
    /// Restrict `only_if_file_matches` to lines at or above each candidate match
    /// (mirroring the upstream hook's `sed -n "1,<line>p"`), instead of scanning
    /// the whole file. The precondition is then evaluated per match: an import
    /// that only appears *below* the match line - e.g. a `use tokio::net::...`
    /// inside a test module further down - no longer arms the rule for correct
    /// production code above it. Requires `only_if_file_matches`. Off by default
    /// (whole-file scan).
    #[serde(default)]
    pub only_if_file_matches_above: bool,
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
    /// Context gate: suppress the match iff `pattern` is found within `lines`
    /// physical lines *above* it (excluding the match line, clamped at the file
    /// top). All four gates have the same behavior: the match is suppressed iff
    /// the pattern is found in the window; the names differ only to document
    /// intent. Reads raw physical text (no region/join). Optional. Its customer
    /// is a preceding `#[cfg(...)]` attribute exempting the site - write the
    /// pattern fragment-tolerant so a rustfmt-wrapped attribute still matches.
    #[serde(default)]
    pub except_above: Option<ContextWindow>,
    /// Context gate: suppress the match iff `pattern` is found within `lines`
    /// physical lines *below* it (excluding the match line, clamped at the file
    /// bottom). All four gates have the same behavior: the match is suppressed
    /// iff the pattern is found in the window; the names differ only to document
    /// intent. Reads raw physical text (no region/join). Optional.
    #[serde(default)]
    pub except_below: Option<ContextWindow>,
    /// Context gate: suppress the match iff `pattern` is found within `lines`
    /// physical lines *above* it (excluding the match line, clamped at the file
    /// top). All four gates have the same behavior: the match is suppressed iff
    /// the pattern is found in the window; the names differ only to document
    /// intent. Reads raw physical text (no region/join). Optional.
    #[serde(default)]
    pub require_above: Option<ContextWindow>,
    /// Context gate: suppress the match iff `pattern` is found within `lines`
    /// physical lines *below* it (excluding the match line, clamped at the file
    /// bottom). All four gates have the same behavior: the match is suppressed
    /// iff the pattern is found in the window; the names differ only to document
    /// intent. Reads raw physical text (no region/join). Optional. Its customer
    /// is a required token that must follow the match (e.g. `biased;` inside a
    /// `tokio::select!` block), a violation unless it appears in the window.
    #[serde(default)]
    pub require_below: Option<ContextWindow>,
}

/// `[rustdoc]` section: opt into the `rustdoc` phase of `brokkr check`, which
/// runs `cargo doc --no-deps --message-format=json` per build shape and fails on
/// any diagnostic, rendered like clippy's. Rustdoc is the one compiler pass
/// clippy and the tests never run, so without it a project's rustdoc lints are
/// declared and never evaluated. An empty table is the whole opt-in.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RustdocConfig {
    /// Pass `--document-private-items`, so private items get pages and links to
    /// them resolve. It does not silence `rustdoc::private_intra_doc_links`,
    /// which fires either way; allow that lint in `Cargo.toml` instead. Off by
    /// default.
    #[serde(default)]
    pub document_private_items: bool,
}

/// `[manifest]` section: native structural `Cargo.toml` conventions checked by
/// `brokkr check` as discrete named toggles, not a rule
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
    /// Globs for manifests excused from the *structural* checks only (the same
    /// set a `cargo-fuzz = true` stub skips: `section_order`,
    /// `crate_type_order`, `package_field_order`, `lints_workspace_required`,
    /// and the `[[bin]]`/`[[example]]` flag checks). Unlike `exclude` (which
    /// skips the file entirely), a manifest listed here is still content-checked;
    /// notably `sort_dependencies` still runs. For a placeholder crate whose
    /// section layout is deliberately non-conventional but whose deps should
    /// still be sorted. Empty by default.
    #[serde(default)]
    pub shape_exclude: Vec<String>,
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
    /// `[manifest.adapter_group]`: a comment-labelled group inside the
    /// workspace root's `[workspace.dependencies]` whose members must not be
    /// used by the named crates. `None` when absent.
    #[serde(default)]
    pub adapter_group: Option<AdapterGroup>,
}

/// `[manifest.adapter_group]`: forbid a comment-delimited workspace-dependency
/// group from being used by a set of crates (cargo-conv check 9).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AdapterGroup {
    /// Substring of the comment header that opens the group in
    /// `[workspace.dependencies]` (e.g. `"Adapter dependencies"`).
    pub marker: String,
    /// Package names that must not depend on any crate in that group.
    #[serde(default)]
    pub forbidden_in: Vec<String>,
}

/// `[deps]` section: tuning for the `brokkr deps` audit.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DepsConfig {
    /// Workspace-dependency names the unused-workspace-dep phase should not
    /// flag even when no member references them - dev tools and top-level
    /// members whose use cargo metadata does not attribute. Empty by default.
    #[serde(default)]
    pub workspace_dep_ignore: Vec<String>,
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
/// A rule has one polarity: `forbid` (the listed crates are violations) or
/// `allow` (every crate *not* listed is a violation). `allow` exists because a
/// forbid-list leaf boundary fails open - a crate added later is permitted
/// until someone remembers to extend every list.
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
    /// Exactly one of `forbid` / `allow` is set (enforced by the parser).
    #[serde(default, deserialize_with = "opt_string_or_vec")]
    pub forbid: Option<Vec<String>>,
    /// The allow-list polarity: every in-scope direct dependency of `from`
    /// whose real package name is not listed is a violation. May be empty
    /// (`allow = []` - no in-scope dependencies at all). Unlisted-but-unused
    /// entries are fine: the list states what is permitted, not required.
    #[serde(default, deserialize_with = "opt_string_or_vec")]
    pub allow: Option<Vec<String>>,
    /// Workspace packages to drop from the `from` set (mainly for
    /// `from = "*"`). Empty by default.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub except: Vec<String>,
    /// Dependency kinds the rule applies to (`normal` / `dev` / `build`). Empty
    /// = all kinds. Parsed at load; a single string or an array.
    #[serde(default, deserialize_with = "dependency_kinds")]
    pub kinds: Vec<crate::deps::DependencyKind>,
    /// When set, only match deps whose `optional` flag equals this. `false`
    /// expresses "must be optional". Unset = match regardless.
    #[serde(default)]
    pub optional: Option<bool>,
}

/// `kinds`: one kind or a list, each parsed into a [`crate::deps::DependencyKind`]
/// at load, so an unknown spelling fails the config rather than the phase.
fn dependency_kinds<'de, D>(deserializer: D) -> Result<Vec<crate::deps::DependencyKind>, D::Error>
where
    D: Deserializer<'de>,
{
    string_or_vec(deserializer)?
        .iter()
        .map(|kind| {
            crate::deps::DependencyKind::parse_config(kind).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "unknown kind {kind:?} in `kinds`; expected \"normal\", \"dev\" or \"build\""
                ))
            })
        })
        .collect()
}

fn opt_string_or_vec<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    string_or_vec(deserializer).map(Some)
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
/// so CLI integration tests use binaries built for the same feature set.
///
/// `features` is a configured list, not an argv: each run projects it onto
/// the packages that run selects, dropping a token that only a package
/// outside the run's selection routes (a `-p` narrowing, a package-mode
/// resolution, a support build). See `check_cmd`'s feature routing.
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
    /// Extra `rustc` flags, appended to any inherited `RUSTFLAGS` and scoped
    /// to this sweep's cargo processes only (clippy, pre-build, tests). Given
    /// as a token list (`["--cfg", "madsim"]`). A non-empty value auto-isolates
    /// the sweep into its own target dir (`target/rustflags-<hash>`, keyed by
    /// the flag content) so a global cfg change does not thrash the shared
    /// target cache of the other sweeps; sweeps sharing the same flags share
    /// the dir. Setting `RUSTFLAGS` or `CARGO_TARGET_DIR` in `env` alongside
    /// this is rejected at parse time (one unambiguous source each).
    #[serde(default)]
    pub rustflags: Vec<String>,
    /// Per-check cargo `--test <name>` target filters, ANDed with any profile
    /// `tests` filters (they append, never replace). Scopes the test phase to
    /// named test targets - the curated-subset knob a sim sweep needs.
    #[serde(default)]
    pub tests: Vec<String>,
    /// Per-check libtest `--skip <substr>` filters, ANDed with any profile
    /// `skip` filters.
    #[serde(default)]
    pub skip: Vec<String>,
    /// Per-check libtest positional substring filters (run only tests whose
    /// name contains one of these), ANDed with any profile `only` filters.
    #[serde(default)]
    pub only: Vec<String>,
    /// This entry exists to run a hand-picked subset: its build shape's
    /// non-run pairs are exempt from a `complete` profile's coverage
    /// universe (reported in the trailer, like `test_exclude_packages`),
    /// and a `complete` profile need not reference it at all. Requires
    /// the entry to carry its own `tests`/`skip`/`only` filters - an
    /// unfiltered "curated" entry is just an uncounted full sweep.
    /// Coverage policy only, never part of the build shape.
    #[serde(default)]
    pub curated: bool,
    /// The cargo profile this sweep compiles and runs under. Unset means
    /// the command's own default - dev for `brokkr check`, and `brokkr
    /// test`'s CLI/`[test] debug` resolution - which is what every sweep
    /// did before this key existed.
    ///
    /// This is the only way to express a per-sweep profile split: `[test]
    /// debug` is one project-wide default for `brokkr test` alone, and a
    /// profile lane cannot carry a profile (a lane selects *which tests*
    /// run, never *how they are built*). A repo with a wall-clock contract
    /// that only holds under optimization declares one `[[check]]` entry
    /// with `profile = "release"` and puts the timing tests in it; the rest
    /// of the suite keeps the fast dev build.
    ///
    /// Part of the build shape (the sweep's build shape key):
    /// `cfg(debug_assertions)` decides which code exists, so a dev and a
    /// release sweep of the same features are two different compiles and
    /// two different lint surfaces, and neither may dedupe into the other.
    #[serde(default)]
    pub profile: Option<SweepProfile>,
    /// Run this sweep's test binaries concurrently under a budget of
    /// in-flight tests, instead of letting cargo run them one at a time.
    /// Execution policy only - never part of the build shape, since it
    /// changes neither what is compiled nor which tests are selected.
    /// See [`ParallelBinaries`].
    #[serde(default)]
    pub parallel: Option<ParallelBinaries>,
    /// Which feature graph this sweep resolves against. See
    /// [`FeatureUnification`] - the default (`auto`) is what every sweep
    /// did before this key existed.
    ///
    /// Part of the build shape: package and workspace resolution compile
    /// different code for the same source, so two sweeps differing only
    /// here are two compiles and two lint surfaces, and neither may dedupe
    /// into the other.
    #[serde(default)]
    pub feature_unification: FeatureUnification,
    /// Which test harness runs this sweep. `libtest` (the default) is
    /// brokkr's own lanes - serial, `test_threads`, `parallel`,
    /// `isolation = "process"`. `nextest` hands the sweep to the linked
    /// nextest engine as a process-per-test executor, for suites that rely on
    /// that isolation contract. brokkr synthesizes the engine's config: the
    /// checkout's `.config/nextest.toml` is never read, and brokkr owns the
    /// filters, concurrency and per-test timeout. See [`Harness`].
    ///
    /// NOT part of the compile shape: a nextest and a libtest sweep with
    /// equal compile inputs share the target dir and dedupe in clippy - the
    /// harness decides how tests execute, never what cargo compiles.
    #[serde(default)]
    pub harness: Harness,
    /// This entry runs `cargo test --doc` - doctests and nothing else - with
    /// its selection, features, unification pin and cargo profile. The
    /// doctest twin for a `parallel` workspace sweep: doctests live in
    /// cargo's `--doc` pseudo-target, which no binary fan-out can carry, so
    /// a project that parallelizes its main sweep pairs it with a serial
    /// doc-only entry and keeps `[test] doctests = true` honest.
    ///
    /// Runs doctests regardless of `[test] doctests` (the key IS this
    /// entry's opt-in). Owns cargo target selection outright: `tests` on the
    /// entry and forwarded target selectors are refused, as are entry-level
    /// `skip`/`only` (doctests cannot be enumerated, so a filter on them can
    /// never be audited for liveness). Execution policy, never part of the
    /// build shape.
    #[serde(default)]
    pub doc_only: bool,
}

/// Which test harness runs a `[[check]]` sweep's test phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    /// Brokkr's own libtest lanes.
    #[default]
    Libtest,
    /// The linked nextest engine as a process-per-test executor, under a
    /// config brokkr synthesizes (the checkout's `.config/nextest.toml` is
    /// never read); brokkr owns the compile shape.
    Nextest,
}

/// Which feature graph a sweep's cargo invocations resolve against.
///
/// The three explicit values mirror cargo's own `resolver.feature-unification`
/// vocabulary exactly (`selected`, `workspace`, `package` - verified against
/// cargo's own error text, which lists those three and nothing else), and each
/// **pins** its value on the argv, so the resolution a sweep declares does not
/// depend on whether the consuming repository happens to carry a matching
/// `[resolver]` block in `.cargo/config.toml`. A CLI `--config` outranks a
/// config file, so the pin is authoritative.
///
/// `auto` is brokkr's own policy rather than a cargo mode, which is why it is
/// spelled rather than left as an absent key: it emits no flags in the ordinary
/// case and lets the parallel lane (`src/check_cmd/parallel.rs`) promote an eligible
/// whole-workspace fan-out to `workspace`, which is exactly what every sweep did
/// before this key existed. Absence and `auto` mean the same thing, so a config
/// rewrite that materializes the default cannot change behaviour.
///
/// The distinction that matters: `auto` DEFERS to ambient config, `selected`
/// PINS cargo's selected mode. Spelling the policy as `selected` would promise
/// a pin that the unpromoted case does not perform.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeatureUnification {
    /// Brokkr policy: no flags, except the parallel lane's workspace
    /// promotion. The pre-existing behaviour.
    #[default]
    Auto,
    /// Pin cargo's default: each selected package resolved with only the
    /// features that selection requires.
    Selected,
    /// Pin workspace-wide unification: every member contributes features,
    /// whatever the selection.
    Workspace,
    /// Pin per-package resolution - the shape `cargo install` produces, and
    /// the only one that catches a crate depending on features a sibling
    /// member donates. Requires a non-empty `packages` list, and resolves
    /// one cargo invocation per package (see
    /// [`crate::config::check::CargoUnification::Package`]).
    Package,
}

/// The cargo build profile a `[[check]]` entry may name. `dev` and
/// `release` are cargo's own names for the two built-in profiles;
/// `debug` is accepted as an alias for `dev` because that is what the
/// directory under `target/` is called and what brokkr's `--debug` flag
/// says.
///
/// Deliberately not open to custom `[profile.*]` names: brokkr derives
/// `BROKKR_TEST_BIN_DIR` from the profile's target subdirectory, and
/// cargo's mapping from a custom profile to its directory is not
/// something brokkr can read back reliably.
///
/// Not to be confused with [`crate::build::CargoProfile`], which is the
/// provenance tag stored on a result row (release / java / cmake) and
/// says which *build system* produced a benchmarked binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SweepProfile {
    #[serde(alias = "debug")]
    Dev,
    Release,
}

/// `[test]` section.
///
/// - `default_package` is the cargo package `brokkr test` should pass to
///   `cargo test -p <pkg>` when the user doesn't supply `-p`. Required for
///   multi-crate workspaces (e.g. ratatoskr) where there's no single
///   "obvious" package; optional for single-crate projects that have a
///   built-in default via [`crate::project::Project::cli_package`]. An explicit CLI `-p`
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
    /// The profile `brokkr check --gate` runs. Must name an existing
    /// profile with `certifies = "complete"` (both checked at load time),
    /// so AGENTS.md and pre-commit hooks can say `brokkr check --gate`
    /// and stay correct through profile renames.
    pub gate_profile: Option<String>,
    #[serde(default)]
    pub debug: bool,
    #[serde(default)]
    pub doctests: bool,
    /// Run every cargo invocation of clippy, rustdoc and the prepare phase's
    /// builds one after another, instead of one chain per target directory
    /// with the chains concurrent. For a project whose build scripts write
    /// outside their `OUT_DIR` (into the source tree or a shared directory),
    /// which no target-dir boundary separates.
    #[serde(default)]
    pub serial_builds: bool,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileDef>,
}

/// What a green result from this profile claims.
///
/// `Complete` may print the success verdict and exit 0, and in exchange
/// rejects what narrows the run outside the plan: `skip_phases`, package
/// scoping (`-p`) and trailing `-- ...` test args. The profile's own
/// `skip`/`only` filters are allowed, because the plan is built from them
/// and the coverage audit accounts for every pair they remove. `Partial`
/// gets the narrowing permissions and pays with exit code 10 and a
/// `check partial` verdict that no `passed` grep can mistake for a gate
/// result. Profiles without `certifies` keep binary exit codes and
/// `check passed`, with neither set of permissions or restrictions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Certifies {
    Complete,
    Partial,
}

/// One entry in a profile's `skip` list: either a bare test-name
/// substring (libtest `--skip` semantics, applied by libtest itself) or a
/// package-qualified skip, filtered out of the
/// enumerated set rather than expressed as cargo selection - the only way
/// to distinguish identical module paths in different packages
/// (integration-test paths carry no crate prefix). Qualified entries
/// require `isolation = "process"`: the enumerated set only exists on
/// the isolated path.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SkipSpec {
    Name(String),
    Qualified(QualifiedSkip),
}

/// The `{ package = "...", pattern = "..." }` form of a skip entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualifiedSkip {
    /// Cargo package whose test binaries the pattern applies to.
    pub package: String,
    /// Test-name substring, matched within that package only.
    pub pattern: String,
}

/// How a profile's tests execute relative to process boundaries. The only
/// mode is `process`: one `cargo test … -- --exact <name>` invocation per
/// test, giving the fresh-process guarantee (fresh logger, fresh globals)
/// that CI's nextest provides and a shared-process libtest run cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    Process,
}

/// Check phase identifiers, in execution order. The universe for the
/// `failed_phase` field of the `check --json` summary - every phase can be the
/// one that failed, `coverage` included.
///
/// It is **not** the `skip_phases` universe: `prepare` and `coverage` are not
/// skippable (they run only under a `certifies = "complete"` profile, while
/// `skip_phases` requires `certifies = "partial"`, and they are not gated on
/// the skip predicate anyway). [`skippable_phase_names`] is that universe, so
/// `skip_phases = ["coverage"]` cannot load clean and announce a no-op.
pub fn phase_names() -> impl Iterator<Item = &'static str> {
    crate::check_cmd::Phase::ALL.into_iter().map(crate::check_cmd::Phase::as_str)
}

/// The phases a `skip_phases` list may name: every phase the check phase
/// type marks skippable. `prepare` and `coverage` belong to the complete
/// claim alone, so they are not.
pub fn skippable_phase_names() -> impl Iterator<Item = &'static str> {
    crate::check_cmd::Phase::ALL
        .into_iter()
        .filter(|phase| phase.skippable())
        .map(crate::check_cmd::Phase::as_str)
}

/// One `[[quarantine]]` entry: a justified, counted suppression.
/// `issue` is required, not decorative - it
/// is what turns the list from a graveyard with good manners into a
/// countdown that cannot silently grow, and staleness is mechanical: an
/// entry justifying nothing fails the check.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineEntry {
    /// Substring matched against test names (libtest filter semantics).
    /// Exactly one of `pattern` / `category` must be set.
    pub pattern: Option<String>,
    /// Restrict `pattern` to one package's pairs. Without it, a name-only
    /// pattern absorbs same-named pairs in every package - so a test that
    /// later stops running for an unrelated reason lands as accounted
    /// instead of orphaned, a green tick for something nobody checked.
    /// Only valid alongside `pattern`.
    pub package: Option<String>,
    /// Non-test suppression channels. The only category is `"doctests"`,
    /// which justifies `[test] doctests = false` under a complete profile.
    /// `parse_quarantine` enforces this at config load; this remains a string
    /// because coverage consumers render the category label.
    pub category: Option<String>,
    /// Backlog / issue identifier (e.g. "B14"). Opaque to brokkr; its
    /// stability is the repo's contract.
    pub issue: String,
    /// One line on why the suppression exists.
    pub reason: String,
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
    /// The claim this profile's green makes; permissions derive from it
    /// (see [`Certifies`]). Deliberately NOT inherited through `extends`
    /// and not merged: a claim is per-profile and explicit, or absent.
    pub certifies: Option<Certifies>,
    /// Check phases this profile skips, subtractive (a new native phase
    /// is included everywhere until opted out, never silently excluded).
    /// Requires `certifies = "partial"` - the claim is what grants the
    /// permission. Valid names are [`skippable_phase_names`] - every check
    /// phase but `prepare` and `coverage`, which run only under a complete
    /// claim. Not inherited through `extends`.
    pub skip_phases: Option<Vec<String>>,
    /// Run each of this profile's tests in its own process.
    /// `--test-threads=1` serializes tests
    /// inside one process per test binary; it does not isolate them, and
    /// tests touching process-global state (a global logger) need the
    /// process-per-test guarantee CI's nextest provides. Requires
    /// `test_threads` unset or 1 - per-process execution is serial by
    /// construction. Merges through `extends` like other run-shaping
    /// fields.
    pub isolation: Option<Isolation>,
    /// Compose other profiles as a *list of runs*:
    /// each lane resolves independently and the test phase runs every
    /// lane's sweeps in order, while clippy dedupes on build shape so two
    /// lanes sharing a `[[check]]` entry are linted once. The opposite of
    /// `extends` (which merges): tier1's skips and serial's `only` are
    /// contradictory by construction, so no merge can express them. A
    /// profile with `lanes` carries no run-shaping fields of its own, a
    /// lane may not declare `certifies` (the claim belongs to the
    /// composing profile), and lanes do not nest - all load-time errors.
    pub lanes: Option<Vec<String>>,
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
    /// Skip entries: bare substrings become libtest `--skip` flags;
    /// `{ package, pattern }` tables are package-qualified skips filtered
    /// out of the enumerated set (see [`SkipSpec`]; requires
    /// `isolation = "process"`). Cumulative with a `[[check]]` entry's
    /// own `skip`.
    pub skip: Option<Vec<SkipSpec>>,
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
    pub xxhash: Option<FileDigest>,
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
    pub xxhash: Option<FileDigest>,
}

/// A PMTiles archive entry, keyed by variant name (e.g. "elivagar").
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PmtilesEntry {
    pub file: String,
    pub xxhash: Option<FileDigest>,
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
    /// A plain out-of-git input: one path, one digest, no variant structure.
    ///
    /// The map-data fields above describe a dataset that HAS variants - a PBF
    /// in three forms, a run of OSC deltas anchored to it. A delivered archive
    /// has none of that: it is a path, and the only question worth asking is
    /// whether what is under it drifted since a row was recorded against it.
    ///
    /// May name a DIRECTORY. Deliveries frequently are one (a Databento
    /// delivery is two `.csv.zst` archives beside three JSON descriptors, and
    /// the consuming CLI takes the directory), and a pin that could only name
    /// one file inside it would leave the real input unpinned while reading as
    /// verified. See [`crate::preflight::cached_path_digest`].
    ///
    /// This is not a substitute for the run's own content verification, which
    /// asks a different question - whether the data is what the ledger says.
    /// This asks whether the bytes moved under a recorded row.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Expected XXH128 digest of [`Dataset::path`], parsed as 32 hexadecimal
    /// digits. The runtime determines whether this digest covers a file or a
    /// directory tree from the path on disk.
    ///
    /// XXH128 is brokkr's standard content digest, NOT the SHA-256 that delivery
    /// manifests carry, so transcribing a manifest is not enough - register the
    /// digest brokkr reports. A second hash implementation would buy nothing
    /// against what this defends (drift, not tampering) and would forfeit the
    /// mtime cache that keeps a multi-gigabyte delivery from being re-read.
    ///
    /// Optional, because the digest has to be read off this machine before it
    /// can be registered: `brokkr env` lists an entry without one and prints
    /// the digest computed from disk, ready to paste back.
    #[serde(default)]
    pub xxh128: Option<Xxh128>,
}

impl Dataset {
    /// Absolute path to a plain input, if this dataset is one.
    ///
    /// Relative registrations resolve against the directory holding
    /// `brokkr.toml`; absolute ones pass through, because a delivery commonly
    /// sits outside the repository.
    pub fn resolve_path(&self, project_root: &Path) -> Option<PathBuf> {
        let path = self.path.as_ref()?;
        Some(if path.is_absolute() {
            path.clone()
        } else {
            project_root.join(path)
        })
    }
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

impl DevConfig {
    /// Worktree retention for `hostname`, or the default.
    pub fn worktree_keep(&self, hostname: &str) -> usize {
        self.hosts
            .get(hostname)
            .and_then(|h| h.worktree_keep.map(std::num::NonZeroUsize::get))
            .unwrap_or(crate::worktree_record::DEFAULT_KEEP)
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
    /// How many persistent `--commit` worktrees to keep for this project
    /// before the least-recently-used ones are evicted. Defaults to
    /// [`crate::worktree_record::DEFAULT_KEEP`]. Per host because the disk is a
    /// host property; per project because the store is - so the disk-wide total
    /// is this times the number of projects in play, which is why it is a
    /// growth damper rather than a bound.
    pub worktree_keep: Option<std::num::NonZeroUsize>,
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
    pub tilegen: HashMap<String, project_config::elivagar::TilegenConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriveConfig {
    pub source: Option<String>,
    pub data: Option<String>,
    pub scratch: Option<String>,
    pub target: Option<String>,
}

/// `[ratatoskr.harness]` - self-contained build spec for ratatoskr's
/// orchestration commands (`service`,
/// `mock-serve`, `sync`). Decoupled from `[[check]]`
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


/// The `parallel` key on a `[[check]]` entry: run the sweep's test binaries
/// concurrently instead of letting cargo run them one after another.
///
/// The floor this exists to dissolve: cargo runs each test binary
/// sequentially, and `--test-threads` parallelizes only *within* a binary, so
/// a sweep's wall time cannot fall below the sum, over binaries, of each
/// binary's slowest test. A single-test binary contributes its full duration
/// because it has nobody to overlap with. Running the binaries concurrently
/// collapses that sum to a maximum.
///
/// # Why the budget counts tests, not binaries
///
/// `budget` is the number of tests allowed in flight across the whole sweep,
/// not the number of concurrent binaries. Binaries times `test_threads` is the
/// real in-flight count, and a key that named binaries would let a config ask
/// for seven binaries at eight threads - fifty-six concurrent tests - while
/// looking like it asked for seven. The number a project has already tuned is
/// the total, so the total is what the key takes: each binary claims a slice
/// of the budget and runs with a matching `--test-threads`.
///
/// # What it does not do
///
/// It does not isolate. Tests within one binary still share a process, which
/// is deliberate - a shared-process parallel lane is the only place the
/// process-global-state class (two tests contending over a global logger) is
/// visible at all. Process-per-test dissolves that contention along with the
/// bug's visibility; see `isolation = "process"` for the case where isolation
/// is what is actually wanted.
///
/// What it *adds* is exposure to **machine-global** state: several test
/// binaries at once is several processes at once, so a per-machine singleton
/// (a daemon holding an instance lock, a fixed socket path, a shared state
/// dir) becomes contended where a sequential lane never showed it. There is no
/// per-entry serial-group key for this, because the sweep list already
/// composes: `[[check]]` entries run strictly one after another, so a second
/// unfiltered entry with no `parallel` key is the serial lane, and the
/// enumerated `tests` list on the parallel entry is the opt-in.
///
/// Enumerate the parallel side and leave the complement unfiltered, never the
/// reverse: a binary added later then shows up serial and slow rather than
/// parallel and flaky, and a *stale* name in the enumerated list is a hard
/// cargo error (`no test target named X`) rather than a silent shrink.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParallelBinaries {
    /// Tests allowed in flight across the sweep. A binary claims
    /// `min(its test count, budget)` slots, at least one, and runs under
    /// `--test-threads=<claim>`.
    ///
    /// Unset means **the physical cores sharing one last-level cache** - a
    /// full CCX/CCD on AMD, the die on Intel (see `crate::cpu_topology`).
    /// Not logical CPUs: two tests on the SMT siblings of one core contend
    /// for that core, and on a chiplet part two tests on separate dies pay
    /// fabric latency on every shared line. Both look like parallelism
    /// without delivering it, and a default that overstates the machine
    /// turns a lane meant to save wall time into one that spends it.
    #[serde(default)]
    pub budget: Option<u32>,
}
