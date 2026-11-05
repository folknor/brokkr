//! `[bench.*]`, `[bench_params.*]` and `[data]`: the script-bench schema.
//!
//! A bench is a Lua script plus the build settings and params it runs with.
//! This module reads and validates the tables at load; nothing here reads a
//! script file or compiles Lua (`brokkr check`'s `bench` phase does), so a
//! broken script never blocks an unrelated command.
//!
//! Everything a later refusal would have to add is refused here, so a bench
//! table that loads keeps loading: param declarations are parsed strictly and
//! their reference graph (unknown `{param}` references, cycles, collections
//! named in a `from` path) is judged on each bench's effective params, after
//! `uses` has pulled in the shared sets and the bench's own params have
//! replaced theirs. What needs the resolved config (whether a key exists
//! under a `from` path, digests) is judged when a bench runs.
//!
//! `[data]` is reserved for project data that `from` paths walk; brokkr never
//! interprets it beyond requiring a table.

use super::ConfigSources;
use crate::build::RustProfile;
use crate::error::DevError;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The param name brokkr keeps for itself: every bench's `--help`. A bench's
/// params follow its name on the command line and brokkr's own options precede
/// it, so this is the whole set of names a param cannot take.
pub const RESERVED_PARAM_NAMES: &[&str] = &["help"];

/// One `[bench.NAME]`, its params already composed with the sets it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bench {
    pub name: String,
    pub script: BenchScript,
    /// The bin target to build; `None` means `[bin] default`.
    pub target: Option<String>,
    pub profile: Option<RustProfile>,
    pub features: Vec<String>,
    /// Whether cargo builds the package's default features; `true` unless the
    /// table sets `default_features = false`.
    pub default_features: bool,
    /// The package's own features that instrument it for hotpath: added to
    /// the build under `brokkr bench --hotpath`, and refused in a timing
    /// run's resolved features. Empty means the bench cannot be profiled.
    pub hotpath_features: Vec<String>,
    /// The `[bench_params]` sets this bench pulled in, in the order given.
    pub uses: Vec<String>,
    /// The effective params: every used set's, then the bench's own, which
    /// replace a set's param of the same name whole.
    pub params: BTreeMap<String, Param>,
}

/// Where a bench's Lua lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchScript {
    /// `script = "..."`, inline in the config file `declared_in`.
    Inline {
        source: String,
        declared_in: PathBuf,
    },
    /// `script_file = "..."`, resolved against the directory of the config
    /// file that set it (`declared_in`) to the absolute `path`.
    File { path: PathBuf, declared_in: PathBuf },
}

/// One param declaration, from a bench's `params` or a `[bench_params]` set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub help: Option<String>,
    /// The config table whose keys are the param's values; `None` makes the
    /// param a plain string.
    pub from: Option<FromPath>,
    /// The value used when the command line gives none, in the syntax the
    /// command line takes.
    pub default: Option<String>,
    /// A collection: one sample binds the whole list of values.
    pub many: bool,
    /// The command line must give it: its absence is refused when the bench
    /// binds, before anything is built. Never beside a `default`.
    pub required: bool,
    /// Where the declaration came from.
    pub origin: ParamOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamOrigin {
    /// The bench's own `params`.
    Bench,
    /// The `[bench_params]` set of this name.
    Set(String),
}

/// A parsed `from` path: dot-separated segments, each a literal key or a
/// whole-segment `{param}` reference to another param's chosen key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromPath {
    pub segments: Vec<FromSegment>,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromSegment {
    Key(String),
    Param(String),
}

impl FromPath {
    /// The path as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The params this path references, in path order.
    pub fn references(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|s| match s {
            FromSegment::Param(name) => Some(name.as_str()),
            FromSegment::Key(_) => None,
        })
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut segments = Vec::new();
        for segment in text.split('.') {
            if segment.is_empty() {
                return Err("has an empty segment".into());
            }
            let parsed = match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(name) => {
                    if !is_param_name(name) {
                        return Err(format!("references {{{name}}}, which is not a param name"));
                    }
                    FromSegment::Param(name.to_owned())
                }
                None if segment.contains(['{', '}']) => {
                    return Err(format!(
                        "segment {segment:?} mixes a {{param}} reference with other text; \
                         a reference is a whole segment"
                    ));
                }
                None => FromSegment::Key(segment.to_owned()),
            };
            segments.push(parsed);
        }
        Ok(Self {
            segments,
            text: text.to_owned(),
        })
    }
}

