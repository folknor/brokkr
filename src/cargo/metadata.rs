//! Shared Cargo metadata model and its indexed dependency graph.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Deserializer};

use crate::error::DevError;
use crate::output;

/// The Cargo metadata query whose result a caller needs. Results are memoized
/// for this root, query, and explicit environment for the life of this brokkr
/// invocation.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) struct Query {
    pub(crate) no_deps: bool,
    pub(crate) platform: Option<String>,
    pub(crate) locked: bool,
    pub(crate) all_features: bool,
}

impl Query {
    pub(crate) fn workspace() -> Self {
        Self {
            no_deps: true,
            ..Self::default()
        }
    }

    pub(crate) fn resolving() -> Self {
        Self::default()
    }

    /// The cargo argv (without the `cargo` program) this query runs. The one
    /// construction both the loader and `--commands` displays read, so a
    /// displayed line cannot drift from the subprocess.
    pub(crate) fn cargo_args(&self) -> Vec<String> {
        let mut args = vec![
            "metadata".to_owned(),
            "--format-version".to_owned(),
            "1".to_owned(),
        ];
        if self.no_deps {
            args.push("--no-deps".to_owned());
        }
        if let Some(platform) = &self.platform {
            args.push("--filter-platform".to_owned());
            args.push(platform.clone());
        }
        if self.locked {
            args.push("--locked".to_owned());
        }
        if self.all_features {
            args.push("--all-features".to_owned());
        }
        args
    }

    /// `cargo <args>` as a display line.
    pub(crate) fn display(&self) -> String {
        format!("cargo {}", self.cargo_args().join(" "))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    root: PathBuf,
    query: Query,
    env: Vec<(String, String)>,
}

type CachedLoad = Result<Arc<CargoMetadata>, Arc<DevError>>;

static CACHE: OnceLock<Mutex<HashMap<CacheKey, CachedLoad>>> = OnceLock::new();

/// Load the typed metadata snapshot. The cache key includes every input that
/// changes Cargo's answer, including the working root and caller environment.
/// A failed query is cached too, so several phases do not repeat the same
/// failing subprocess.
pub(crate) fn load(
    root: &Path,
    query: Query,
    env: &[(&str, &str)],
    phase: Option<crate::cargo_env::Phase>,
) -> Result<Arc<CargoMetadata>, DevError> {
    let key = CacheKey {
        root: root.to_owned(),
        query,
        env: env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    };
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cached) = cache.get(&key) {
        return match cached {
            Ok(metadata) => Ok(Arc::clone(metadata)),
            Err(error) => Err(clone_error(error)),
        };
    }

    let result = load_uncached(root, &key.query, env, phase).map(Arc::new);
    let cached_result = match &result {
        Ok(metadata) => Ok(Arc::clone(metadata)),
        Err(error) => Err(Arc::new(clone_error(error))),
    };
    cache.insert(key, cached_result);
    result
}

pub(crate) fn clone_error(error: &DevError) -> DevError {
    match error {
        DevError::Io(error) => DevError::Io(std::io::Error::new(error.kind(), error.to_string())),
        DevError::Config(message) => DevError::Config(message.clone()),
        DevError::Usage(message) => DevError::Usage(message.clone()),
        DevError::WrongProject {
            command,
            expected,
            current,
        } => DevError::WrongProject {
            command: command.clone(),
            expected: expected.clone(),
            current: current.clone(),
        },
        DevError::Internal(message) => DevError::Internal(message.clone()),
        DevError::Build(message) => DevError::Build(message.clone()),
        DevError::ContractViolation(message) => DevError::ContractViolation(message.clone()),
        DevError::Preflight(failures) => DevError::Preflight(failures.clone()),
        DevError::Subprocess {
            program,
            ending,
            stderr,
        } => DevError::Subprocess {
            program: program.clone(),
            ending: *ending,
            stderr: stderr.clone(),
        },
        DevError::ChildFailure { program, failure } => DevError::ChildFailure {
            program: program.clone(),
            failure: *failure,
        },
        DevError::Spawn { program, error } => DevError::Spawn {
            program: program.clone(),
            error: std::io::Error::new(error.kind(), error.to_string()),
        },
        DevError::Reported(label) => DevError::Reported(label.clone()),
        DevError::Refused(label) => DevError::Refused(label.clone()),
        DevError::Lock(crate::error::LockError::Other(message)) => {
            DevError::Lock(crate::error::LockError::Other(message.clone()))
        }
        DevError::Lock(crate::error::LockError::Acquisition(refusal)) => DevError::Lock(
            crate::error::LockError::Acquisition(clone_lock_refusal(refusal)),
        ),
        DevError::Database(message) => DevError::Database(message.clone()),
        DevError::Verify(message) => DevError::Verify(message.clone()),
        DevError::ExitCode(code) => DevError::ExitCode(*code),
        DevError::CargoEnv(label) => DevError::CargoEnv(label.clone()),
        DevError::Interrupted => DevError::Interrupted,
    }
}

