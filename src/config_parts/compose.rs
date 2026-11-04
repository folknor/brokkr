// `include = [...]`: one project config composed from several files.
//
// A `brokkr.toml` (or any file it includes) may carry a top-level
// `include = ["../shared/hosts.toml", ...]`. Each path is resolved against the
// directory of the file naming it, and the included files are folded in
// *beneath* it: a file's own keys override everything it includes, and a later
// include overrides an earlier one. Flattened, that is a post-order walk of
// the include graph, lowest precedence first, with the user-wide config first
// and the project's `brokkr.toml` last - and folding the files in that order
// is the whole algorithm.
//
// The merge rules, applied key by key:
// - table + table: merged recursively (a host section split across files),
//   except a `[cargo_env]` entry's inline table, which the higher layer
//   replaces whole. In a `[bench.NAME]` table, `script` and `script_file`
//   are one setting: a layer giving either removes both from the layers
//   beneath it.
// - array of tables + array of tables (`[[check]]`, `[[textlint]]`, ...):
//   joined, lower-precedence entries first, and an entry whose `name` the
//   higher-precedence side reuses is dropped - a higher layer can redefine
//   a shared entry to replace it.
// - anything else: the higher-precedence value replaces the lower one.
//
// The composition is resolved here, before any section parser runs, so every
// section supports inclusion without knowing it exists. The price is that a
// parser's error names the section, not the file. [`ConfigSources`] records
// which files define every table, so the one place brokkr *writes* its config
// (dataset registration) can edit the file that actually holds the entry, and
// which file supplied every other value (an array of tables aside, whose
// entries join across files), so a bench's script is located and
// named by the file that set it.
//
// Paths *inside* an included file (dataset files, script commands, globs) mean
// what they mean in `brokkr.toml`: relative to the project root. Only `include`
// paths and a bench's `script_file` resolve against their own file's
// directory, because they are the things whose meaning is the file layout: a
// script file is config split out of the TOML that names it.

/// Where each part of a composed config came from.
#[derive(Debug, Clone, Default)]
pub struct ConfigSources {
    /// Every file in the composition, lowest precedence first; the project's
    /// `brokkr.toml` is last when present.
    files: Vec<PathBuf>,
    /// Project include files only. The user layer is a composition source but
    /// must not be treated as an in-tree include by dirty-check callers.
    included_files: Vec<PathBuf>,
    /// The directory each file's relative paths resolve against, aligned with
    /// `files`: the canonical parent, as `include` entries use, even where
    /// `files` keeps the root's spelling for messages (a symlinked
    /// `brokkr.toml` resolves beside its target).
    bases: Vec<PathBuf>,
    /// Table path -> the files (indices into `files`) that define it, whether
    /// with a header of their own or implicitly through a sub-table.
    tables: BTreeMap<Vec<String>, BTreeSet<usize>>,
    /// Value path -> the file that supplied the value the composition kept,
    /// for every value that is not a table. A plain array is one value; an
    /// array of tables is not tracked, since its entries join across files.
    values: BTreeMap<Vec<String>, usize>,
    /// The user layer's effective contribution after higher layers shadow
    /// named entries.
    user_layer: Option<UserLayerSources>,
    /// The composed table itself, as every section was parsed from it; set
    /// by [`load_with_sources`], empty otherwise. A script-bench run stores
    /// it as the configuration it ran under.
    composed: toml::Table,
}

/// The user-wide config file and the entries it contributed to the composed
/// config after project layers shadow entries by name.
#[derive(Debug, Clone)]
pub struct UserLayerSources {
    pub path: PathBuf,
    pub textlint: usize,
    pub script_checks: usize,
    /// The names of the `[[script_check]]` entries the user layer
    /// contributed. Disjoint from every project-layer entry's name, since a
    /// project entry reusing a name shadows the user one.
    pub script_check_names: BTreeSet<String>,
}

impl ConfigSources {
    /// The highest-precedence file (`brokkr.toml` for a project composition).
    pub fn root(&self) -> Option<&Path> {
        self.files.last().map(PathBuf::as_path)
    }

    /// The project config's included files, lowest precedence first.
    pub fn included(&self) -> &[PathBuf] {
        &self.included_files
    }

    /// The composed table every section was parsed from.
    pub fn composed(&self) -> &toml::Table {
        &self.composed
    }

