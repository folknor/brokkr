// `include = [...]`: one project config composed from several files.
//
// A `brokkr.toml` (or any file it includes) may carry a top-level
// `include = ["../shared/hosts.toml", ...]`. Each path is resolved against the
// directory of the file naming it, and the included files are folded in
// *beneath* it: a file's own keys override everything it includes, and a later
// include overrides an earlier one. Flattened, that is a post-order walk of
// the include graph, lowest precedence first, with the project's `brokkr.toml`
// last - and folding the files in that order is the whole algorithm.
//
// The merge rules, applied key by key:
// - table + table: merged recursively (a host section split across files).
// - array of tables + array of tables (`[[check]]`, `[[textlint]]`, ...):
//   joined, lower-precedence entries first, and an entry whose `name` the
//   higher-precedence side reuses is dropped - the user layer's shadowing
//   rule, so a project redefines a shared entry to replace it.
// - anything else: the higher-precedence value replaces the lower one.
//
// The composition is resolved here, before any section parser runs, so every
// section supports inclusion without knowing it exists. The price is that a
// parser's error names the section, not the file. [`ConfigSources`] records
// which files define every table, so the one place brokkr *writes* its config
// (dataset registration) can edit the file that actually holds the entry.
//
// Paths *inside* an included file (dataset files, script commands, globs) mean
// what they mean in `brokkr.toml`: relative to the project root. Only the
// `include` paths themselves resolve against their own file, because they are
// the one thing whose meaning is the file layout.

/// Where each part of a composed config came from.
#[derive(Debug, Clone, Default)]
pub struct ConfigSources {
    /// Every file in the composition, lowest precedence first; the project's
    /// `brokkr.toml` is last.
    files: Vec<PathBuf>,
    /// Table path -> the files (indices into `files`) that define it, whether
    /// with a header of their own or implicitly through a sub-table.
    tables: BTreeMap<Vec<String>, BTreeSet<usize>>,
}

impl ConfigSources {
    /// The project's own `brokkr.toml`.
    pub fn root(&self) -> Option<&Path> {
        self.files.last().map(PathBuf::as_path)
    }

    /// The included files, lowest precedence first (everything but the root).
    pub fn included(&self) -> &[PathBuf] {
        self.files.split_last().map_or(&[], |(_, rest)| rest)
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

    /// Forget every table at or below `prefix`: a higher-precedence file
    /// replaced that value outright.
    fn forget(&mut self, prefix: &[String]) {
        self.tables.retain(|path, _| !path.starts_with(prefix));
    }
}

/// Read `root` and everything it includes, and fold them into one table.
///
/// `root` is the project's `brokkr.toml`; only it may carry `project`. An
/// include that is missing, unreadable, malformed, part of a cycle, or reached
/// twice is an error - skipping it would silently drop whatever it configured,
/// and a file folded in twice would duplicate its array entries.
pub fn compose(root: &Path) -> Result<(toml::Table, ConfigSources), DevError> {
    let mut files = Vec::new();
    let mut walk = Walk {
        stack: Vec::new(),
        seen: HashSet::new(),
        out: &mut files,
    };
    walk.visit(root, None)?;

    let mut sources = ConfigSources::default();
    let mut merged = toml::Table::new();
    for (idx, (path, table)) in files.into_iter().enumerate() {
        sources.files.push(path);
        merge_into(&mut merged, table, idx, &mut Vec::new(), &mut sources);
    }
    Ok((merged, sources))
}

/// Depth-first walk of the include graph, collecting files in post-order.
struct Walk<'a> {
    /// Canonical paths of the files currently being expanded, for cycles.
    stack: Vec<PathBuf>,
    /// Canonical paths of every file reached so far.
    seen: HashSet<PathBuf>,
    out: &'a mut Vec<(PathBuf, toml::Table)>,
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
    for (key, value) in overlay {
        path.push(key.clone());
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => {
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
                        base.insert(key, other);
                    }
                }
            }
        }
        path.pop();
    }
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
        let (_, cfg) = load(&dir).unwrap();
        assert_eq!(cfg.hosts["h"].port, Some(4000));
    }
}
