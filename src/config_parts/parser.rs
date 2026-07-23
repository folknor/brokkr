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
        "ratatoskr" => Project::Ratatoskr,
        "saehrimnir" => Project::Saehrimnir,
        "piners" => Project::Piners,
        other => Project::Other(Box::leak(other.to_owned().into_boxed_str())),
    };

    let litehtml = parse_litehtml(table)?;
    let sluggrs = parse_sluggrs(table)?;
    let ratatoskr = parse_ratatoskr(table)?;
    let piners = parse_piners(table)?;
    let dependency_rules = parse_dependency_rules(table)?;
    let check = parse_check(table)?;
    let test = parse_test(table)?;
    let quarantine = parse_quarantine(table)?;
    validate_check_against_test(&check, test.as_ref(), &quarantine)?;
    let capture_env = parse_capture_env(table)?;
    let gremlins = parse_gremlins(table)?;
    let style = parse_style(table)?;
    let header = parse_header(table)?;
    let textlint = parse_textlint(table)?;
    let script_checks = parse_script_checks(table)?;
    let manifest = parse_manifest(table)?;
    let deps = parse_deps(table)?;
    let disable_toolchain = parse_disable_toolchain(table)?;
    let hosts = parse_hosts(table)?;
    validate_datasets(&hosts)?;
    validate_tilegen(&hosts)?;

    Ok((
        project,
        DevConfig {
            hosts,
            litehtml,
            sluggrs,
            ratatoskr,
            piners,
            dependency_rules,
            check,
            test,
            quarantine,
            capture_env,
            gremlins,
            style,
            header,
            textlint,
            script_checks,
            manifest,
            deps,
            disable_toolchain,
        },
    ))
}

/// Parse the optional `[manifest]` section. Absent -> `None`.
fn parse_manifest(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<ManifestConfig>, DevError> {
    let Some(value) = table.get("manifest") else {
        return Ok(None);
    };
    let cfg: ManifestConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[manifest]: {e}")))?;
    if let Some(ag) = &cfg.adapter_group {
        // The engine's `labelled_group_keys` finds the group whose header
        // comment `contains(marker)`, and `contains("")` is always true - a
        // blank marker binds to the *first* comment-labelled group in the
        // manifest rather than the intended one. Reject it here.
        if ag.marker.trim().is_empty() {
            return Err(DevError::Config(
                "[manifest.adapter_group] has an empty `marker` - it would match \
                 the first comment-labelled dependency group rather than the \
                 intended one. Name a substring of the group's header comment."
                    .into(),
            ));
        }
        // With an empty `forbidden_in`, `check_adapter_group` skips every
        // manifest (`forbidden_in.iter().any(...)` is always false), so the
        // whole check is a guaranteed no-op. Reject it as pointless config.
        if ag.forbidden_in.is_empty() {
            return Err(DevError::Config(
                "[manifest.adapter_group] has an empty `forbidden_in` - the check \
                 would never flag anything. List the package names that must not \
                 depend on the adapter group."
                    .into(),
            ));
        }
        for pkg in &ag.forbidden_in {
            if pkg.trim().is_empty() {
                return Err(DevError::Config(
                    "[manifest.adapter_group] has a blank string in `forbidden_in`."
                        .into(),
                ));
            }
        }
    }
    Ok(Some(cfg))
}

/// Parse the optional `[deps]` section. Absent -> `None`.
fn parse_deps(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<DepsConfig>, DevError> {
    let Some(value) = table.get("deps") else {
        return Ok(None);
    };
    let cfg: DepsConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[deps]: {e}")))?;
    Ok(Some(cfg))
}

/// Parse the optional `[header]` section. Absent -> `None`. Requires a
/// non-empty `paths` list and `pattern`.
fn parse_header(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<HeaderConfig>, DevError> {
    let Some(value) = table.get("header") else {
        return Ok(None);
    };
    let cfg: HeaderConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[header]: {e}")))?;
    if cfg.paths.is_empty() {
        return Err(DevError::Config(
            "[header] requires a non-empty `paths` list".into(),
        ));
    }
    if cfg.pattern.trim().is_empty() {
        return Err(DevError::Config("[header] requires a non-empty `pattern`".into()));
    }
    Ok(Some(cfg))
}

/// `[[textlint]]` fields holding string lists. When a rule draws on a preset
/// these *concatenate* (preset entries first) rather than the rule's value
/// replacing the preset's, so a rule can add one more path to a shared
/// `exclude` without restating the whole list.
const TEXTLINT_LIST_FIELDS: [&str; 3] = ["paths", "exclude", "except"];

/// Fields a `[textlint_preset.<name>]` block may carry: everything a
/// `[[textlint]]` rule accepts except the three that identify one rule
/// (`name`, `pattern`, `message`), which are never shareable.
const TEXTLINT_PRESET_FIELDS: [&str; 16] = [
    "paths",
    "exclude",
    "except",
    "allow_marker",
    "allow_marker_above",
    "in_toml_section",
    "table_row_only",
    "skip_after",
    "only_if_file_matches",
    "only_if_file_matches_above",
    "region",
    "join_wrapped_use",
    "except_above",
    "except_below",
    "require_above",
    "require_below",
];

/// Parse the optional `[textlint_preset.<name>]` blocks into raw TOML tables,
/// keyed by name. They are merged into rules by [`apply_textlint_preset`]
/// *before* deserialization, so a rule that explicitly sets a field back to its
/// default (`join_wrapped_use = false`) still overrides the preset - something
/// a post-deserialization merge cannot see.
fn parse_textlint_presets(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<HashMap<String, toml::value::Table>, DevError> {
    let Some(value) = table.get("textlint_preset") else {
        return Ok(HashMap::new());
    };
    let map = value.as_table().ok_or_else(|| {
        DevError::Config(
            "[textlint_preset] must be a table of named blocks, e.g. \
             `[textlint_preset.dst-scope]`"
                .into(),
        )
    })?;
    let mut out = HashMap::new();
    for (name, body) in map {
        let body = body.as_table().ok_or_else(|| {
            DevError::Config(format!("[textlint_preset.{name}] must be a table"))
        })?;
        for key in body.keys() {
            if matches!(key.as_str(), "name" | "pattern" | "message") {
                return Err(DevError::Config(format!(
                    "[textlint_preset.{name}] may not set `{key}` - it identifies a \
                     single rule, not a shared scope"
                )));
            }
            if !TEXTLINT_PRESET_FIELDS.contains(&key.as_str()) {
                return Err(DevError::Config(format!(
                    "[textlint_preset.{name}] has unknown field `{key}`"
                )));
            }
        }
        out.insert(name.clone(), body.clone());
    }
    Ok(out)
}

/// Fold one preset into the running *combined* preset a multi-preset rule
/// resolves against, in declaration order. Scalars: the first preset to set a
/// key wins (an earlier-listed preset is never overridden by a later one), so
/// precedence follows declaration order. Lists concatenate in declaration order
/// (`preset = ["a", "b"]` -> `a`'s entries then `b`'s). The combined table is
/// then layered under the rule by [`apply_textlint_preset`], which keeps every
/// preset entry ahead of the rule's own - so the final list is
/// `a ++ b ++ rule`, matching the first-listed-wins order scalars already use.
fn merge_preset_into_combined(combined: &mut toml::value::Table, preset: &toml::value::Table) {
    for (key, preset_value) in preset {
        if TEXTLINT_LIST_FIELDS.contains(&key.as_str()) {
            if let Some(toml::Value::Array(existing)) = combined.get_mut(key) {
                if let Some(add) = preset_value.as_array() {
                    existing.extend(add.iter().cloned());
                }
            } else {
                combined.insert(key.clone(), preset_value.clone());
            }
        } else if !combined.contains_key(key) {
            // First-listed preset wins for scalars.
            combined.insert(key.clone(), preset_value.clone());
        }
    }
}

/// Layer `preset` underneath `rule`: nearest value wins, so a key the rule sets
/// itself is left alone. The exception is [`TEXTLINT_LIST_FIELDS`], which
/// concatenate preset-first. `preset` is the already-combined table when a rule
/// names several (see [`merge_preset_into_combined`]).
fn apply_textlint_preset(rule: &mut toml::value::Table, preset: &toml::value::Table) {
    for (key, preset_value) in preset {
        let Some(rule_value) = rule.get(key) else {
            rule.insert(key.clone(), preset_value.clone());
            continue;
        };
        if !TEXTLINT_LIST_FIELDS.contains(&key.as_str()) {
            continue;
        }
        if let (Some(from_preset), Some(from_rule)) =
            (preset_value.as_array(), rule_value.as_array())
        {
            let mut merged = from_preset.clone();
            merged.extend(from_rule.iter().cloned());
            rule.insert(key.clone(), toml::Value::Array(merged));
        }
    }
}

/// Pull the `preset` key off a raw rule table, accepting either a single name
/// or a list of them. Removing it keeps `TextlintRule`'s `deny_unknown_fields`
/// intact - the merged table it eventually sees has no `preset` field.
fn take_textlint_presets(
    body: &mut toml::value::Table,
    label: &str,
) -> Result<Vec<String>, DevError> {
    let Some(value) = body.remove("preset") else {
        return Ok(Vec::new());
    };
    match value {
        toml::Value::String(s) => Ok(vec![s]),
        toml::Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    DevError::Config(format!(
                        "[[textlint]] {label}: `preset` list entries must be strings"
                    ))
                })
            })
            .collect(),
        _ => Err(DevError::Config(format!(
            "[[textlint]] {label}: `preset` must be a preset name or a list of them"
        ))),
    }
}