/// A param name: also its `--NAME` flag, so lowercase, starting with a
/// letter, then letters, digits, `_` or `-`. A hotpath stage name follows
/// the same grammar.
pub fn is_param_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A bench or set name: the argument `brokkr bench` takes, so it may not
/// start with `-`.
fn is_bench_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParam {
    help: Option<String>,
    from: Option<String>,
    default: Option<String>,
    #[serde(default)]
    many: bool,
    #[serde(default)]
    required: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBench {
    script: Option<String>,
    script_file: Option<String>,
    target: Option<String>,
    profile: Option<RustProfile>,
    #[serde(default)]
    features: Vec<String>,
    default_features: Option<bool>,
    #[serde(default)]
    hotpath_features: Vec<String>,
    #[serde(default)]
    uses: Vec<String>,
    #[serde(default)]
    params: BTreeMap<String, toml::Value>,
}

/// Parse `[bench.*]`, `[bench_params.*]` and `[data]` from the composed
/// table. `sources` locates the file that set each `script` and
/// `script_file`.
pub(crate) fn parse(
    table: &toml::Table,
    sources: &ConfigSources,
) -> Result<BTreeMap<String, Bench>, DevError> {
    if let Some(data) = table.get("data")
        && !data.is_table()
    {
        return Err(DevError::Config(
            "data must be a table: [data] holds project data that bench params select from".into(),
        ));
    }
    let sets = parse_sets(table)?;
    let Some(value) = table.get("bench") else {
        return Ok(BTreeMap::new());
    };
    let benches = value
        .as_table()
        .ok_or_else(|| DevError::Config("bench must be a table of [bench.NAME] tables".into()))?;
    let mut out = BTreeMap::new();
    for (name, value) in benches {
        out.insert(name.clone(), parse_bench(name, value, &sets, sources)?);
    }
    Ok(out)
}

fn parse_sets(table: &toml::Table) -> Result<BTreeMap<String, BTreeMap<String, Param>>, DevError> {
    let Some(value) = table.get("bench_params") else {
        return Ok(BTreeMap::new());
    };
    let sets = value.as_table().ok_or_else(|| {
        DevError::Config("bench_params must be a table of [bench_params.NAME] sets".into())
    })?;
    let mut out = BTreeMap::new();
    for (set, value) in sets {
        let at = format!("[bench_params.{set}]");
        if !is_bench_name(set) {
            return Err(DevError::Config(format!(
                "{at}: a set name is letters, digits, '_' and '-', not starting with '-'"
            )));
        }
        let params = value.as_table().ok_or_else(|| {
            DevError::Config(format!("{at} must be a table of param declarations"))
        })?;
        let mut parsed = BTreeMap::new();
        for (param, decl) in params {
            let declared = parse_param(
                &format!("{at} {param}"),
                param,
                decl,
                ParamOrigin::Set(set.clone()),
            )
            .map_err(DevError::Config)?;
            parsed.insert(param.clone(), declared);
        }
        out.insert(set.clone(), parsed);
    }
    Ok(out)
}

/// Parse one declaration. `at` names it in errors.
fn parse_param(
    at: &str,
    name: &str,
    value: &toml::Value,
    origin: ParamOrigin,
) -> Result<Param, String> {
    if !is_param_name(name) {
        return Err(format!(
            "{at}: a param name is its --flag: lowercase, starting with a letter, then \
             letters, digits, '_' or '-'"
        ));
    }
    if RESERVED_PARAM_NAMES.contains(&name) {
        return Err(format!(
            "{at}: '{name}' is reserved: every bench takes --{name} from brokkr"
        ));
    }
    let raw: RawParam = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| format!("{at}: {e}"))?;
    let from = raw
        .from
        .as_deref()
        .map(|text| FromPath::parse(text).map_err(|e| format!("{at}: from {text:?} {e}")))
        .transpose()?;
    if raw.default.as_deref().is_some_and(|d| d.trim().is_empty()) {
        return Err(format!(
            "{at}: default is blank - give a value or remove the key"
        ));
    }
    if raw.required && raw.default.is_some() {
        return Err(format!(
            "{at}: required and default exclude each other - a param with a default is never \
             absent; remove one"
        ));
    }
    Ok(Param {
        help: raw.help,
        from,
        default: raw.default,
        many: raw.many,
        required: raw.required,
        origin,
    })
}

