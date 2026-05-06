//! Discovery for service-test scripts.
//!
//! Default location: `<project_root>/crates/app/tests/service-harness/*.lua`.
//! That path mirrors the open-question recommendation in
//! `notes/ratatoskr-service-harness.md` (scripts co-located with the
//! existing tokio-tests). A configurable root can be added later via a
//! `[ratatoskr.harness]` section in `brokkr.toml` if a different layout
//! emerges.
//!
//! Frontmatter is parsed from a contiguous block of `--` line comments
//! at the top of each script, before the first non-comment / non-blank
//! line. Recognized keys: `description`, `expected` (`pass` or
//! `ignored`). Unknown keys are ignored rather than rejected so scripts
//! can carry their own annotations without breaking discovery.
//!
//! ```lua
//! -- description: Verify the Service exits with code 73 when the keyfile is missing
//! -- expected: ignored
//!
//! local client = harness.spawn(...)
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::DevError;

/// Where service-test scripts live, relative to the project root.
pub const SCRIPT_DIR: &str = "crates/app/tests/service-harness";

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

#[derive(Debug, Clone)]
pub struct ScriptInfo {
    /// Filename minus `.lua`, e.g. `service_subprocess_ping_and_shutdown`.
    pub name: String,
    /// Used by `service-test` to resolve a script by name; `service-list`
    /// itself only consumes `name` / `description` / `expected`.
    #[allow(dead_code)]
    pub path: PathBuf,
    pub description: Option<String>,
    pub expected: Expected,
}

/// Discover all service-test scripts under `<project_root>/<SCRIPT_DIR>/`.
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

    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(DevError::Io(err)),
    };

    let mut out = Vec::new();
    for entry in read {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lua") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let body = fs::read_to_string(&path)?;
        let (description, expected) = parse_frontmatter(&body);
        out.push(ScriptInfo {
            name,
            path,
            description,
            expected: expected.unwrap_or(Expected::Pass),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Parse a leading block of `-- key: value` line comments. Stops at the
/// first non-comment / non-blank line. Whitespace around the `--` and
/// around the `:` is tolerated.
fn parse_frontmatter(body: &str) -> (Option<String>, Option<Expected>) {
    let mut description = None;
    let mut expected = None;
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
            "description" => {
                if description.is_none() && !value.is_empty() {
                    description = Some(value.to_owned());
                }
            }
            "expected" => {
                if expected.is_none()
                    && let Some(parsed) = Expected::parse(value)
                {
                    expected = Some(parsed);
                }
            }
            _ => {}
        }
    }
    (description, expected)
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
        let dir = project_root.join(SCRIPT_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.lua")), body).unwrap();
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
    }

    #[test]
    fn missing_frontmatter_defaults_to_pass() {
        let root = tmpdir("default");
        write_script(&root, "bare", "local c = harness.spawn(...)\n");
        let scripts = discover(&root).unwrap();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].description.is_none());
        assert_eq!(scripts[0].expected, Expected::Pass);
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
}
