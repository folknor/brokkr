//! Discovery for service-test scripts.
//!
//! Default location: `<project_root>/crates/app/tests/service-harness/`.
//! Discovery is recursive (`**/*.lua`), so cohorts can live under
//! subdirectories such as `t1/`, `extract/`, etc.
//!
//! Frontmatter is parsed from a contiguous block of `--` line comments
//! at the top of each script, before the first non-comment / non-blank
//! line. Recognized keys:
//!
//! - `description` (free text shown by `brokkr service-list`)
//! - `expected = pass | ignored`
//! - `ceiling = 60s` / `5m` / `1h` / `90` - per-script wall-clock
//!   backstop enforced by brokkr around the whole run. Bare numbers are
//!   read as seconds. Omitted scripts use [`DEFAULT_CEILING`].
//! - `preserve_data_dir = on_success_too` - keep the artefact dir even
//!   when the run succeeds (failures are always preserved). Other values
//!   are ignored, equivalent to omitting the field.
//!
//! Unknown keys are ignored rather than rejected so scripts can carry
//! their own annotations without breaking discovery.
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

/// Where service-test scripts live, relative to the project root.
pub const SCRIPT_DIR: &str = "crates/app/tests/service-harness";

/// Default per-script wall-clock ceiling when no `ceiling` frontmatter
/// is present. Generous enough that a healthy script finishes well
/// inside it, tight enough to catch a runaway before it eats a soak.
pub const DEFAULT_CEILING: Duration = Duration::from_secs(60);

/// Whether the script is expected to pass or to remain ignored (e.g. a
/// reproducer for an open Service bug). The `service-test` runner can
/// use this to flip a `Fail`->`Pass` outcome to "expected failure" so
/// suite runs are not blocked on known-broken cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    Pass,
    Ignored,
}

impl Expected {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "pass" => Some(Self::Pass),
            "ignored" => Some(Self::Ignored),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Ignored => "ignored",
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
    /// Display name relative to `SCRIPT_DIR`, without the `.lua`
    /// extension. Top-level scripts: just the file stem (e.g.
    /// `ping_and_shutdown`). Nested: `t1/journal_replays_after_respawn`.
    pub name: String,
    /// Used by `service-test` to resolve a script by name; `service-list`
    /// itself only consumes `name` / `description` / `expected`.
    #[allow(dead_code)]
    pub path: PathBuf,
    pub description: Option<String>,
    pub expected: Expected,
    /// Effective ceiling: either parsed from frontmatter or the default.
    pub ceiling: Duration,
    pub preserve_data_dir: PreserveDataDir,
}