fn parse_bench(
    name: &str,
    value: &toml::Value,
    sets: &BTreeMap<String, BTreeMap<String, Param>>,
    sources: &ConfigSources,
) -> Result<Bench, DevError> {
    let at = format!("[bench.{name}]");
    let err = |msg: String| DevError::Config(format!("{at}: {msg}"));
    if !is_bench_name(name) {
        return Err(err(
            "a bench name is letters, digits, '_' and '-', not starting with '-'".into(),
        ));
    }
    if !value.is_table() {
        return Err(DevError::Config(format!("{at} must be a table")));
    }
    let raw: RawBench = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| err(e.to_string()))?;
    let script = bench_script(name, raw.script, raw.script_file, sources).map_err(err)?;
    if raw.target.as_deref().is_some_and(|t| t.trim().is_empty()) {
        return Err(err(
            "target is blank - name a bin target or remove the key".into()
        ));
    }
    if raw.features.iter().any(|f| f.trim().is_empty()) {
        return Err(err("features has a blank entry".into()));
    }
    // The package's own feature names, as cargo reports the package's
    // resolved features: a dependency's (`dep/feat`) or an optional
    // dependency (`dep:name`) is not one brokkr can find in that list.
    if let Some(bad) = raw
        .hotpath_features
        .iter()
        .find(|f| f.trim().is_empty() || f.contains(['/', ':']) || f.contains(char::is_whitespace))
    {
        return Err(err(format!(
            "hotpath_features entry {bad:?} is not one of the package's own feature names"
        )));
    }
    if let Some(both) = raw
        .hotpath_features
        .iter()
        .find(|f| raw.features.contains(f))
    {
        return Err(err(format!(
            "{both:?} is in both features and hotpath_features: a timing run would be \
             instrumented"
        )));
    }
    let params = effective_params(&raw.uses, raw.params, sets).map_err(err)?;
    check_references(&params).map_err(err)?;
    Ok(Bench {
        name: name.to_owned(),
        script,
        target: raw.target,
        profile: raw.profile,
        features: raw.features,
        default_features: raw.default_features.unwrap_or(true),
        hotpath_features: raw.hotpath_features,
        uses: raw.uses,
        params,
    })
}

fn bench_script(
    name: &str,
    script: Option<String>,
    script_file: Option<String>,
    sources: &ConfigSources,
) -> Result<BenchScript, String> {
    let declaring = |field: &str| {
        sources
            .value_origin(&["bench", name, field])
            .ok_or_else(|| format!("no config file is recorded as setting {field}"))
    };
    match (script, script_file) {
        (Some(_), Some(_)) => Err(
            "sets both script and script_file - give the Lua inline or name its file, not both"
                .into(),
        ),
        (None, None) => Err("needs script (inline Lua) or script_file (a .lua file)".into()),
        (Some(source), None) => {
            if source.trim().is_empty() {
                return Err("script is blank".into());
            }
            let origin = declaring("script")?;
            Ok(BenchScript::Inline {
                source,
                declared_in: origin.file.to_path_buf(),
            })
        }
        (None, Some(file)) => {
            if file.trim().is_empty() {
                return Err("script_file is blank".into());
            }
            let origin = declaring("script_file")?;
            Ok(BenchScript::File {
                path: origin.base.join(file),
                declared_in: origin.file.to_path_buf(),
            })
        }
    }
}