/// Error for a `[textlint_preset.<name>]` block that no rule references. Dead
/// config that loads clean is exactly what this parser rejects everywhere else,
/// so an unused preset is a load-time error too.
fn unreferenced_preset_err(name: &str) -> DevError {
    DevError::Config(format!(
        "[textlint_preset.{name}] is defined but no `[[textlint]]` rule \
         references it (via `preset = \"{name}\"`). Reference it from a rule or \
         delete the block."
    ))
}

/// Parse the optional `[[textlint]]` array of rules. Absent -> empty. Each rule
/// needs a `name`, `pattern`, and non-empty `paths`. A rule may name one or
/// more `[textlint_preset.<name>]` blocks via `preset`; those are combined in
/// declaration order (see [`merge_preset_into_combined`]) and layered under the
/// rule (see [`apply_textlint_preset`]), so `paths` may come from the preset.
/// A preset that no rule references is rejected.
fn parse_textlint(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<TextlintRule>, DevError> {
    let presets = parse_textlint_presets(table)?;
    // Presets referenced by at least one rule; anything left over is dead.
    let mut referenced: HashSet<String> = HashSet::new();
    let Some(value) = table.get("textlint") else {
        // No rules at all, so any defined preset is unreferenced.
        if let Some(name) = presets.keys().next() {
            return Err(unreferenced_preset_err(name));
        }
        return Ok(Vec::new());
    };
    if value.is_table() {
        return Err(DevError::Config(
            "[textlint] (table form) is not supported. Use one or more \
             `[[textlint]]` array-of-table entries."
                .into(),
        ));
    }
    let entries = value.as_array().ok_or_else(|| {
        DevError::Config(
            "[[textlint]] must be a sequence of array-of-table entries".into(),
        )
    })?;
    let mut rules: Vec<TextlintRule> = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut body = entry
            .as_table()
            .cloned()
            .ok_or_else(|| DevError::Config("[[textlint]] entry is not a table".into()))?;
        let label = body
            .get("name")
            .and_then(|v| v.as_str())
            .map_or_else(|| "<unnamed>".to_owned(), |n| format!("{n:?}"));
        // Combine the named presets in declaration order into one table, then
        // layer that under the rule - so a later preset never reorders an
        // earlier one's list entries ahead of it (S3-25).
        let mut combined = toml::value::Table::new();
        for name in take_textlint_presets(&mut body, &label)? {
            let preset = presets.get(&name).ok_or_else(|| {
                DevError::Config(format!(
                    "[[textlint]] {label} references unknown preset {name:?}; \
                     define it as [textlint_preset.{name}]"
                ))
            })?;
            merge_preset_into_combined(&mut combined, preset);
            referenced.insert(name);
        }
        apply_textlint_preset(&mut body, &combined);
        let rule: TextlintRule = toml::Value::Table(body)
            .try_into()
            .map_err(|e: toml::de::Error| {
                DevError::Config(format!("[[textlint]] {label}: {e}"))
            })?;
        rules.push(rule);
    }
    for rule in &rules {
        if rule.name.trim().is_empty() {
            return Err(DevError::Config(
                "[[textlint]] entry has empty `name`".into(),
            ));
        }
        if rule.pattern.is_empty() {
            return Err(DevError::Config(format!(
                "[[textlint]] {:?} has empty `pattern`",
                rule.name
            )));
        }
        if rule.paths.is_empty() {
            return Err(DevError::Config(format!(
                "[[textlint]] {:?} has empty `paths`",
                rule.name
            )));
        }
    }
    // Every defined preset must have been drawn on by a rule above.
    for name in presets.keys() {
        if !referenced.contains(name) {
            return Err(unreferenced_preset_err(name));
        }
    }
    Ok(rules)
}