/// Discover all service-test scripts under `<project_root>/<SCRIPT_DIR>/`,
/// recursively.
///
/// Returns an empty list (without error) when the directory does not
/// exist - the harness module has not landed in ratatoskr yet, so a
/// fresh checkout legitimately has no scripts. I/O errors during the
/// walk surface as `DevError::Io`. Scripts are returned sorted by name
/// for stable output.
pub fn discover(project_root: &Path) -> Result<Vec<ScriptInfo>, DevError> {
    let dir = project_root.join(SCRIPT_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    walk(&dir, &dir, &mut out)?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Parse one script at `path` into a `ScriptInfo`.
///
/// `display_name` is the name surfaced in user-facing output - typically
/// the path relative to `SCRIPT_DIR` minus the `.lua` extension. When
/// `service-test` is invoked with an arbitrary script path, the file
/// stem is a reasonable fallback.
pub fn parse_script(path: &Path, display_name: &str) -> Result<ScriptInfo, DevError> {
    let body = fs::read_to_string(path)?;
    Ok(build_info(display_name, path.to_path_buf(), &body))
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<ScriptInfo>) -> Result<(), DevError> {
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
            walk(root, &path, out)?;
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
        out.push(build_info(&name, path, &body));
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

fn build_info(name: &str, path: PathBuf, body: &str) -> ScriptInfo {
    let parsed = parse_frontmatter(body);
    ScriptInfo {
        name: name.to_owned(),
        path,
        description: parsed.description,
        expected: parsed.expected.unwrap_or(Expected::Pass),
        ceiling: parsed.ceiling.unwrap_or(DEFAULT_CEILING),
        preserve_data_dir: parsed.preserve_data_dir.unwrap_or_default(),
    }
}

#[derive(Debug, Default)]
struct ParsedFrontmatter {
    description: Option<String>,
    expected: Option<Expected>,
    ceiling: Option<Duration>,
    preserve_data_dir: Option<PreserveDataDir>,
}

/// Parse a leading block of `-- key: value` line comments. Stops at the
/// first non-comment / non-blank line. Whitespace around the `--` and
/// around the `:` is tolerated. The first occurrence of each key wins;
/// later occurrences are ignored.
fn parse_frontmatter(body: &str) -> ParsedFrontmatter {
    let mut out = ParsedFrontmatter::default();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            // blank line within or after the frontmatter is fine - keep going
            // until the first real code line.
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("--") else {
            break;
        };
        let rest = rest.trim();
        let Some((key, value)) = rest.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "description" if out.description.is_none() && !value.is_empty() => {
                out.description = Some(value.to_owned());
            }
            "expected" if out.expected.is_none() => {
                if let Some(parsed) = Expected::parse(value) {
                    out.expected = Some(parsed);
                }
            }
            "ceiling" if out.ceiling.is_none() => {
                if let Some(parsed) = parse_duration(value) {
                    out.ceiling = Some(parsed);
                }
            }
            "preserve_data_dir" if out.preserve_data_dir.is_none() => {
                if let Some(parsed) = PreserveDataDir::parse(value) {
                    out.preserve_data_dir = Some(parsed);
                }
            }
            _ => {}
        }
    }
    out
}

/// Parse a duration spelled in the brokkr/architecture style: a positive
/// integer optionally followed by a unit suffix (`s`, `m`, `h`, `ms`).
/// Bare numbers are seconds. Returns `None` on any parse failure.
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (num_part, unit) = split_unit(value);
    let n: u64 = num_part.parse().ok()?;
    match unit {
        "" | "s" => Some(Duration::from_secs(n)),
        "ms" => Some(Duration::from_millis(n)),
        "m" => Some(Duration::from_secs(n.checked_mul(60)?)),
        "h" => Some(Duration::from_secs(n.checked_mul(3600)?)),
        _ => None,
    }
}

fn split_unit(value: &str) -> (&str, &str) {
    let split = value
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(value.len());
    (&value[..split], &value[split..])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;

    use super::*;

    fn tmpdir(test_name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/discover")
            .join(test_name);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
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
        assert_eq!(s.expected, Expected::Pass);
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
        assert_eq!(scripts[0].expected, Expected::Pass);
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
        assert_eq!(scripts[0].expected, Expected::Ignored);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let root = tmpdir("unknown");
        write_script(
            &root,
            "noisy",
            "-- description: the description\n\
             -- author: someone\n\
             -- ticket: PROJ-42\n",
        );
        let scripts = discover(&root).unwrap();
        assert_eq!(scripts[0].description.as_deref(), Some("the description"));
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
    fn malformed_ceiling_falls_back_to_default() {
        let root = tmpdir("ceiling_malformed");
        write_script(&root, "garbage", "-- ceiling: not-a-number\n");
        write_script(&root, "negative", "-- ceiling: -5s\n");
        write_script(&root, "weird_unit", "-- ceiling: 5y\n");
        let scripts = discover(&root).unwrap();
        for s in &scripts {
            assert_eq!(
                s.ceiling, DEFAULT_CEILING,
                "{} should fall back to default",
                s.name
            );
        }
    }

    #[test]
    fn preserve_data_dir_round_trip() {
        let root = tmpdir("preserve");
        write_script(&root, "soaky", "-- preserve_data_dir: on_success_too\n");
        write_script(&root, "default", "");
        write_script(&root, "weird", "-- preserve_data_dir: maybe\n");
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
        assert_eq!(by_name("weird"), PreserveDataDir::OnFailureOnly);
    }

    #[test]
    fn parse_script_handles_single_path() {
        let root = tmpdir("single");
        write_script(
            &root,
            "lone",
            "-- description: lone wolf\n-- ceiling: 10s\n",
        );
        let info = parse_script(&root.join(SCRIPT_DIR).join("lone.lua"), "lone").unwrap();
        assert_eq!(info.name, "lone");
        assert_eq!(info.description.as_deref(), Some("lone wolf"));
        assert_eq!(info.ceiling, Duration::from_secs(10));
    }
}
