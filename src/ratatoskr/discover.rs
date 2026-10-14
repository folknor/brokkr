//! Discovery and frontmatter parsing for service and sync harness scripts.
//!
//! Service scripts default to
//! `<project_root>/crates/app/tests/service-harness/`; sync scripts use the
//! configured sync-script directory or its own convention. Discovery is
//! recursive (`**/*.lua`), so cohorts can live under subdirectories such as
//! `t1/`, `extract/`, etc.
//!
//! Frontmatter is parsed from a contiguous block of `--` line comments
//! at the top of each script, before the first non-comment / non-blank
//! line. Recognized keys:
//!
//! - `description` (free text shown by bare `brokkr service`)
//! - `expected = pass | ignored` (ignored scripts are skipped by default)
//! - `fixture` (optional for service scripts, required for sync scripts)
//! - `protocol` (informational label)
//! - `ceiling = 60s` / `5m` / `1h` / `90` - per-script wall-clock
//!   backstop enforced by brokkr around the whole run. Bare numbers are
//!   read as seconds. Omitted scripts use [`DEFAULT_CEILING`].
//! - `preserve_data_dir = on_success_too` - keep the artefact dir even
//!   when the run succeeds (failures are always preserved).
//!
//! Unknown lowercase annotation keys and duplicate or malformed metadata
//! are rejected.
//! Free-form script comments may follow a blank line after the frontmatter.
//!
//! ```lua
//! -- description: Verify the Service exits with code 73 when the keyfile is missing
//! -- expected: ignored
//! -- ceiling: 30s
//! -- preserve_data_dir: on_success_too
//!
//! local client = harness.spawn(...)
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::DevError;
use crate::ratatoskr::timing::parse_duration;

/// Where service-harness scripts live, relative to the project root.
pub const SCRIPT_DIR: &str = "crates/app/tests/service-harness";

/// Default per-script wall-clock ceiling when no `ceiling` frontmatter
/// is present. Generous enough that a healthy script finishes well
/// inside it, tight enough to catch a runaway before it eats a soak.
pub use crate::ratatoskr::timing::DEFAULT_CEILING;

/// Whether a script participates in a cohort by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Run,
    Skip,
}

impl Selection {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "pass" => Some(Self::Run),
            "ignored" => Some(Self::Skip),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "pass",
            Self::Skip => "ignored",
        }
    }
}

/// Artefact-dir retention override declared in script frontmatter.
///
/// `OnFailureOnly` is the default: the dir is preserved on failure and
/// deleted on success unless the CLI `--keep-artefacts` flag overrides
/// that. `OnSuccessToo` keeps the dir on success too, regardless of CLI
/// flags - useful for scripts whose successful runs are themselves the
/// interesting state to inspect (e.g. soak-stability checks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreserveDataDir {
    #[default]
    OnFailureOnly,
    OnSuccessToo,
}

impl PreserveDataDir {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "on_success_too" => Some(Self::OnSuccessToo),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScriptInfo {
    /// Display name relative to the discovery root, without the `.lua`
    /// extension. Top-level scripts: just the file stem (e.g.
    /// `ping_and_shutdown`). Nested: `t1/journal_replays_after_respawn`.
    pub name: String,
    /// Used by `service` to resolve a script by name and by
    /// `sync --all` to invoke each cohort member; the index listings
    /// only consume `name` / `description` / the rendered selection label.
    pub path: PathBuf,
    pub description: Option<String>,
    pub selection: Selection,
    /// Effective ceiling: either parsed from frontmatter or the default.
    pub ceiling: Duration,
    pub preserve_data_dir: PreserveDataDir,
    pub kind: ScriptKind,
}

/// Frontmatter fields whose meaning depends on the runner.
#[derive(Debug, Clone)]
pub enum ScriptKind {
    /// Service scripts may optionally use sæhrimnir.
    Service {
        fixture: Option<String>,
        protocol: Option<String>,
    },
    /// Every sync script must name the fixture it exercises.
    Sync {
        fixture: String,
        protocol: Option<String>,
    },
}

impl ScriptInfo {
    pub fn fixture(&self) -> Option<&str> {
        match &self.kind {
            ScriptKind::Service { fixture, .. } => fixture.as_deref(),
            ScriptKind::Sync { fixture, .. } => Some(fixture),
        }
    }