/// Parse the optional `[[script_check]]` array of gates. Absent -> empty. Each
/// entry needs a non-empty `name`, `command`, and `expect`.
fn parse_script_checks(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<ScriptCheck>, DevError> {
    let Some(value) = table.get("script_check") else {
        return Ok(Vec::new());
    };
    if value.is_table() {
        return Err(DevError::Config(
            "[script_check] (table form) is not supported. Use one or more \
             `[[script_check]]` array-of-table entries."
                .into(),
        ));
    }
    let checks: Vec<ScriptCheck> = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[[script_check]]: {e}")))?;
    for check in &checks {
        if check.name.trim().is_empty() {
            return Err(DevError::Config(
                "[[script_check]] entry has empty `name`".into(),
            ));
        }
        if check.command.trim().is_empty() {
            return Err(DevError::Config(format!(
                "[[script_check]] {:?} has empty `command`",
                check.name
            )));
        }
        if check.expect.is_empty() {
            return Err(DevError::Config(format!(
                "[[script_check]] {:?} has empty `expect`",
                check.name
            )));
        }
    }
    Ok(checks)
}

/// Parse the optional `[style]` section. Absent - or present but with no rule
/// enabled - collapses to `None` so the style phase stays inert.
fn parse_style(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<StyleConfig>, DevError> {
    let Some(value) = table.get("style") else {
        return Ok(None);
    };
    let cfg: StyleConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[style]: {e}")))?;
    if cfg.is_empty() {
        return Ok(None);
    }
    Ok(Some(cfg))
}

/// Parse the optional top-level `disable_toolchain = true`. Absent is `false`.
fn parse_disable_toolchain(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<bool, DevError> {
    match table.get("disable_toolchain") {
        None => Ok(false),
        Some(value) => value.as_bool().ok_or_else(|| {
            DevError::Config("disable_toolchain must be a boolean".into())
        }),
    }
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
            || key == "ratatoskr"
            || key == "piners"
            || key == "dependency_rule"
            || key == "check"
            || key == "test"
            || key == "quarantine"
            || key == "capture_env"
            || key == "gremlins"
            || key == "style"
            || key == "header"
            || key == "textlint"
            || key == "textlint_preset"
            || key == "script_check"
            || key == "manifest"
            || key == "deps"
            || key == "disable_toolchain"
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

/// Parse the optional `[ratatoskr]` section from the root table.
///
/// Rejects the pre-decoupling `[ratatoskr.harness].sweep` field with a
/// migration message - that field is gone; orchestration builds are now
/// fully described by `package`/`binary`/`features` directly under
/// `[ratatoskr.harness]` and never reference `[[check]]`. See
/// `docs/projects/ratatoskr.md`.
fn parse_ratatoskr(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<RatatoskrConfig>, DevError> {
    let Some(value) = table.get("ratatoskr") else {
        return Ok(None);
    };
    if let Some(harness) = value.as_table().and_then(|t| t.get("harness"))
        && let Some(ht) = harness.as_table()
        && ht.contains_key("sweep")
    {
        return Err(DevError::Config(
            "[ratatoskr.harness].sweep is no longer supported. The harness \
             build spec is now self-contained: drop the `[[check]]` entry \
             that the sweep referenced, then under `[ratatoskr.harness]` \
             declare `package = \"<crate>\"` (required), optional \
             `binary = \"<bin>\"` (defaults to `package`), optional \
             `features = [...]`, and optional `debug = true`. See \
             docs/projects/ratatoskr.md."
                .into(),
        ));
    }
    let config: RatatoskrConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("ratatoskr: {e}")))?;
    Ok(Some(config))
}

/// Parse the optional `[piners]` section from the root table.
fn parse_piners(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<PinersConfig>, DevError> {
    let Some(value) = table.get("piners") else {
        return Ok(None);
    };
    let config: PinersConfig = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("piners: {e}")))?;
    Ok(Some(config))
}

/// The `[gremlins]` section exactly as it appears in TOML, before `allow`/`ban`
/// codepoint strings are parsed into char sets.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GremlinsRaw {
    #[serde(default)]
    disable: bool,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    ban: Vec<String>,
}

/// Parse the optional `[gremlins]` section from the root table.
///
/// Validates each `exclude` entry (no empty strings - a blank prefix would
/// match every path and silently disable the whole scan - and no absolute
/// paths), and parses `allow`/`ban` `U+XXXX` codepoints into char sets. A
/// codepoint listed in both is allowed - `allow` wins at scan time, so
/// allow-listing exceptions to a banned range is the intended usage.
fn parse_gremlins(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<GremlinsConfig>, DevError> {
    let Some(value) = table.get("gremlins") else {
        return Ok(None);
    };
    let raw: GremlinsRaw = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("gremlins: {e}")))?;
    for entry in &raw.exclude {
        if entry.trim().is_empty() {
            return Err(DevError::Config(
                "[gremlins].exclude contains an empty string - a blank prefix \
                 would skip every file. List the directories to exclude."
                    .into(),
            ));
        }
        if Path::new(entry).is_absolute() {
            return Err(DevError::Config(format!(
                "[gremlins].exclude entry {entry:?} is absolute; exclusions are \
                 project-root-relative directories."
            )));
        }
        // `.` / `./` (any path whose only components are the current-dir marker)
        // passes the empty/absolute gates but matches nothing at scan time:
        // `is_excluded` compares `rel.starts_with(".")`, false for every real
        // path. Reject it rather than let it silently exclude the whole tree of
        // nothing.
        let all_curdir = Path::new(entry)
            .components()
            .all(|c| c == std::path::Component::CurDir);
        if all_curdir {
            return Err(DevError::Config(format!(
                "[gremlins].exclude entry {entry:?} normalizes to the current \
                 directory and would exclude nothing. List the directories to \
                 exclude (relative to the project root)."
            )));
        }
    }

    let allow = parse_codepoint_set("allow", &raw.allow)?;
    let ban = parse_codepoint_set("ban", &raw.ban)?;
    // A codepoint appearing in both `allow` and `ban` is NOT an error: the
    // scanner's `gremlin_name` resolves `allow` before `ban` (allow wins over
    // everything), so `ban = ["U+2000..U+206F"]` + `allow = ["U+2011"]` - ban a
    // block, keep one character - is the canonical, semantically-valid form.
    // Rejecting the overlap here would be stricter than the runtime it guards.

    Ok(Some(GremlinsConfig {
        disable: raw.disable,
        exclude: raw.exclude,
        allow,
        ban,
    }))
}