fn clone_lock_refusal(refusal: &crate::error::AcquireRefusal) -> crate::error::AcquireRefusal {
    use crate::error::AcquireRefusal;

    let clone_io = |error: &std::io::Error| std::io::Error::new(error.kind(), error.to_string());
    match refusal {
        AcquireRefusal::NestedDescendant { command } => AcquireRefusal::NestedDescendant {
            command: command.clone(),
        },
        AcquireRefusal::Busy { holder } => AcquireRefusal::Busy {
            holder: holder.clone(),
        },
        AcquireRefusal::InsideCompilation { marker } => {
            AcquireRefusal::InsideCompilation { marker }
        }
        AcquireRefusal::DiskGate(failures) => AcquireRefusal::DiskGate(failures.clone()),
        AcquireRefusal::DrainTimeout { waited } => AcquireRefusal::DrainTimeout { waited: *waited },
        AcquireRefusal::LockFileOpen { path, source } => AcquireRefusal::LockFileOpen {
            path: path.clone(),
            source: clone_io(source),
        },
        AcquireRefusal::LockFlock { source } => AcquireRefusal::LockFlock {
            source: clone_io(source),
        },
        AcquireRefusal::CompileLeaseOpen { path, source } => AcquireRefusal::CompileLeaseOpen {
            path: path.clone(),
            source: clone_io(source),
        },
        AcquireRefusal::CompileLeaseFlock { path, source } => AcquireRefusal::CompileLeaseFlock {
            path: path.clone(),
            source: clone_io(source),
        },
        AcquireRefusal::MetadataPublish { stage, source } => AcquireRefusal::MetadataPublish {
            stage: *stage,
            source: clone_io(source),
        },
        AcquireRefusal::Entropy { purpose } => AcquireRefusal::Entropy { purpose: *purpose },
        AcquireRefusal::SocketBind { path, source } => AcquireRefusal::SocketBind {
            path: path.clone(),
            source: clone_io(source),
        },
        AcquireRefusal::SocketDirectory { reason } => AcquireRefusal::SocketDirectory {
            reason: reason.clone(),
        },
        AcquireRefusal::ServiceThread { source } => AcquireRefusal::ServiceThread {
            source: clone_io(source),
        },
    }
}

fn load_uncached(
    root: &Path,
    query: &Query,
    env: &[(&str, &str)],
    phase: Option<crate::cargo_env::Phase>,
) -> Result<CargoMetadata, DevError> {
    if !root.ancestors().any(|dir| dir.join("Cargo.toml").exists()) {
        let mut message = format!(
            "no Cargo.toml in {} or any parent - brokkr runs cargo in the working \
             directory, and this one is not a Rust tree",
            root.display()
        );
        if root.join("brokkr.toml").exists() {
            message.push_str(
                ". This directory holds the brokkr.toml governing a checkout beside it; \
                 run the command from inside that checkout, not from here",
            );
        }
        return Err(DevError::Usage(message));
    }

    let args = query.cargo_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_cargo(&args, root, env, phase)?;
    if !captured.success() {
        let error = match captured.check_success("cargo metadata") {
            Err(error) => error,
            Ok(()) => DevError::Internal(
                "cargo metadata returned an unsuccessful result without a process error".into(),
            ),
        };
        return Err(with_locked_metadata_hint(error, query.locked));
    }
    let mut metadata: CargoMetadata =
        serde_json::from_slice(&captured.stdout).map_err(|error| {
            DevError::ContractViolation(format!("cargo metadata returned invalid JSON: {error}"))
        })?;
    metadata.raw_json = Some(Arc::from(
        String::from_utf8_lossy(&captured.stdout).as_ref(),
    ));
    Ok(metadata)
}