    /// The user-wide config's effective contribution, if its file was read.
    pub fn user_layer(&self) -> Option<&UserLayerSources> {
        self.user_layer.as_ref()
    }

    /// The files that define the table at `path`.
    pub fn table_files(&self, path: &[&str]) -> Vec<&Path> {
        let key: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        self.tables
            .get(&key)
            .into_iter()
            .flatten()
            .map(|&i| self.files[i].as_path())
            .collect()
    }

    /// The file a writer should edit to change the table at `path`.
    ///
    /// The file that defines it, when exactly one does. A table no file
    /// defines yet goes where its nearest defined ancestor lives, when that is
    /// one file - a new dataset lands beside the host's other datasets - and
    /// in the root otherwise. A table split across several files is refused:
    /// an edit to one half would leave the composed entry a mixture of the
    /// edited half and a stale other half.
    pub fn owner_of_table(&self, path: &[&str]) -> Result<&Path, DevError> {
        let root = self
            .root()
            .ok_or_else(|| DevError::Config("no config files resolved".into()))?;
        for depth in (1..=path.len()).rev() {
            let files = self.table_files(&path[..depth]);
            match files.as_slice() {
                [] => {}
                [one] => return Ok(one),
                many if depth == path.len() => {
                    let list: Vec<String> =
                        many.iter().map(|p| p.display().to_string()).collect();
                    return Err(DevError::Config(format!(
                        "[{}] is defined in more than one config file ({}); brokkr \
                         writes a table into the one file that holds it - move the \
                         whole table into one of them",
                        path.join("."),
                        list.join(", ")
                    )));
                }
                _ => return Ok(root),
            }
        }
        Ok(root)
    }

    /// The file that supplied the value at `path`, and the directory a
    /// relative path in it resolves against. `None` for a table, or a path
    /// the composition does not hold.
    pub fn value_origin(&self, path: &[&str]) -> Option<ValueOrigin<'_>> {
        let key: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        let &idx = self.values.get(&key)?;
        Some(ValueOrigin {
            file: self.files[idx].as_path(),
            base: self.bases[idx].as_path(),
        })
    }

    /// Forget every table and value at or below `prefix`: a higher-precedence
    /// file replaced that value outright.
    fn forget(&mut self, prefix: &[String]) {
        self.tables.retain(|path, _| !path.starts_with(prefix));
        self.values.retain(|path, _| !path.starts_with(prefix));
    }
}

/// Where one composed value came from: [`ConfigSources::value_origin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueOrigin<'a> {
    /// The config file, spelled as messages show it.
    pub file: &'a Path,
    /// The canonical directory the file's relative paths resolve against.
    pub base: &'a Path,
}

/// Read the user-wide config, `root`, and everything it includes, and fold
/// them into one table. The user file is the lowest-precedence layer.
///
/// `root` is the project's `brokkr.toml`; only it may carry `project`. An
/// include that is missing, unreadable, malformed, part of a cycle, or reached
/// twice is an error - skipping it would silently drop whatever it configured,
/// and a file folded in twice would duplicate its array entries.
pub fn compose(root: &Path) -> Result<(toml::Table, ConfigSources), DevError> {
    compose_layers(Some(root))
}

/// Compose only the user-wide config. Used when a caller needs a normal
/// `DevConfig` in a tree with no project config.
pub fn compose_user_only() -> Result<(toml::Table, ConfigSources), DevError> {
    compose_layers(None)
}