/// Parse a `[gremlins]` `allow`/`ban` list into a [`CodepointSet`]. Each entry
/// is either a `U+XXXX` singleton or a `U+AAAA..U+BBBB` range (both ends
/// inclusive; `..=` is also accepted).
fn parse_codepoint_set(field: &str, entries: &[String]) -> Result<CodepointSet, DevError> {
    let mut out = CodepointSet::default();
    for entry in entries {
        if entry.contains("..") {
            let range = parse_codepoint_range(entry)
                .map_err(|msg| DevError::Config(format!("[gremlins].{field}: {msg}")))?;
            out.ranges.push(range);
        } else {
            let c = parse_codepoint(entry)
                .map_err(|msg| DevError::Config(format!("[gremlins].{field}: {msg}")))?;
            out.singles.insert(c);
        }
    }
    Ok(out)
}

/// Parse a `U+AAAA..U+BBBB` (or `..=`) codepoint range. Both ends inclusive;
/// the low end must not exceed the high end.
fn parse_codepoint_range(s: &str) -> Result<std::ops::RangeInclusive<u32>, String> {
    let t = s.trim();
    let (lo_s, hi_s) = t
        .split_once("..=")
        .or_else(|| t.split_once(".."))
        .ok_or_else(|| format!("{t:?}: malformed range, expected U+AAAA..U+BBBB"))?;
    let lo = parse_codepoint(lo_s)? as u32;
    let hi = parse_codepoint(hi_s)? as u32;
    if lo > hi {
        return Err(format!(
            "{t:?}: range start U+{lo:04X} is greater than end U+{hi:04X}"
        ));
    }
    Ok(lo..=hi)
}

/// Parse one `U+XXXX` token into a `char`. Accepts a case-insensitive `U+`
/// prefix followed by 1-6 hex digits. Rejects anything else so the config
/// itself stays ASCII (no literal, possibly-invisible gremlin characters in
/// `brokkr.toml`).
fn parse_codepoint(s: &str) -> Result<char, String> {
    let t = s.trim();
    let hex = t
        .strip_prefix("U+")
        .or_else(|| t.strip_prefix("u+"))
        .ok_or_else(|| {
            format!("{t:?} must be a codepoint in U+XXXX form, e.g. \"U+2011\"")
        })?;
    if hex.is_empty() || hex.len() > 6 {
        return Err(format!("{t:?}: expected 1-6 hex digits after `U+`"));
    }
    let cp = u32::from_str_radix(hex, 16)
        .map_err(|_| format!("{t:?}: {hex:?} is not hexadecimal"))?;
    char::from_u32(cp)
        .ok_or_else(|| format!("{t:?}: U+{cp:04X} is not a valid Unicode scalar value"))
}

/// Parse the optional `[[dependency_rule]]` array of tables.
fn parse_dependency_rules(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<DependencyRule>, DevError> {
    let Some(value) = table.get("dependency_rule") else {
        return Ok(Vec::new());
    };
    if value.is_table() {
        return Err(DevError::Config(
            "[dependency_rule] (table form) is not supported. Use one or \
             more `[[dependency_rule]]` array-of-table entries with `from` \
             and `forbid`."
                .into(),
        ));
    }

    let rules: Vec<DependencyRule> = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[[dependency_rule]]: {e}")))?;

    for (idx, rule) in rules.iter().enumerate() {
        let label = rule
            .name
            .as_deref()
            .map_or_else(|| format!("#{}", idx + 1), |name| format!("{name:?}"));
        if rule.name.as_deref().is_some_and(|name| name.trim().is_empty()) {
            return Err(DevError::Config(format!(
                "[[dependency_rule]] {label} has empty `name`"
            )));
        }
        validate_non_empty_string_list("from", &rule.from, &label)?;
        validate_non_empty_string_list("forbid", &rule.forbid, &label)?;
    }

    Ok(rules)
}

fn validate_non_empty_string_list(
    field: &str,
    values: &[String],
    label: &str,
) -> Result<(), DevError> {
    if values.is_empty() {
        return Err(DevError::Config(format!(
            "[[dependency_rule]] {label} has empty `{field}` list"
        )));
    }
    for value in values {
        if value.trim().is_empty() {
            return Err(DevError::Config(format!(
                "[[dependency_rule]] {label} has blank string in `{field}`"
            )));
        }
    }
    Ok(())
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
        if !seen.insert(entry.name.as_str()) && !entry.name.trim().is_empty() {
            return Err(DevError::Config(format!(
                "[[check]] has duplicate name '{}' - each entry must have a unique name.",
                entry.name
            )));
        }
        validate_check_entry(entry)?;
    }
    Ok(entries)
}

/// Env keys brokkr composes itself for a sweep's target-dir + `RUSTFLAGS`
/// isolation (`sweep_runtime_env` / `sweep_cargo_env`). Both a `[[check]]`
/// entry's `env` and a profile's `env` feed `merged_env`, which lets the sweep
/// env win over the composed isolation overlay - so a hand-set value here
/// silently *replaces* it rather than layering onto it.
const RESERVED_SWEEP_ENV: [&str; 3] = ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR"];