fn with_locked_metadata_hint(error: DevError, locked: bool) -> DevError {
    if !locked {
        return error;
    }

    match error {
        DevError::Subprocess {
            program,
            ending,
            stderr,
        } => {
            let hint = "`brokkr deps` runs cargo metadata with --locked: it audits Cargo.lock as \
                       committed and never rewrites it. A missing or out-of-date lockfile must be \
                       brought current by a build first.";
            let stderr = if stderr.is_empty() {
                hint.to_owned()
            } else {
                format!("{stderr}\n{hint}")
            };
            DevError::Subprocess {
                program,
                ending,
                stderr,
            }
        }
        other => other,
    }
}

/// Cargo's opaque package identity. Names and versions are not unique in a
/// resolved graph, so phase code keys graph operations by this type.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(transparent)]
pub(crate) struct PackageId(String);

impl From<&str> for PackageId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for PackageId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Cargo dependency kind shared by metadata edges, declared dependencies,
/// manifest tables, and dependency-rule diagnostics.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum DependencyKind {
    Normal,
    Dev,
    Build,
    // Metadata may grow a new kind. Metadata fields use
    // `deserialize_cargo_kind`, while config fields use this enum's strict
    // derived deserializer and cannot spell this sentinel.
    #[serde(skip_deserializing)]
    Unknown,
}

impl DependencyKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Dev => "dev",
            Self::Build => "build",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn from_metadata(value: Option<&str>) -> Self {
        match value {
            None | Some("normal") => Self::Normal,
            Some("dev") => Self::Dev,
            Some("build") => Self::Build,
            Some(_) => Self::Unknown,
        }
    }

    pub(crate) fn parse_config(value: &str) -> Option<Self> {
        match value {
            "normal" => Some(Self::Normal),
            "dev" => Some(Self::Dev),
            "build" => Some(Self::Build),
            _ => None,
        }
    }

    pub(crate) fn is_normal(self) -> bool {
        self == Self::Normal
    }
}

fn deserialize_cargo_kind<'de, D>(deserializer: D) -> Result<DependencyKind, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(DependencyKind::from_metadata(value.as_deref()))
}

fn normal_kind() -> DependencyKind {
    DependencyKind::Normal
}

#[derive(Deserialize)]
struct RawCargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<PackageId>,
    #[serde(default)]
    workspace_default_members: Option<Vec<PackageId>>,
    /// Cargo writes null under `--no-deps`.
    #[serde(default, deserialize_with = "null_as_default")]
    resolve: CargoResolve,
    #[serde(default)]
    workspace_root: String,
    #[serde(default)]
    target_directory: String,
}

/// Typed workspace, target, package, and resolve facts with an index built
/// once as JSON is deserialized.
#[derive(Clone)]
pub(crate) struct CargoMetadata {
    pub(crate) packages: Vec<CargoPackage>,
    pub(crate) workspace_root: String,
    pub(crate) target_directory: String,
    pub(crate) workspace_members: Vec<PackageId>,
    pub(crate) workspace_default_members: Option<Vec<PackageId>>,
    /// Original JSON for engines such as guppy that consume Cargo's complete
    /// wire format. Core brokkr consumers use the typed fields above.
    raw_json: Option<Arc<str>>,
    resolve: CargoResolve,
    graph: Graph,
}