fn compose_layers(root: Option<&Path>) -> Result<(toml::Table, ConfigSources), DevError> {
    let user_layer = selected_user_table()?;
    let mut seen = HashSet::new();
    let mut user_base = None;
    if let Some((path, _)) = &user_layer {
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| DevError::Config(format!("{}: {e}", path.display())))?;
        user_base = Some(canonical.parent().map(Path::to_path_buf).unwrap_or_default());
        seen.insert(canonical);
    }

    let mut files = Vec::new();
    let mut bases = Vec::new();
    {
        let mut walk = Walk {
            stack: Vec::new(),
            seen,
            out: &mut files,
            bases: &mut bases,
        };
        if let Some(root) = root {
            walk.visit(root, None)?;
        }
    }
    let included_files: Vec<PathBuf> = if root.is_some() {
        files
            .iter()
            .take(files.len().saturating_sub(1))
            .map(|(path, _)| path.clone())
            .collect()
    } else {
        Vec::new()
    };
    let has_user_layer = user_layer.is_some();
    let user_sources = user_layer.as_ref().map(|(path, table)| {
        let scripts = surviving_user_entries(table, "script_check", &files);
        UserLayerSources {
            path: path.clone(),
            textlint: surviving_user_entries(table, "textlint", &files).len(),
            script_checks: scripts.len(),
            script_check_names: scripts.into_iter().filter_map(entry_name).map(str::to_owned).collect(),
        }
    });
    if let Some(user_layer) = user_layer {
        files.insert(0, user_layer);
        bases.insert(0, user_base.unwrap_or_default());
    }

    let mut sources = ConfigSources {
        included_files,
        bases,
        user_layer: user_sources,
        ..ConfigSources::default()
    };
    let mut merged = toml::Table::new();
    for (idx, (path, table)) in files.into_iter().enumerate() {
        sources.files.push(path);
        merge_into(&mut merged, table, idx, &mut Vec::new(), &mut sources);
    }
    if has_user_layer {
        drop_orphaned_user_presets(&mut merged, &sources);
    }
    Ok((merged, sources))
}

/// Select the normal user file in production. Unit tests run with no user
/// layer so a developer's XDG config cannot alter fixture loads; user-layer
/// merging is tested through `merge_into` directly.
#[cfg(not(test))]
fn selected_user_table() -> Result<Option<(PathBuf, toml::Table)>, DevError> {
    load_user_table()
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)] // mirrors the production signature
fn selected_user_table() -> Result<Option<(PathBuf, toml::Table)>, DevError> {
    Ok(None)
}

/// The user entries that remain after the higher-precedence project layers
/// have replaced same-named entries, or replaced the whole array with a value
/// that is not an array of tables. Unnamed entries always survive the array
/// merge, matching `merge_entries`. A higher layer reusing a name drops every
/// user entry of that name, so a surviving entry's name is never also a
/// project entry's.
fn surviving_user_entries<'a>(
    user: &'a toml::Table,
    key: &str,
    higher_layers: &[(PathBuf, toml::Table)],
) -> Vec<&'a toml::Value> {
    let Some(user_entries) = user.get(key).and_then(toml::Value::as_array) else {
        return Vec::new();
    };
    let mut shadowed = HashSet::new();
    let mut replaced = false;
    for (_, layer) in higher_layers {
        let Some(value) = layer.get(key) else {
            continue;
        };
        match value.as_array() {
            Some(entries) if is_table_array(entries) => {
                shadowed.extend(entries.iter().filter_map(entry_name).map(str::to_owned));
            }
            _ => replaced = true,
        }
    }
    if replaced {
        return Vec::new();
    }
    user_entries
        .iter()
        .filter(|entry| entry_name(entry).is_none_or(|name| !shadowed.contains(name)))
        .collect()
}

/// Remove `[textlint_preset.*]` blocks that only the user layer (file 0)
/// defines and that no composed `[[textlint]]` rule references any more.
///
/// The parser rejects an unreferenced preset as dead config. That is right for
/// a file's own presets, but a user preset can be orphaned by the project
/// itself: shadowing a user rule by `name` is the documented opt-out, and the
/// shadowed rule may have been the preset's only reference. Refusing the load
/// would turn the opt-out into an error the project cannot fix from its own
/// tree, so such a preset is dropped instead. A preset any project file
/// defines still goes through the dead-config check.
fn drop_orphaned_user_presets(merged: &mut toml::Table, sources: &ConfigSources) {
    let mut referenced: HashSet<String> = HashSet::new();
    if let Some(toml::Value::Array(rules)) = merged.get("textlint") {
        for rule in rules {
            match rule.get("preset") {
                Some(toml::Value::String(name)) => {
                    referenced.insert(name.clone());
                }
                Some(toml::Value::Array(names)) => {
                    referenced.extend(names.iter().filter_map(|n| n.as_str().map(str::to_owned)));
                }
                _ => {}
            }
        }
    }
    let Some(toml::Value::Table(presets)) = merged.get_mut("textlint_preset") else {
        return;
    };
    presets.retain(|name, _| {
        let user_only = sources
            .tables
            .get(&vec!["textlint_preset".to_owned(), name.to_owned()])
            .is_some_and(|defs| defs.len() == 1 && defs.contains(&0));
        !user_only || referenced.contains(name)
    });
    if presets.is_empty() {
        merged.remove("textlint_preset");
    }
}