    pub fn protocol(&self) -> Option<&str> {
        match &self.kind {
            ScriptKind::Service { protocol, .. } | ScriptKind::Sync { protocol, .. } => {
                protocol.as_deref()
            }
        }
    }
}

/// Discover all service-harness scripts under `<project_root>/<SCRIPT_DIR>/`,
/// recursively.
///
/// Returns an empty list (without error) when the directory does not
/// exist - the harness module has not landed in ratatoskr yet, so a
/// fresh checkout legitimately has no scripts. I/O errors during the
/// walk surface as `DevError::Io`. Scripts are returned sorted by name
/// for stable output.
pub fn discover(project_root: &Path) -> Result<Vec<ScriptInfo>, DevError> {
    discover_service_at(&project_root.join(SCRIPT_DIR))
}

/// Discover sync scripts under an arbitrary root. Sync frontmatter must
/// include a fixture, so the runner never discovers an unusable entry.
pub fn discover_sync_at(root: &Path) -> Result<Vec<ScriptInfo>, DevError> {
    discover_kind_at(root, ScriptType::Sync)
}

/// Discover service scripts under an arbitrary root.
pub fn discover_service_at(root: &Path) -> Result<Vec<ScriptInfo>, DevError> {
    discover_kind_at(root, ScriptType::Service)
}

fn discover_kind_at(root: &Path, script_type: ScriptType) -> Result<Vec<ScriptInfo>, DevError> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    walk(root, root, &mut out, script_type)?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Parse one sync script. Its fixture is mandatory and validated here,
/// before any runner can start a build.
pub fn parse_sync_script(path: &Path, display_name: &str) -> Result<ScriptInfo, DevError> {
    parse_script_as(path, display_name, ScriptType::Sync)
}

/// Parse one service script at `path` into a `ScriptInfo`.
///
/// `display_name` is the name surfaced in user-facing output - typically
/// the path relative to `SCRIPT_DIR` minus the `.lua` extension. When
/// `service` is invoked with an arbitrary script path, the file
/// stem is a reasonable fallback.
pub fn parse_service_script(path: &Path, display_name: &str) -> Result<ScriptInfo, DevError> {
    parse_script_as(path, display_name, ScriptType::Service)
}

fn parse_script_as(
    path: &Path,
    display_name: &str,
    script_type: ScriptType,
) -> Result<ScriptInfo, DevError> {
    let body = fs::read_to_string(path)?;
    build_info(display_name, path.to_path_buf(), &body, script_type)
}

#[derive(Debug, Clone, Copy)]
enum ScriptType {
    Service,
    Sync,
}

fn walk(
    root: &Path,
    dir: &Path,
    out: &mut Vec<ScriptInfo>,
    script_type: ScriptType,
) -> Result<(), DevError> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(DevError::Io(err)),
    };

    for entry in read {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk(root, &path, out, script_type)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("lua") {
            continue;
        }
        let Some(name) = display_name_for(root, &path) else {
            continue;
        };
        let body = fs::read_to_string(&path)?;
        out.push(build_info(&name, path, &body, script_type)?);
    }
    Ok(())
}