/// Reject any [`RESERVED_SWEEP_ENV`] key set by hand in an env map that feeds a
/// sweep, **unconditionally** - not gated on `rustflags` being present, because
/// the hazard is the hand-set key itself: it defeats the per-sweep isolation
/// brokkr owns these keys for. cargo would build into one `CARGO_TARGET_DIR`
/// while `BROKKR_TEST_BIN_DIR` still points at another (a silent wrong-binary),
/// or a set `RUSTFLAGS` would drop the composed `--cfg` and a sim gate would
/// report green without its cfg. `at` labels the offending block.
fn reject_reserved_sweep_env(
    env: &BTreeMap<String, String>,
    at: &str,
) -> Result<(), DevError> {
    for banned in RESERVED_SWEEP_ENV {
        if env.contains_key(banned) {
            return Err(DevError::Config(format!(
                "{at} sets `env.{banned}`, a key brokkr composes itself for the \
                 sweep's target-dir / RUSTFLAGS isolation. A hand-set value \
                 silently replaces the composed one (a wrong-binary \
                 BROKKR_TEST_BIN_DIR, or a dropped `--cfg`). Remove it; pass \
                 extra rustc flags through a `[[check]]` entry's `rustflags` \
                 field, which auto-isolates the target dir."
            )));
        }
    }
    Ok(())
}