/// Depth-first walk of the include graph, collecting files in post-order.
struct Walk<'a> {
    /// Canonical paths of the files currently being expanded, for cycles.
    stack: Vec<PathBuf>,
    /// Canonical paths of every file reached so far.
    seen: HashSet<PathBuf>,
    out: &'a mut Vec<(PathBuf, toml::Table)>,
    /// Each collected file's canonical directory, aligned with `out`.
    bases: &'a mut Vec<PathBuf>,
}

impl Walk<'_> {
    /// `from` is the file whose `include` named `path`; `None` for the root.
    fn visit(&mut self, path: &Path, from: Option<&Path>) -> Result<(), DevError> {
        let named_by = |msg: String| match from {
            Some(f) => DevError::Config(format!("{}: include {}: {msg}", f.display(), path.display())),
            None => DevError::Config(format!("{}: {msg}", path.display())),
        };

        let text = std::fs::read_to_string(path).map_err(|e| named_by(e.to_string()))?;
        // The root keeps its bare path in messages; an include is recorded
        // canonically, since its spelling depends on who named it.
        let canonical = std::fs::canonicalize(path).map_err(|e| named_by(e.to_string()))?;
        if self.stack.contains(&canonical) {
            let mut chain: Vec<String> =
                self.stack.iter().map(|p| p.display().to_string()).collect();
            chain.push(canonical.display().to_string());
            return Err(named_by(format!("include cycle: {}", chain.join(" -> "))));
        }
        if !self.seen.insert(canonical.clone()) {
            return Err(named_by(
                "included more than once - each file may enter the composition once, \
                 or its [[array]] entries would be counted twice"
                    .into(),
            ));
        }

        let mut table: toml::Table = if from.is_none() {
            toml::from_str(&text)?
        } else {
            toml::from_str(&text).map_err(|e| DevError::Config(format!("{}: {e}", canonical.display())))?
        };
        let shown = if from.is_none() { path.to_path_buf() } else { canonical.clone() };
        let at = |msg: &str| DevError::Config(format!("{}: {msg}", shown.display()));

        if from.is_some() && table.contains_key("project") {
            return Err(at(
                "'project' belongs in the project's own brokkr.toml, not in an included file",
            ));
        }
        let includes = match table.remove("include") {
            None => Vec::new(),
            Some(toml::Value::Array(items)) => items
                .into_iter()
                .map(|v| match v {
                    toml::Value::String(s) if !s.trim().is_empty() => Ok(s),
                    _ => Err(at("'include' entries must be non-empty path strings")),
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(at("'include' must be an array of paths")),
        };

        let dir = canonical.parent().map(Path::to_path_buf).unwrap_or_default();
        self.stack.push(canonical);
        for inc in includes {
            self.visit(&dir.join(inc), Some(&shown))?;
        }
        self.stack.pop();
        self.out.push((shown, table));
        self.bases.push(dir);
        Ok(())
    }
}

/// Fold `overlay` (from file `idx`) into `base` under the rules in the module
/// header, recording provenance as it goes. `path` is the table path of
/// `base`, for [`ConfigSources`].
fn merge_into(
    base: &mut toml::Table,
    overlay: toml::Table,
    idx: usize,
    path: &mut Vec<String>,
    sources: &mut ConfigSources,
) {
    sources.tables.entry(path.clone()).or_default().insert(idx);
    clear_replaced_alternatives(base, &overlay, path, sources);
    for (key, value) in overlay {
        path.push(key.clone());
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) if !is_atomic(path) => {
                merge_into(b, o, idx, path, sources);
            }
            (Some(toml::Value::Array(b)), toml::Value::Array(o))
                if is_table_array(b) && is_table_array(&o) =>
            {
                merge_entries(b, o);
            }
            (_, value) => {
                sources.forget(path);
                match value {
                    toml::Value::Table(o) => {
                        let mut fresh = toml::Table::new();
                        merge_into(&mut fresh, o, idx, path, sources);
                        base.insert(key, toml::Value::Table(fresh));
                    }
                    other => {
                        // An array of tables may later join entries from
                        // several files, so no one file supplied it.
                        let joinable = other.as_array().is_some_and(|items| is_table_array(items));
                        if !joinable {
                            sources.values.insert(path.clone(), idx);
                        }
                        base.insert(key, other);
                    }
                }
            }
        }
        path.pop();
    }
}

