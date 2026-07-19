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
    validate_check_against_test(&check, test.as_ref())?;
    let capture_env = parse_capture_env(table)?;
    let gremlins = parse_gremlins(table)?;
    let style = parse_style(table)?;
    let header = parse_header(table)?;
    let textlint = parse_textlint(table)?;
    let manifest = parse_manifest(table)?;
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
            capture_env,
            gremlins,
            style,
            header,
            textlint,
            manifest,
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

/// Parse the optional `[[textlint]]` array of rules. Absent -> empty. Each rule
/// needs a `name`, `pattern`, and non-empty `paths`.
fn parse_textlint(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<TextlintRule>, DevError> {
    let Some(value) = table.get("textlint") else {
        return Ok(Vec::new());
    };
    if value.is_table() {
        return Err(DevError::Config(
            "[textlint] (table form) is not supported. Use one or more \
             `[[textlint]]` array-of-table entries."
                .into(),
        ));
    }
    let rules: Vec<TextlintRule> = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[[textlint]]: {e}")))?;
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
    Ok(rules)
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
            || key == "capture_env"
            || key == "gremlins"
            || key == "style"
            || key == "header"
            || key == "textlint"
            || key == "manifest"
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
/// paths), parses `allow`/`ban` `U+XXXX` codepoints into char sets, and
/// rejects a codepoint listed in both.
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
    }

    let allow = parse_codepoint_set("allow", &raw.allow)?;
    let ban = parse_codepoint_set("ban", &raw.ban)?;
    // Catch a codepoint listed on both sides, including a singleton that falls
    // inside the other side's range. Range-vs-range overlap is left to the
    // author.
    for c in allow.singles.iter().chain(ban.singles.iter()) {
        if allow.contains(*c) && ban.contains(*c) {
            return Err(DevError::Config(format!(
                "[gremlins]: U+{:04X} is listed in both `allow` and `ban`.",
                *c as u32
            )));
        }
    }

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
        for pkg in &entry.packages {
            if pkg.trim().is_empty() {
                return Err(DevError::Config(format!(
                    "[[check]] entry '{}' has a blank string in `packages`.",
                    entry.name
                )));
            }
        }
        for pkg in &entry.test_exclude_packages {
            if pkg.trim().is_empty() {
                return Err(DevError::Config(format!(
                    "[[check]] entry '{}' has a blank string in `test_exclude_packages`.",
                    entry.name
                )));
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