/// Compose a bench's params: each used set's in turn, then the bench's own,
/// which replace a set's whole. Two sets declaring one param the bench does
/// not declare itself are refused: neither order is something the config says.
fn effective_params(
    uses: &[String],
    own: BTreeMap<String, toml::Value>,
    sets: &BTreeMap<String, BTreeMap<String, Param>>,
) -> Result<BTreeMap<String, Param>, String> {
    let mut out: BTreeMap<String, Param> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for set in uses {
        if !seen.insert(set.as_str()) {
            return Err(format!("uses lists {set:?} twice"));
        }
        let params = sets.get(set).ok_or_else(|| {
            let known: Vec<&str> = sets.keys().map(String::as_str).collect();
            let known = if known.is_empty() {
                "none".to_owned()
            } else {
                known.join(", ")
            };
            format!("uses {set:?}, which is not a [bench_params] set (defined: {known})")
        })?;
        for (name, param) in params {
            if own.contains_key(name) {
                continue;
            }
            if let Some(previous) = out.get(name)
                && let ParamOrigin::Set(other) = &previous.origin
            {
                return Err(format!(
                    "param {name:?} is declared by both used sets {other:?} and {set:?}; \
                     declare it in the bench to choose"
                ));
            }
            out.insert(name.clone(), param.clone());
        }
    }
    for (name, decl) in own {
        let param = parse_param(&format!("params.{name}"), &name, &decl, ParamOrigin::Bench)?;
        out.insert(name, param);
    }
    Ok(out)
}

/// The static half of the `from` rules, on the effective params: every
/// `{param}` names a param of this bench, none names a collection, and the
/// references form no cycle.
fn check_references(params: &BTreeMap<String, Param>) -> Result<(), String> {
    for (name, param) in params {
        let Some(from) = &param.from else { continue };
        for reference in from.references() {
            let Some(target) = params.get(reference) else {
                return Err(format!(
                    "param {name:?}: from {:?} references {{{reference}}}, which is not a param \
                     of this bench",
                    from.as_str()
                ));
            };
            if target.many {
                return Err(format!(
                    "param {name:?}: from {:?} references {{{reference}}}, a collection \
                     (many = true); a path can only take one key",
                    from.as_str()
                ));
            }
        }
    }
    // Depth-first, three colours: a reference back onto the current path is a
    // cycle, reported along that path.
    let mut done: BTreeSet<&str> = BTreeSet::new();
    for start in params.keys() {
        let mut path: Vec<&str> = Vec::new();
        visit(start, params, &mut path, &mut done)?;
    }
    Ok(())
}