/// Build `<relative path under root>` minus `.lua`, with `/` separators.
/// Returns `None` for paths the walker should skip (no usable stem,
/// non-UTF-8 components, etc.).
fn display_name_for(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts: Vec<String> = Vec::new();
    for component in rel.components() {
        let part = component.as_os_str().to_str()?;
        parts.push(part.to_owned());
    }
    let last = parts.last_mut()?;
    if let Some(stripped) = last.strip_suffix(".lua") {
        *last = stripped.to_owned();
    }
    if last.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn build_info(
    name: &str,
    path: PathBuf,
    body: &str,
    script_type: ScriptType,
) -> Result<ScriptInfo, DevError> {
    let parsed = parse_frontmatter(body)
        .map_err(|error| DevError::Config(format!("{}: {error}", path.display())))?;
    let kind = match script_type {
        ScriptType::Service => ScriptKind::Service {
            fixture: parsed.fixture,
            protocol: parsed.protocol,
        },
        ScriptType::Sync => {
            let fixture = parsed.fixture.ok_or_else(|| {
                DevError::Config(format!(
                    "{}: sync frontmatter requires `fixture: <NAME>`",
                    path.display()
                ))
            })?;
            ScriptKind::Sync {
                fixture,
                protocol: parsed.protocol,
            }
        }
    };
    Ok(ScriptInfo {
        name: name.to_owned(),
        path,
        description: parsed.description,
        selection: parsed.selection.unwrap_or(Selection::Run),
        ceiling: parsed.ceiling.unwrap_or(DEFAULT_CEILING),
        preserve_data_dir: parsed.preserve_data_dir.unwrap_or_default(),
        kind,
    })
}

#[derive(Debug, Default)]
struct ParsedFrontmatter {
    description: Option<String>,
    selection: Option<Selection>,
    ceiling: Option<Duration>,
    preserve_data_dir: Option<PreserveDataDir>,
    fixture: Option<String>,
    protocol: Option<String>,
}

/// Parse the leading metadata block. A blank line ends the block after the
/// first metadata line; subsequent comments are ordinary script comments.
fn parse_frontmatter(body: &str) -> Result<ParsedFrontmatter, String> {
    let mut out = ParsedFrontmatter::default();
    let mut in_frontmatter = false;
    let mut seen = std::collections::BTreeSet::new();
    for (line_index, line) in body.lines().enumerate() {
        let line_number = line_index + 1;
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            if in_frontmatter {
                break;
            }
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("--") else {
            break;
        };
        in_frontmatter = true;
        let rest = rest.trim();
        if rest.is_empty() {
            break;
        }
        let Some((key, value)) = rest.split_once(':') else {
            let key = rest.split_whitespace().next().unwrap_or_default();
            if matches!(
                key,
                "description"
                    | "expected"
                    | "ceiling"
                    | "preserve_data_dir"
                    | "fixture"
                    | "protocol"
            ) || is_annotation_key(key)
            {
                return Err(format!(
                    "frontmatter line {line_number}: malformed `{key}` field; expected `key: value`"
                ));
            }
            continue;
        };
        let value = value.trim();
        let key = key.trim();
        // These annotations are consumed by ratatoskr's script tooling,
        // not by brokkr's runner. They may occur more than once.
        if matches!(key, "@covers" | "measured") {
            continue;
        }
        if !seen.insert(key.to_owned()) {
            return Err(format!(
                "frontmatter line {line_number}: duplicate `{key}` field"
            ));
        }
        match key {
            "description" if !value.is_empty() => {
                out.description = Some(value.to_owned());
            }
            "description" => {
                return Err(invalid_frontmatter_value(
                    line_number,
                    "description",
                    value,
                    "non-empty text",
                ));
            }
            "expected" => {
                out.selection = Some(Selection::parse(value).ok_or_else(|| {
                    invalid_frontmatter_value(line_number, "expected", value, "pass or ignored")
                })?);
            }
            "ceiling" => {
                out.ceiling = Some(parse_duration(value).ok_or_else(|| {
                    invalid_frontmatter_value(
                        line_number,
                        "ceiling",
                        value,
                        "a positive duration such as 60s, 5m, or 90",
                    )
                })?);
            }
            "preserve_data_dir" => {
                out.preserve_data_dir = Some(PreserveDataDir::parse(value).ok_or_else(|| {
                    invalid_frontmatter_value(
                        line_number,
                        "preserve_data_dir",
                        value,
                        "on_success_too",
                    )
                })?);
            }
            "fixture" => out.fixture = Some(fixture_value(line_number, value)?),
            "protocol" => out.protocol = Some(protocol_value(line_number, value)?),
            key if is_annotation_key(key) => {
                return Err(format!(
                    "frontmatter line {line_number}: unknown key `{key}`"
                ));
            }
            _ => continue,
        }
    }
    Ok(out)
}

/// The fixture directory comes from project config, so runners validate this
/// opaque name with `resolve_fixture`; here it only has to be present.
fn fixture_value(line: usize, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(invalid_frontmatter_value(
            line,
            "fixture",
            value,
            "a non-empty fixture name",
        ));
    }
    Ok(value.to_owned())
}