impl<'de> Deserialize<'de> for CargoMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawCargoMetadata::deserialize(deserializer)?;
        let graph = Graph::build(&raw.packages, &raw.workspace_members, &raw.resolve, None);
        Ok(Self {
            packages: raw.packages,
            workspace_root: raw.workspace_root,
            target_directory: raw.target_directory,
            workspace_members: raw.workspace_members,
            workspace_default_members: raw.workspace_default_members,
            raw_json: None,
            resolve: raw.resolve,
            graph,
        })
    }
}

impl CargoMetadata {
    #[cfg(test)]
    pub(crate) fn new(
        packages: Vec<CargoPackage>,
        workspace_members: &[PackageId],
        resolve: &CargoResolve,
        workspace_root: String,
    ) -> Self {
        let graph = Graph::build(&packages, workspace_members, resolve, None);
        Self {
            packages,
            workspace_root,
            target_directory: String::new(),
            workspace_members: workspace_members.to_vec(),
            workspace_default_members: None,
            raw_json: None,
            resolve: resolve.clone(),
            graph,
        }
    }

    /// Cargo's `--filter-platform` view as a projection of the unfiltered
    /// resolving snapshot. Cargo records each resolved edge's target selector,
    /// so the host graph can be formed without launching a second metadata
    /// process.
    pub(crate) fn filtered_for_platform(&self, platform: &str) -> Self {
        let graph = Graph::build(
            &self.packages,
            &self.workspace_members,
            &self.resolve,
            Some(platform),
        );
        let reachable = graph.reachable_from_members(&self.workspace_members);
        let packages = self
            .packages
            .iter()
            .filter(|package| reachable.contains(&package.id))
            .cloned()
            .collect::<Vec<_>>();
        let resolve = self.resolve.filtered_for_platform(platform, &reachable);
        let graph = Graph::build(&packages, &self.workspace_members, &resolve, Some(platform));
        Self {
            packages,
            workspace_root: self.workspace_root.clone(),
            target_directory: self.target_directory.clone(),
            workspace_members: self.workspace_members.clone(),
            workspace_default_members: self.workspace_default_members.clone(),
            raw_json: None,
            resolve,
            graph,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn raw_json(&self) -> Option<&str> {
        self.raw_json.as_deref()
    }

    pub(crate) fn workspace_member_name_map(&self) -> HashMap<String, String> {
        self.packages
            .iter()
            .filter(|package| self.is_member(&package.id))
            .map(|package| (package.id.to_string(), package.name.clone()))
            .collect()
    }

    pub(crate) fn default_member_ids(&self) -> Vec<String> {
        self.workspace_default_members
            .as_ref()
            .unwrap_or(&self.workspace_members)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    pub(crate) fn members_are_all_default(&self) -> bool {
        let Some(defaults) = &self.workspace_default_members else {
            return false;
        };
        let members: HashSet<&PackageId> = self.workspace_members.iter().collect();
        let defaults: HashSet<&PackageId> = defaults.iter().collect();
        members == defaults
    }

    pub(crate) fn member_manifests(&self) -> HashMap<PathBuf, String> {
        self.packages
            .iter()
            .filter(|package| self.is_member(&package.id))
            .map(|package| {
                (
                    PathBuf::from(&package.manifest_path),
                    package.id.to_string(),
                )
            })
            .collect()
    }

    pub(crate) fn is_member(&self, id: &PackageId) -> bool {
        self.graph.members.contains(id)
    }

    pub(crate) fn name(&self, id: &PackageId) -> Option<&str> {
        self.graph
            .package_indexes
            .get(id)
            .and_then(|&index| self.packages.get(index))
            .map(|package| package.name.as_str())
    }

    pub(crate) fn version(&self, id: &PackageId) -> Option<&str> {
        self.graph
            .package_indexes
            .get(id)
            .and_then(|&index| self.packages.get(index))
            .map(|package| package.version.as_str())
    }

    pub(crate) fn label(&self, id: &PackageId) -> String {
        match (self.name(id), self.version(id)) {
            (Some(name), Some(version)) => format!("{name} {version}"),
            _ => id.to_string(),
        }
    }

    pub(crate) fn normal_parents(&self, id: &PackageId) -> &[PackageId] {
        self.graph
            .normal_parents
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(crate) fn resolved_dependencies(&self, id: &PackageId) -> &[ResolvedDependency] {
        self.graph
            .resolved_dependencies
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(crate) fn source_kind(&self, id: &PackageId) -> PackageSourceKind {
        self.graph
            .source_kinds
            .get(id)
            .copied()
            .unwrap_or(PackageSourceKind::Path)
    }

    /// Breadth-first paths from `target` to workspace members over Normal
    /// edges. The path is tail-first (`target` to member). Focus passes false
    /// to report a member target as the trivial chain; blame passes true to
    /// continue through a member target and find workspace routes above it.
    /// Non-member nodes are visited once, keeping converging and cyclic graphs
    /// bounded while giving both callers one traversal policy.
    pub(crate) fn normal_paths_to_members(
        &self,
        target: &PackageId,
        include_target_member: bool,
    ) -> Vec<Vec<PackageId>> {
        if self.is_member(target) && !include_target_member {
            return vec![vec![target.clone()]];
        }

        let mut queue = VecDeque::new();
        queue.push_back(vec![target.clone()]);
        let mut visited = HashSet::new();
        visited.insert(target.clone());
        let mut paths = Vec::new();
        while let Some(path) = queue.pop_front() {
            let Some(last) = path.last() else {
                continue;
            };
            for parent in self.normal_parents(last) {
                if self.is_member(parent) {
                    let mut found = path.clone();
                    found.push(parent.clone());
                    paths.push(found);
                } else if visited.insert(parent.clone()) {
                    let mut next = path.clone();
                    next.push(parent.clone());
                    queue.push_back(next);
                }
            }
        }
        paths
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PackageSourceKind {
    Registry,
    Git,
    Path,
    Member,
}

/// Indexed package identities, membership, source provenance, and reverse
/// Normal-kind edges for one Cargo metadata snapshot.
#[derive(Clone)]
pub(crate) struct Graph {
    package_indexes: HashMap<PackageId, usize>,
    members: HashSet<PackageId>,
    normal_parents: HashMap<PackageId, Vec<PackageId>>,
    resolved_dependencies: HashMap<PackageId, Vec<ResolvedDependency>>,
    source_kinds: HashMap<PackageId, PackageSourceKind>,
}

#[derive(Clone)]
pub(crate) struct ResolvedDependency {
    pub(crate) package: PackageId,
    pub(crate) kind: DependencyKind,
}

impl Graph {
    fn build(
        packages: &[CargoPackage],
        members: &[PackageId],
        resolve: &CargoResolve,
        platform: Option<&str>,
    ) -> Self {
        let members: HashSet<PackageId> = members.iter().cloned().collect();
        let package_indexes: HashMap<PackageId, usize> = packages
            .iter()
            .enumerate()
            .map(|(index, package)| (package.id.clone(), index))
            .collect();
        let source_kinds = packages
            .iter()
            .map(|package| {
                let kind = if members.contains(&package.id) {
                    PackageSourceKind::Member
                } else {
                    match package.source.as_deref() {
                        None => PackageSourceKind::Path,
                        Some(source) if source.starts_with("git+") => PackageSourceKind::Git,
                        Some(_) => PackageSourceKind::Registry,
                    }
                };
                (package.id.clone(), kind)
            })
            .collect();

        let mut parents: HashMap<PackageId, BTreeSet<PackageId>> = HashMap::new();
        let mut edges: HashMap<PackageId, BTreeSet<(PackageId, DependencyKind)>> = HashMap::new();
        for node in &resolve.nodes {
            for dep in &node.deps {
                for dep_kind in &dep.dep_kinds {
                    if platform.is_some_and(|platform| {
                        !target_applies(dep_kind.target.as_deref(), platform)
                    }) {
                        continue;
                    }
                    edges
                        .entry(node.id.clone())
                        .or_default()
                        .insert((dep.pkg.clone(), dep_kind.kind));
                    if dep_kind.kind.is_normal() {
                        parents
                            .entry(dep.pkg.clone())
                            .or_default()
                            .insert(node.id.clone());
                    }
                }
            }
        }
        let normal_parents = parents
            .into_iter()
            .map(|(id, parents)| (id, parents.into_iter().collect()))
            .collect();
        let resolved_dependencies = edges
            .into_iter()
            .map(|(id, edges)| {
                (
                    id,
                    edges
                        .into_iter()
                        .map(|(package, kind)| ResolvedDependency { package, kind })
                        .collect(),
                )
            })
            .collect();

        Self {
            package_indexes,
            members,
            normal_parents,
            resolved_dependencies,
            source_kinds,
        }
    }

    fn reachable_from_members(&self, members: &[PackageId]) -> HashSet<PackageId> {
        let mut reachable = HashSet::new();
        let mut queue = VecDeque::new();
        for member in members {
            if reachable.insert(member.clone()) {
                queue.push_back(member.clone());
            }
        }
        while let Some(package) = queue.pop_front() {
            for dependency in self
                .resolved_dependencies
                .get(&package)
                .into_iter()
                .flatten()
            {
                if reachable.insert(dependency.package.clone()) {
                    queue.push_back(dependency.package.clone());
                }
            }
        }
        reachable
    }
}

fn target_applies(selector: Option<&str>, platform: &str) -> bool {
    let Some(selector) = selector else {
        return true;
    };
    if let Some(expression) = selector
        .strip_prefix("cfg(")
        .and_then(|value| value.strip_suffix(')'))
    {
        // An unknown selector is retained: omitting an edge from a diagnostic
        // graph is more misleading than including one whose cfg we cannot
        // evaluate.
        return crate::rustflags::eval_cfg_for_host(expression, Some(platform)).unwrap_or(true);
    }
    selector == platform
}

#[derive(Clone, Deserialize)]
pub(crate) struct CargoPackage {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) version: String,
    pub(crate) id: PackageId,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) manifest_path: String,
    #[serde(default)]
    pub(crate) links: Option<String>,
    #[serde(default)]
    pub(crate) publish: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) dependencies: Vec<PackageDep>,
    #[serde(default)]
    pub(crate) targets: Vec<CargoTarget>,
    #[serde(default)]
    pub(crate) features: HashMap<String, Vec<String>>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) authors: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) homepage: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) license: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) license_file: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) repository: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) rust_version: Option<String>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct CargoTarget {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) kind: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) crate_types: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) doctest: Option<bool>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) harness: Option<bool>,
}