/// Validate one `[[check]]` entry's fields (name, package lists, env, and the
/// rustflags/filter fields). Split out of `parse_check` to keep that function
/// under the line ceiling; duplicate-name detection stays in the caller since
/// it is cross-entry.
fn validate_check_entry(entry: &CheckEntry) -> Result<(), DevError> {
    if entry.name.trim().is_empty() {
        return Err(DevError::Config(
            "[[check]] entry has empty `name` - every entry needs a label \
             used by output and by `[test.profiles].sweeps` references."
                .into(),
        ));
    }
    for (field, values) in [
        ("packages", &entry.packages),
        ("test_exclude_packages", &entry.test_exclude_packages),
        ("build_packages", &entry.build_packages),
        ("rustflags", &entry.rustflags),
        ("tests", &entry.tests),
        ("skip", &entry.skip),
        ("only", &entry.only),
    ] {
        for v in values {
            if v.trim().is_empty() {
                return Err(DevError::Config(format!(
                    "[[check]] entry '{}' has a blank string in `{field}`.",
                    entry.name
                )));
            }
        }
    }
    if !entry.packages.is_empty() && !entry.test_exclude_packages.is_empty() {
        return Err(DevError::Config(format!(
            "[[check]] entry '{}' sets both `packages` (-p scoping) and \
             `test_exclude_packages` (--workspace --exclude); they are \
             mutually exclusive test-selection modes.",
            entry.name
        )));
    }
    for key in entry.env.keys() {
        if key.trim().is_empty() {
            return Err(DevError::Config(format!(
                "[[check]] entry '{}' has a blank `env` key.",
                entry.name
            )));
        }
    }
    reject_reserved_sweep_env(&entry.env, &format!("[[check]] entry '{}'", entry.name))?;
    Ok(())
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
/// Parse the `[[quarantine]]` array (absent -> empty). Shape rules are
/// enforced here so a malformed ledger never reaches the coverage phase:
/// exactly one of `pattern`/`category`, non-empty `issue` and `reason`,
/// and `"doctests"` as the only category.
fn parse_quarantine(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<QuarantineEntry>, DevError> {
    let Some(value) = table.get("quarantine") else {
        return Ok(Vec::new());
    };
    let entries: Vec<QuarantineEntry> = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[[quarantine]]: {e}")))?;
    for (i, q) in entries.iter().enumerate() {
        let label = q.issue.trim();

        if q.pattern.is_some() == q.category.is_some() {
            return Err(DevError::Config(format!(
                "[[quarantine]] entry {i}: set exactly one of `pattern` (a test-name \
                 substring) or `category`."
            )));
        }
        if q.pattern.as_deref().is_some_and(|p| p.trim().is_empty()) {
            return Err(DevError::Config(format!(
                "[[quarantine]] entry {i}: `pattern` is empty - it would match every test."
            )));
        }
        if let Some(cat) = &q.category
            && cat != "doctests"
        {
            return Err(DevError::Config(format!(
                "[[quarantine]] entry {i}: unknown category '{cat}'. The only \
                 category is \"doctests\"."
            )));
        }

        if q.package.is_some() && q.pattern.is_none() {
            return Err(DevError::Config(format!(
                "[[quarantine]] entry {i}: `package` only qualifies a `pattern` \
                 entry."
            )));
        }
        if q.package.as_deref().is_some_and(|p| p.trim().is_empty()) {
            return Err(DevError::Config(format!(
                "[[quarantine]] entry {i}: `package` is empty."
            )));
        }
        if label.is_empty() || q.reason.trim().is_empty() {
            return Err(DevError::Config(format!(
                "[[quarantine]] entry {i}: `issue` and `reason` are required - an \
                 unjustified quarantine is a graveyard with good manners."
            )));
        }
    }
    Ok(entries)
}

/// Validate a profile's `skip_phases`: it is a permission the `partial` claim
/// grants (a profile without `certifies = "partial"` may not skip phases), and
/// every named phase must be real AND skippable. `coverage` is real (a valid
/// `failed_phase`) but not skippable - it runs only under a complete claim, the
/// opposite of the partial one `skip_phases` requires, so skipping it is a
/// guaranteed no-op and is rejected loudly rather than announced.
fn validate_skip_phases(profile_name: &str, def: &ProfileDef) -> Result<(), DevError> {
    let Some(phases) = &def.skip_phases else {
        return Ok(());
    };
    if def.certifies != Some(Certifies::Partial) {
        return Err(DevError::Config(format!(
            "[test.profiles.{profile_name}] sets `skip_phases` without \
             `certifies = \"partial\"`. Skipping phases is a permission \
             the partial claim grants; a complete or unclaimed profile \
             may not skip phases."
        )));
    }
    // The skippable universe is every phase minus the non-skippable ones,
    // derived from `PHASE_NAMES` so a renamed phase can't drift a second
    // hard-coded list out of sync.
    let skippable = || {
        PHASE_NAMES
            .iter()
            .copied()
            .filter(|n| !NON_SKIPPABLE_PHASES.contains(n))
    };
    for p in phases {
        if NON_SKIPPABLE_PHASES.contains(&p.as_str()) {
            return Err(DevError::Config(format!(
                "[test.profiles.{profile_name}] skip_phases entry '{p}' \
                 cannot be skipped: it runs only under `certifies = \
                 \"complete\"`, while skip_phases requires `certifies = \
                 \"partial\"` - skipping it would do nothing. Remove it."
            )));
        }
        if !skippable().any(|n| n == p.as_str()) {
            return Err(DevError::Config(format!(
                "[test.profiles.{profile_name}] skip_phases entry '{p}' \
                 is not a skippable check phase. Valid phases: {}.",
                skippable().collect::<Vec<_>>().join(", ")
            )));
        }
    }
    Ok(())
}

fn validate_check_against_test(
    check: &[CheckEntry],
    test: Option<&TestConfig>,
    quarantine: &[QuarantineEntry],
) -> Result<(), DevError> {
    let Some(t) = test else {
        return Ok(());
    };
    // `default_profile` must name an existing profile - catch a typo at load
    // time instead of at `brokkr check` time. (Checked even when `profiles` is
    // empty: a `default_profile` set with no profiles defined is always wrong.)
    if let Some(default) = &t.default_profile
        && !t.profiles.contains_key(default.as_str())
    {
        return Err(DevError::Config(format!(
            "[test].default_profile = '{default}' names no `[test.profiles.*]` entry."
        )));
    }
    // `gate_profile` must name an existing profile, and that profile must
    // certify "complete": `--gate` exists to be the invocation whose green
    // may be treated as a gate result, so a gate resolving to a partial
    // (or unaccounted legacy) profile is rejected at load time.
    if let Some(gate) = &t.gate_profile {
        let Some(def) = t.profiles.get(gate.as_str()) else {
            return Err(DevError::Config(format!(
                "[test].gate_profile = '{gate}' names no `[test.profiles.*]` entry."
            )));
        };
        if def.certifies != Some(Certifies::Complete) {
            return Err(DevError::Config(format!(
                "[test].gate_profile = '{gate}' must name a profile with \
                 `certifies = \"complete\"` - the gate invocation is exactly \
                 the run whose green is allowed to mean \"ready\"."
            )));
        }
    }
    // Staleness, direction two: a doctests quarantine while doctests
    // actually run justifies nothing. The ledger must shrink when the
    // suppression it covers is removed.
    if t.doctests
        && quarantine
            .iter()
            .any(|q| q.category.as_deref() == Some("doctests"))
    {
        return Err(DevError::Config(
            "[[quarantine]] category = \"doctests\" is stale: `[test] doctests = \
             true`, so nothing is suppressed. Delete the entry."
                .into(),
        ));
    }
    if t.profiles.is_empty() {
        return Ok(());
    }
    let names: BTreeSet<&str> = check.iter().map(|e| e.name.as_str()).collect();
    for (profile_name, def) in &t.profiles {
        // A profile's `env` reaches the sweep too (`build_resolved_sweep`
        // merges it into `sweep.env`, which then wins over the composed
        // isolation in `merged_env`), so the same reserved keys are rejected
        // here as on a `[[check]]` entry - a profile-set CARGO_TARGET_DIR or
        // RUSTFLAGS would silently redirect the build past the isolation.
        if let Some(env) = &def.env {
            reject_reserved_sweep_env(env, &format!("[test.profiles.{profile_name}]"))?;
        }
        // A profile's `extends` target must itself be a defined profile.
        if let Some(parent) = &def.extends
            && !t.profiles.contains_key(parent.as_str())
        {
            return Err(DevError::Config(format!(
                "[test.profiles.{profile_name}] extends '{parent}', \
                 but no `[test.profiles.*]` entry with that name exists."
            )));
        }
        validate_skip_phases(profile_name, def)?;
        // Per-process execution is serial by construction: a parallel
        // thread count under `isolation = "process"` has no meaning.
        if def.isolation == Some(Isolation::Process)
            && matches!(def.test_threads, Some(n) if n != 1)
        {
            return Err(DevError::Config(format!(
                "[test.profiles.{profile_name}] combines `isolation = \
                 \"process\"` with `test_threads = {}` - per-process \
                 execution is serial by construction; drop `test_threads` \
                 or set it to 1.",
                def.test_threads.unwrap_or_default()
            )));
        }
        // A `lanes` profile is a list of runs, not a merged run: it carries
        // no run-shaping fields of its own, its lanes exist, don't nest,
        // and don't declare claims of their own.
        if let Some(lanes) = &def.lanes {
            validate_lanes_profile(profile_name, def, lanes, t, quarantine)?;
        }
        // A "complete" profile's load-time rules; the finer-grained
        // narrowing (`skip`/`only`) is audited at run time by the coverage
        // phase against `[[quarantine]]`. A lanes profile was already
        // checked per-lane above.
        if def.certifies == Some(Certifies::Complete) && def.lanes.is_none() {
            validate_complete_profile(profile_name, def, t, quarantine)?;
        }
        // The universe of a complete profile is every `[[check]]` entry, not
        // its own sweep list - checked once at the certifying-profile level
        // (a single lane referencing a subset is correct; the composed
        // profile's union must be total).
        if def.certifies == Some(Certifies::Complete) {
            validate_complete_universe(profile_name, t, check)?;
        }
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

/// Composition rules for a `lanes` profile (TIERED-CHECK.md feature 2),
/// fixed at load time so implementation details never decide them: no
/// run-shaping fields beside `lanes`, every lane exists, lanes don't nest,
/// and a lane never declares `certifies` (the claim belongs to the
/// composing profile). Under a `complete` claim, every lane must
/// individually satisfy the interim no-narrowing rule.
fn validate_lanes_profile(
    name: &str,
    def: &ProfileDef,
    lanes: &[String],
    t: &TestConfig,
    quarantine: &[QuarantineEntry],
) -> Result<(), DevError> {
    if lanes.is_empty() {
        return Err(DevError::Config(format!(
            "[test.profiles.{name}] declares `lanes = []` - list at least one \
             profile to compose."
        )));
    }
    if def.sweeps.is_some()
        || def.tests.is_some()
        || def.only.is_some()
        || def.skip.is_some()
        || def.include_ignored.is_some()
        || def.test_threads.is_some()
        || def.isolation.is_some()
        || def.env.is_some()
        || def.extends.is_some()
    {
        return Err(DevError::Config(format!(
            "[test.profiles.{name}] combines `lanes` with run-shaping fields. \
             A lanes profile is a list of runs, not a merged run: it may carry \
             only `lanes`, `certifies`, `skip_phases`, and `description` - \
             put sweeps/filters/env on the lane profiles themselves."
        )));
    }
    for lane in lanes {
        let Some(lane_def) = t.profiles.get(lane.as_str()) else {
            return Err(DevError::Config(format!(
                "[test.profiles.{name}] references lane '{lane}', but no \
                 `[test.profiles.*]` entry with that name exists."
            )));
        };
        if lane_def.certifies.is_some() {
            return Err(DevError::Config(format!(
                "[test.profiles.{name}] lane '{lane}' declares `certifies`. \
                 Certification belongs to the composing profile; a lane is a \
                 run, not a claim."
            )));
        }
        if lane_def.lanes.is_some() {
            return Err(DevError::Config(format!(
                "[test.profiles.{name}] lane '{lane}' has lanes of its own - \
                 lanes do not nest."
            )));
        }
        if def.certifies == Some(Certifies::Complete) {
            validate_complete_profile(lane, lane_def, t, quarantine).map_err(|e| match e {
                DevError::Config(msg) => DevError::Config(format!(
                    "[test.profiles.{name}] certifies \"complete\" via lanes: {msg}"
                )),
                other => other,
            })?;
        }
    }
    Ok(())
}

/// A `certifies = "complete"` profile's load-time rules (TIERED-CHECK.md
/// feature 4 relaxed the interim step-3 rule): libtest-level narrowing
/// (`skip` / `only` / `tests` / `include_ignored`) is now legal and audited
/// at run time by the coverage phase - every non-run (sweep, test) pair
/// must be quarantined or the check fails as orphaned. What remains
/// structural: no `extends` (an inherited filter set defeats an explicit
/// claim), and doctests off is itself a suppression that needs a
/// `[[quarantine]] category = "doctests"` entry, because doctests are
/// invisible to the `--list` enumeration.
fn validate_complete_profile(
    name: &str,
    def: &ProfileDef,
    t: &TestConfig,
    quarantine: &[QuarantineEntry],
) -> Result<(), DevError> {
    // Phrased as "cannot back a complete claim" because `name` is either
    // the certifying profile itself or a lane composing into one.
    let reject = |what: String| {
        Err(DevError::Config(format!(
            "[test.profiles.{name}] cannot back a \"complete\" claim: {what}."
        )))
    };
    if def.extends.is_some() {
        return reject("uses `extends` (an inherited filter set defeats an explicit claim)".into());
    }
    if !t.doctests
        && !quarantine
            .iter()
            .any(|q| q.category.as_deref() == Some("doctests"))
    {
        return reject(
            "doctests are disabled with no justification. Doctests are \
             invisible to the coverage enumeration, so `[test] doctests = \
             false` needs a `[[quarantine]] category = \"doctests\"` entry \
             with an issue, or set doctests = true"
                .into(),
        );
    }
    Ok(())
}

/// The `[[check]]` entry names a profile's runs reference, following
/// `lanes` (union across every lane) and `extends` (the closest `sweeps`
/// in the chain wins, matching resolution). A visited set guards against a
/// `lanes` cycle, and the inner walk against an `extends` cycle, so a
/// malformed config cannot spin here ahead of the resolver's own checks.
fn referenced_check_entries(
    profiles: &BTreeMap<String, ProfileDef>,
    name: &str,
    visited: &mut BTreeSet<String>,
    out: &mut BTreeSet<String>,
) {
    if !visited.insert(name.to_owned()) {
        return;
    }
    let Some(def) = profiles.get(name) else {
        return;
    };
    if let Some(lanes) = &def.lanes {
        for lane in lanes {
            referenced_check_entries(profiles, lane, visited, out);
        }
        return;
    }
    // Non-lanes: the effective sweep list is the closest `sweeps` up the
    // `extends` chain (child replaces parent), so walk until one is found.
    let mut cur = Some(name);
    let mut chain_seen: BTreeSet<&str> = BTreeSet::new();
    while let Some(n) = cur {
        if !chain_seen.insert(n) {
            break;
        }
        let Some(d) = profiles.get(n) else {
            break;
        };
        if let Some(sweeps) = &d.sweeps {
            for s in sweeps {
                out.insert(s.clone());
            }
            break;
        }
        cur = d.extends.as_deref();
    }
}

/// The universe of a `complete` profile is every `[[check]]` entry, not its
/// own sweep list (TIERED-CHECK.md feature 4). If the universe were the
/// sweeps a lane happens to reference, an entry no lane names would be
/// enumerated nowhere and the coverage audit would print `0 orphaned` over
/// tests that never ran - the exact hole the audit exists to close. Every
/// `[[check]]` entry is unconditional today (feature 6 `when` is unbuilt),
/// so a complete profile must reference every one; an omission is a
/// resolve-time error.
fn validate_complete_universe(
    name: &str,
    t: &TestConfig,
    check: &[CheckEntry],
) -> Result<(), DevError> {
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    referenced_check_entries(&t.profiles, name, &mut visited, &mut referenced);
    let unreferenced: Vec<&str> = check
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| !referenced.contains(*n))
        .collect();
    if !unreferenced.is_empty() {
        let (entry_word, it_word) = if unreferenced.len() == 1 {
            ("entry", "it")
        } else {
            ("entries", "them")
        };
        return Err(DevError::Config(format!(
            "[test.profiles.{name}] cannot back a \"complete\" claim: [[check]] \
             {entry_word} {} referenced by no sweep or lane. The universe of a \
             complete profile is every [[check]] entry - an unreferenced entry \
             is enumerated nowhere, so its tests would be certified without \
             running. Reference {it_word} from a lane (or delete the entry).",
            unreferenced.join(", "),
        )));
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
            if let Some(blessed) = &ds.blessed {
                if blessed.file.is_empty() {
                    return Err(DevError::Config(format!(
                        "{host}.datasets.{ds_name}.blessed: file name is empty"
                    )));
                }
                if blessed.commit.is_empty() {
                    return Err(DevError::Config(format!(
                        "{host}.datasets.{ds_name}.blessed: commit is empty"
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

/// Validate every `[<host>.tilegen.<name>]` block's ocean statement.
///
/// These rules are elivagar's, enforced here so a bad block fails at parse
/// time - before a build, let alone a planet-scale run - and names the field.
/// The partition rule is not arbitrary: `ocean::selected_pass_grid` implements
/// one split, at z7/z8, and nowhere else, so a `z0-z5` request is rejected
/// rather than accepted and quietly served at z7. The alternative is a false
/// statement in the recorded invocation, which is the failure this whole
/// config exists to prevent.
fn validate_tilegen(hosts: &HashMap<String, HostConfig>) -> Result<(), DevError> {
    for (host, hc) in hosts {
        for (name, tg) in &hc.tilegen {
            let at = format!("{host}.tilegen.{name}");
            let specs = tg
                .ocean_specs()
                .map_err(|e| DevError::Config(format!("{at}.ocean: {e}")))?;

            let mut bands: BTreeSet<&'static str> = BTreeSet::new();
            let mut artifacts = 0usize;
            for spec in &specs {
                if spec.file().is_empty() {
                    return Err(DevError::Config(format!("{at}.ocean: file name is empty")));
                }
                let band = match spec {
                    OceanSpec::ShapefileAll(_) => "z0-z14",
                    OceanSpec::ShapefileLow(_) => "z0-z7",
                    OceanSpec::ShapefileHigh(_) => "z8-z14",
                    OceanSpec::Artifact(_) => {
                        artifacts += 1;
                        continue;
                    }
                };
                if !bands.insert(band) {
                    return Err(DevError::Config(format!(
                        "{at}.ocean: band {band} named twice"
                    )));
                }
            }

            if artifacts > 1 {
                return Err(DevError::Config(format!(
                    "{at}.ocean: {artifacts} .pmtiles artifacts named, expected at most one"
                )));
            }

            // Absent/empty is a legal statement: it means no ocean.
            if bands.is_empty() {
                if artifacts > 0 {
                    // The artifact is a cache over the shapefiles, not a
                    // substitute. An extract computes its boundary band near
                    // the bbox edge from the shapefiles and takes only the
                    // interior from the artifact, and the artifact's key is
                    // validated by re-hashing the shapefiles it claims to have
                    // been built from - so both sides must be present for the
                    // check to mean anything.
                    return Err(DevError::Config(format!(
                        "{at}.ocean: a .pmtiles artifact is a cache over the shapefiles, \
                         not a substitute, and cannot stand alone; name the shapefiles \
                         it was built from too"
                    )));
                }
                continue;
            }

            let all: BTreeSet<&str> = ["z0-z14"].into_iter().collect();
            let split: BTreeSet<&str> = ["z0-z7", "z8-z14"].into_iter().collect();
            if bands != all && bands != split {
                let got: Vec<&str> = bands.iter().copied().collect();
                return Err(DevError::Config(format!(
                    "{at}.ocean: shapefiles must partition z0-z14 exactly - either a \
                     single z0-z14 or the z0-z7 + z8-z14 pair, got {}",
                    got.join(" + ")
                )));
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

    let output_rel = host
        .and_then(|h| h.output.as_deref())
        .unwrap_or("data/tilegen");

    let data_dir = resolve_relative(project_root, data_rel);
    let scratch_dir = resolve_relative(project_root, scratch_rel);
    let output_dir = resolve_relative(project_root, output_rel);

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
        output_dir,
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
mod universe_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn check_entries(names: &[&str]) -> Vec<CheckEntry> {
        names
            .iter()
            .map(|n| CheckEntry {
                name: (*n).into(),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn complete_lanes_profile_must_reference_every_check_entry() {
        // The S3-12 hole: a complete profile whose lanes name only
        // default+ffi leaves live+sim enumerated nowhere, so the audit would
        // print `0 orphaned` over tests that never ran.
        let cfg: TestConfig = toml::from_str(
            r#"
doctests = true
[profiles.gate]
certifies = "complete"
lanes = ["tier1"]
[profiles.tier1]
sweeps = ["default", "ffi"]
"#,
        )
        .unwrap();
        let check = check_entries(&["default", "ffi", "live", "sim"]);
        let err = validate_check_against_test(&check, Some(&cfg), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("live"), "got: {err}");
        assert!(err.contains("sim"), "got: {err}");
        assert!(
            err.contains("referenced by no sweep or lane"),
            "got: {err}"
        );
    }

    #[test]
    fn complete_single_profile_must_reference_every_check_entry() {
        let cfg: TestConfig = toml::from_str(
            r#"
doctests = true
[profiles.gate]
certifies = "complete"
sweeps = ["default"]
"#,
        )
        .unwrap();
        let check = check_entries(&["default", "ffi"]);
        let err = validate_check_against_test(&check, Some(&cfg), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ffi"), "got: {err}");
        assert!(err.contains("cannot back a \"complete\" claim"), "got: {err}");
    }

    #[test]
    fn complete_profile_covering_every_entry_across_lanes_passes() {
        let cfg: TestConfig = toml::from_str(
            r#"
doctests = true
[profiles.gate]
certifies = "complete"
lanes = ["a", "b"]
[profiles.a]
sweeps = ["default", "ffi"]
[profiles.b]
sweeps = ["live", "sim"]
"#,
        )
        .unwrap();
        let check = check_entries(&["default", "ffi", "live", "sim"]);
        validate_check_against_test(&check, Some(&cfg), &[]).unwrap();
    }

    #[test]
    fn partial_profile_may_reference_a_subset() {
        // The universe rule is complete-only: a partial profile is allowed
        // to narrow, and legitimately references a subset of entries.
        let cfg: TestConfig = toml::from_str(
            r#"
[profiles.edit]
certifies = "partial"
sweeps = ["default"]
"#,
        )
        .unwrap();
        let check = check_entries(&["default", "ffi", "live"]);
        validate_check_against_test(&check, Some(&cfg), &[]).unwrap();
    }
}