fn protocol_value(line: usize, value: &str) -> Result<String, String> {
    if !crate::ratatoskr::saehrimnir::is_protocol_name(value) {
        return Err(invalid_frontmatter_value(
            line,
            "protocol",
            value,
            "one of jmap, imap, smtp, graph, gmail, caldav, carddav, people, \
             gcal, or discovery",
        ));
    }
    Ok(value.to_owned())
}

/// Lowercase key-shaped comments with a colon are annotations; title-case
/// prose headings such as `-- Outside-review:` remain ordinary comments.
fn is_annotation_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn invalid_frontmatter_value(line: usize, key: &str, value: &str, expected: &str) -> String {
    format!("frontmatter line {line}: invalid `{key}` value {value:?}; expected {expected}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;

    use super::*;

    /// A fresh scratch dir standing in for a project root. `test_name` must
    /// be unique within this module - the allocator asserts it.
    fn tmpdir(test_name: &str) -> PathBuf {
        crate::test_scratch::scratch("discover", test_name)
    }

    fn write_script(project_root: &Path, name: &str, body: &str) {
        let path = project_root.join(SCRIPT_DIR).join(format!("{name}.lua"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn missing_dir_returns_empty() {
        let root = tmpdir("missing");
        let scripts = discover(&root).unwrap();
        assert!(scripts.is_empty());
    }

    #[test]
    fn parses_description_and_expected() {
        let root = tmpdir("parse");
        write_script(
            &root,
            "drop_test",
            "-- description: dropping client terminates child\n\
             -- expected: pass\n\
             \n\
             local c = harness.spawn(...)\n",
        );
        let scripts = discover(&root).unwrap();
        assert_eq!(scripts.len(), 1);
        let s = &scripts[0];
        assert_eq!(s.name, "drop_test");
        assert_eq!(
            s.description.as_deref(),
            Some("dropping client terminates child")
        );
        assert_eq!(s.selection, Selection::Run);
        assert_eq!(s.ceiling, DEFAULT_CEILING);
        assert_eq!(s.preserve_data_dir, PreserveDataDir::OnFailureOnly);
    }

    #[test]
    fn missing_frontmatter_defaults_to_pass() {
        let root = tmpdir("default");
        write_script(&root, "bare", "local c = harness.spawn(...)\n");
        let scripts = discover(&root).unwrap();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].description.is_none());
        assert_eq!(scripts[0].selection, Selection::Run);
        assert_eq!(scripts[0].ceiling, DEFAULT_CEILING);
    }

    #[test]
    fn ignored_marker_round_trip() {
        let root = tmpdir("ignored");
        write_script(
            &root,
            "wedge",
            "-- description: re-enable the ping-and-shutdown wedge\n\
             -- expected: ignored\n",
        );
        let scripts = discover(&root).unwrap();
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].selection, Selection::Skip);
    }

    #[test]
    fn unknown_annotation_keys_are_rejected_but_script_comments_are_not() {
        let root = tmpdir("unknown");
        write_script(
            &root,
            "noisy",
            "-- description: the description\n\
             -- author: someone\n\
             --\n\
             local x = 1\n",
        );
        let error = discover(&root).unwrap_err().to_string();
        assert!(error.contains("unknown key `author`"), "got: {error}");

        write_script(
            &root,
            "noisy",
            "-- description: the description\n\
             \n\
             -- author: ordinary prose after frontmatter\n\
             local x = 1\n",
        );
        write_script(
            &root,
            "comment",
            "-- description: the description\n\
             \n\
             -- author: ordinary prose after frontmatter\n\
             local x = 1\n",
        );
        let scripts = discover(&root).unwrap();
        assert!(scripts.iter().any(|s| s.name == "comment"));
    }

    #[test]
    fn skips_non_lua_files() {
        let root = tmpdir("nonlua");
        write_script(&root, "real", "-- description: yes\n");
        let dir = root.join(SCRIPT_DIR);
        fs::write(dir.join("README.md"), "# notes").unwrap();
        fs::write(dir.join("helper.txt"), "stuff").unwrap();
        let scripts = discover(&root).unwrap();
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].name, "real");
    }

    #[test]
    fn results_are_sorted() {
        let root = tmpdir("sorted");
        write_script(&root, "zeta", "");
        write_script(&root, "alpha", "");
        write_script(&root, "mu", "");
        let names: Vec<String> = discover(&root)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn frontmatter_stops_at_code() {
        let root = tmpdir("stops");
        write_script(
            &root,
            "stops",
            "-- description: the real one\n\
             local x = 1\n\
             -- description: too late\n",
        );
        let scripts = discover(&root).unwrap();
        assert_eq!(scripts[0].description.as_deref(), Some("the real one"));
    }

    #[test]
    fn discovers_nested_scripts_with_prefixed_names() {
        let root = tmpdir("nested");
        write_script(&root, "ping", "");
        write_script(&root, "t1/journal_replays", "-- description: journal\n");
        write_script(&root, "extract/pdf_basic", "");
        let names: Vec<String> = discover(&root)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "extract/pdf_basic".to_owned(),
                "ping".to_owned(),
                "t1/journal_replays".to_owned(),
            ]
        );
    }

    #[test]
    fn parses_ceiling_with_units() {
        let root = tmpdir("ceiling_units");
        write_script(&root, "secs", "-- ceiling: 30s\n");
        write_script(&root, "min", "-- ceiling: 5m\n");
        write_script(&root, "hour", "-- ceiling: 1h\n");
        write_script(&root, "ms", "-- ceiling: 500ms\n");
        write_script(&root, "bare", "-- ceiling: 90\n");
        let scripts = discover(&root).unwrap();
        let by_name = |n: &str| {
            scripts
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("missing {n}"))
                .ceiling
        };
        assert_eq!(by_name("secs"), Duration::from_secs(30));
        assert_eq!(by_name("min"), Duration::from_secs(300));
        assert_eq!(by_name("hour"), Duration::from_secs(3600));
        assert_eq!(by_name("ms"), Duration::from_millis(500));
        assert_eq!(by_name("bare"), Duration::from_secs(90));
    }

    #[test]
    fn invalid_known_frontmatter_values_fail_with_line() {
        let cases = [
            ("-- expected: ignore\n", "expected"),
            ("-- ceiling: not-a-number\n", "ceiling"),
            ("-- ceiling: -5s\n", "ceiling"),
            ("-- ceiling: 0s\n", "ceiling"),
            ("-- ceiling: 5y\n", "ceiling"),
            ("-- preserve_data_dir: maybe\n", "preserve_data_dir"),
            ("-- fixture: \n", "fixture"),
            ("-- fixtures: jmap-small\n", "fixtures"),
            ("-- protocol: imapp\n", "protocol"),
            ("-- expected pass\n", "expected"),
        ];
        for (body, key) in cases {
            let error = parse_frontmatter(body).expect_err("malformed frontmatter is rejected");
            assert!(error.contains("frontmatter line 1"), "got: {error}");
            assert!(error.contains(key), "got: {error}");
        }
    }

    #[test]
    fn duplicate_frontmatter_is_refused() {
        let error = parse_frontmatter("-- expected: pass\n-- expected: ignored\n")
            .expect_err("duplicate metadata cannot silently shadow the first value");
        assert!(error.contains("duplicate `expected`"), "got: {error}");
    }

    #[test]
    fn discovery_reports_invalid_frontmatter_path() {
        let root = tmpdir("frontmatter_path_error");
        write_script(&root, "bad", "-- expected: ignore\n");
        let error = discover(&root).unwrap_err().to_string();
        assert!(error.contains("bad.lua"), "got: {error}");
        assert!(error.contains("frontmatter line 1"), "got: {error}");
    }

    #[test]
    fn preserve_data_dir_round_trip() {
        let root = tmpdir("preserve");
        write_script(&root, "soaky", "-- preserve_data_dir: on_success_too\n");
        write_script(&root, "default", "");
        let scripts = discover(&root).unwrap();
        let by_name = |n: &str| {
            scripts
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("missing {n}"))
                .preserve_data_dir
        };
        assert_eq!(by_name("soaky"), PreserveDataDir::OnSuccessToo);
        assert_eq!(by_name("default"), PreserveDataDir::OnFailureOnly);
    }

    #[test]
    fn parses_fixture_and_protocol_frontmatter() {
        let root = tmpdir("sync_frontmatter");
        write_script(
            &root,
            "jmap_initial",
            "-- description: cold initial sync\n\
             -- fixture: jmap-small\n\
             -- protocol: jmap\n",
        );
        let scripts = discover_sync_at(&root.join(SCRIPT_DIR)).unwrap();
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].fixture(), Some("jmap-small"));
        assert_eq!(scripts[0].protocol(), Some("jmap"));
    }

    #[test]
    fn discovery_protocol_label_is_valid() {
        let parsed = parse_frontmatter("-- protocol: discovery\n").unwrap();
        assert_eq!(parsed.protocol.as_deref(), Some("discovery"));
    }

    #[test]
    fn fixture_and_protocol_default_to_none() {
        let root = tmpdir("sync_no_frontmatter");
        write_script(&root, "ping_only", "-- description: no fixture needed\n");
        let scripts = discover(&root).unwrap();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].fixture().is_none());
        assert!(scripts[0].protocol().is_none());
    }

    #[test]
    fn discover_at_uses_arbitrary_root() {
        // Plan 3's sync index passes its own root, not SCRIPT_DIR.
        let root = tmpdir("discover_at");
        let custom = root.join("custom-harness");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(
            custom.join("alpha.lua"),
            "-- description: foo\n-- fixture: jmap\n",
        )
        .unwrap();
        std::fs::write(
            custom.join("beta.lua"),
            "-- expected: ignored\n-- fixture: jmap\n",
        )
        .unwrap();
        let scripts = discover_sync_at(&custom).unwrap();
        let names: Vec<&str> = scripts.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn sync_script_without_fixture_is_refused_during_discovery() {
        let root = tmpdir("sync_missing_fixture");
        let custom = root.join("custom-harness");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(
            custom.join("missing.lua"),
            "-- description: missing fixture\n",
        )
        .unwrap();
        let error = discover_sync_at(&custom).unwrap_err().to_string();
        assert!(error.contains("requires `fixture: <NAME>`"), "got: {error}");
    }

    #[test]
    fn parse_script_handles_single_path() {
        let root = tmpdir("single");
        write_script(
            &root,
            "lone",
            "-- description: lone wolf\n-- ceiling: 10s\n",
        );
        let info = parse_service_script(&root.join(SCRIPT_DIR).join("lone.lua"), "lone").unwrap();
        assert_eq!(info.name, "lone");
        assert_eq!(info.description.as_deref(), Some("lone wolf"));
        assert_eq!(info.ceiling, Duration::from_secs(10));
    }
}