/// One entry of `packages[].dependencies`.
#[derive(Clone, Deserialize)]
pub(crate) struct PackageDep {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) rename: Option<String>,
    #[serde(default = "normal_kind", deserialize_with = "deserialize_cargo_kind")]
    pub(crate) kind: DependencyKind,
    #[serde(default)]
    pub(crate) target: Option<String>,
    #[serde(default)]
    pub(crate) optional: bool,
    /// Cargo renders "none specified" - the path-only form that publish strips
    /// from a dev-dep - as `*`.
    #[serde(default)]
    pub(crate) req: String,
}

#[derive(Clone, Deserialize, Default)]
pub(crate) struct CargoResolve {
    #[serde(default)]
    pub(crate) root: Option<PackageId>,
    #[serde(default)]
    pub(crate) nodes: Vec<ResolveNode>,
}

impl CargoResolve {
    fn filtered_for_platform(&self, platform: &str, reachable: &HashSet<PackageId>) -> Self {
        let nodes = self
            .nodes
            .iter()
            .filter(|node| reachable.contains(&node.id))
            .map(|node| ResolveNode {
                id: node.id.clone(),
                deps: node
                    .deps
                    .iter()
                    .filter_map(|dep| {
                        let dep_kinds: Vec<DepKind> = dep
                            .dep_kinds
                            .iter()
                            .filter(|kind| target_applies(kind.target.as_deref(), platform))
                            .cloned()
                            .collect();
                        (!dep_kinds.is_empty()).then(|| NodeDep {
                            pkg: dep.pkg.clone(),
                            dep_kinds,
                        })
                    })
                    .collect(),
            })
            .collect();
        Self {
            root: self.root.clone(),
            nodes,
        }
    }
}

