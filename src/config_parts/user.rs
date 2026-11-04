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
// One key is the opposite case: `results_server`, where this user's
// script-bench runs are stored, belongs to the user and never to a project.
// Composition takes it out of the user table before anything merges, so it
// never reaches the composed config (nor a run's stored copy of it), and
// refuses it in every project layer.
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
const USER_CONFIG_KEYS: [&str; 4] = [
    "textlint",
    "textlint_preset",
    "script_check",
    RESULTS_SERVER_KEY,
];

/// The user-only key naming a results server.
const RESULTS_SERVER_KEY: &str = "results_server";

/// A results server, as the user-wide `results_server` key names it,
/// validated once: an `http` or `https` URL with a host, no credentials, no
/// query or fragment, and an optional path prefix (for a proxy that routes by
/// path). Trailing slashes on the path are dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultsServer {
    /// The URL as messages name it: scheme, authority and path prefix.
    pub shown: String,
    /// `shown` with the wire's route appended, where every request goes.
    pub endpoint: String,
}

impl ResultsServer {
    /// Validate a configured URL; the error says what is wrong with it.
    pub fn parse(value: &str) -> Result<Self, String> {
        let uri: ureq::http::Uri = value
            .parse()
            .map_err(|e| format!("{value:?} is not a URL: {e}"))?;
        let scheme = match uri.scheme_str() {
            Some(s @ ("http" | "https")) => s,
            _ => return Err(format!("{value:?} must be an http:// or https:// URL")),
        };
        let parsed = uri
            .authority()
            .filter(|a| !a.host().is_empty())
            .ok_or_else(|| format!("{value:?} names no host"))?;
        let authority = parsed.as_str();
        if authority.contains('@') {
            return Err(format!(
                "{value:?} carries credentials; a results server has no authentication, so its URL carries none"
            ));
        }
        // An explicit port must be one: the HTTP client would otherwise fall
        // back to the scheme's default and reach another service. Read from
        // the text, since the parser reports an out-of-range port as no port
        // at all; a colon inside brackets is an IPv6 host's.
        let explicit = authority
            .rsplit_once(':')
            .map(|(_, port)| port)
            .filter(|port| !port.contains(']'));
        if explicit.is_some_and(|port| port.parse::<u16>().is_err()) {
            return Err(format!("{value:?} has a port that is not a number from 0 to 65535"));
        }
        if uri.query().is_some() || value.contains('#') {
            return Err(format!("{value:?} must have no query or fragment"));
        }
        let prefix = uri.path().trim_end_matches('/');
        let shown = format!("{scheme}://{authority}{prefix}");
        Ok(Self {
            endpoint: format!("{shown}/bench"),
            shown,
        })
    }
}

/// Take `results_server` out of the user table, validated, so it never
/// merges with a project's config.
fn take_results_server(
    table: &mut toml::Table,
    path: &Path,
) -> Result<Option<ResultsServer>, DevError> {
    let Some(value) = table.remove(RESULTS_SERVER_KEY) else {
        return Ok(None);
    };
    let at = |msg: String| DevError::Config(format!("{}: {RESULTS_SERVER_KEY}: {msg}", path.display()));
    let text = value
        .as_str()
        .ok_or_else(|| at("must be a URL string".into()))?;
    ResultsServer::parse(text).map(Some).map_err(at)
}

/// Refuse `results_server` in a project layer (the project's `brokkr.toml`
/// or a file it includes), naming that file. A table of that name is not the
/// key: it stays the hostname section it always was.
fn refuse_project_results_server(layers: &[(PathBuf, toml::Table)]) -> Result<(), DevError> {
    for (path, table) in layers {
        if table.get(RESULTS_SERVER_KEY).is_some_and(|v| !v.is_table()) {
            return Err(DevError::Config(format!(
                "{}: '{RESULTS_SERVER_KEY}' belongs to the user, not to a project: set it in the \
                 user-wide brokkr.toml, where it applies to every project",
                path.display()
            )));
        }
    }
    Ok(())
}

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
                 [[textlint]], [textlint_preset.*], [[script_check]] and \
                 results_server only. \
                 Everything else describes one project and belongs in that \
                 project's brokkr.toml."
            )));
        }
    }
    Ok(table.clone())
}
