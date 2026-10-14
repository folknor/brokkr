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
// Composition reads this table as the lowest-precedence layer before any
// config section is parsed. Project loading and environment reporting both
// use `compose`, so presets can resolve across the user and project layers.
// Unit-test builds never read the developer's file: composition substitutes
// no user layer there, so the environment-reading path is compiled out.

/// Env override for the user-wide config path. A set-but-empty value disables
/// the user-wide layer entirely, which is the documented opt-out for a shell
/// or CI job that must see only the project's own rules.
#[cfg(not(test))]
const USER_CONFIG_ENV: &str = "BROKKR_USER_CONFIG";

/// Path to the user-wide config: `$XDG_CONFIG_HOME/brokkr/brokkr.toml`,
/// falling back to `$HOME/.config/brokkr/brokkr.toml`.
///
/// `None` means there is no user-wide layer to look for: either
/// `BROKKR_USER_CONFIG` is set to the empty string (an explicit opt-out), or
/// neither `XDG_CONFIG_HOME` nor `HOME` is set, which is a sandbox, not an
/// error. An empty or relative `XDG_CONFIG_HOME` counts as unset, per the XDG
/// spec (`user_dirs`) - taken literally it would read a config out of the cwd.
#[cfg(not(test))]
pub fn user_config_path() -> Option<PathBuf> {
    user_config_path_from_override(std::env::var_os(USER_CONFIG_ENV))
}

/// Resolve an already-read override. Kept separate so tests can cover the
/// explicit opt-out without mutating the process-wide environment.
fn user_config_path_from_override(override_path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(v) = override_path {
        if v.is_empty() {
            return None;
        }
        return Some(PathBuf::from(v));
    }
    let dir = crate::user_dirs::xdg_config_home()?;
    Some(dir.join("brokkr").join("brokkr.toml"))
}

/// The only top-level keys a user-wide config may carry.
const USER_CONFIG_KEYS: [&str; 3] = ["textlint", "textlint_preset", "script_check"];

/// Read the raw user-wide table for config composition. The allow-list is
/// checked here, at the layer boundary, before the table can merge with a
/// project config. Keeping it raw lets presets and named entries resolve with
/// the project layers that sit above it.
#[cfg(not(test))]
fn load_user_table() -> Result<Option<(PathBuf, toml::Table)>, DevError> {
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
    parse_user_table(&text, &path).map(|table| Some((path, table)))
}

fn parse_user_table(text: &str, path: &Path) -> Result<toml::Table, DevError> {
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
    Ok(table.clone())
}