#[derive(Clone, Deserialize)]
pub(crate) struct ResolveNode {
    pub(crate) id: PackageId,
    #[serde(default)]
    pub(crate) deps: Vec<NodeDep>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct NodeDep {
    pub(crate) pkg: PackageId,
    #[serde(default)]
    pub(crate) dep_kinds: Vec<DepKind>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct DepKind {
    #[serde(default = "normal_kind", deserialize_with = "deserialize_cargo_kind")]
    pub(crate) kind: DependencyKind,
    #[serde(default)]
    pub(crate) target: Option<String>,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn metadata_deserialization_builds_one_typed_graph() {
        let json = serde_json::json!({
            "packages": [
                { "name": "app", "id": "member" },
                { "name": "transitive", "id": "transitive" },
                { "name": "leaf", "id": "leaf" },
                { "name": "dev-only", "id": "dev-only" },
                { "name": "registry-crate", "id": "registry", "source": "registry+https://example.com/index" },
                { "name": "git-crate", "id": "git", "source": "git+https://example.com/repo#abc" },
                { "name": "path-crate", "id": "path" }
            ],
            "workspace_members": ["member"],
            "resolve": {
                "nodes": [
                    { "id": "transitive", "deps": [
                        { "pkg": "leaf", "dep_kinds": [{ "kind": null }] }
                    ] },
                    { "id": "member", "deps": [
                        { "pkg": "transitive", "dep_kinds": [{ "kind": null }] },
                        { "pkg": "dev-only", "dep_kinds": [{ "kind": "dev" }] }
                    ] }
                ]
            },
            "workspace_root": "/workspace"
        });
        let metadata: CargoMetadata = serde_json::from_value(json).unwrap();

        assert!(metadata.is_member(&PackageId::from("member")));
        assert_eq!(
            metadata.normal_parents(&PackageId::from("leaf")),
            &[PackageId::from("transitive")]
        );
        assert!(
            metadata
                .normal_parents(&PackageId::from("dev-only"))
                .is_empty()
        );
        assert_eq!(
            metadata.source_kind(&PackageId::from("member")),
            PackageSourceKind::Member
        );
        assert_eq!(
            metadata.source_kind(&PackageId::from("registry")),
            PackageSourceKind::Registry
        );
        assert_eq!(
            metadata.source_kind(&PackageId::from("git")),
            PackageSourceKind::Git
        );
        assert_eq!(
            metadata.source_kind(&PackageId::from("path")),
            PackageSourceKind::Path
        );
        assert_eq!(
            metadata.normal_paths_to_members(&PackageId::from("leaf"), false),
            vec![vec![
                PackageId::from("leaf"),
                PackageId::from("transitive"),
                PackageId::from("member"),
            ]]
        );
    }

    // Order must not matter: Cargo promises the same package IDs in both
    // lists, not the same sequence.
    #[test]
    fn whole_workspace_is_recognized_regardless_of_order() {
        let metadata: CargoMetadata = serde_json::from_value(serde_json::json!({
            "packages": [],
            "workspace_members": ["id-a", "id-b"],
            "workspace_default_members": ["id-b", "id-a"]
        }))
        .unwrap();
        assert!(metadata.members_are_all_default());
    }

    // A default-members subset means bare cargo selection builds less than
    // the workspace.
    #[test]
    fn a_default_members_subset_is_not_the_whole_workspace() {
        let metadata: CargoMetadata = serde_json::from_value(serde_json::json!({
            "packages": [],
            "workspace_members": ["id-a", "id-b"],
            "workspace_default_members": ["id-a"]
        }))
        .unwrap();
        assert!(!metadata.members_are_all_default());
    }

    // Older Cargo omits this field; selection remains conservative.
    #[test]
    fn missing_default_members_is_not_the_whole_workspace() {
        let metadata: CargoMetadata = serde_json::from_value(serde_json::json!({
            "packages": [],
            "workspace_members": ["id-a"]
        }))
        .unwrap();
        assert!(!metadata.members_are_all_default());
        assert_eq!(metadata.default_member_ids(), vec!["id-a"]);
    }
}