/// Keys that are alternatives for one setting: a layer setting any of them
/// replaces whichever the lower layers set. A bench's script is inline
/// (`script`) or a file (`script_file`), so an including file can swap one
/// for the other. The lower alternatives are removed once, before the
/// overlay merges, so an overlay holding both keeps both and the parser
/// refuses it.
fn clear_replaced_alternatives(
    base: &mut toml::Table,
    overlay: &toml::Table,
    path: &mut Vec<String>,
    sources: &mut ConfigSources,
) {
    const SCRIPT: [&str; 2] = ["script", "script_file"];
    let is_bench = matches!(path.as_slice(), [table, _] if table == "bench");
    if !is_bench || !SCRIPT.iter().any(|key| overlay.contains_key(*key)) {
        return;
    }
    for key in SCRIPT {
        if base.remove(key).is_some() {
            path.push(key.to_owned());
            sources.forget(path);
            path.pop();
        }
    }
}

/// A table at `path` that a higher layer replaces whole rather than merging
/// into: a `[cargo_env]` entry. Its table form pairs a command with the path
/// that decides where it applies, and a layer redefining the command must not
/// inherit a guard written for another one.
fn is_atomic(path: &[String]) -> bool {
    matches!(path, [table, _] if table == "cargo_env")
}

/// An array whose every element is a table - a TOML `[[array]]`. An empty
/// array is not one: nothing about it says what it would hold.
fn is_table_array(items: &[toml::Value]) -> bool {
    !items.is_empty() && items.iter().all(toml::Value::is_table)
}

/// The `name` of an array entry, when it has one.
fn entry_name(item: &toml::Value) -> Option<&str> {
    item.get("name").and_then(toml::Value::as_str)
}

/// Append `overlay` to `base`, dropping every `base` entry whose name an
/// `overlay` entry reuses. Unnamed entries always survive.
fn merge_entries(base: &mut Vec<toml::Value>, overlay: Vec<toml::Value>) {
    let taken: HashSet<String> = overlay
        .iter()
        .filter_map(entry_name)
        .map(str::to_owned)
        .collect();
    base.retain(|item| entry_name(item).is_none_or(|n| !taken.contains(n)));
    base.extend(overlay);
}