fn visit<'a>(
    name: &'a str,
    params: &'a BTreeMap<String, Param>,
    path: &mut Vec<&'a str>,
    done: &mut BTreeSet<&'a str>,
) -> Result<(), String> {
    if done.contains(name) {
        return Ok(());
    }
    if let Some(at) = path.iter().position(|p| *p == name) {
        let mut cycle: Vec<&str> = path[at..].to_vec();
        cycle.push(name);
        return Err(format!(
            "params reference each other in a cycle: {}",
            cycle.join(" -> ")
        ));
    }
    path.push(name);
    if let Some(from) = params.get(name).and_then(|p| p.from.as_ref()) {
        for reference in from.references() {
            if let Some((key, _)) = params.get_key_value(reference) {
                visit(key, params, path, done)?;
            }
        }
    }
    path.pop();
    done.insert(name);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    /// A scratch tree for one test, populated with `(relative path, contents)`.
    fn tree(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = crate::test_scratch::scratch("config_bench", test_name);
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn benches_of(dir: &Path) -> Result<BTreeMap<String, Bench>, String> {
        super::super::load(dir)
            .map(|(_, cfg, _)| cfg.benches)
            .map_err(|e| e.to_string())
    }

    /// A one-file project config with `body` after the project line.
    fn single(test_name: &str, body: &str) -> Result<BTreeMap<String, Bench>, String> {
        let dir = tree(
            test_name,
            &[("brokkr.toml", &format!("project = \"p\"\n{body}"))],
        );
        benches_of(&dir)
    }

    fn err_of(test_name: &str, body: &str) -> String {
        match single(test_name, body) {
            Ok(_) => panic!("{test_name}: the config should be refused"),
            Err(e) => e,
        }
    }

    #[test]
    fn an_inline_script_and_its_build_settings_load() {
        let benches = single(
            "inline",
            "[bench.cat]\nscript = \"function bench() end\"\ntarget = \"pbfhogg\"\n\
             profile = \"release\"\nfeatures = [\"zlib-ng\"]\n",
        )
        .unwrap();
        let cat = &benches["cat"];
        assert!(matches!(
            &cat.script,
            BenchScript::Inline { source, declared_in }
                if source == "function bench() end" && declared_in.ends_with("brokkr.toml")
        ));
        assert_eq!(cat.target.as_deref(), Some("pbfhogg"));
        assert_eq!(cat.profile, Some(RustProfile::Release));
        assert_eq!(cat.features, ["zlib-ng"]);
        assert!(
            cat.default_features,
            "default features unless the table says otherwise"
        );
    }

    #[test]
    fn hotpath_features_are_the_packages_own_and_apart_from_features() {
        let benches = single(
            "hotpath_features",
            "[bench.cat]\nscript = \"x = 1\"\nhotpath_features = [\"hotpath\"]\n",
        )
        .unwrap();
        assert_eq!(benches["cat"].hotpath_features, ["hotpath"]);
        assert!(
            single("hotpath_absent", "[bench.cat]\nscript = \"x = 1\"\n").unwrap()["cat"]
                .hotpath_features
                .is_empty()
        );
        for (name, list) in [
            ("hotpath_dep", "[\"dep/hotpath\"]"),
            ("hotpath_optional", "[\"dep:hotpath\"]"),
            ("hotpath_blank", "[\" \"]"),
        ] {
            let err = err_of(
                name,
                &format!("[bench.cat]\nscript = \"x = 1\"\nhotpath_features = {list}\n"),
            );
            assert!(err.contains("own feature names"), "{name}: {err}");
        }
        let err = err_of(
            "hotpath_both",
            "[bench.cat]\nscript = \"x = 1\"\nfeatures = [\"hotpath\"]\n\
             hotpath_features = [\"hotpath\"]\n",
        );
        assert!(err.contains("both features and hotpath_features"), "{err}");
    }

    #[test]
    fn a_script_file_resolves_beside_the_file_that_set_it() {
        let dir = tree(
            "script_file_base",
            &[
                (
                    "brokkr.toml",
                    "project = \"p\"\ninclude = [\"benches/benches.toml\"]\n",
                ),
                (
                    "benches/benches.toml",
                    "[bench.cat]\nscript_file = \"cat.lua\"\n",
                ),
                ("benches/cat.lua", "function bench() end\n"),
            ],
        );
        let benches = benches_of(&dir).unwrap();
        let BenchScript::File { path, declared_in } = &benches["cat"].script else {
            panic!("expected a script file");
        };
        let base = std::fs::canonicalize(dir.join("benches")).unwrap();
        assert_eq!(path, &base.join("cat.lua"));
        assert_eq!(declared_in, &base.join("benches.toml"));
    }

    #[test]
    fn script_and_script_file_are_one_setting_across_layers() {
        // The including file swaps the included inline script for a file;
        // the included target still applies.
        let dir = tree(
            "script_slot",
            &[
                (
                    "brokkr.toml",
                    "project = \"p\"\ninclude = [\"a.toml\"]\n[bench.cat]\nscript_file = \"cat.lua\"\n",
                ),
                (
                    "a.toml",
                    "[bench.cat]\nscript = \"function bench() end\"\ntarget = \"t\"\n",
                ),
            ],
        );
        let cat = &benches_of(&dir).unwrap()["cat"];
        assert!(matches!(cat.script, BenchScript::File { .. }));
        assert_eq!(cat.target.as_deref(), Some("t"));

        // One file giving both is refused, whatever the layers beneath hold.
        let both = tree(
            "script_slot_both",
            &[
                (
                    "brokkr.toml",
                    "project = \"p\"\ninclude = [\"a.toml\"]\n[bench.cat]\nscript = \"x = 1\"\nscript_file = \"cat.lua\"\n",
                ),
                ("a.toml", "[bench.cat]\nscript = \"function bench() end\"\n"),
            ],
        );
        let err = benches_of(&both).unwrap_err();
        assert!(err.contains("both script and script_file"), "{err}");
    }

    #[test]
    fn a_bench_needs_exactly_one_nonblank_script_and_known_fields() {
        assert!(err_of("no_script", "[bench.cat]\ntarget = \"t\"\n").contains("needs script"));
        assert!(err_of("blank_script", "[bench.cat]\nscript = \"  \"\n").contains("blank"));
        assert!(
            err_of(
                "unknown_field",
                "[bench.cat]\nscript = \"x = 1\"\nexpect = 1\n"
            )
            .contains("expect")
        );
        assert!(
            err_of(
                "blank_target",
                "[bench.cat]\nscript = \"x = 1\"\ntarget = \"\"\n"
            )
            .contains("target")
        );
        assert!(
            err_of(
                "bad_profile",
                "[bench.cat]\nscript = \"x = 1\"\nprofile = \"debug\"\n"
            )
            .contains("debug")
        );
        assert!(err_of("bad_name", "[bench.\"a b\"]\nscript = \"x = 1\"\n").contains("bench name"));
        assert!(err_of("not_table", "bench = 1\n").contains("table"));
    }

    #[test]
    fn sets_compose_into_a_bench_and_the_bench_overrides_whole() {
        let benches = single(
            "uses",
            "[bench_params.data]\ndataset = { from = \"datasets\", default = \"denmark\" }\n\
             variant = { from = \"datasets.{dataset}.pbf\", help = \"set\" }\n\
             [bench.cat]\nscript = \"x = 1\"\nuses = [\"data\"]\n\
             params.variant = { from = \"datasets.{dataset}.pbf\", default = \"indexed\" }\n\
             params.jobs = { help = \"threads\" }\n",
        )
        .unwrap();
        let params = &benches["cat"].params;
        assert_eq!(
            params.keys().collect::<Vec<_>>(),
            ["dataset", "jobs", "variant"]
        );
        assert_eq!(params["dataset"].origin, ParamOrigin::Set("data".into()));
        // Replaced whole: the set's help does not survive the bench's own.
        assert_eq!(params["variant"].origin, ParamOrigin::Bench);
        assert_eq!(params["variant"].help, None);
        assert_eq!(params["variant"].default.as_deref(), Some("indexed"));
        let refs: Vec<&str> = params["variant"]
            .from
            .as_ref()
            .unwrap()
            .references()
            .collect();
        assert_eq!(refs, ["dataset"]);
    }

    #[test]
    fn uses_must_name_distinct_sets_that_do_not_overlap() {
        let sets = "[bench_params.a]\nx = {}\n[bench_params.b]\nx = {}\n";
        let err = err_of(
            "uses_unknown",
            &format!("{sets}[bench.c]\nscript = \"x = 1\"\nuses = [\"z\"]\n"),
        );
        assert!(
            err.contains("not a [bench_params] set") && err.contains("a, b"),
            "{err}"
        );
        let err = err_of(
            "uses_twice",
            &format!("{sets}[bench.c]\nscript = \"x = 1\"\nuses = [\"a\", \"a\"]\n"),
        );
        assert!(err.contains("twice"), "{err}");
        let err = err_of(
            "uses_overlap",
            &format!("{sets}[bench.c]\nscript = \"x = 1\"\nuses = [\"a\", \"b\"]\n"),
        );
        assert!(err.contains("both used sets"), "{err}");
        // The bench declaring the param itself settles the overlap.
        single(
            "uses_overlap_settled",
            &format!(
                "{sets}[bench.c]\nscript = \"x = 1\"\nuses = [\"a\", \"b\"]\nparams.x = {{}}\n"
            ),
        )
        .unwrap();
        // A set no bench uses is not an error: a shared include may carry
        // sets for several projects.
        single("unused_set", sets).unwrap();
    }

    #[test]
    fn from_references_are_judged_on_the_effective_params() {
        // A set referencing a param it does not declare is fine once the
        // bench supplies it.
        single(
            "ref_supplied",
            "[bench_params.v]\nvariant = { from = \"datasets.{dataset}.pbf\" }\n\
             [bench.c]\nscript = \"x = 1\"\nuses = [\"v\"]\nparams.dataset = { from = \"datasets\" }\n",
        )
        .unwrap();
        let err = err_of(
            "ref_unknown",
            "[bench.c]\nscript = \"x = 1\"\nparams.variant = { from = \"datasets.{dataset}.pbf\" }\n",
        );
        assert!(err.contains("not a param of this bench"), "{err}");
        let err = err_of(
            "ref_cycle",
            "[bench.c]\nscript = \"x = 1\"\nparams.a = { from = \"x.{b}\" }\nparams.b = { from = \"y.{a}\" }\n",
        );
        assert!(
            err.contains("cycle") && err.contains("a -> b -> a"),
            "{err}"
        );
        let err = err_of(
            "ref_many",
            "[bench.c]\nscript = \"x = 1\"\nparams.osc = { from = \"o\", many = true }\n\
             params.v = { from = \"x.{osc}\" }\n",
        );
        assert!(err.contains("collection"), "{err}");
        let err = err_of(
            "ref_mixed",
            "[bench.c]\nscript = \"x = 1\"\nparams.v = { from = \"x.a{b}\" }\n",
        );
        assert!(err.contains("whole segment"), "{err}");
        let err = err_of(
            "ref_empty",
            "[bench.c]\nscript = \"x = 1\"\nparams.v = { from = \"x..y\" }\n",
        );
        assert!(err.contains("empty segment"), "{err}");
    }

    #[test]
    fn param_names_are_flags_and_help_is_brokkrs() {
        let err = err_of(
            "param_help",
            "[bench.c]\nscript = \"x = 1\"\nparams.help = {}\n",
        );
        assert!(err.contains("reserved"), "{err}");
        let err = err_of(
            "param_upper",
            "[bench.c]\nscript = \"x = 1\"\nparams.Jobs = {}\n",
        );
        assert!(err.contains("--flag"), "{err}");
        let err = err_of(
            "param_field",
            "[bench.c]\nscript = \"x = 1\"\nparams.j = { flag = \"x\" }\n",
        );
        assert!(err.contains("flag"), "{err}");
        let err = err_of(
            "param_required_default",
            "[bench.c]\nscript = \"x = 1\"\nparams.j = { required = true, default = \"1\" }\n",
        );
        assert!(err.contains("required and default"), "{err}");
        // Judged on the declaration, so in a set no bench uses too.
        let err = err_of(
            "set_required_default",
            "[bench_params.s]\nj = { required = true, default = \"1\" }\n",
        );
        assert!(err.contains("required and default"), "{err}");
        let benches = single(
            "param_required",
            "[bench.c]\nscript = \"x = 1\"\nparams.j = { required = true }\nparams.k = {}\n",
        )
        .unwrap();
        assert!(benches["c"].params["j"].required);
        assert!(!benches["c"].params["k"].required, "absent means optional");
        single(
            "param_kebab",
            "[bench.c]\nscript = \"x = 1\"\nparams.osc-range = { many = true }\n",
        )
        .unwrap();
    }

    #[test]
    fn data_is_an_uninterpreted_table() {
        // Including a `datasets` table holding `sha256`, which in a host
        // section is the retired digest key.
        single(
            "data_opaque",
            "[data.datasets.x]\nsha256 = \"project data\"\n[data.workloads.w]\nanything = [1, 2]\n",
        )
        .unwrap();
        assert!(err_of("data_scalar", "data = 1\n").contains("data must be a table"));
    }

    #[test]
    fn hosts_named_like_the_bench_sections_use_the_host_namespace() {
        let dir = tree(
            "host_namespace",
            &[(
                "brokkr.toml",
                "project = \"p\"\n[host.data]\nport = 1\n[host.bench]\nport = 2\n",
            )],
        );
        let (_, cfg, _) = super::super::load(&dir).unwrap();
        let hosts: HashMap<&str, Option<u16>> = cfg
            .hosts
            .iter()
            .map(|(name, host)| (name.as_str(), host.port))
            .collect();
        assert_eq!(hosts["data"], Some(1));
        assert_eq!(hosts["bench"], Some(2));
        assert!(cfg.benches.is_empty());
    }
}
