// The user-wide `brokkr.toml`: an XDG-resolved config carrying conventions
// that belong to the developer rather than to any one project.
//
// It is deliberately a *thin* layer. Only `[[textlint]]` (with its
// `[textlint_preset.*]` blocks) and `[[script_check]]` are accepted, because
// those are the two sections that express a personal convention - "never write
// this phrase", "this script must keep passing" - rather than a fact about a
// particular tree. Everything else in `brokkr.toml` describes the project
// (its datasets, its sweeps, its bin targets) and has no meaning outside one.
// Rejecting the rest at parse time keeps that boundary from eroding.
//
// The layer is applied at *detection* (`project::detect`), not inside
// `config::load`. `load` stays exactly what its doc says it is - the single
// code path that reads one `brokkr.toml` - so a caller parsing a fixture file
// gets that file and nothing from the machine it runs on.

/// Env override for the user-wide config path. A set-but-empty value disables
/// the user-wide layer entirely, which is the documented opt-out for a shell
/// or CI job that must see only the project's own rules.
const USER_CONFIG_ENV: &str = "BROKKR_USER_CONFIG";

/// Path to the user-wide config: `$XDG_CONFIG_HOME/brokkr/brokkr.toml`,
/// falling back to `$HOME/.config/brokkr/brokkr.toml`.
///
/// `None` means there is no user-wide layer to look for: either
/// `BROKKR_USER_CONFIG` is set to the empty string (an explicit opt-out), or
/// neither `XDG_CONFIG_HOME` nor `HOME` is set, which is a sandbox, not an
/// error.
pub fn user_config_path() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(USER_CONFIG_ENV) {
        if v.is_empty() {
            return None;
        }
        return Some(PathBuf::from(v));
    }
    let dir = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".config")
    };
    Some(dir.join("brokkr").join("brokkr.toml"))
}

/// The parsed user-wide layer.
#[derive(Debug, Clone)]
pub struct UserConfig {
    /// Where it was read from, for error messages.
    pub path: PathBuf,
    pub textlint: Vec<TextlintRule>,
    pub script_checks: Vec<ScriptCheck>,
}

/// The only top-level keys a user-wide config may carry.
const USER_CONFIG_KEYS: [&str; 3] = ["textlint", "textlint_preset", "script_check"];

/// Read and parse the user-wide config. `Ok(None)` when there is no such file -
/// the overwhelmingly common case, and not a condition worth reporting. A file
/// that exists but does not parse is an error: silently ignoring it would mean
/// a typo quietly switches off every rule the user wrote.
pub fn load_user() -> Result<Option<UserConfig>, DevError> {
    let Some(path) = user_config_path() else {
        return Ok(None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(DevError::Config(format!("{}: {e}", path.display())));
        }
    };
    parse_user(&text, path).map(Some)
}

/// Parse the user-wide config text. Split from [`load_user`] so the schema is
/// testable without touching the filesystem or the environment.
fn parse_user(text: &str, path: PathBuf) -> Result<UserConfig, DevError> {
    let at = |msg: String| DevError::Config(format!("{}: {msg}", path.display()));

    let root: toml::Value = toml::from_str(text).map_err(|e| at(e.to_string()))?;
    let table = root
        .as_table()
        .ok_or_else(|| at("root is not a table".into()))?;

    for key in table.keys() {
        if !USER_CONFIG_KEYS.contains(&key.as_str()) {
            return Err(at(format!(
                "'{key}' is not allowed in the user-wide config, which carries \
                 [[textlint]], [textlint_preset.*] and [[script_check]] only. \
                 Everything else describes one project and belongs in that \
                 project's brokkr.toml."
            )));
        }
    }

    let reword = |e: DevError| match e {
        DevError::Config(msg) => at(msg),
        other => other,
    };
    let textlint = parse_textlint(table).map_err(reword)?;
    let script_checks = parse_script_checks(table).map_err(reword)?;

    Ok(UserConfig {
        path,
        textlint,
        script_checks,
    })
}

impl DevConfig {
    /// Fold the user-wide layer into this project config.
    ///
    /// User entries run first and project entries after, so a project's rules
    /// are the last word in the output. Shadowing is by `name`: a project entry
    /// that reuses a user entry's name replaces it outright. That is the
    /// opt-out - a project that must not run a personal rule redefines it,
    /// rather than needing a suppression syntax nobody would remember.
    pub fn apply_user_layer(&mut self) -> Result<(), DevError> {
        let Some(user) = load_user()? else {
            return Ok(());
        };
        merge_named(&mut self.textlint, user.textlint, |r| r.name.as_str());
        merge_named(&mut self.script_checks, user.script_checks, |c| {
            c.name.as_str()
        });
        Ok(())
    }
}

/// Prepend `incoming` to `project`, dropping any incoming entry whose name a
/// project entry already uses.
fn merge_named<T>(project: &mut Vec<T>, incoming: Vec<T>, name: impl Fn(&T) -> &str) {
    if incoming.is_empty() {
        return;
    }
    let taken: HashSet<String> = project.iter().map(|e| name(e).to_owned()).collect();
    let kept: Vec<T> = incoming
        .into_iter()
        .filter(|e| !taken.contains(name(e)))
        .collect();
    let tail = std::mem::replace(project, kept);
    project.extend(tail);
}