#[cfg(test)]
mod compose_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    /// A scratch dir for one test, populated with `(relative path, contents)`.
    fn tree(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = crate::test_scratch::scratch("config_compose", test_name);
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn err_of(dir: &Path) -> String {
        match compose(&dir.join("brokkr.toml")) {
            Ok(_) => panic!("composition should fail"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn tables_merge_and_the_including_file_wins() {
        let dir = tree("tables_merge", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"a.toml\"]\n[h]\nport = 2\n"),
            ("a.toml", "[h]\nport = 1\ndata = \"d\"\n[h.datasets.x]\norigin = \"o\"\n"),
        ]);
        let (t, _) = compose(&dir.join("brokkr.toml")).unwrap();
        assert_eq!(t["h"]["port"].as_integer(), Some(2));
        assert_eq!(t["h"]["data"].as_str(), Some("d"));
        assert_eq!(t["h"]["datasets"]["x"]["origin"].as_str(), Some("o"));
        assert!(t.get("include").is_none());
    }

    #[test]
    fn a_cargo_env_entry_is_replaced_whole_not_merged() {
        let dir = tree("cargo_env_atomic", &[
            (
                "brokkr.toml",
                "project = \"p\"\ninclude = [\"a.toml\"]\n[cargo_env]\nID = { command = \"new\" }\n",
            ),
            (
                "a.toml",
                "[cargo_env]\nID = { command = \"old\", if_exists = \"old.py\" }\nOTHER = \"o\"\n",
            ),
        ]);
        let (t, _) = compose(&dir.join("brokkr.toml")).unwrap();
        assert_eq!(t["cargo_env"]["ID"]["command"].as_str(), Some("new"));
        assert!(t["cargo_env"]["ID"].get("if_exists").is_none());
        // The table itself still merges: an entry only one layer names stays.
        assert_eq!(t["cargo_env"]["OTHER"].as_str(), Some("o"));
    }

    #[test]
    fn array_entries_join_and_shadow_by_name() {
        let dir = tree("arrays", &[
            (
                "brokkr.toml",
                "project = \"p\"\ninclude = [\"a.toml\"]\n\
                 [[check]]\nname = \"b\"\nfeatures = [\"local\"]\n\
                 [[check]]\nname = \"c\"\n",
            ),
            ("a.toml", "[[check]]\nname = \"a\"\n[[check]]\nname = \"b\"\nfeatures = [\"shared\"]\n"),
        ]);
        let (t, _) = compose(&dir.join("brokkr.toml")).unwrap();
        let checks = t["check"].as_array().unwrap();
        let names: Vec<&str> = checks.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(checks[1]["features"][0].as_str(), Some("local"));
    }

    #[test]
    fn nested_includes_resolve_against_their_own_file_and_later_ones_win() {
        let dir = tree("nested", &[
            ("proj/brokkr.toml", "project = \"p\"\ninclude = [\"../shared/a.toml\", \"../shared/b.toml\"]\n"),
            ("shared/a.toml", "include = [\"deep/c.toml\"]\n[h]\nport = 1\n"),
            ("shared/b.toml", "[h]\nport = 2\n"),
            ("shared/deep/c.toml", "[h]\nport = 0\ndata = \"c\"\n"),
        ]);
        let root = dir.join("proj/brokkr.toml");
        let (t, sources) = compose(&root).unwrap();
        assert_eq!(t["h"]["port"].as_integer(), Some(2));
        assert_eq!(t["h"]["data"].as_str(), Some("c"));
        let names: Vec<String> = sources
            .included()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["c.toml", "a.toml", "b.toml"]);
        assert_eq!(sources.root(), Some(root.as_path()));
    }

    #[test]
    fn a_replaced_value_takes_its_provenance_with_it() {
        let dir = tree("replaced", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"a.toml\"]\n[h]\ndatasets = \"gone\"\n"),
            ("a.toml", "[h.datasets.x]\norigin = \"o\"\n"),
        ]);
        let (_, sources) = compose(&dir.join("brokkr.toml")).unwrap();
        assert!(sources.table_files(&["h", "datasets", "x"]).is_empty());
    }

    #[test]
    fn every_value_knows_the_file_that_supplied_it() {
        let dir = tree("values", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"sub/a.toml\"]\n[h]\nport = 2\n"),
            ("sub/a.toml", "[h]\nport = 1\ndata = \"d\"\n[t]\nx = 1\n"),
        ]);
        let root = dir.join("brokkr.toml");
        let sub = std::fs::canonicalize(dir.join("sub")).unwrap();
        let (_, s) = compose(&root).unwrap();
        let port = s.value_origin(&["h", "port"]).unwrap();
        assert_eq!(port.file, root);
        assert_eq!(port.base, std::fs::canonicalize(&dir).unwrap());
        let data = s.value_origin(&["h", "data"]).unwrap();
        assert_eq!(data.file, sub.join("a.toml"));
        assert_eq!(data.base, sub);
        // A table is not a value, and neither is an array of tables, whose
        // entries may come from several files.
        assert!(s.value_origin(&["h"]).is_none());
        let arrays = tree("values_arrays", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"a.toml\"]\n[[check]]\nname = \"b\"\n"),
            ("a.toml", "[[check]]\nname = \"a\"\n"),
        ]);
        let (_, s) = compose(&arrays.join("brokkr.toml")).unwrap();
        assert!(s.value_origin(&["check"]).is_none());

        // A higher layer replacing a table with a value takes the values
        // beneath it along.
        let replaced = tree("values_replaced", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"a.toml\"]\nt = 3\n"),
            ("a.toml", "[t]\nx = 1\n"),
        ]);
        let (_, s) = compose(&replaced.join("brokkr.toml")).unwrap();
        assert!(s.value_origin(&["t", "x"]).is_none());
        assert!(s.value_origin(&["t"]).is_some());
    }

    #[test]
    fn a_symlinked_root_resolves_beside_its_target() {
        let dir = tree("symlinked_root", &[("shared/config.toml", "project = \"p\"\nx = 1\n")]);
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        std::os::unix::fs::symlink(dir.join("shared/config.toml"), dir.join("proj/brokkr.toml")).unwrap();
        let root = dir.join("proj/brokkr.toml");
        let (_, s) = compose(&root).unwrap();
        let origin = s.value_origin(&["x"]).unwrap();
        // Shown as spelled, resolved where the file really is.
        assert_eq!(origin.file, root);
        assert_eq!(origin.base, std::fs::canonicalize(dir.join("shared")).unwrap());
    }

    #[test]
    fn owner_is_the_defining_file_then_the_ancestors_then_the_root() {
        let dir = tree("owner", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"hosts.toml\"]\n[h.datasets.local]\norigin = \"l\"\n"),
            ("hosts.toml", "[h.datasets.shared]\norigin = \"s\"\n[g.datasets.only]\norigin = \"o\"\n"),
        ]);
        let root = dir.join("brokkr.toml");
        let hosts = std::fs::canonicalize(dir.join("hosts.toml")).unwrap();
        let (_, s) = compose(&root).unwrap();
        assert_eq!(s.owner_of_table(&["h", "datasets", "shared"]).unwrap(), hosts);
        assert_eq!(s.owner_of_table(&["h", "datasets", "local"]).unwrap(), root);
        // New dataset, host's datasets split across both files -> root.
        assert_eq!(s.owner_of_table(&["h", "datasets", "new"]).unwrap(), root);
        // New dataset, host's datasets live in one file -> that file.
        assert_eq!(s.owner_of_table(&["g", "datasets", "new"]).unwrap(), hosts);
        // Unknown host -> root.
        assert_eq!(s.owner_of_table(&["z", "datasets", "new"]).unwrap(), root);
    }

    #[test]
    fn a_split_table_has_no_owner() {
        let dir = tree("split", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"a.toml\"]\n[h.datasets.d]\norigin = \"l\"\n"),
            ("a.toml", "[h.datasets.d.pbf.raw]\nfile = \"f\"\nxxhash = \"x\"\n"),
        ]);
        let (_, s) = compose(&dir.join("brokkr.toml")).unwrap();
        let err = s.owner_of_table(&["h", "datasets", "d"]).unwrap_err().to_string();
        assert!(err.contains("more than one config file"), "{err}");
    }

    #[test]
    fn cycles_duplicates_and_project_are_refused() {
        let cycle = tree("cycle", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"a.toml\"]\n"),
            ("a.toml", "include = [\"brokkr.toml\"]\n"),
        ]);
        assert!(err_of(&cycle).contains("include cycle"));

        let twice = tree("twice", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"a.toml\", \"./a.toml\"]\n"),
            ("a.toml", ""),
        ]);
        assert!(err_of(&twice).contains("included more than once"));

        let project = tree("project", &[
            ("brokkr.toml", "project = \"p\"\ninclude = [\"a.toml\"]\n"),
            ("a.toml", "project = \"q\"\n"),
        ]);
        assert!(err_of(&project).contains("'project' belongs"));
    }

    #[test]
    fn a_missing_or_malformed_include_is_an_error_naming_the_includer() {
        let missing = tree("missing", &[("brokkr.toml", "project = \"p\"\ninclude = [\"nope.toml\"]\n")]);
        let err = err_of(&missing);
        assert!(err.contains("include nope.toml") || err.contains("nope.toml"), "{err}");
        assert!(err.contains("brokkr.toml"), "{err}");

        let shape = tree("shape", &[("brokkr.toml", "project = \"p\"\ninclude = \"a.toml\"\n")]);
        assert!(err_of(&shape).contains("must be an array"));
    }

    #[test]
    fn load_sees_datasets_from_an_include() {
        let dir = tree("load", &[
            ("brokkr.toml", "project = \"pbfhogg\"\ninclude = [\"hosts.toml\"]\n"),
            ("hosts.toml", "[h]\nport = 4000\n"),
        ]);
        let (_, cfg, _) = load(&dir).unwrap();
        assert_eq!(cfg.hosts["h"].port, Some(4000));
    }
}
